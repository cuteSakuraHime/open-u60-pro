import Foundation

func expect(_ c: Bool, _ m: String) { if !c { print("FAIL:", m); exit(1) } }

// Packetizer: framing, header fields, wraparound, remainder kept
var tx = RTPPacketizer(ssrc: 0xA1B2C3D4, sequence: 65535, timestamp: 0xFFFF_FF60)
expect(tx.packets(for: [Int16](repeating: 0, count: 100)).isEmpty, "partial frame waits")
let sent = tx.packets(for: [Int16](repeating: 1000, count: 260))
expect(sent.count == 2 && sent.allSatisfy { $0.count == 172 }, "two 172-byte packets")
let first = RTPPacket(sent[0])!, second = RTPPacket(sent[1])!
expect(first.sequence == 65535 && second.sequence == 0, "sequence wraps")
expect(first.timestamp == 0xFFFF_FF60 && second.timestamp == 0, "timestamp wraps by 160")
expect(first.ssrc == 0xA1B2C3D4 && first.payloadType == 0, "ssrc and PCMU")
expect(first.payload.first == G711.encode(0) && first.payload.last == G711.encode(1000), "leftover samples lead next frame")

func frame(_ seq: UInt16, _ value: Int16, ssrc: UInt32 = 7) -> RTPPacket {
    var p = RTPPacketizer(ssrc: ssrc, sequence: seq, timestamp: 0)
    return RTPPacket(p.packets(for: [Int16](repeating: value, count: 160))[0])!
}
func level(_ f: [Int16]?) -> Int { f.map { Int($0[0]) } ?? -1 }

// Prebuffer, reorder and loss concealment
var jb = JitterBuffer(targetDepth: 3)
jb.push(frame(10, 0))
expect(jb.pop() == nil, "prebuffering")
jb.push(frame(12, 2000)); jb.push(frame(11, 1000)); jb.push(frame(14, 4000))
expect(level(jb.pop()) == 0, "seq 10")
expect(level(jb.pop()) > 900, "seq 11 reordered into place")
expect(level(jb.pop()) > 1900, "seq 12")
expect(level(jb.pop()) == 0 && jb.lost == 1, "seq 13 concealed")
expect(level(jb.pop()) > 3900, "seq 14")
expect(jb.pop() == nil, "underrun rebuffers, not counted as loss")
expect(jb.lost == 1, "loss count stable")

// Late packets dropped; wraparound ordering
jb.push(frame(11, 1000))
expect(jb.late == 1, "late packet dropped")
var wrap = JitterBuffer(targetDepth: 2)
wrap.push(frame(65535, 100)); wrap.push(frame(0, 200))
expect(level(wrap.pop()) > 90 && level(wrap.pop()) > 190, "wraparound in order")

// Overflow keeps the newest window; a new SSRC restarts cleanly
var over = JitterBuffer(targetDepth: 3, maxDepth: 5)
for s in UInt16(0)..<8 { over.push(frame(s, Int16(s) * 100 + 50)) }
// Overflow at seq 5 trims to the newest 3 (3...5); 6 and 7 then fit under the cap.
// μ-law is lossy, so compare levels with a quantization tolerance.
let a = level(over.pop()), b = level(over.pop())
expect(abs(a - 350) < 20 && abs(b - 450) < 20, "drops 0...2 on overflow, got \(a) \(b)")
over.push(frame(500, 999, ssrc: 99))
expect(over.pop() == nil && over.lost == 0, "new stream prebuffers again")
print("JITTER ALL PASS")
