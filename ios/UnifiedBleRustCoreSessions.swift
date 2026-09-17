// ios/UnifiedBleRustCoreSessions.swift
//
// R01/D3(a) production session facade: Swift owner of the UniFFI
// `EchoSession` map behind the `UnifiedBleRustCore` TurboModule. Mirrors
// the Android `RustCoreSessionRouter` op for op (same table, same wire
// identities, same fail-loud rules) — see the routing table in
// docs/superpowers/plans/2026-09-17-r01-binding-producer.md.
//
// Revision admission follows PKG-02 (the UniFFI constructor never fails):
// every open is verified with a real `centralStatus()` call and a foreign
// revision rejects `protocol.incompatible` before the session is stored.

import Foundation

@objc(UnifiedBleRustCoreSessions)
@objcMembers
public final class UnifiedBleRustCoreSessions: NSObject {
  public static let contractRevision = "C-UBM.0.1.2-DRAFT"
  public static let shared = UnifiedBleRustCoreSessions()

  private let lock = NSLock()
  private var sessions: [String: EchoSession] = [:]
  private var closed: Set<String> = []

  private override init() {
    super.init()
  }

  public func openSession(_ owner: String) throws -> String {
    guard !owner.isEmpty else {
      throw rustCoreError(code: "argument.invalid", operation: "rust-core.open")
    }
    let session = EchoSession(revision: Self.contractRevision)
    // PKG-02: the constructor cannot fail, so admission is proven with a
    // real call. A foreign linked core rejects here, before any effect.
    let probe = session.centralStatus()
    guard probe.ok else {
      throw rustCoreError(code: probe.code, operation: "rust-core.open")
    }
    let sessionId = UUID().uuidString
    lock.lock()
    sessions[sessionId] = session
    lock.unlock()
    return sessionId
  }

  public func invoke(sessionId: String, op: String, argsJson: String) -> [String: Any] {
    func fail(code: String, operation: String) -> [String: Any] {
      return ["ok": false, "value": "", "code": code, "domain": failureDomain(code: code), "operation": operation]
    }
    lock.lock()
    let session = sessions[sessionId]
    let retired = closed.contains(sessionId)
    lock.unlock()
    guard let session, !retired else {
      return fail(code: "argument.invalid", operation: op)
    }
    let args = parseArgs(argsJson)
    switch op {
    case "central.status":
      return counterRecord(session.centralStatus(), operation: op)
    case "echo.bytes":
      guard let input = args["input"], let bytes = Data(base64Encoded: input) else {
        return fail(code: "argument.invalid", operation: op)
      }
      let result = session.echoBytes(input: bytes)
      return [
        "ok": result.ok,
        "value": result.ok ? result.data.base64EncodedString() : "",
        "code": result.code, "domain": result.domain, "operation": result.operation,
      ]
    case "echo.counter":
      guard let decimal = args["decimal"] else {
        return fail(code: "argument.invalid", operation: op)
      }
      return counterRecord(session.echoCounter(decimal: decimal), operation: op)
    case "kernel.expire-sweep":
      guard let nowMs = args["nowMs"] else {
        return fail(code: "argument.invalid", operation: op)
      }
      return counterRecord(session.driveExpireSweep(nowMs: nowMs), operation: op)
    case "kernel.destroy":
      return counterRecord(session.driveDestroy(), operation: op)
    case "ble.transition":
      guard let transition = args["transition"] else {
        return fail(code: "argument.invalid", operation: op)
      }
      let status = session.requestBleTransition(transition: transition)
      return [
        "ok": status.ok, "value": "",
        "code": status.code, "domain": status.domain, "operation": status.operation,
      ]
    case "staged.step":
      guard let line = args["line"] else {
        return fail(code: "argument.invalid", operation: op)
      }
      return counterRecord(session.stagedStep(line: line), operation: op)
    case "staged.drain":
      return counterRecord(session.stagedDrainLog(), operation: op)
    case "staged.counters":
      return counterRecord(session.stagedCounters(), operation: op)
    case "scan.start":
      guard let owner = args["owner"], let timeoutMs = args["timeoutMs"], let nowMs = args["nowMs"] else {
        return fail(code: "argument.invalid", operation: op)
      }
      return counterRecord(session.bleScanStart(owner: owner, timeoutMs: timeoutMs, nowMs: nowMs), operation: op)
    case "scan.take":
      return counterRecord(session.bleScanTake(), operation: op)
    case "scan.stop":
      guard let opId = args["opId"], let nowMs = args["nowMs"] else {
        return fail(code: "argument.invalid", operation: op)
      }
      return counterRecord(session.bleScanStop(opId: opId, nowMs: nowMs), operation: op)
    default:
      return fail(code: "capability.unsupported", operation: op)
    }
  }

  public func closeSession(_ sessionId: String) {
    lock.lock()
    let first = closed.insert(sessionId).inserted
    let session = sessions.removeValue(forKey: sessionId)
    lock.unlock()
    // Idempotent: repeats (or races) are no-ops, never errors.
    guard first else { return }
    _ = session?.close()
  }

  public func revision() -> String {
    return Self.contractRevision
  }

  // MARK: - Private

  private func counterRecord(_ result: EchoCounterResult, operation: String) -> [String: Any] {
    return [
      "ok": result.ok,
      "value": result.ok ? result.value : "",
      "code": result.code, "domain": result.domain, "operation": result.operation,
    ]
  }

  private func failureDomain(code: String) -> String {
    switch code {
    case "capability.unsupported": return "capability"
    default: return "core"
    }
  }

  private func parseArgs(_ json: String) -> [String: String] {
    guard let data = json.data(using: .utf8),
      let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
    else { return [:] }
    var args: [String: String] = [:]
    for (key, value) in object {
      if let text = value as? String { args[key] = text }
    }
    return args
  }

  private func rustCoreError(code: String, operation: String) -> NSError {
    return NSError(
      domain: code,
      code: 1,
      userInfo: [NSLocalizedDescriptionKey: "\(code)|core|\(operation)"]
    )
  }
}
