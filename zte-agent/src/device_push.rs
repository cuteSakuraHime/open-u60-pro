//! Direct APNs wake-ups for SMS and incoming calls.
//!
//! APNs only sees an opaque, short-lived capability. SMS/caller data stays in
//! memory on the router and can be fetched once, over the LAN, by the phone.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::{
    ecdsa::{signature::Signer, Signature, SigningKey},
    pkcs8::DecodePrivateKey,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const REGISTRATION_FILE: &str = "/data/open-u60/push/registration.json";
const EVENT_TTL: u64 = 120;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Environment {
    Sandbox,
    Production,
}
impl Environment {
    fn host(self) -> &'static str {
        match self {
            Self::Sandbox => "https://api.sandbox.push.apple.com",
            Self::Production => "https://api.push.apple.com",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    enabled: bool,
    team_id: String,
    key_id: String,
    bundle_id: String,
    key_file: String,
    environment: Environment,
}
struct Provider {
    config: Config,
    key: SigningKey,
}

#[derive(Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    #[serde(default)]
    notification_token: Option<String>,
    #[serde(default)]
    voip_token: Option<String>,
    /// Informational only. The router's protected provider config is authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
}
struct Event {
    expires_at: u64,
    value: Value,
}
#[derive(Default)]
struct State {
    registration: Registration,
    events: HashMap<String, Event>,
    last_error: Option<String>,
    last_sms_push_at: Option<u64>,
    last_call_push_at: Option<u64>,
}
pub struct DevicePush {
    provider: Option<Provider>,
    state: Mutex<State>,
    client: Option<reqwest::blocking::Client>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
fn private_file(path: &Path) -> Result<String, &'static str> {
    let meta = fs::symlink_metadata(path).map_err(|_| "private file unavailable")?;
    if !meta.is_file()
        || meta.mode() & 0o077 != 0
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.len() > 16384
    {
        return Err(
            "private file must be owned by agent user, regular, <=16 KiB, and mode 0600 or 0400",
        );
    }
    fs::read_to_string(path).map_err(|_| "cannot read private file")
}
fn valid_identifier(value: &str) -> bool {
    value.len() == 10
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}
fn valid_bundle(value: &str) -> bool {
    value.contains('.')
        && value.len() <= 255
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}
fn load_provider() -> Result<Option<Provider>, &'static str> {
    let Ok(path) = std::env::var("ZTE_AGENT_PUSH_CONFIG") else {
        return Ok(None);
    };
    let config: Config = serde_json::from_str(&private_file(Path::new(&path))?)
        .map_err(|_| "invalid push config JSON")?;
    if !config.enabled {
        return Ok(None);
    }
    if !valid_identifier(&config.team_id)
        || !valid_identifier(&config.key_id)
        || !valid_bundle(&config.bundle_id)
        || !Path::new(&config.key_file).is_absolute()
    {
        return Err("invalid push provider configuration");
    }
    let key = SigningKey::from_pkcs8_pem(&private_file(Path::new(&config.key_file))?)
        .map_err(|_| "key must be an Apple ES256 PKCS#8 PEM .p8 key")?;
    Ok(Some(Provider { config, key }))
}
fn valid_token(value: &str) -> bool {
    (32..=1024).contains(&value.len())
        && value.len().is_multiple_of(2)
        && value.bytes().all(|b| b.is_ascii_hexdigit())
}

