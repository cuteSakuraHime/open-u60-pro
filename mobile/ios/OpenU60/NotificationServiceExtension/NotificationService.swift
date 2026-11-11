import Foundation
import UserNotifications

/// APNs payload contract for SMS notifications:
/// `{ "aps": { "mutable-content": 1, "alert": { "title": "", "body": "" } },
///    "gateway": "192.168.0.1", "event_token": "<single-use opaque token>" }`
///
/// `event_token` authorizes only one event fetch. Router credentials and the AgentClient bearer
/// token are deliberately absent. The extension exchanges it over the local network for details.
final class NotificationService: UNNotificationServiceExtension {
    private var contentHandler: ((UNNotificationContent) -> Void)?
    private var bestAttemptContent: UNMutableNotificationContent?
    private var fetchTask: URLSessionDataTask?
    private let finishLock = NSLock()

    override func didReceive(_ request: UNNotificationRequest,
                             withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void) {
        self.contentHandler = contentHandler
        let content = (request.content.mutableCopy() as? UNMutableNotificationContent)
            ?? UNMutableNotificationContent()
        bestAttemptContent = content
        applySafeFallback(to: content)

        guard let gateway = request.content.userInfo["gateway"] as? String,
              let eventToken = request.content.userInfo["event_token"] as? String,
              let url = eventURL(gateway: gateway, token: eventToken) else {
            finish(with: content)
            return
        }

        var urlRequest = URLRequest(url: url)
        urlRequest.httpMethod = "GET"
        urlRequest.cachePolicy = .reloadIgnoringLocalCacheData
        urlRequest.timeoutInterval = 8
        // Intentionally no Authorization header: event_token is narrow and single-use.
        fetchTask = URLSession.shared.dataTask(with: urlRequest) { [weak self] data, response, _ in
            guard let self else { return }
            guard let http = response as? HTTPURLResponse,
                  (200...299).contains(http.statusCode),
                  let data,
                  let event = Self.decodeEvent(data) else {
                self.finish(with: content)
                return
            }
            Self.rewrite(content, with: event)
            self.finish(with: content)
        }
        fetchTask?.resume()
    }

    override func serviceExtensionTimeWillExpire() {
        fetchTask?.cancel()
        if let bestAttemptContent { finish(with: bestAttemptContent) }
    }

    private func finish(with content: UNNotificationContent) {
        finishLock.lock()
        let handler = contentHandler
        contentHandler = nil
        finishLock.unlock()
        handler?(content)
    }

    private func applySafeFallback(to content: UNMutableNotificationContent) {
        if content.title.isEmpty { content.title = "U60 新消息" }
        if content.body.isEmpty { content.body = "请打开 OpenU60 查看详情" }
    }

    private func eventURL(gateway: String, token: String) -> URL? {
        guard !gateway.isEmpty, !token.isEmpty else { return nil }
        let rawBase = gateway.contains("://") ? gateway : "http://\(gateway)"
        guard var components = URLComponents(string: rawBase),
              components.scheme?.lowercased() == "http",
              components.host != nil,
              components.user == nil,
              components.password == nil else { return nil }
        if components.port == nil { components.port = 9090 }
        components.path = ""
        components.query = nil
        components.fragment = nil
        return components.url?
            .appendingPathComponent("api/push/event", isDirectory: true)
            .appendingPathComponent(token, isDirectory: false)
    }

    private static func decodeEvent(_ data: Data) -> [String: Any]? {
        guard let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        if let ok = root["ok"] as? Bool, !ok { return nil }
        return (root["data"] as? [String: Any]) ?? root
    }

    private static func rewrite(_ content: UNMutableNotificationContent, with event: [String: Any]) {
        // The router wraps the firmware SMS row so the event can also carry a `kind` field.
        let details = (event["message"] as? [String: Any]) ?? event
        let rawMessage = firstString(in: details, keys: ["body", "message", "content", "text"])
        let message = rawMessage.map(decodeUCS2IfNeeded)
        let sender = firstString(in: details, keys: ["sender", "from", "phone", "number"])

        if let code = verificationCode(in: message ?? "") {
            content.title = "验证码：\(code)"
        } else if let sender, !sender.isEmpty {
            content.title = "来自 \(sender) 的短信"
        } else {
            content.title = "U60 新短信"
        }
        if let message, !message.isEmpty { content.body = message }
    }

    private static func firstString(in event: [String: Any], keys: [String]) -> String? {
        for key in keys {
            if let value = event[key] as? String { return value }
        }
        return nil
    }

    private static func decodeUCS2IfNeeded(_ value: String) -> String {
        guard !value.isEmpty, value.count.isMultiple(of: 4),
              value.unicodeScalars.allSatisfy({ CharacterSet(charactersIn: "0123456789abcdefABCDEF").contains($0) }) else {
            return value
        }
        var units: [UInt16] = []
        var index = value.startIndex
        while index < value.endIndex {
            let next = value.index(index, offsetBy: 4)
            guard let unit = UInt16(value[index..<next], radix: 16) else { return value }
            units.append(unit)
            index = next
        }
        return String(decoding: units, as: UTF16.self)
    }

    private static func verificationCode(in message: String) -> String? {
        let range = NSRange(message.startIndex..., in: message)
        let patterns = [
            #"(?i)(?:验证码|校验码|动态码|verification\s*code|code|otp)[^0-9]{0,12}([0-9]{4,8})"#,
            #"(?<![0-9])([0-9]{4,8})(?![0-9])"#
        ]
        for pattern in patterns {
            guard let regex = try? NSRegularExpression(pattern: pattern),
                  let match = regex.firstMatch(in: message, range: range),
                  match.numberOfRanges > 1,
                  let codeRange = Range(match.range(at: 1), in: message) else { continue }
            return String(message[codeRange])
        }
        return nil
    }
}
