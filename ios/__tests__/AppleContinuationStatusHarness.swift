// ios/__tests__/AppleContinuationStatusHarness.swift
//
// Executable harness for the Apple background-continuation posture: strict
// declare validation by Android's rules, the validated `continuationStatus`
// (peerId, resubscribe, malformedDeclarations, a lastWake reader, and the
// `detail` disclaimer saying the strategy is not implemented in this
// release), and the persistent malformed counter.
//
// Evidence level: deterministic only. UserDefaults stands in for the
// persisted declaration; no wake executes and nothing here is
// physical-radio proof.

import Foundation

private func check(_ condition: @autoclosure () -> Bool, _ message: String, line: UInt = #line) {
  if !condition() {
    FileHandle.standardError.write(Data("[AppleContinuationStatusHarness] FAILED (line \(line)): \(message)\n".utf8))
    exit(1)
  }
}

private func json(_ text: String) -> [String: Any] {
  guard let data = text.data(using: .utf8),
        let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
    check(false, "not a JSON object: \(text)")
    return [:]
  }
  return object
}

private func jsonText(_ object: [String: Any]) -> String {
  String(data: try! JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]), encoding: .utf8)!
}

// The persisted keys, as declared in UnifiedBleRustCoreSessions.swift.
private let continuationKey = "com.sfourdrinier.unifiedblemanager.background-continuation"
private let malformedCountKey = "com.sfourdrinier.unifiedblemanager.background-continuation.malformed-count"
private let malformedPayloadKey = "com.sfourdrinier.unifiedblemanager.background-continuation.malformed-payload"
private let lastWakeKey = "com.sfourdrinier.unifiedblemanager.background-continuation.last-wake"

private let hrService = "0000180d-0000-1000-8000-00805f9b34fb"
private let hrMeasurement = "00002a37-0000-1000-8000-00805f9b34fb"
private let peer = "a0:9e:1a:e9:b9:3d"

