import AVFoundation
import CallKit
import Foundation

/// Bridges router call state to the system call UI.
/// Incoming calls are reported only while the app is running; waking a suspended app
/// would require PushKit, which relies on Apple's push service.
@MainActor
final class CallKitManager: NSObject, CXProviderDelegate {
    var onAnswer: (() async -> Bool)?
    var onEnd: (() async -> Bool)?
    var onDial: ((String) async -> Bool)?
    /// CallKit owns the audio session: start/stop media only from these callbacks.
    var onAudioActivated: (() -> Void)?
    var onAudioDeactivated: (() -> Void)?
    var onMute: ((Bool) -> Void)?

    private let provider: CXProvider
    private let controller = CXCallController()
    private(set) var currentCall: UUID?
    private var outgoingStarted = false
    private(set) var isOutgoing = false

    override init() {
        let configuration = CXProviderConfiguration()
        configuration.supportsVideo = false
        configuration.maximumCallGroups = 1
        configuration.maximumCallsPerCallGroup = 1
        configuration.supportedHandleTypes = [.phoneNumber]
        configuration.includesCallsInRecents = true
        provider = CXProvider(configuration: configuration)
        // Voice chat mode enables the system echo canceller for the VoIP path.
        let session = AVAudioSession.sharedInstance()
        try? session.setCategory(.playAndRecord, mode: .voiceChat, options: [.allowBluetooth])
        // 10 ms I/O keeps playout and mic latency low; CallKit applies it on activation.
        try? session.setPreferredIOBufferDuration(0.01)
        super.init()
        provider.setDelegate(self, queue: nil)
    }

    func reportIncoming(number: String) {
        guard currentCall == nil else { return }
        let id = UUID()
        currentCall = id
        isOutgoing = false
        let update = CXCallUpdate()
        update.remoteHandle = CXHandle(type: .phoneNumber, value: number)
        update.hasVideo = false
        provider.reportNewIncomingCall(with: id, update: update) { [weak self] error in
            guard error != nil else { return }
            Task { @MainActor in
                if self?.currentCall == id { self?.currentCall = nil }
            }
        }
    }

    func startOutgoing(number: String) {
        guard currentCall == nil else { return }
        let id = UUID()
        currentCall = id
        isOutgoing = true
        outgoingStarted = false
        let action = CXStartCallAction(call: id, handle: CXHandle(type: .phoneNumber, value: number))
        controller.request(CXTransaction(action: action)) { [weak self] error in
            guard error != nil else { return }
            Task { @MainActor in
                if self?.currentCall == id { self?.currentCall = nil }
            }
        }
    }

    func requestAnswer() {
        guard let id = currentCall, !isOutgoing else { return }
        controller.request(CXTransaction(action: CXAnswerCallAction(call: id))) { _ in }
    }

    func reportConnected() {
        guard let id = currentCall, isOutgoing else { return }
        if !outgoingStarted {
            provider.reportOutgoingCall(with: id, startedConnectingAt: nil)
            outgoingStarted = true
        }
        provider.reportOutgoingCall(with: id, connectedAt: nil)
    }

    /// Router confirmed the call is gone.
    func reportEnded() {
        guard let id = currentCall else { return }
        currentCall = nil
        outgoingStarted = false
        provider.reportCall(with: id, endedAt: nil, reason: .remoteEnded)
    }

    func requestEnd() {
        guard let id = currentCall else { return }
        controller.request(CXTransaction(action: CXEndCallAction(call: id))) { _ in }
    }

    // MARK: - CXProviderDelegate

    nonisolated func providerDidReset(_ provider: CXProvider) {
        Task { @MainActor in self.currentCall = nil }
    }

    nonisolated func provider(_ provider: CXProvider, perform action: CXStartCallAction) {
        let number = action.handle.value
        Task { @MainActor in
            let accepted = await self.onDial?(number) ?? false
            if accepted {
                provider.reportOutgoingCall(with: action.callUUID, startedConnectingAt: nil)
                self.outgoingStarted = true
                action.fulfill()
            } else {
                if self.currentCall == action.callUUID { self.currentCall = nil }
                action.fail()
            }
        }
    }

    nonisolated func provider(_ provider: CXProvider, didActivate audioSession: AVAudioSession) {
        Task { @MainActor in self.onAudioActivated?() }
    }

    nonisolated func provider(_ provider: CXProvider, didDeactivate audioSession: AVAudioSession) {
        Task { @MainActor in self.onAudioDeactivated?() }
    }

    nonisolated func provider(_ provider: CXProvider, perform action: CXSetMutedCallAction) {
        let muted = action.isMuted
        Task { @MainActor in self.onMute?(muted) }
        action.fulfill()
    }

    nonisolated func provider(_ provider: CXProvider, perform action: CXAnswerCallAction) {
        Task { @MainActor in
            (await self.onAnswer?() ?? false) ? action.fulfill() : action.fail()
        }
    }

    nonisolated func provider(_ provider: CXProvider, perform action: CXEndCallAction) {
        Task { @MainActor in
            // Fulfil only after the router accepted the hangup request; the status poll
            // still confirms idle before local state is cleared.
            let accepted = await self.onEnd?() ?? false
            if accepted {
                if self.currentCall == action.callUUID { self.currentCall = nil }
                action.fulfill()
            } else {
                action.fail()
            }
        }
    }
}
