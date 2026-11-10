import Foundation

func check(_ c: Bool, _ m: String) { if !c { print("FAIL:", m); exit(1) } }

func rtp(seq: UInt16, payload: [UInt8], pt: UInt8 = 0, pad: Int = 0) -> Data {
    var b: [UInt8] = [0x80 | (pad > 0 ? 0x20 : 0), pt, UInt8(seq >> 8), UInt8(seq & 0xFF), 0, 0, 0, 160, 1, 2, 3, 4]
    b += payload
    if pad > 0 { b += [UInt8](repeating: 0, count: pad - 1) + [UInt8(pad)] }
    return Data(b)
}

// G.711: decoding then encoding is the identity for every code except negative zero.
for code in 0...255 where code != 0x7F {
    check(G711.encode(G711.decode(UInt8(code))) == UInt8(code), "mu-law roundtrip \(code)")
}
check(G711.decode(0xFF) == 0 && abs(Int(G711.decode(0x00))) > 32000, "mu-law endpoints")

// RTP parsing
let p = RTPPacket(rtp(seq: 513, payload: [1, 2, 3], pad: 4))!
check(p.sequence == 513 && p.timestamp == 160 && p.ssrc == 0x01020304 && p.payload == Data([1, 2, 3]), "rtp fields + padding")
check(RTPPacket(Data([0x40] + [UInt8](repeating: 0, count: 11))) == nil, "rejects version 1")
check(RTPPacket(Data([0x80, 0])) == nil, "rejects short")

// Recorder: loss concealment, duplicate drop, wraparound, alignment, header
let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".wav")
let rec = try! CallRecorder(url: url)
let frame = [UInt8](repeating: 0xFF, count: 160)
rec.appendRemote(RTPPacket(rtp(seq: 65534, payload: frame))!)
rec.appendRemote(RTPPacket(rtp(seq: 65534, payload: frame))!) // duplicate
rec.appendRemote(RTPPacket(rtp(seq: 1, payload: frame))!)     // wrap, lost 65535 and 0
rec.appendRemote(RTPPacket(rtp(seq: 1, payload: frame, pt: 8))!) // non-PCMU ignored
rec.appendLocal([Int16](repeating: 7, count: 100))
try! rec.close()
let wav = try! Data(contentsOf: url)
check(rec.frameCount == 160 * 4, "4 frames incl. 2 concealed, got \(rec.frameCount)")
check(wav.count == 44 + 160 * 4 * 4, "file size")
check(String(decoding: wav[0..<4], as: UTF8.self) == "RIFF" && wav[22] == 2 && wav[24] == 0x40 && wav[25] == 0x1F, "wav header stereo 8k")
let dataSize = wav[40..<44].reversed().reduce(0) { $0 << 8 | Int($1) }
check(dataSize == 160 * 4 * 4, "data chunk size")
check(wav[46] == 7 && wav[50] == 7 && wav[44 + 100 * 4 + 2] == 0, "local channel then silence pad")
print("AUDIO ALL PASS")
