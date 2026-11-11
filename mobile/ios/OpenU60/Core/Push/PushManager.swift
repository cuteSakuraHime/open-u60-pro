import Foundation
import PushKit
import UIKit
import UserNotifications
import os

private let pushLogger = Logger(subsystem: "com.zte.companion", category: "Push")

/// Receives UIApplication remote-notification callbacks and forwards them to the process-wide
/// push coordinator. Keeping this adapter dependency-free lets SwiftUI create it before the app
/// has assembled AgentClient and CallViewModel.
final class PushAppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication,
                     didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        Task { @MainActor in PushManager.shared.setNotificationToken(deviceToken) }
    }

    func application(_ application: UIApplication,
                     didFailToRegisterForRemoteNotificationsWithError error: Error) {
        pushLogger.error("Remote notification registration failed: \(error.localizedDescription, privacy: .public)")
    }

    func application(_ application: UIApplication,
                     didReceiveRemoteNotification userInfo: [AnyHashable: Any],
                     fetchCompletionHandler completionHandler: @escaping (UIBackgroundFetchResult) -> Void) {
        // Establishes the ordinary silent-push wake-up path. Event detail retrieval remains in the
        // Notification Service Extension, so no broad credential is needed in this payload.
        completionHandler(.noData)
    }
}

/// Owns APNs/PushKit registration and bridges a VoIP wake-up into the existing CallKit lifecycle.
///
/// The server registration contract is:
/// `PUT /api/push/register`
/// `{ notification_token, voip_token, environment }`.
/// Both device tokens are hexadecimal strings. The request uses AgentClient, so its current
/// bearer session is attached by the existing authenticated transport rather than placed in APNs.
@MainActor
final class PushManager: NSObject, PKPushRegistryDelegate {
    static let shared = PushManager()

    private enum DefaultsKey {
        static let notificationToken = "push_notification_token"
        static let voipToken = "push_voip_token"
    }

    private var client: AgentClient?
    private weak var callModel: CallViewModel?
    private var voipRegistry: PKPushRegistry?
    private var registrationTask: Task<Void, Never>?
    private var lastRegistrationFingerprint: String?

    private var notificationToken: String? = UserDefaults.standard.string(forKey: DefaultsKey.notificationToken)
    private var voipToken: String? = UserDefaults.standard.string(forKey: DefaultsKey.voipToken)

    private override init() {
        super.init()
    }

    func configure(client: AgentClient, callModel: CallViewModel) {
        self.client = client
        self.callModel = callModel
    }

    func start() {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert, .badge, .sound]) { granted, error in
            if let error {
                pushLogger.error("Notification authorization failed: \(error.localizedDescription, privacy: .public)")
            } else if !granted {
                pushLogger.info("Notification authorization was not granted")
            }
            Task { @MainActor in UIApplication.shared.registerForRemoteNotifications() }
        }

        if voipRegistry == nil {
            let registry = PKPushRegistry(queue: .main)
            registry.delegate = self
            registry.desiredPushTypes = [.voIP]
            voipRegistry = registry
        }
        scheduleRegistration()
    }

    /// Retry after login/reauthentication or after the selected gateway changes.
    func refreshRegistration() {
        lastRegistrationFingerprint = nil
        scheduleRegistration()
    }

    func setNotificationToken(_ data: Data) {
        notificationToken = data.hexadecimalString
        UserDefaults.standard.set(notificationToken, forKey: DefaultsKey.notificationToken)
        scheduleRegistration()
    }

    private func setVoIPToken(_ data: Data) {
        voipToken = data.hexadecimalString
        UserDefaults.standard.set(voipToken, forKey: DefaultsKey.voipToken)
        scheduleRegistration()
    }

    private func scheduleRegistration() {
        registrationTask?.cancel()
        registrationTask = Task { [weak self] in
            guard let self else { return }
            await self.registerTokensIfReady()
        }
    }

    private func registerTokensIfReady() async {
        guard let client, client.token?.isEmpty == false,
              let notificationToken, let voipToken else { return }

        let environment = Bundle.main.object(forInfoDictionaryKey: "PushEnvironment") as? String ?? "sandbox"
        let fingerprint = [client.baseURL, notificationToken, voipToken, environment].joined(separator: "|")
        guard fingerprint != lastRegistrationFingerprint else { return }

        do {
            _ = try await client.putJSON("/api/push/register", body: [
                "notification_token": notificationToken,
                "voip_token": voipToken,
                "environment": environment
            ])
            guard !Task.isCancelled else { return }
            lastRegistrationFingerprint = fingerprint
            pushLogger.info("Push tokens registered for \(environment, privacy: .public)")
        } catch {
            guard !error.isCancellation else { return }
            pushLogger.error("Push token registration failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    // MARK: - PKPushRegistryDelegate

    nonisolated func pushRegistry(_ registry: PKPushRegistry,
                                  didUpdate pushCredentials: PKPushCredentials,
                                  for type: PKPushType) {
        guard type == .voIP else { return }
        let token = pushCredentials.token
        Task { @MainActor in self.setVoIPToken(token) }
    }

    nonisolated func pushRegistry(_ registry: PKPushRegistry,
                                  didInvalidatePushTokenFor type: PKPushType) {
        guard type == .voIP else { return }
        Task { @MainActor in
            self.voipToken = nil
            self.lastRegistrationFingerprint = nil
            UserDefaults.standard.removeObject(forKey: DefaultsKey.voipToken)
        }
    }

    nonisolated func pushRegistry(_ registry: PKPushRegistry,
                                  didReceiveIncomingPushWith payload: PKPushPayload,
                                  for type: PKPushType,
                                  completion: @escaping () -> Void) {
        guard type == .voIP else {
            completion()
            return
        }

        // This registry is constructed on DispatchQueue.main, so synchronous actor access is safe
        // and keeps the required CallKit report ahead of PushKit's completion deadline.
        MainActor.assumeIsolated {
            // APNs carries only call-display metadata. Credentials and router passwords never
            // belong here; the model later reconciles against the authenticated local agent.
            let dictionary = payload.dictionaryPayload
            let number = (dictionary["caller_number"] as? String)
                ?? (dictionary["number"] as? String)
                ?? (dictionary["caller"] as? String)
            callModel?.receiveVoIPPush(number: number)
        }
        completion()
    }
}

private extension Data {
    var hexadecimalString: String {
        map { String(format: "%02x", $0) }.joined()
    }
}
