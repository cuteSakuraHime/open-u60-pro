import AVFoundation
import Darwin
import os

private let logger = Logger(subsystem: "com.zte.companion", category: "Voice")

/// Two-way call audio with the router's vendor RTP bridge (PCMU over UDP 48894).
///
/// - Downlink: router RTP → adaptive jitter buffer → source node → speaker. The output
///   hardware pulls samples, so playback is paced by the audio clock.
/// - Uplink: mic → 8 kHz → μ-law RTP → router, one packet per 20 ms. The input tap delivers
///   about 100 ms at a time; pacing turns those bursts into a steady stream.
///
/// One UDP socket on 48894 carries both directions, pinned to the Wi-Fi interface. Voice
/// processing on the input node provides echo cancellation. Start only after CallKit
/// activates the audio session.
final class VoiceStream {
    enum Failure: LocalizedError {
        case noWiFi
        case noMicrophone

        var errorDescription: String? {
            switch self {
            case .noWiFi: return "未连接到路由器 Wi-Fi"
            case .noMicrophone: return "麦克风不可用"
            }
        }
    }

    static let port: UInt16 = 48894
    /// Uplink packets allowed to wait for their slot (200 ms); older ones are dropped.
    static let maxQueuedUplink = 10

    private let queue = DispatchQueue(label: "voice.stream", qos: .userInteractive)
    private let engine = AVAudioEngine()
    private let playout = PlayoutBuffer()
    private lazy var source = AVAudioSourceNode(format: PlayoutBuffer.format, renderBlock: playout.renderBlock)
    private let wireFormat = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 8_000, channels: 1, interleaved: true)!
    private var graphReady = false
    private var configurationObserver: NSObjectProtocol?

    // Owned by `queue`.
    private var socket: RTPSocket?
    private var packetizer = RTPPacketizer()
    private var uplink: [Data] = []
    private var uplinkTimer: DispatchSourceTimer?
    private var recorder: CallRecorder?
    // Owned by the input tap's thread (replaced only while the tap is removed).
    private var converter: AVAudioConverter?

    private(set) var isRunning = false
    var isMuted = false

    /// Received/lost/late counts and the jitter target, for the call screen and diagnostics.
    var stats: PlayoutBuffer.Stats { playout.stats }

    /// This phone's Wi-Fi interface and IPv4 on the router's /24; the router's bridge
    /// sends call audio to that address.
    static func wifiInterface(sameSubnetAs router: String) -> (name: String, address: String)? {
        let prefix = router.split(separator: ".").prefix(3).joined(separator: ".") + "."
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return nil }
        defer { freeifaddrs(head) }
        for ptr in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let name = String(cString: ptr.pointee.ifa_name)
            guard let sa = ptr.pointee.ifa_addr, sa.pointee.sa_family == UInt8(AF_INET),
                  name.hasPrefix("en") else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard getnameinfo(sa, socklen_t(sa.pointee.sa_len), &host, socklen_t(host.count),
                              nil, 0, NI_NUMERICHOST) == 0 else { continue }
            let address = String(cString: host)
            if address.hasPrefix(prefix), address != router { return (name, address) }
        }
        return nil
    }

    static func wifiAddress(sameSubnetAs router: String) -> String? {
        wifiInterface(sameSubnetAs: router)?.address
    }

    func start(router: String, recordTo url: URL?) throws {
        guard !isRunning else { return }
        guard let wifi = Self.wifiInterface(sameSubnetAs: router) else { throw Failure.noWiFi }
        do {
            let recorder = try url.map(CallRecorder.init)
            playout.reset()
            try startNetwork(router: router, interface: wifi.name, recorder: recorder)
            try startAudio()
        } catch {
            stop()
            throw error
        }
        isRunning = true
    }

    /// Idempotent; also finalizes the recording.
    func stop() {
        if let configurationObserver {
            NotificationCenter.default.removeObserver(configurationObserver)
            self.configurationObserver = nil
        }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        queue.sync {
            uplinkTimer?.cancel(); uplinkTimer = nil
            socket?.close(); socket = nil
            uplink = []
            try? recorder?.close(); recorder = nil
        }
        playout.reset()
        isRunning = false
    }

    // MARK: - Network

    private func startNetwork(router: String, interface: String, recorder: CallRecorder?) throws {
        let playout = self.playout
        let socket = try RTPSocket(router: router, routerPort: Self.port, localPort: Self.port,
                                   interface: interface, queue: queue) { [weak self] data in
            guard let packet = RTPPacket(data) else { return }
            playout.push(packet)
            self?.recorder?.appendRemote(packet)
        }
        let timer = DispatchSource.makeTimerSource(flags: .strict, queue: queue)
        timer.schedule(deadline: .now() + .milliseconds(20), repeating: .milliseconds(20), leeway: .milliseconds(1))
        timer.setEventHandler { [weak self] in self?.sendQueuedFrame() }
        queue.sync {
            self.recorder = recorder
            self.packetizer = RTPPacketizer()
            self.uplink = []
            self.socket = socket
            self.uplinkTimer = timer
        }
        timer.resume()
    }

    /// On `queue`, every 20 ms.
    private func sendQueuedFrame() {
        guard !uplink.isEmpty, let socket else { return }
        socket.send(uplink.removeFirst())
    }

    // MARK: - Audio

    private func startAudio() throws {
        let input = engine.inputNode
        if !input.isVoiceProcessingEnabled {
            try input.setVoiceProcessingEnabled(true)
        }
        if !graphReady {
            engine.attach(source)
            engine.connect(source, to: engine.mainMixerNode, format: PlayoutBuffer.format)
            graphReady = true
        }
        try installMicTap()
        engine.prepare()
        try engine.start()
        configurationObserver = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main
        ) { [weak self] _ in self?.restartAfterConfigurationChange() }
    }

    private func installMicTap() throws {
        let input = engine.inputNode
        input.removeTap(onBus: 0)
        let micFormat = input.outputFormat(forBus: 0)
        guard micFormat.sampleRate > 0, micFormat.channelCount > 0,
              let converter = AVAudioConverter(from: micFormat, to: wireFormat) else {
            throw Failure.noMicrophone
        }
        self.converter = converter
        input.installTap(onBus: 0, bufferSize: 1024, format: micFormat) { [weak self] buffer, _ in
            self?.sendMic(buffer)
        }
    }

    /// A route change (Bluetooth, speaker, wired headset) stops the engine and may change
    /// the hardware formats; without a restart the call would go silent both ways.
    private func restartAfterConfigurationChange() {
        guard isRunning else { return }
        do {
            engine.connect(engine.mainMixerNode, to: engine.outputNode, format: nil)
            try installMicTap()
            engine.prepare()
            try engine.start()
            logger.info("Voice engine restarted after an audio route change")
        } catch {
            logger.error("Voice engine restart failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    private func sendMic(_ buffer: AVAudioPCMBuffer) {
        guard let converter else { return }
        let ratio = wireFormat.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 16
        guard let out = AVAudioPCMBuffer(pcmFormat: wireFormat, frameCapacity: capacity) else { return }
        var consumed = false
        var error: NSError?
        converter.convert(to: out, error: &error) { _, status in
            if consumed { status.pointee = .noDataNow; return nil }
            consumed = true
            status.pointee = .haveData
            return buffer
        }
        guard error == nil, out.frameLength > 0 else { return }
        let count = Int(out.frameLength)
        var samples = Array(UnsafeBufferPointer(start: out.int16ChannelData![0], count: count))
        queue.async { [weak self] in
            guard let self, self.socket != nil else { return }
            if self.isMuted { samples = [Int16](repeating: 0, count: count) }
            self.recorder?.appendLocal(samples)
            self.uplink += self.packetizer.packets(for: samples)
            if self.uplink.count > Self.maxQueuedUplink {
                self.uplink.removeFirst(self.uplink.count - Self.maxQueuedUplink)
            }
        }
    }
}
