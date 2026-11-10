import SwiftUI
import os

private let logger = Logger(subsystem: "com.zte.companion", category: "Call")

enum CallState: Equatable {
    case idle
    case dialing
    case alerting
    case active
    case incoming(from: String)
}

@Observable
@MainActor
final class CallViewModel {
    var phoneNumber: String = ""
    var callState: CallState = .idle
    var isMuted: Bool = false
    var callDuration: TimeInterval = 0
    var error: String?
    var showKeypad: Bool = false

    private let client: AgentClient
    private let authManager: AuthManager
    private let pollLoop = PollingLoop()
    private let durationLoop = PollingLoop()
    private var connectionGeneration = 0

    /// Forget local state when authentication or the router changes; never sends hangup.
    func resetConnection() {
        connectionGeneration += 1
        // The router call may continue; only the local system UI is closed.
        callKit.reportEnded()
        voice.stop()
        history.abandon()
        stopPolling()
        stopDurationTimer()
        callState = .idle
        phoneNumber = ""
        isMuted = false
        showKeypad = false
        error = nil
    }

    private let callKit = CallKitManager()
    private let voice = VoiceStream()
    let history = CallHistoryStore()
    /// Recording is opt-in; recordings stay in Application Support on this phone.
    var recordCalls = UserDefaults.standard.bool(forKey: "recordCalls") {
        didSet { UserDefaults.standard.set(recordCalls, forKey: "recordCalls") }
    }

    init(client: AgentClient, authManager: AuthManager) {
        self.client = client
        self.authManager = authManager
        callKit.onDial = { [weak self] number in await self?.performDial(number) ?? false }
        callKit.onAnswer = { [weak self] in await self?.performAnswer() ?? false }
        callKit.onEnd = { [weak self] in await self?.performHangup() ?? false }
        callKit.onAudioActivated = { [weak self] in self?.startVoice() }
        callKit.onAudioDeactivated = { [weak self] in self?.voice.stop() }
        callKit.onMute = { [weak self] muted in self?.voice.isMuted = muted }
    }

    private func startVoice() {
        guard let host = URL(string: client.baseURL)?.host else { return }
        var url: URL?
        if recordCalls {
            let folder = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
                .appendingPathComponent("Recordings", isDirectory: true)
            let stamp = ISO8601DateFormatter().string(from: Date()).replacingOccurrences(of: ":", with: "-")
            url = folder.appendingPathComponent("call-\(stamp).wav")
        }
        do {
            try voice.start(router: host, recordTo: url)
        } catch {
            voice.stop()
            logger.error("Voice stream failed: \(error.localizedDescription, privacy: .public)")
            self.error = "通话音频启动失败"
        }
    }

    // MARK: - Number Input

    func appendDigit(_ digit: String) {
        phoneNumber.append(digit)
        if callState == .active {
            Task { await sendDTMF(digit) }
        }
    }

    func deleteDigit() {
        guard !phoneNumber.isEmpty else { return }
        phoneNumber.removeLast()
    }

    // MARK: - Call Actions

    /// UI entry points go through CallKit so the system call UI and Recents stay in sync.
    func dial() async {
        let number = phoneNumber.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !number.isEmpty else { return }
        error = nil
        callKit.startOutgoing(number: number)
    }

    func hangup() async {
        if callKit.currentCall != nil {
            callKit.requestEnd()
        } else {
            _ = await performHangup()
        }
    }

    func answer() async {
        if callKit.currentCall != nil {
            callKit.requestAnswer()
        } else {
            _ = await performAnswer()
        }
    }

    private func performDial(_ number: String) async -> Bool {
        let generation = connectionGeneration
        phoneNumber = number
        callState = .dialing
        do {
            let _ = try await client.postJSON("/api/call/dial", body: ["number": number])
            guard generation == connectionGeneration else { return false }
            startPolling()
            return true
        } catch {
            guard generation == connectionGeneration else { return false }
            callState = .idle
            if !error.isCancellation { self.error = error.localizedDescription }
            // The POST can time out after the router already placed the call.
            startPolling()
            return false
        }
    }

