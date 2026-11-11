import AVFoundation
import os

/// Call audio from the router to the speaker. The network queue pushes RTP packets;
/// the audio render thread pulls exactly the samples the hardware needs. The output
/// clock paces playback, so nothing can drift from it or leave the output empty
/// between scheduled buffers (a 20 ms timer feeding a player node did both).
final class PlayoutBuffer: @unchecked Sendable {
    /// What the source node renders; the mixer converts it to the output rate.
    static let format = AVAudioFormat(standardFormatWithSampleRate: 8_000, channels: 1)!

    struct Stats: Equatable {
        var received = 0
        var lost = 0
        var late = 0
        var trimmed = 0
        var underruns = 0
        var targetDepth = 0
    }

    private struct State {
        var jitter: JitterBuffer
        var frame: [Int16] = []
        var offset = 0
        var playing = false
        var received = 0
    }

    private static let silence = [Int16](repeating: 0, count: RTPPacketizer.samplesPerFrame)

    private let initialDepth: Int
    private let state: OSAllocatedUnfairLock<State>

    /// 4 frames (80 ms) to start; the jitter buffer adapts from there.
    init(targetDepth: Int = 4) {
        initialDepth = targetDepth
        state = OSAllocatedUnfairLock(initialState: State(jitter: JitterBuffer(targetDepth: targetDepth)))
    }

    func push(_ packet: RTPPacket) {
        state.withLock {
            $0.jitter.push(packet)
            $0.received += 1
        }
    }

    func reset() {
        state.withLock { $0 = State(jitter: JitterBuffer(targetDepth: initialDepth)) }
    }

    var stats: Stats {
        state.withLock {
            Stats(received: $0.received, lost: $0.jitter.lost, late: $0.jitter.late,
                  trimmed: $0.jitter.trimmed, underruns: $0.jitter.underruns,
                  targetDepth: $0.jitter.targetDepth)
        }
    }

    /// Fills `count` samples in [-1, 1]; silence while prebuffering or after an underrun.
    /// Returns whether any received audio was written.
    @discardableResult
    func read(into out: UnsafeMutablePointer<Float>, count: Int) -> Bool {
        // Unchecked: `out` is only written inside the lock, on the calling thread.
        state.withLockUnchecked { s in
            var written = 0
            var audible = false
            while written < count {
                if s.offset >= s.frame.count {
                    // Waiting also advances in 20 ms frames, so the jitter buffer sees one
                    // pop per frame of output and can size its target in real time.
                    if let next = s.jitter.pop() {
                        s.frame = next
                        s.playing = true
                    } else {
                        s.frame = Self.silence
                        s.playing = false
                    }
                    s.offset = 0
                }
                let n = min(count - written, s.frame.count - s.offset)
                for i in 0..<n { out[written + i] = Float(s.frame[s.offset + i]) / 32_768 }
                audible = audible || s.playing
                written += n
                s.offset += n
            }
            return audible
        }
    }

    /// Render block for an `AVAudioSourceNode` created with `PlayoutBuffer.format`.
    var renderBlock: AVAudioSourceNodeRenderBlock {
        { isSilence, _, frameCount, bufferList in
            let buffers = UnsafeMutableAudioBufferListPointer(bufferList)
            guard let first = buffers.first?.mData?.assumingMemoryBound(to: Float.self) else { return noErr }
            let count = Int(frameCount)
            if !self.read(into: first, count: count) { isSilence.pointee = true }
            for buffer in buffers.dropFirst() {
                buffer.mData?.copyMemory(from: first, byteCount: count * MemoryLayout<Float>.size)
            }
            return noErr
        }
    }
}
