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
struct JitterBuffer {
    let targetDepth: Int
    let maxDepth: Int
    private var frames: [UInt16: [Int16]] = [:]
    private var next: UInt16?
    private var ssrc: UInt32?
    private var started = false
    private(set) var lost = 0
    private(set) var late = 0

    init(targetDepth: Int = 3, maxDepth: Int = 25) {
        self.targetDepth = targetDepth
        self.maxDepth = maxDepth
    }

    mutating func push(_ packet: RTPPacket) {
        guard packet.payloadType == 0, !packet.payload.isEmpty else { return }
        if ssrc != packet.ssrc {
            self = JitterBuffer(targetDepth: targetDepth, maxDepth: maxDepth)
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
        guard let current = next else { return nil }
        if !started {
            guard frames.count >= targetDepth else { return nil }
            started = true
        }
        next = current &+ 1
        if let frame = frames.removeValue(forKey: current) { return frame }
        if frames.isEmpty {
            // Underrun: rebuffer instead of counting every empty tick as loss.
            started = false
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
