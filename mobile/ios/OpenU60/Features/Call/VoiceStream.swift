import AVFoundation
import Darwin
import Network

/// Two-way call audio with the router's vendor RTP bridge (PCMU over UDP 48894).
/// Mic → 8 kHz → μ-law RTP → router; router RTP → jitter buffer → speaker.
/// Voice processing on the input node provides echo cancellation.
/// Start only after CallKit activates the audio session.
final class VoiceStream {
    static let port: UInt16 = 48894
    private let queue = DispatchQueue(label: "voice.stream")
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let wireFormat = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 8_000, channels: 1, interleaved: true)!
    private var listener: NWListener?
    private var sender: NWConnection?
    private var converter: AVAudioConverter?
    private var packetizer = RTPPacketizer()
    private var jitter = JitterBuffer()
    private var playbackTimer: DispatchSourceTimer?
    private var recorder: CallRecorder?
    private(set) var isRunning = false
    var isMuted = false

    /// This phone's IPv4 on the Wi-Fi interface that shares a /24 with the router;
    /// the router's bridge sends call audio here.
    static func wifiAddress(sameSubnetAs router: String) -> String? {
        let prefix = router.split(separator: ".").prefix(3).joined(separator: ".") + "."
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return nil }
        defer { freeifaddrs(head) }
        for ptr in sequence(first: first, next: { $0.pointee.ifa_next }) {
            guard let sa = ptr.pointee.ifa_addr, sa.pointee.sa_family == UInt8(AF_INET),
                  String(cString: ptr.pointee.ifa_name).hasPrefix("en") else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            guard getnameinfo(sa, socklen_t(sa.pointee.sa_len), &host, socklen_t(host.count),
                              nil, 0, NI_NUMERICHOST) == 0 else { continue }
            let address = String(cString: host)
            if address.hasPrefix(prefix), address != router { return address }
        }
        return nil
    }

    func start(router: String, recordTo url: URL?) throws {
        guard !isRunning else { return }
        recorder = try url.map(CallRecorder.init)
        try startNetwork(router: router)
        try startAudio()
        isRunning = true
    }

    /// Idempotent; also finalizes the recording.
    func stop() {
        queue.sync {
            playbackTimer?.cancel(); playbackTimer = nil
            listener?.cancel(); listener = nil
            sender?.cancel(); sender = nil
            try? recorder?.close(); recorder = nil
            jitter = JitterBuffer()
        }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        isRunning = false
    }

    private func startNetwork(router: String) throws {
        let params = NWParameters.udp
        params.allowLocalEndpointReuse = true
        params.requiredInterfaceType = .wifi   // never leak call audio over cellular
        let listener = try NWListener(using: params, on: NWEndpoint.Port(rawValue: Self.port)!)
        listener.newConnectionHandler = { [weak self] connection in
            // Only the router may feed audio; anything else is dropped unopened.
            guard case let .hostPort(host, _) = connection.endpoint, "\(host)" == router else {
                connection.cancel(); return
            }
            connection.start(queue: self?.queue ?? .main)
            self?.receive(on: connection)
        }
        listener.start(queue: queue)
        self.listener = listener

        let sender = NWConnection(host: NWEndpoint.Host(router), port: NWEndpoint.Port(rawValue: Self.port)!, using: params)
        sender.start(queue: queue)
        self.sender = sender

        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + .milliseconds(20), repeating: .milliseconds(20), leeway: .milliseconds(2))
        timer.setEventHandler { [weak self] in self?.playOneFrame() }
        timer.resume()
        playbackTimer = timer
    }

    private func receive(on connection: NWConnection) {
        connection.receiveMessage { [weak self] data, _, _, error in
            guard let self, error == nil else { return }
            if let data, let packet = RTPPacket(data) {
                self.jitter.push(packet)
                self.recorder?.appendRemote(packet)
            }
            self.receive(on: connection)
        }
    }

    private func playOneFrame() {
        guard let frame = jitter.pop(),
              let buffer = AVAudioPCMBuffer(pcmFormat: wireFormat, frameCapacity: AVAudioFrameCount(frame.count)) else { return }
        buffer.frameLength = buffer.frameCapacity
        frame.withUnsafeBufferPointer { buffer.int16ChannelData![0].update(from: $0.baseAddress!, count: frame.count) }
        player.scheduleBuffer(buffer)
    }

    private func startAudio() throws {
        let input = engine.inputNode
        try input.setVoiceProcessingEnabled(true)
        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: wireFormat)

        let micFormat = input.outputFormat(forBus: 0)
        converter = AVAudioConverter(from: micFormat, to: wireFormat)
        input.installTap(onBus: 0, bufferSize: 1024, format: micFormat) { [weak self] buffer, _ in
            self?.sendMic(buffer)
        }
        engine.prepare()
        try engine.start()
        player.play()
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
            guard let self, let sender = self.sender else { return }
            if self.isMuted { samples = [Int16](repeating: 0, count: count) }
            self.recorder?.appendLocal(samples)
            for packet in self.packetizer.packets(for: samples) {
                sender.send(content: packet, completion: .idempotent)
            }
        }
    }
}
