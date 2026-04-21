import Foundation

// Protocol:
// - stdin:  4-byte LE length (u32) + N * 4 bytes of float32 samples (16 kHz mono)
// - stdout: line-delimited JSON events
//     {"type":"ready","model":"<name>"}
//     {"type":"committed","t0":<sec>,"t1":<sec>,"text":"..."}
//     {"type":"tentative","text":"..."}
//     {"type":"error","message":"..."}

struct OutEvent: Encodable {
    let type: String
    let model: String?
    let t0: Double?
    let t1: Double?
    let text: String?
    let message: String?
}

func emit(_ e: OutEvent) {
    let data = try! JSONEncoder().encode(e)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
}
