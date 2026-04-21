import Foundation
import WhisperKit

@main
struct Main {
    static func main() async {
        // Handshake: JSON line with {"model": "<id>", "language": "<en|auto>"}.
        guard let line = readLine(strippingNewline: true),
              let data = line.data(using: .utf8),
              let handshake = try? JSONDecoder().decode([String: String].self, from: data),
              let model = handshake["model"] else {
            emit(OutEvent(type: "error", model: nil, t0: nil, t1: nil, text: nil, message: "missing handshake"))
            return
        }

        do {
            let whisperKit = try await WhisperKit(model: model)
            emit(OutEvent(type: "ready", model: model, t0: nil, t1: nil, text: nil, message: nil))

            var accum = [Float]()
            let stdin = FileHandle.standardInput
            var lastDecode = Date()
            let hopSeconds = 0.75

            while true {
                guard let header = try stdin.read(upToCount: 4), header.count == 4 else { break }
                let n = header.withUnsafeBytes { $0.load(as: UInt32.self).littleEndian }
                guard let body = try stdin.read(upToCount: Int(n) * 4), body.count == Int(n) * 4 else { break }
                let samples: [Float] = body.withUnsafeBytes { ptr in Array(ptr.bindMemory(to: Float.self)) }
                accum.append(contentsOf: samples)

                if Date().timeIntervalSince(lastDecode) >= hopSeconds, accum.count >= 16_000 {
                    lastDecode = Date()
                    let result = try await whisperKit.transcribe(audioArray: accum)
                    for seg in result?.segments ?? [] {
                        emit(OutEvent(
                            type: "committed", model: nil,
                            t0: Double(seg.start), t1: Double(seg.end),
                            text: seg.text, message: nil
                        ))
                    }
                    if let tail = result?.text {
                        emit(OutEvent(type: "tentative", model: nil, t0: nil, t1: nil, text: tail, message: nil))
                    }
                }
            }
        } catch {
            emit(OutEvent(type: "error", model: nil, t0: nil, t1: nil, text: nil, message: "\(error)"))
        }
    }
}
