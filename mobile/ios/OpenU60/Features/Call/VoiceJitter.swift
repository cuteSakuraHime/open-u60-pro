import Foundation

/// Builds outgoing RTP/PCMU packets: 20 ms frames of 160 μ-law samples at 8 kHz.
struct RTPPacketizer {
    static let samplesPerFrame = 160
    let ssrc: UInt32
    private(set) var sequence: UInt16
    private(set) var timestamp: UInt32
    private var pending: [Int16] = []

    init(ssrc: UInt32 = .random(in: 1...UInt32.max),
         sequence: UInt16 = .random(in: 0...UInt16.max),
         timestamp: UInt32 = .random(in: 0...UInt32.max)) {
        self.ssrc = ssrc
        self.sequence = sequence
        self.timestamp = timestamp
    }

    /// Accepts any number of 8 kHz samples and returns complete packets only.
    mutating func packets(for samples: [Int16]) -> [Data] {
        pending += samples
        var out: [Data] = []
        while pending.count >= Self.samplesPerFrame {
            let frame = pending.prefix(Self.samplesPerFrame)
            var packet = Data([0x80, 0x00,
                               UInt8(sequence >> 8), UInt8(sequence & 0xFF)])
            for shift in stride(from: 24, through: 0, by: -8) { packet.append(UInt8((timestamp >> UInt32(shift)) & 0xFF)) }
            for shift in stride(from: 24, through: 0, by: -8) { packet.append(UInt8((ssrc >> UInt32(shift)) & 0xFF)) }
            packet.append(contentsOf: frame.map(G711.encode))
            out.append(packet)
            pending.removeFirst(Self.samplesPerFrame)
            sequence &+= 1
            timestamp &+= UInt32(Self.samplesPerFrame)
        }
        return out
    }
}

/// Reorders incoming PCMU frames and hides loss with silence.
/// Playback starts once `targetDepth` frames are buffered; a new SSRC resets it.
///
/// The target adapts to the Wi-Fi link. After an underrun every pop spent waiting raises
/// it by a frame (up to `maxTarget`), so playback resumes with enough buffered to ride out
/// the same stall; 10 s without an underrun gives one frame back, down to the initial
/// depth. Call `pop()` once per 20 ms of output, also while it returns nil. A queue that
/// stays more than two frames above target for half a second drops its oldest frame, so
/// a burst after a stall cannot leave the call permanently delayed.
struct JitterBuffer {
    let minDepth: Int
    let maxDepth: Int
    let maxTarget: Int
    private(set) var targetDepth: Int
    private var frames: [UInt16: [Int16]] = [:]
    private var next: UInt16?
    private var ssrc: UInt32?
    private var started = false
    private var rebuffering = false
    private var steadyPops = 0
    private var excessPops = 0
    private(set) var lost = 0
    private(set) var late = 0
    private(set) var trimmed = 0
    private(set) var underruns = 0

    static let steadyPopsToShrink = 500   // 10 s
    static let excessPopsToTrim = 25      // 0.5 s

    init(targetDepth: Int = 3, maxDepth: Int = 25) {
        minDepth = targetDepth
        self.targetDepth = targetDepth
        self.maxDepth = maxDepth
        maxTarget = max(targetDepth, min(10, maxDepth - 2))
    }

    mutating func push(_ packet: RTPPacket) {
        guard packet.payloadType == 0, !packet.payload.isEmpty else { return }
        if ssrc != packet.ssrc {
            // A restarted bridge: same link, so keep what was learnt about its jitter.
            let learnt = targetDepth
            self = JitterBuffer(targetDepth: minDepth, maxDepth: maxDepth)
            targetDepth = learnt
            ssrc = packet.ssrc
        }
        if let next, Int16(bitPattern: packet.sequence &- next) < 0 {
            late += 1
            return
        }
        frames[packet.sequence] = packet.payload.map(G711.decode)
        if next == nil { next = packet.sequence }
        // Overflow: the sender is ahead (clock drift or a stall); drop to the newest window.
        if frames.count > maxDepth, let newest = newestSequence {
            let keepFrom = newest &- UInt16(targetDepth - 1)
            for key in frames.keys where Int16(bitPattern: key &- keepFrom) < 0 { frames[key] = nil }
            next = keepFrom
        }
    }

    /// One 20 ms frame for the speaker, or nil while still prebuffering.
    mutating func pop() -> [Int16]? {
        guard var current = next else { return nil }
        if !started {
            // Each 20 ms spent waiting after an underrun is jitter the target failed to
            // cover: wait for one more frame, so the next stall of this length is absorbed.
            if rebuffering && frames.count < targetDepth {
                targetDepth = min(maxTarget, targetDepth + 1)
            }
            guard frames.count >= targetDepth else { return nil }
            rebuffering = false
            started = true
            excessPops = 0
        }
        if frames.count > targetDepth + 2 { excessPops += 1 } else { excessPops = 0 }
        if excessPops >= Self.excessPopsToTrim {
            // Persistently ahead of the target: skip one frame to bring latency back.
            excessPops = 0
            frames[current] = nil
            current &+= 1
            trimmed += 1
        }
        next = current &+ 1
        if let frame = frames.removeValue(forKey: current) {
            steadyPops += 1
            if steadyPops >= Self.steadyPopsToShrink {
                steadyPops = 0
                targetDepth = max(minDepth, targetDepth - 1)
            }
            return frame
        }
        if frames.isEmpty {
            // Underrun: rebuffer instead of counting every empty tick as loss.
            started = false
            rebuffering = true
            steadyPops = 0
            underruns += 1
            next = current
            return nil
        }
        lost += 1
        return [Int16](repeating: 0, count: RTPPacketizer.samplesPerFrame)
    }

    private var newestSequence: UInt16? {
        guard let next else { return nil }
        return frames.keys.max { (($0 &- next)) < (($1 &- next)) }
    }
}
