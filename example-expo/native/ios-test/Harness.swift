import Foundation
import BlePlx

@main struct Harness {
  static func main() {
    let module = ReferenceContinuationModule()
    let host = UnifiedBleRustCoreSessions.shared
    let peer = "9828347E-45DF-2EEB-E928-6E443F4065E3"
    let declaration = "{\"onAppearance\":\"native\",\"peerId\":\"\(peer)\"}"
    func invoke(_ operation: String, declarationJson: String? = nil, token: String = "claim", items: Double = 256, bytes: Double = 65536) -> String {
      let done = DispatchSemaphore(value: 0)
      var result = ""
      module.invoke(operation, peer: peer, declarationJson: declarationJson ?? declaration, token: token, maxItems: items, maxBytes: bytes,
        resolve: { value in result = value as? String ?? ""; done.signal() },
        reject: { _, message, _ in fatalError("unexpected rejection: \(message ?? "")") })
      precondition(done.wait(timeout: .now() + 5) == .success, "control never settled")
      return result
    }
    for operation in ["execute", "status", "prepare", "acknowledge"] {
      precondition(invoke(operation) == UnifiedBleRustCoreSessions.envelope, "native envelope changed")
    }
    let callbackLock = NSLock()
    var duplicateResolutions = 0
    host.duplicateNextCallback()
    module.invoke("status", peer: "", declarationJson: "", token: "", maxItems: 0, maxBytes: 0,
      resolve: { _ in callbackLock.lock(); duplicateResolutions += 1; callbackLock.unlock() },
      reject: { _, _, _ in fatalError("unexpected reject") })
    // Same serial native queue: this response follows both attempted callbacks.
    _ = invoke("status")
    callbackLock.lock(); let resolutionCount = duplicateResolutions; callbackLock.unlock()
    precondition(resolutionCount == 1, "duplicate native callback resolved the promise twice")
    let before = host.callCount()
    for envelope in [invoke("execute", declarationJson: "{}"), invoke("execute", declarationJson: String(repeating: "x", count: 65537)),
                     invoke("prepare", items: .nan), invoke("prepare", bytes: 4194305), invoke("prepare", items: 1.5),
                     invoke("acknowledge", token: ""), invoke("acknowledge", token: String(repeating: "x", count: 257)), invoke("bad"),
                     invoke("x" + String(repeating: "\u{0301}", count: 600000))] {
      precondition(envelope.utf8.count < 512, "unknown operation expanded the failure envelope")
      let root = try! JSONSerialization.jsonObject(with: Data(envelope.utf8)) as! [String: Any]
      precondition((root["error"] as? [String: Any])?["code"] as? String == "argument.invalid")
      print("invalid-envelope=\(envelope)")
    }
    let unknown = try! JSONSerialization.jsonObject(with: Data(invoke("bad").utf8)) as! [String: Any]
    precondition((unknown["error"] as? [String: Any])?["operation"] as? String == "continuation.invoke", "unknown operation echoed caller text")
    precondition(host.callCount() == before, "invalid arguments reached native owner")
    host.setHolding(true)
    let completed = DispatchSemaphore(value: 0)
    for _ in 0..<16 {
      module.invoke("status", peer: "", declarationJson: "", token: "", maxItems: 0, maxBytes: 0,
        resolve: { _ in completed.signal() }, reject: { _, _, _ in fatalError("unexpected reject") })
    }
    let busy = invoke("status")
    precondition(busy.contains("lifecycle.invalid-state"), "unbounded native admission")
    print("busy-envelope=\(busy)")
    let deadline = Date().addingTimeInterval(5)
    while host.callCount() < before + 16 && Date() < deadline { Thread.sleep(forTimeInterval: 0.001) }
    precondition(host.callCount() == before + 16)
    host.release()
    for _ in 0..<16 { precondition(completed.wait(timeout: .now() + 5) == .success) }
    precondition(invoke("status") == UnifiedBleRustCoreSessions.envelope, "admission did not recover")
    print("Apple app-only warm controls: forwarding, validation, off-main execution, bounded admission and recovery passed")
  }
}
