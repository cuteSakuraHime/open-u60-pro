import AVFoundation
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
    var isSpeaker: Bool = false
    var callDuration: TimeInterval = 0
    var error: String?
    var showKeypad: Bool = false

    private let client: AgentClient
    private let authManager: AuthManager
    private let pollLoop = PollingLoop()
    private let durationLoop = PollingLoop()
    private var connectionGeneration = 0
    private var inForeground = true
    private var pollingFast = false
    /// At most one `/api/call/audio/start` in flight.
    private var routerAudioTask: Task<Void, Never>?
    /// Downlink packets seen at the last poll, and when that count last moved.
    private var audioWatch = (received: 0, changedAt: Date())
    private var lastAudioRestart = Date.distantPast
    private static let noAudioMessage = "未收到路由器的通话声音，正在重新连接…"

    /// Forget local state when authentication or the router changes; never sends hangup.
    func resetConnection() {
        connectionGeneration += 1
        // The router call may continue; only the local system UI is closed.
        callKit.reportEnded()
        voice.stop()
        routerAudioTask?.cancel()
        routerAudioTask = nil
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
        callKit.onAudioDeactivated = { [weak self] in
            self?.voice.stop()
            self?.isSpeaker = false
        }
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
            audioWatch = (0, Date())
            // Outgoing calls activate audio before they connect; then the poll starts
            // the router bridge once the call is active.
            requestRouterAudio()
        } catch {
            voice.stop()
            logger.error("Voice stream failed: \(error.localizedDescription, privacy: .public)")
            self.error = "通话音频启动失败：\(error.localizedDescription)"
        }
    }

    /// Ask the router to stream the active call to this phone. One request at a time;
    /// the status poll repeats it until the router reports its bridge running.
    private func requestRouterAudio(restart: Bool = false) {
        guard routerAudioTask == nil, voice.isRunning, callState == .active,
              let router = URL(string: client.baseURL)?.host else { return }
        guard let phone = VoiceStream.wifiAddress(sameSubnetAs: router) else {
            error = "未连接到路由器 Wi-Fi，无法传输通话声音"
            return
        }
        let generation = connectionGeneration
        var body: [String: Any] = ["host": phone]
        if restart { body["restart"] = true }
        routerAudioTask = Task {
            defer { if generation == connectionGeneration { routerAudioTask = nil } }
            do {
                _ = try await client.postJSON("/api/call/audio/start", body: body)
            } catch {
                guard generation == connectionGeneration, !error.isCancellation else { return }
                logger.error("Router audio start failed: \(error.localizedDescription, privacy: .public)")
                self.error = "通话声音启动失败：\(error.localizedDescription)"
            }
        }
    }

    /// The router reports its bridge running: make sure audio actually arrives. A bridge
    /// that runs without sending (the voice path was busy when it started) is restarted,
    /// at most every 15 s.
    private func checkAudioFlow() {
        guard voice.isRunning else { return }
        let received = voice.stats.received
        if received != audioWatch.received {
            audioWatch = (received, Date())
            if error == Self.noAudioMessage { error = nil }
            return
        }
        guard Date().timeIntervalSince(audioWatch.changedAt) > 4,
              Date().timeIntervalSince(lastAudioRestart) > 15 else { return }
        lastAudioRestart = Date()
        audioWatch.changedAt = Date()
        error = Self.noAudioMessage
        requestRouterAudio(restart: true)
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

    /// Route call audio between the receiver and the loudspeaker. CallKit owns the
    /// `.playAndRecord` session, so only the output port is overridden here.
    func toggleSpeaker() {
        let enable = !isSpeaker
        do {
            try AVAudioSession.sharedInstance().overrideOutputAudioPort(enable ? .speaker : .none)
            isSpeaker = enable
        } catch {
            logger.error("Speaker route failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    // MARK: - Polling

    /// Every second while a call is in progress (so a connected call gets its audio
    /// quickly), every two seconds while idle.
    func startPolling() {
        pollingFast = callState != .idle
        pollLoop.start(interval: pollingFast ? .seconds(1) : .seconds(2)) { [weak self] in
            guard let self else { return .failure }
            return await self.pollCallStatus()
        }
    }

    /// The app lifecycle controls polling; closing the dialer does not stop it.
    /// Suspending polling never hangs up a router call.
    func stopPolling() {
        pollLoop.stop()
    }

    /// Called as the scene becomes active or leaves it. Idle polling is for the foreground
    /// only. A call in progress keeps polling in the background (the system call screen,
    /// a locked phone): its audio session keeps the app running, and only the poll requests
    /// the router's audio bridge and notices a remote hang-up.
    func setForeground(_ foreground: Bool) {
        inForeground = foreground
        if foreground {
            startPolling()
        } else if callState == .idle && callKit.currentCall == nil {
            stopPolling()
        }
    }

    private func adjustPolling() {
        let inCall = callState != .idle
        guard pollLoop.isRunning else { return }
        if !inCall && !inForeground && callKit.currentCall == nil {
            stopPolling()
        } else if inCall != pollingFast {
            startPolling()
        }
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
        let outcome = await readCallStatus()
        adjustPolling()
        return outcome
    }

    private func readCallStatus() async -> PollingLoop.Outcome {
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
            let audio = result["audio"] as? [String: Any]
            if (audio?["running"] as? Bool) == true {
                checkAudioFlow()
            } else {
                requestRouterAudio()
            }
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