    private func performHangup() async -> Bool {
        let generation = connectionGeneration
        error = nil
        var accepted = true
        do {
            let _ = try await client.postJSON("/api/call/hangup")
        } catch {
            accepted = false
            guard generation == connectionGeneration else { return false }
            if !error.isCancellation { self.error = "挂断尚未确认：\(error.localizedDescription)" }
        }
        guard generation == connectionGeneration else { return false }
        // A successful command is not proof the modem released the call.
        _ = await pollCallStatus()
        guard generation == connectionGeneration else { return false }
        startPolling()
        return accepted
    }

    private func performAnswer() async -> Bool {
        let generation = connectionGeneration
        error = nil
        do {
            let _ = try await client.postJSON("/api/call/answer")
            guard generation == connectionGeneration else { return false }
            startPolling()
            return true
        } catch {
            guard generation == connectionGeneration else { return false }
            if !error.isCancellation { self.error = "接听尚未确认：\(error.localizedDescription)" }
            startPolling()
            return false
        }
    }

    func sendDTMF(_ digits: String) async {
        let _ = try? await client.postJSON("/api/call/dtmf", body: ["digits": digits])
    }

    func toggleMute() async {
        let generation = connectionGeneration
        let newMuted = !isMuted
        do {
            let result = try await client.postJSON("/api/call/mute", body: ["enabled": newMuted])
            guard generation == connectionGeneration else { return }
            if let muted = result["muted"] as? Bool {
                isMuted = muted
            } else {
                isMuted = newMuted
            }
        } catch {
            // keep current state
        }
    }

    // MARK: - Polling

    func startPolling() {
        pollLoop.start(interval: .seconds(2)) { [weak self] in
            guard let self else { return .failure }
            return await self.pollCallStatus()
        }
    }

    /// The app lifecycle controls polling; closing the dialer does not stop it.
    /// Suspending polling never hangs up a router call.
    func stopPolling() {
        pollLoop.stop()
    }

    /// One-shot reconciliation when the screen appears.
    ///
    /// The shared model survives dismissal. If lifecycle polling has not started,
    /// reconcile once so an existing router call remains controllable.
    func syncWithRouter() async {
        guard !pollLoop.isRunning else { return }
        _ = await pollCallStatus()
        if callState != .idle { startPolling() }
    }

    private func pollCallStatus() async -> PollingLoop.Outcome {
        let generation = connectionGeneration
        let result: [String: Any]
        do {
            result = try await client.getJSON("/api/call/status")
        } catch {
            return error.isCancellation ? .success : .failure
        }

        guard !Task.isCancelled, generation == connectionGeneration else { return .success }
        guard let listedCalls = result["calls"] as? [[String: Any]] else { return .failure }
        // MiFi firmware also lists persistent data/fax bearers in CLCC.
        let calls = listedCalls.filter { call in
            let mode = (call["mode"] as? Int) ?? Int(call["mode"] as? String ?? "")
            return mode != 1 && mode != 2
        }

        if calls.isEmpty {
            if callState != .idle {
                callState = .idle
                isMuted = false
                stopDurationTimer()
            }
            // Only a complete router status clears the system call.
            callKit.reportEnded()
            history.finish()
            return .success
        }

        guard let first = calls.first,
              let stat = first["stat"] as? String else { return .success }

        let number = first["number"] as? String ?? ""
        let dir = first["dir"] as? String ?? "mo"

        if ["dialing", "alerting", "active", "incoming", "waiting"].contains(stat) {
            history.begin(number: number, direction: dir == "mt" ? .incoming : .outgoing)
        }

        switch stat {
        case "dialing":
            callState = .dialing
        case "alerting":
            callState = .alerting
        case "active":
            if callState != .active {
                callState = .active
                startDurationTimer()
            }
            callKit.reportConnected()
            history.markConnected()
        case "incoming", "waiting":
            let caller = number.isEmpty ? (dir == "mt" ? "Unknown" : number) : number
            callState = .incoming(from: caller)
            callKit.reportIncoming(number: caller)
        case "held":
            break // keep current state
        case "releasing":
            callState = .idle
            isMuted = false
            stopDurationTimer()
            callKit.reportEnded()
            history.finish()
        default:
            break
        }

        return .success
    }

    // MARK: - Duration Timer

    private func startDurationTimer() {
        let start = Date()
        callDuration = 0
        durationLoop.start(interval: .seconds(1)) { [weak self] in
            guard let self else { return .failure }
            self.callDuration = Date().timeIntervalSince(start)
            return .success
        }
    }

    private func stopDurationTimer() {
        durationLoop.stop()
        callDuration = 0
    }
}