impl DevicePush {
    pub fn new() -> Self {
        let (provider, initial_error) = match load_provider() {
            Ok(provider) => (provider, None),
            Err(error) => (None, Some(error.to_owned())),
        };
        let registration = fs::read_to_string(REGISTRATION_FILE)
            .ok()
            .and_then(|raw| serde_json::from_str::<Registration>(&raw).ok())
            .filter(|r| {
                r.notification_token.as_deref().is_none_or(valid_token)
                    && r.voip_token.as_deref().is_none_or(valid_token)
            })
            .unwrap_or_default();
        let client = reqwest::blocking::Client::builder()
            .https_only(true)
            .http2_prior_knowledge()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .ok();
        Self {
            provider,
            client,
            state: Mutex::new(State {
                registration,
                last_error: initial_error,
                ..State::default()
            }),
        }
    }
    pub fn status(&self) -> (u16, Value) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (
            200,
            json!({"ok":true,"data":{"enabled":self.provider.is_some(),"notification_registered":state.registration.notification_token.is_some(),
            "voip_registered":state.registration.voip_token.is_some(),"last_sms_push_at":state.last_sms_push_at,
            "last_call_push_at":state.last_call_push_at,"last_error":state.last_error}}),
        )
    }
    pub fn register(&self, body: &[u8]) -> (u16, Value) {
        if self.provider.is_none() {
            return (
                503,
                json!({"ok":false,"error":"push provider is disabled or invalid"}),
            );
        }
        let registration: Registration = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return (400, json!({"ok":false,"error":"invalid registration JSON"})),
        };
        if registration
            .environment
            .as_deref()
            .is_some_and(|value| !matches!(value, "sandbox" | "production"))
        {
            return (
                400,
                json!({"ok":false,"error":"environment must be sandbox or production"}),
            );
        }
        if registration
            .notification_token
            .as_deref()
            .is_some_and(|t| !valid_token(t))
            || registration
                .voip_token
                .as_deref()
                .is_some_and(|t| !valid_token(t))
        {
            return (
                400,
                json!({"ok":false,"error":"push tokens must be hexadecimal"}),
            );
        }
        if registration.notification_token.is_none() && registration.voip_token.is_none() {
            return (
                400,
                json!({"ok":false,"error":"at least one push token is required"}),
            );
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(token) = registration.notification_token {
            state.registration.notification_token = Some(token)
        }
        if let Some(token) = registration.voip_token {
            state.registration.voip_token = Some(token)
        }
        if registration.environment.is_some() {
            state.registration.environment = registration.environment
        }
        if let Err(error) = persist_registration(&state.registration) {
            return (500, json!({"ok":false,"error":error}));
        }
        (200, json!({"ok":true}))
    }
    pub fn unregister(&self) -> (u16, Value) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.registration = Registration::default();
        let _ = fs::remove_file(REGISTRATION_FILE);
        (200, json!({"ok":true}))
    }
    /// Deliberately unauthenticated: the capability is unguessable, short-lived and one-shot.
    pub fn take_event(&self, token: &str) -> (u16, Value) {
        if token.len() != 43
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return not_found();
        }
        let clock = now();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.events.retain(|_, event| event.expires_at > clock);
        match state.events.remove(token) {
            Some(event) => (200, json!({"ok":true,"data":event.value})),
            None => not_found(),
        }
    }
    pub fn start(
        self: &Arc<Self>,
        app: Arc<crate::handlers::AppState>,
        sms_events: mpsc::Receiver<Value>,
    ) {
        if self.provider.is_none() {
            return;
        }
        let sms = Arc::clone(self);
        let sms_app = Arc::clone(&app);
        crate::system::spawn_named("push-sms", move || {
            for event in sms_events {
                if event["wms_status"].as_str() != Some("new_sms_received") {
                    continue;
                }
                let body = br#"{"page":0,"data_per_page":1,"mem_store":2,"tags":10,"order_by":"order by id desc"}"#;
                let (status, reply) = crate::sms::sms_list(&sms_app, body);
                if status == 200 {
                    let message = reply["data"]["messages"]
                        .as_array()
                        .and_then(|m| m.first())
                        .cloned()
                        .unwrap_or(Value::Null);
                    sms.push("sms", json!({"kind":"sms","message":message}));
                }
            }
        });
        let calls = Arc::clone(self);
        crate::system::spawn_named("push-call", move || calls.call_watch(&app));
    }
    fn call_watch(&self, app: &crate::handlers::AppState) {
        let mut announced: Option<i64> = None;
        loop {
            let enabled = self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .registration
                .voip_token
                .is_some();
            if !enabled {
                announced = None;
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
            match crate::telephony::read_voice_calls_for_push(app) {
                Ok(calls) => {
                    let incoming = calls.into_iter().find(|call| {
                        call["dir"] == "mt"
                            && matches!(call["stat"].as_str(), Some("incoming" | "waiting"))
                    });
                    let id = incoming.as_ref().and_then(|c| c["id"].as_i64());
                    if id.is_some() && id != announced {
                        self.push("voip", json!({"kind":"call","call":incoming.unwrap()}));
                    }
                    announced = id;
                }
                Err(error) => self.set_error(format!("call status: {error}")),
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    fn push(&self, kind: &str, value: Value) {
        let Some(provider) = &self.provider else {
            return;
        };
        let Some(client) = &self.client else {
            self.set_error("APNs client initialization failed".into());
            return;
        };
        if now() < 1_700_000_000 {
            self.set_error("device clock is not valid".into());
            return;
        }
        let mut bytes = [0u8; 32];
        if getrandom::getrandom(&mut bytes).is_err() {
            self.set_error("event token entropy unavailable".into());
            return;
        }
        let capability = URL_SAFE_NO_PAD.encode(bytes);
        let device_token = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.events.retain(|_, event| event.expires_at > now());
            state.events.insert(
                capability.clone(),
                Event {
                    expires_at: now() + EVENT_TTL,
                    value,
                },
            );
            if kind == "voip" {
                state.registration.voip_token.clone()
            } else {
                state.registration.notification_token.clone()
            }
        };
        let Some(device_token) = device_token else {
            return;
        };
        let payload = if kind == "voip" {
            json!({"aps":{"content-available":1},"gateway":"192.168.0.1","event_token":capability})
        } else {
            json!({"aps":{"alert":{"title":"收到新短信","body":"连接 U60 Pro 后读取内容"},"sound":"default","mutable-content":1,"category":"local-router-sms"},"gateway":"192.168.0.1","event_token":capability})
        };
        let topic = if kind == "voip" {
            format!("{}.voip", provider.config.bundle_id)
        } else {
            provider.config.bundle_id.clone()
        };
        let result = client
            .post(format!(
                "{}/3/device/{device_token}",
                provider.config.environment.host()
            ))
            .bearer_auth(provider_token(provider, now()))
            .header("apns-topic", topic)
            .header(
                "apns-push-type",
                if kind == "voip" { "voip" } else { "alert" },
            )
            .header("apns-priority", "10")
            .header("apns-expiration", (now() + EVENT_TTL).to_string())
            .json(&payload)
            .send();
        match result {
            Ok(response) if response.status().as_u16() == 200 => {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state.last_error = None;
                if kind == "voip" {
                    state.last_call_push_at = Some(now())
                } else {
                    state.last_sms_push_at = Some(now())
                }
            }
            Ok(response) => {
                let status = response.status();
                let mut body = String::new();
                let _ = response.take(4096).read_to_string(&mut body);
                self.set_error(format!("APNs rejected {kind} push ({status}): {body}"));
            }
            Err(_) => self.set_error(format!("APNs transport failed for {kind} push")),
        }
    }
    fn set_error(&self, error: String) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_error = Some(error);
    }
}
fn not_found() -> (u16, Value) {
    (404, json!({"ok":false,"error":"event not found"}))
}
fn persist_registration(registration: &Registration) -> Result<(), &'static str> {
    let path = Path::new(REGISTRATION_FILE);
    fs::create_dir_all(path.parent().unwrap()).map_err(|_| "cannot create push state directory")?;
    let temp = path.with_extension("tmp");
    fs::write(
        &temp,
        serde_json::to_vec(registration).map_err(|_| "cannot encode push registration")?,
    )
    .map_err(|_| "cannot write push registration")?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot protect push registration")?;
    fs::rename(temp, path).map_err(|_| "cannot install push registration")
}
fn provider_token(provider: &Provider, issued: u64) -> String {
    let header =
        URL_SAFE_NO_PAD.encode(json!({"alg":"ES256","kid":provider.config.key_id}).to_string());
    let claims =
        URL_SAFE_NO_PAD.encode(json!({"iss":provider.config.team_id,"iat":issued}).to_string());
    let input = format!("{header}.{claims}");
    let signature: Signature = provider.key.sign(input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_reject_header_injection() {
        assert!(valid_token(&"ab".repeat(32)));
        assert!(!valid_token("ab\r\napns-topic:x"));
    }
    #[test]
    fn event_capabilities_are_one_shot() {
        let push = DevicePush {
            provider: None,
            client: None,
            state: Mutex::new(State::default()),
        };
        let token = URL_SAFE_NO_PAD.encode([7u8; 32]);
        push.state.lock().unwrap().events.insert(
            token.clone(),
            Event {
                expires_at: now() + 10,
                value: json!({"kind":"sms"}),
            },
        );
        assert_eq!(push.take_event(&token).0, 200);
        assert_eq!(push.take_event(&token).0, 404);
    }
}
