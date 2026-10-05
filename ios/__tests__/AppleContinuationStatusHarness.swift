// ios/__tests__/AppleContinuationStatusHarness.swift
//
// Executable harness for the Apple background-continuation posture: strict
// declare validation by Android's rules, the validated `continuationStatus`
// (peerId, resubscribe, malformedDeclarations, lastWake and lastRecovery),
// explicit platform-strategy refusals, installation failure outcomes, and
// the persistent malformed counter.
//
// Evidence level: deterministic only. UserDefaults stands in for the
// persisted declaration; no native radio work executes and nothing here is
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
private let androidOnlyDeclarations = [
  "{\"onAppearance\":\"headless-task\",\"headlessTaskName\":\"collect\"}",
  "{\"onAppearance\":\"foreground-service\",\"foregroundService\":{\"notification\":{\"title\":\"Collect\",\"channelName\":\"Collect\",\"channelId\":\"collect\"}}}"
]

@main
enum AppleContinuationStatusHarness {
  static func main() {
    clear()
    checkRecordOnlyLaunchBootstrap()
    // Metadata reads never install a host. Operations requiring the native
    // owner exercise this explicit installation failure instead of a radio.
    let sessions = UnifiedBleRustCoreSessions(installer: { _ in
      throw NSError(domain: "harness", code: 1, userInfo: nil)
    })
    let startupFailure = UnifiedBleRustCoreSessions.platformFailureJson(
      NSError(domain: "ASErrorDomain", code: 550),
      operation: "continuation.bootstrap.accessory-authorization", detail: "authorization refused")
    let original = statusOf(sessions)
    let reported = try! UnifiedBleRustCoreSessions.continuationStatusWithStartupFailure(original, failure: startupFailure)
    let diagnostic = reported["startupFailure"] as? [String: Any]
    let platform = diagnostic?["platform"] as? [String: Any]
    check(platform?["domain"] as? String == "ASErrorDomain" && platform?["code"] as? String == "550",
          "startup diagnostic preserves actual native domain/code")
    check(reported["strategy"] as? String == original["strategy"] as? String && reported["lastWake"] is NSNull,
          "startup diagnostic does not hide posture or invent a wake")
    check((try! UnifiedBleRustCoreSessions.continuationStatusWithStartupFailure(original, failure: nil))["startupFailure"] is NSNull,
          "successful startup retry clears its diagnostic")
    var coldStatus: String?
    sessions.describeNativeContinuation { coldStatus = $0 }
    check(coldStatus == "{\"ok\":true,\"value\":null}", "cold process status must not install the radio: \(coldStatus ?? "nil")")
    var coldPrepare: String?
    sessions.prepareNativeContinuationClaim(maxItems: 256, maxBytes: 65536) { coldPrepare = $0 }
    check((json(coldPrepare ?? "{}")["error"] as? [String: Any])?["code"] as? String == "lifecycle.invalid-state",
          "cold claim must refuse without installing the radio: \(coldPrepare ?? "nil")")
    var coldAck: String?
    sessions.acknowledgeNativeContinuationClaim("unowned-token") { coldAck = $0 }
    check((json(coldAck ?? "{}")["error"] as? [String: Any])?["code"] as? String == "lifecycle.invalid-state",
          "cold ACK must refuse without installing the radio: \(coldAck ?? "nil")")
    let recordingWorker = UnifiedBleRustCoreSessions(installer: { _ in
      check(false, "offline recording controls must not install the BLE owner")
      throw MobileCoreError.Failed(code: "platform.failure", domain: "restoration",
        operation: "continuation.recording.configure", detail: "test storage owner unavailable")
    })
    let recordingDone = DispatchSemaphore(value: 0)
    recordingWorker.recordingControl("unknown-operation", id: "recording_1", token: "", maxItems: 0, maxBytes: 0) { value, failure in
      check(!Thread.isMainThread, "durable control runs on its native storage worker")
      check(failure == nil, "an unavailable BLE installer cannot prevent offline control")
      check((json(value ?? "{}")["error"] as? [String: Any])?["code"] as? String == "argument.invalid", "closed opcode dispatch remains authoritative offline")
      recordingDone.signal()
    }
    check(recordingDone.wait(timeout: .now() + 5) == .success, "recording worker completion watchdog")
    let storageError = NSError(domain: NSCocoaErrorDomain, code: 513, userInfo: [NSFilePathErrorKey: "/private/sensor-data", NSLocalizedDescriptionKey: "secret path /private/sensor-data"])
    let safeStorage = UnifiedBleRustCoreSessions.recordingFailureJson(storageError)
    let safePlatform = json(safeStorage)["platform"] as? [String: Any]
    check(safePlatform?["domain"] as? String == NSCocoaErrorDomain && safePlatform?["code"] as? String == "513", "storage failure retains native domain and code")
    check(!safeStorage.contains("/private/sensor-data"), "storage diagnostics never expose private paths")

    // A valid native standing order is accepted with the peer in canonical form.
    let selector: [String: Any] = [
      "serviceUuid": hrService, "serviceOccurrence": 1,
      "characteristicUuid": hrMeasurement, "characteristicOccurrence": 1,
    ]
    let native: [String: Any] = ["onAppearance": "native", "peerId": peer, "resubscribe": [selector]]
    func accepts(_ declaration: [String: Any]) -> Bool {
      if case .success = UnifiedBleRustCoreSessions.validatedContinuation(jsonText(declaration)) { return true }
      return false
    }
    var omitted = selector
    omitted.removeValue(forKey: "serviceOccurrence")
    omitted.removeValue(forKey: "characteristicOccurrence")
    check(accepts(["onAppearance": "native", "resubscribe": [omitted]]), "missing occurrences default to one")
    for invalid in [NSNull(), NSNumber(value: true), NSNumber(value: 1.5), NSNumber(value: 9007199254740992)] {
      var bad = selector
      bad["serviceOccurrence"] = invalid
      check(!accepts(["onAppearance": "native", "resubscribe": [bad]]), "invalid occurrence refused")
    }
    let reply: [String: Any] = ["subscriptionIndex": 0, "prefix": [240], "minLength": 2,
      "maxLength": 512, "status": ["offset": 1, "accepted": [0, 255]]]
    let step: [String: Any] = ["selector": omitted, "value": [0, 255], "timeoutMs": 20000, "response": reply]
    func withSetup(_ steps: Any) -> [String: Any] {
      var declaration = native
      declaration["setup"] = steps
      return declaration
    }
    let mtu: [String: Any] = ["requested": 517, "timeoutMs": 20000, "onUnsupported": "continue"]
    let recording: [String: Any] = ["id": "recording_1", "maxBytes": 1073741824, "maxRecords": 1000000]
    var durable = native
    durable["link"] = ["mtu": mtu]
    durable["recording"] = recording
    check(accepts(durable), "bounded MTU and recording declaration accepted")
    for (key, invalid) in [("requested", 22), ("requested", 518), ("timeoutMs", 0), ("timeoutMs", 20001), ("unknown", 1)] {
      var bad = durable
      var badMtu = mtu
      badMtu[key] = invalid
      bad["link"] = ["mtu": badMtu]
      check(!accepts(bad), "invalid MTU refused")
    }
    for (key, invalid): (String, Any) in [("id", "../escape"), ("maxBytes", 1048575), ("maxRecords", 1000001), ("path", "/tmp")] {
      var bad = durable
      var badRecording = recording
      badRecording[key] = invalid
      bad["recording"] = badRecording
      check(!accepts(bad), "invalid recording refused")
    }
    check(accepts(withSetup([step])), "bounded setup accepted")
    var trailingReply = reply
    trailingReply["maxLength"] = 3
    trailingReply["trailing"] = ["offset": 2, "accepted": [0]]
    var trailingStep = step
    trailingStep["response"] = trailingReply
    check(accepts(withSetup([trailingStep])), "one optional validated trailing byte accepted")
    for invalid: [String: Any] in [["offset": 1, "accepted": [0]], ["offset": 3, "accepted": [0]],
      ["offset": 2, "accepted": [0, 0]], ["offset": 2, "accepted": []], ["offset": 2, "accepted": [0], "unknown": 1]] {
      var badReply = trailingReply
      badReply["trailing"] = invalid
      var bad = step
      bad["response"] = badReply
      check(!accepts(withSetup([bad])), "invalid trailing rule refused")
    }
    trailingReply["maxLength"] = 4
    trailingStep["response"] = trailingReply
    check(!accepts(withSetup([trailingStep])), "trailing maxLength must allow exactly one byte")
    check(accepts(withSetup(Array(repeating: step, count: 3))), "aggregate timeout boundary accepted")
    check(!accepts(withSetup(Array(repeating: step, count: 4))), "aggregate timeout overflow refused")
    check(!accepts(withSetup(Array(repeating: step, count: 17))), "step overflow refused")
    check(!accepts(withSetup(NSNull())), "null setup refused")
    for (key, invalid) in [("value", []), ("value", [256]), ("value", [-1]), ("value", Array(repeating: 0, count: 513))] {
      var bad = step
      bad[key] = invalid
      check(!accepts(withSetup([bad])), "invalid setup byte array refused")
    }
    for (key, invalid) in [("timeoutMs", 0), ("timeoutMs", 20001), ("unknown", 1)] {
      var bad = step
      bad[key] = invalid
      check(!accepts(withSetup([bad])), "invalid setup step refused")
    }
    for (key, invalid) in [("subscriptionIndex", 1), ("minLength", 0), ("maxLength", 1), ("maxLength", 513), ("unknown", 1)] {
      var badReply = reply
      badReply[key] = invalid
      var bad = step
      bad["response"] = badReply
      check(!accepts(withSetup([bad])), "invalid response boundary refused")
    }
    for invalidStatus: [String: Any] in [["offset": 0, "accepted": [0]], ["offset": 2, "accepted": [0]],
      ["offset": 1, "accepted": [0, 0]], ["offset": 1, "accepted": []],
      ["offset": 1, "accepted": [256]], ["offset": 1, "accepted": [0], "unknown": 1]] {
      var badReply = reply
      badReply["status"] = invalidStatus
      var bad = step
      bad["response"] = badReply
      check(!accepts(withSetup([bad])), "invalid status refused")
    }
    for field in ["body", "icon"] {
      for invalid: Any in [NSNull(), "", 2, true] {
        let notification: [String: Any] = ["channelId": "ble", "channelName": "BLE", "title": "BLE", field: invalid]
        check(!accepts(["onAppearance": "foreground-service", "foregroundService": ["notification": notification]]), "invalid optional text refused")
      }
    }
    let setupText = jsonText(withSetup([step]))
    let (_, setupFailure) = declare(sessions, setupText)
    check(setupFailure == nil, "valid setup declaration must persist")
    check(UserDefaults.standard.string(forKey: continuationKey) == setupText, "setup JSON must persist unchanged for native execution")
    let (declared, declareFailure) = declare(sessions, jsonText(native))
    check(declareFailure == nil, "valid declaration refused: \(declareFailure ?? "")")
    check(json(declared!)["state"] as? String == "declared", "declare answer: \(declared ?? "")")

    // The status reports what was actually declared: validated strategy and
    // peer, the resubscription count, zero malformed declarations, no wake
    // yet, and no obsolete deferred-implementation disclaimer.
    var status = statusOf(sessions)
    check(status["strategy"] as? String == "native", "strategy: \(status)")
    check(status["peerId"] as? String == peer.uppercased(), "peerId: \(status)")
    check(status["resubscribe"] as? Int == 1, "resubscribe: \(status)")
    check(status["malformedDeclarations"] as? Int == 0, "malformed: \(status)")
    check(status["lastWake"] is NSNull, "lastWake before any wake: \(status)")
    check(status["lastRecovery"] is NSNull, "lastRecovery before native admission: \(status)")
    check(status["detail"] == nil, "implemented native strategy must not report a deferred disclaimer: \(status)")
    var scopedFailure: String?
    sessions.continueRestoredPeer("AA:BB:CC:DD:EE:FF") { _, failure in scopedFailure = failure }
    check(json(scopedFailure ?? "{}")["code"] as? String == "operation.aborted", "outside-scope wake must match Android refusal")
    check((statusOf(sessions)["lastWake"] as? [String: Any])?["code"] as? String == "operation.aborted", "scoped wake refusal must remain observable")

    // Host installation is part of this OS wake attempt. Persist its exact
    // failure before completion, rather than leaving the earlier wake visible.
    let installationFailure = UnifiedBleRustCoreSessions(installer: { _ in
      throw MobileCoreError.Failed(code: "lifecycle.invalid-state", domain: "core",
        operation: "rust-core.host.install", detail: "test installer refuses radio ownership")
    })
    var installCompletions = 0
    installationFailure.continueRestoredPeer(peer.uppercased()) { value, failure in
      installCompletions += 1
      check(value == nil, "failed host install returned a success value")
      let error = json(failure ?? "{}")
      check(error["code"] as? String == "lifecycle.invalid-state" && error["domain"] as? String == "core"
            && error["operation"] as? String == "rust-core.host.install"
            && error["detail"] as? String == "test installer refuses radio ownership", "installer error changed: \(error)")
      let wake = json(UserDefaults.standard.string(forKey: lastWakeKey) ?? "{}")
      check(wake["event"] as? String == "continuation.failed" && wake["code"] as? String == error["code"] as? String
            && wake["reason"] as? String == error["detail"] as? String, "install failure was not persisted before completion: \(wake)")
      check(wake["peerAddress"] as? String == peer.uppercased() && wake["strategy"] as? String == "native", "wrong failed wake identity: \(wake)")
    }
    check(installCompletions == 1, "installer failure must complete exactly once")

    // An empty declaration stays a valid record-only order with no disclaimer.
    let (emptyDeclared, emptyFailure) = declare(sessions, "{}")
    check(emptyFailure == nil && json(emptyDeclared!)["state"] as? String == "declared", "empty declaration: \(emptyDeclared ?? "") \(emptyFailure ?? "")")
    status = statusOf(sessions)
    check(status["strategy"] as? String == "record-only", "empty strategy: \(status)")
    check(status["peerId"] is NSNull && status["resubscribe"] as? Int == 0, "empty peer/resubscribe: \(status)")
    check(status["detail"] == nil, "record-only carries no disclaimer: \(status)")

    // Valid platform-specific declarations are not malformed arguments.
    // Apple must refuse their unavailable mechanism before installing a radio.
    for deferred in androidOnlyDeclarations {
      let (_, declarationFailure) = declare(sessions, deferred)
      check(declarationFailure == nil, "valid deferred declaration rejected: \(declarationFailure ?? "")")
      check((statusOf(sessions)["detail"] as? String)?.contains("Android-specific") == true,
            "Apple must describe an unavailable platform mechanism, not unfinished implementation")
      var wakeFailure: String?
      sessions.continueRestoredPeer(peer) { _, failure in wakeFailure = failure }
      check(json(wakeFailure ?? "{}")["code"] as? String == "capability.unsupported",
            "deferred strategy must be unsupported, not invalid or platform failure: \(wakeFailure ?? "")")
      let refusedWake = statusOf(sessions)["lastWake"] as? [String: Any]
      check(refusedWake?["code"] as? String == "capability.unsupported", "unsupported mechanism refusal must remain observable")
    }

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

    // A cold claim must not install the radio just to discover that no native
    // process owner exists. The legacy wrapper maps that same raw refusal.
    var claimFailure: String?
    sessions.prepareContinuationClaim(maxItems: 256, maxBytes: 65536) { _, failure in claimFailure = failure }
    let claim = json(claimFailure ?? "{}")
    check(claim["code"] as? String == "lifecycle.invalid-state", "claim code: \(claim)")
    check(claim["operation"] as? String == "continuation.claim", "claim operation: \(claim)")

    clear()
    print("[AppleContinuationStatusHarness] validated declare, status, strategy refusal, installation failure, malformed counter and lastWake passed (deterministic; no physical radio).")
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

  static func checkRecordOnlyLaunchBootstrap() {
    // Execute the production launch selector's Swift control flow, before any
    // openSession/JS manager. A refusing installer proves host reachability
    // without allocating a physical CBCentralManager in the harness.
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent("ubm-launch-\(UUID().uuidString).bundle")
    try! FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try! FileManager.default.removeItem(at: directory); clear() }
    let info: [String: Any] = ["CFBundleIdentifier": "com.ubm.bootstrap.test",
      "UnifiedBleProtocolRestorationId": "record-only", "UnifiedBleProtocolRestorationGeneration": "1"]
    let data = try! PropertyListSerialization.data(fromPropertyList: info, format: .xml, options: 0)
    try! data.write(to: directory.appendingPathComponent("Info.plist"))
    let bundle = Bundle(path: directory.path)!
    check(UnifiedBleRustCoreSessions.productionRadioConfiguration(bundle: bundle).restoreIdentifierKey != nil,
          "launch fixture must configure an actual restoration identity")
    var installations = 0
    let sessions = UnifiedBleRustCoreSessions(installer: { _ in
      installations += 1
      throw MobileCoreError.Failed(code: "platform.failure", domain: "platform", operation: "launch.test", detail: "injected launch installation refusal")
    })
    check(sessions.bootstrapNativeContinuation(bundle: .main) == nil && installations == 0,
          "unconfigured installation remains inert")
    let launchDeclarations: [String?] = [nil, "{\"onAppearance\":\"record-only\"}", "{\"onAppearance\":\"native\"}"] + androidOnlyDeclarations.map { Optional($0) }
    for declaration in launchDeclarations {
      UserDefaults.standard.set(declaration, forKey: continuationKey)
      let before = installations
      let failure = sessions.bootstrapNativeContinuation(bundle: bundle)
      check(installations == before + 1, "configured record-only/native launch must install before JS: \(declaration ?? "default record-only")")
      check(json(failure ?? "{}")["code"] as? String == "platform.failure",
            "launch installation refusal remains observable")
    }
    UserDefaults.standard.set("{\"onAppearance\":\"native\"}", forKey: continuationKey)
    let before = installations
    check(json(sessions.bootstrapNativeContinuation(bundle: .main) ?? "{}")["code"] as? String == "capability.unsupported",
          "native standing order without restoration identity refuses")
    check(installations == before, "unconfigured native order never installs")
    UserDefaults.standard.set("{\"onAppearance\":\"invalid\"}", forKey: continuationKey)
    check(json(sessions.bootstrapNativeContinuation(bundle: bundle) ?? "{}")["code"] as? String == "argument.invalid",
          "malformed standing order refuses without silently substituting record-only")
    check(installations == before, "malformed launch declaration never installs")
  }

  static func clear() {
    let defaults = UserDefaults.standard
    defaults.removeObject(forKey: continuationKey)
    defaults.removeObject(forKey: malformedCountKey)
    defaults.removeObject(forKey: malformedPayloadKey)
    defaults.removeObject(forKey: lastWakeKey)
  }
}
