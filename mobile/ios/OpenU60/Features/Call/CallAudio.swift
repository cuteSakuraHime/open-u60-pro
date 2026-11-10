import Foundation

/// Minimal RTP header view (RFC 3550). The router's voice bridge sends
/// G.711 μ-law (payload type 0), 160 samples per 20 ms packet.
struct RTPPacket: Equatable {
    let payloadType: UInt8
    let sequence: UInt16
    let timestamp: UInt32
    let ssrc: UInt32
    let payload: Data

    init?(_ data: Data) {
        let bytes = [UInt8](data)
        guard bytes.count >= 12, bytes[0] >> 6 == 2 else { return nil }
        let csrcCount = Int(bytes[0] & 0x0F)
        var offset = 12 + csrcCount * 4
        guard bytes.count >= offset else { return nil }
        if bytes[0] & 0x10 != 0 { // header extension
            guard bytes.count >= offset + 4 else { return nil }
            let words = Int(bytes[offset + 2]) << 8 | Int(bytes[offset + 3])
            offset += 4 + words * 4
            guard bytes.count >= offset else { return nil }
        }
        var end = bytes.count
        if bytes[0] & 0x20 != 0 { // padding
            let pad = Int(bytes[end - 1])
            guard pad > 0, end - pad >= offset else { return nil }
            end -= pad
        }
        payloadType = bytes[1] & 0x7F
        sequence = UInt16(bytes[2]) << 8 | UInt16(bytes[3])
        timestamp = bytes[4..<8].reduce(0) { $0 << 8 | UInt32($1) }
        ssrc = bytes[8..<12].reduce(0) { $0 << 8 | UInt32($1) }
        payload = Data(bytes[offset..<end])
    }
}

enum G711 {
    static func decode(_ byte: UInt8) -> Int16 {
        let u = ~byte
        let exponent = Int((u >> 4) & 0x07)
        let mantissa = Int(u & 0x0F)
        let magnitude = ((mantissa << 3) + 0x84) << exponent
        return Int16(u & 0x80 != 0 ? 0x84 - magnitude : magnitude - 0x84)
    }

    static func encode(_ sample: Int16) -> UInt8 {
        var value = Int(sample)
        let sign: UInt8 = value < 0 ? 0x80 : 0
        if value < 0 { value = -value }
        value = min(value, 32_635) + 0x84
        var exponent: UInt8 = 7
        var mask = 0x4000
        while exponent > 0 && value & mask == 0 { exponent -= 1; mask >>= 1 }
        let mantissa = UInt8((value >> (Int(exponent) + 3)) & 0x0F)
        return ~(sign | exponent << 4 | mantissa)
    }
}

/// Writes a stereo 8 kHz 16-bit WAV: left = far end, right = this phone.
/// Lost or late packets become silence so both channels stay time-aligned.
final class CallRecorder {
    static let sampleRate = 8_000
    let url: URL
    private let handle: FileHandle
    private var remote: [Int16] = []
    private var local: [Int16] = []
    private var lastRemoteSequence: UInt16?
    private(set) var frameCount = 0
    private var closed = false

    init(url: URL) throws {
        self.url = url
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        guard FileManager.default.createFile(atPath: url.path, contents: Self.header(frames: 0)) else {
            throw CocoaError(.fileWriteUnknown)
        }
        handle = try FileHandle(forWritingTo: url)
        try handle.seekToEnd()
    }

    func appendRemote(_ packet: RTPPacket) {
        guard packet.payloadType == 0, !closed else { return }
        if let last = lastRemoteSequence {
            let gap = packet.sequence &- last
            if gap == 0 || gap > 0x8000 { return } // duplicate or reordered-late
            if gap > 1 {
                // Fill at most 1 s of loss; longer gaps mean the stream restarted.
                remote += [Int16](repeating: 0, count: min(Int(gap) - 1, 50) * packet.payload.count)
            }
        }
        lastRemoteSequence = packet.sequence
        remote += packet.payload.map(G711.decode)
        flush()
    }

    func appendLocal(_ samples: [Int16]) {
        guard !closed else { return }
        local += samples
        flush()
    }

    /// Writes any unpaired tail padded with silence and finalizes the header.
    func close() throws {
        guard !closed else { return }
        let count = max(remote.count, local.count)
        remote += [Int16](repeating: 0, count: count - remote.count)
        local += [Int16](repeating: 0, count: count - local.count)
        flush()
        closed = true
        try handle.seek(toOffset: 0)
        handle.write(Self.header(frames: frameCount))
        try handle.close()
    }

    private func flush() {
        // A 2 s lead on one side means the other side stalled; pad it with silence.
        let maxLead = Self.sampleRate * 2
        if remote.count - local.count > maxLead { local += [Int16](repeating: 0, count: remote.count - local.count - maxLead) }
        if local.count - remote.count > maxLead { remote += [Int16](repeating: 0, count: local.count - remote.count - maxLead) }
        let count = min(remote.count, local.count)
        guard count > 0 else { return }
        var data = Data(capacity: count * 4)
        for i in 0..<count {
            withUnsafeBytes(of: remote[i].littleEndian) { data.append(contentsOf: $0) }
            withUnsafeBytes(of: local[i].littleEndian) { data.append(contentsOf: $0) }
        }
        handle.write(data)
        remote.removeFirst(count)
        local.removeFirst(count)
        frameCount += count
    }

    static func header(frames: Int) -> Data {
        let dataSize = UInt32(frames * 4)
        var d = Data("RIFF".utf8)
        func u32(_ v: UInt32) { withUnsafeBytes(of: v.littleEndian) { d.append(contentsOf: $0) } }
        func u16(_ v: UInt16) { withUnsafeBytes(of: v.littleEndian) { d.append(contentsOf: $0) } }
        u32(36 + dataSize)
        d.append(contentsOf: Array("WAVEfmt ".utf8))
        u32(16); u16(1); u16(2); u32(UInt32(sampleRate)); u32(UInt32(sampleRate * 4)); u16(4); u16(16)
        d.append(contentsOf: Array("data".utf8))
        u32(dataSize)
        return d
    }
}
