import AVFoundation
// Run from the repository root: sh scripts/test-ios-duo.sh (macOS only: AVFoundation).

func expect(_ c: Bool, _ m: String) { if !c { print("FAIL:", m); exit(1) } }

func frame(_ seq: UInt16, _ value: Int16) -> RTPPacket {
    var p = RTPPacketizer(ssrc: 7, sequence: seq, timestamp: 0)
    return RTPPacket(p.packets(for: [Int16](repeating: value, count: 160)))!
}
extension RTPPacket {
    init?(_ packets: [Data]) { self.init(packets[0]) }
}
func level(_ v: Int16) -> Float { Float(G711.decode(G711.encode(v))) / 32_768 }

let playout = PlayoutBuffer(targetDepth: 2)
var out = [Float](repeating: 1, count: 640)

// Nothing received: silence, reported as not audible.
expect(!playout.read(into: &out, count: 85) && out[..<85].allSatisfy { $0 == 0 }, "silence while prebuffering")

// The render thread pulls odd-sized chunks (85/86 samples at 8 kHz on a 48 kHz device);
// frames must come out back to back, sample-exact, in sequence order.
// The first packet anchors the stream; 2 arriving before 1 is put back in order.
for (seq, value) in [(UInt16(0), Int16(1000)), (2, 3000), (1, 2000), (3, 4000)] { playout.push(frame(seq, value)) }
// The rest of the 20 ms silence frame started while prebuffering comes first.
expect(!playout.read(into: &out, count: 75) && out[..<75].allSatisfy { $0 == 0 }, "prebuffer frame finishes")
var played: [Float] = []
var buffer = [Float](repeating: 0, count: 86)
while played.count < 640 {
    let n = min(played.count % 2 == 0 ? 85 : 86, 640 - played.count)  // stop at the end of frame 3
    expect(playout.read(into: &buffer, count: n), "audio while frames are buffered")
    played += buffer[..<n]
}
for (i, value) in [Int16(1000), 2000, 3000, 4000].enumerated() {
    let slice = played[(i * 160)..<(i * 160 + 160)]
    expect(slice.allSatisfy { abs($0 - level(value)) < 1e-6 }, "frame \(i) intact and in order")
}

// Stream stalls: silence, and the jitter target grows by one per 20 ms of waiting,
// however the render thread slices its pulls.
let before = playout.stats
for _ in 0..<19 { _ = playout.read(into: &buffer, count: 85) }   // ≈ 1615 samples ≈ 10 frames
let after = playout.stats
expect(after.underruns == before.underruns + 1, "one underrun")
expect(after.targetDepth >= 8 && after.targetDepth <= 10, "target grew by ~one per frame waited: \(after.targetDepth)")
expect(after.received == 4 && after.lost == 0 && after.late == 0, "counters: \(after)")

// The render block fills every channel buffer the engine hands it.
playout.reset()
expect(playout.stats == PlayoutBuffer.Stats(targetDepth: 2), "reset clears state")
let format = AVAudioFormat(standardFormatWithSampleRate: 8_000, channels: 2)!
let pcm = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: 64)!
pcm.frameLength = 64
for ch in 0..<2 { for i in 0..<64 { pcm.floatChannelData![ch][i] = 9 } }
var silent: ObjCBool = false
var timestamp = AudioTimeStamp()
let status = playout.renderBlock(&silent, &timestamp, 64, pcm.mutableAudioBufferList)
expect(status == noErr && silent.boolValue, "prebuffering renders silence flagged as such")
expect((0..<2).allSatisfy { ch in (0..<64).allSatisfy { pcm.floatChannelData![ch][$0] == 0 } }, "both channels zeroed")
print("PLAYOUT ALL PASS")