@main
enum AppleContinuationStatusHarness {
  static func main() {
    clear()
    // The continuation surface never installs the host, so the installer is
    // unreachable; it only satisfies the type.
    let sessions = UnifiedBleRustCoreSessions(installer: { _ in
      throw NSError(domain: "harness", code: 1, userInfo: nil)
    })

    // A valid native standing order is accepted with the peer in canonical form.
    let selector: [String: Any] = [
      "serviceUuid": hrService, "serviceOccurrence": 1,
      "characteristicUuid": hrMeasurement, "characteristicOccurrence": 1,
    ]
    let native: [String: Any] = ["onAppearance": "native", "peerId": peer, "resubscribe": [selector]]
    let (declared, declareFailure) = declare(sessions, jsonText(native))
    check(declareFailure == nil, "valid declaration refused: \(declareFailure ?? "")")
    check(json(declared!)["state"] as? String == "declared", "declare answer: \(declared ?? "")")

    // The status reports what was actually declared: validated strategy and
    // peer, the resubscription count, zero malformed declarations, no wake
    // yet, and the deferred-execution disclaimer with Android's words.
    var status = statusOf(sessions)
    check(status["strategy"] as? String == "native", "strategy: \(status)")
    check(status["peerId"] as? String == peer.uppercased(), "peerId: \(status)")
    check(status["resubscribe"] as? Int == 1, "resubscribe: \(status)")
    check(status["malformedDeclarations"] as? Int == 0, "malformed: \(status)")
    check(status["lastWake"] is NSNull, "lastWake before any wake: \(status)")
    check(status["detail"] as? String == "native continuation is not implemented in this release",
          "detail disclaimer: \(status)")

    // An empty declaration stays a valid record-only order with no disclaimer.
    let (emptyDeclared, emptyFailure) = declare(sessions, "{}")
    check(emptyFailure == nil && json(emptyDeclared!)["state"] as? String == "declared", "empty declaration: \(emptyDeclared ?? "") \(emptyFailure ?? "")")
    status = statusOf(sessions)
    check(status["strategy"] as? String == "record-only", "empty strategy: \(status)")
    check(status["peerId"] is NSNull && status["resubscribe"] as? Int == 0, "empty peer/resubscribe: \(status)")
    check(status["detail"] == nil, "record-only carries no disclaimer: \(status)")

    // Restore the native order for the refusal checks below.
    let (_, restoreFailure) = declare(sessions, jsonText(native))
    check(restoreFailure == nil, "re-declare refused: \(restoreFailure ?? "")")

    // A malformed order is refused with no effect, never stored verbatim.
    for malformed in [
      "{\"onAppearance\":\"bogus\"}",
      "{\"onAppearance\":\"native\",\"peerId\":\"not-a-mac\"}",
      "{\"onAppearance\":\"native\",\"resubscribe\":[{\"serviceUuid\":\"zzz\"}]}",
      "{\"onAppearance\":\"native\",\"mystery\":1}",
      "[[[",
    ] {
      let (answer, failure) = declare(sessions, malformed)
      check(answer == nil, "malformed declaration accepted: \(malformed)")
      let record = json(failure ?? "{}")
      check(record["code"] as? String == "argument.invalid", "malformed code: \(record)")
      check(record["operation"] as? String == "continuation.declare", "malformed operation: \(record)")
    }
    status = statusOf(sessions)
    check(status["strategy"] as? String == "native" && status["peerId"] as? String == peer.uppercased(),
          "a refused declaration changed the stored order: \(status)")

    // A malformed persisted record (written before validation existed) falls
    // back to record-only and is counted once per distinct payload — reported,
    // never silently kept, and not once per status read.
    UserDefaults.standard.set("pre-validation garbage", forKey: continuationKey)
    status = statusOf(sessions)
    check(status["strategy"] as? String == "record-only" && status["resubscribe"] as? Int == 0,
          "malformed fallback: \(status)")
    check(status["malformedDeclarations"] as? Int == 1, "malformed count: \(status)")
    status = statusOf(sessions)
    check(status["malformedDeclarations"] as? Int == 1, "the same payload counted twice: \(status)")
    UserDefaults.standard.set("other garbage", forKey: continuationKey)
    status = statusOf(sessions)
    check(status["malformedDeclarations"] as? Int == 2, "a distinct payload not counted: \(status)")

    // The last wake outcome reads back verbatim once written, and reads as no
    // wake when the record is not the expected shape — never invented.
    let wake: [String: Any] = [
      "observedAtMs": 123, "event": "continuation.completed", "strategy": "native", "peerAddress": peer.uppercased(),
    ]
    UserDefaults.standard.set(jsonText(wake), forKey: lastWakeKey)
    status = statusOf(sessions)
    check((status["lastWake"] as? [String: Any])?["event"] as? String == "continuation.completed",
          "lastWake: \(status)")
    UserDefaults.standard.set("garbage", forKey: lastWakeKey)
    status = statusOf(sessions)
    check(status["lastWake"] is NSNull, "a malformed wake reads as no wake: \(status)")

    // The claim stays an unsupported stub with nothing to abandon.
    var claimFailure: String?
    sessions.prepareContinuationClaim(maxItems: 256, maxBytes: 65536) { _, failure in claimFailure = failure }
    let claim = json(claimFailure ?? "{}")
    check(claim["code"] as? String == "capability.unsupported", "claim code: \(claim)")
    check(claim["operation"] as? String == "continuation.claim", "claim operation: \(claim)")

    clear()
    print("[AppleContinuationStatusHarness] validated declare, status, disclaimer, malformed counter and lastWake passed (deterministic; no physical radio).")
  }

  static func declare(_ sessions: UnifiedBleRustCoreSessions, _ text: String) -> (String?, String?) {
    var result: (String?, String?) = (nil, nil)
    sessions.declareBackgroundContinuation(text) { result = ($0, $1) }
    return result
  }

  static func statusOf(_ sessions: UnifiedBleRustCoreSessions) -> [String: Any] {
    var answer: String?
    var report: String?
    sessions.continuationStatus { value, problem in
      answer = value
      report = problem
    }
    check(report == nil, "status refused: \(report ?? "")")
    return json(answer!)
  }

  static func clear() {
    let defaults = UserDefaults.standard
    defaults.removeObject(forKey: continuationKey)
    defaults.removeObject(forKey: malformedCountKey)
    defaults.removeObject(forKey: malformedPayloadKey)
    defaults.removeObject(forKey: lastWakeKey)
  }
}
