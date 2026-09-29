// ios/__tests__/AppleRustRadioAdapterHarness.swift
//
// Executable harness for the Apple Rust route: the REAL process-owned Rust
// mobile host (ubm-mobile through the UniFFI binding, linked from the host
// build of `ubm5_uniffi_echo`) drives `UnifiedBleRustRadioAdapter` over a
// scripted `UnifiedBleRustRadioDriver`, reached through the same
// `UnifiedBleRustCoreSessions` pass-through the TurboModule uses.
//
// Evidence level: deterministic only. The scripted driver stands in for
// CoreBluetooth; nothing here is physical-radio proof.

import CoreBluetooth
import Foundation

private let hrService = "0000180d-0000-1000-8000-00805f9b34fb"
private let hrMeasurement = "00002a37-0000-1000-8000-00805f9b34fb"
private let timeout: DispatchTimeInterval = .seconds(10)

private func check(_ condition: @autoclosure () -> Bool, _ message: String, line: UInt = #line) {
  if !condition() {
    FileHandle.standardError.write(Data("[AppleRustRadioAdapterHarness] FAILED (line \(line)): \(message)\n".utf8))
    exit(1)
  }
}

private func waitFor<T>(_ what: String, _ start: (@escaping (T) -> Void) -> Void) -> T {
  let semaphore = DispatchSemaphore(value: 0)
  let box = NSLock()
  var result: T?
  start { value in
    box.lock()
    result = value
    box.unlock()
    semaphore.signal()
  }
  check(semaphore.wait(timeout: .now() + timeout) == .success, "timed out waiting for \(what)")
  box.lock()
  defer { box.unlock() }
  return result!
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

private func completedRecovery(_ outcome: [String: Any]?, previousSubscription: String, currentSubscription: String) -> Bool {
  currentSubscription != previousSubscription && outcome?["event"] as? String == "continuation.completed"
}

private func continuationStatusIsBusy(_ failure: String) -> Bool {
  let error = json(failure)
  return error["code"] as? String == "lifecycle.invalid-state"
    && error["domain"] as? String == "restoration"
    && error["operation"] as? String == "continuation"
    && error["detail"] as? String == "continuation execution or handoff is in progress"
}

private func readyContinuationStatus(
  deadline: Date,
  beforeRead: () -> String,
  read: (@escaping ((String?, String?)) -> Void) -> Void
) -> (String, (String?, String?)) {
  while true {
    check(Date() < deadline, "timed out waiting for continuation status admission")
    let identity = beforeRead()
    let semaphore = DispatchSemaphore(value: 0)
    let lock = NSLock()
    var response: (String?, String?)?
    read { value in
      lock.lock()
      response = value
      lock.unlock()
      semaphore.signal()
    }
    check(semaphore.wait(timeout: .now() + max(0, deadline.timeIntervalSinceNow)) == .success,
          "timed out waiting for continuation status callback")
    lock.lock()
    let observed = response!
    lock.unlock()
    guard let failure = observed.1, continuationStatusIsBusy(failure) else {
      return (identity, observed)
    }
    // This is only backoff; the exact response and absolute watchdog, not a
    // sleep or a number of attempts, decide admission and failure.
    Thread.sleep(forTimeInterval: min(0.01, max(0, deadline.timeIntervalSinceNow)))
  }
}

private func continuationStatusReadinessChecks() {
  let busy = jsonText(["code": "lifecycle.invalid-state", "domain": "restoration",
                       "operation": "continuation", "detail": "continuation execution or handoff is in progress"])
  var reads = 0
  let result = readyContinuationStatus(deadline: Date().addingTimeInterval(10), beforeRead: { "subscription-\(reads)" }) { done in
    reads += 1
    done(reads == 1 ? (nil, busy) : ("{\"lastWake\":null}", nil))
  }
  check(reads == 2 && result.0 == "subscription-1" && result.1.1 == nil,
        "status must retry exact admission contention and recapture identity before the successful read")
  for field in ["code", "domain", "operation", "detail"] {
    var other = json(busy)
    other[field] = "different"
    let failure = jsonText(other)
    check(!continuationStatusIsBusy(failure), "status must not hide a different \(field)")
    var attempts = 0
    let refused = readyContinuationStatus(deadline: Date().addingTimeInterval(10), beforeRead: { "same" }) { done in
      attempts += 1
      done((nil, failure))
    }
    check(attempts == 1 && refused.1.1 == failure, "non-busy status failure must propagate unchanged")
  }
}

private final class HarnessInvokeCompletion: MobileInvokeCompletion, @unchecked Sendable {
  private let body: (String) -> Void
  init(_ body: @escaping (String) -> Void) { self.body = body }
  func complete(envelope: String) { body(envelope) }
}

private final class DisposalGate {
  private let lock = NSLock()
  private var counts = [UInt64: Int]()
  private var fail = Set<UInt64>()
  private var hold = Set<UInt64>()
  private var held = [UInt64: (String) -> Void]()
  private var completedHeld = [UInt64: (String) -> Void]()
  private let failure = "{\"ok\":false,\"error\":{\"code\":\"platform.failure\",\"domain\":\"platform\",\"operation\":\"session.dispose\"}}"

  func failOnce(_ id: String) { lock.lock(); fail.insert(UInt64(id)!); lock.unlock() }
  func holdOnce(_ id: String) { lock.lock(); hold.insert(UInt64(id)!); lock.unlock() }
  func attempts(_ id: String) -> Int { lock.lock(); defer { lock.unlock() }; return counts[UInt64(id)!] ?? 0 }
  func waitForAttempts(_ id: String, _ count: Int) -> Bool {
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline {
      if attempts(id) >= count { return true }
      Thread.sleep(forTimeInterval: 0.01)
    }
    return false
  }
  func releaseHeldFailure(_ id: String) {
    lock.lock()
    let callback = held.removeValue(forKey: UInt64(id)!)
    if let callback { completedHeld[UInt64(id)!] = callback }
    lock.unlock()
    check(callback != nil, "missing held disposer")
    callback?(failure)
  }
  func repeatHeldFailure(_ id: String) {
    lock.lock()
    let callback = completedHeld[UInt64(id)!]
    lock.unlock()
    check(callback != nil, "missing completed held disposer")
    callback?(failure)
  }
  func invoke(_ session: MobileCoreSession, _ completion: @escaping (String) -> Void) {
    let id = session.sessionId()
    lock.lock()
    counts[id, default: 0] += 1
    let shouldHold = hold.remove(id) != nil
    let shouldFail = fail.remove(id) != nil
    if shouldHold { held[id] = completion }
    lock.unlock()
    if shouldHold { return }
    if shouldFail { completion(failure); return }
    session.invoke(op: "session.dispose", argsJson: "{}", completion: HarnessInvokeCompletion { completion($0) })
  }
}

/// Scripted CoreBluetooth stand-in. Mutable state is confined to `workQueue`,
/// exactly like the production radio.
final class ScriptedDriver: UnifiedBleRustRadioDriver {
  let workQueue = DispatchQueue(label: "harness.scripted-driver")
  var snapshot: NSDictionary = ["availability": "available", "authorization": "granted", "power": "on", "safeReason": NSNull()]
  var restored: [NSDictionary] = []
  var canSendWithoutResponse = true
  var readProvenance = OwnedCoreBluetoothReadProvenance.readResponse
  var calls = [String]()
  var cancelled = [String]()
  var subscriptionIdentifiers = [String]()
  var unsubscribeFailures = [NSError]()
  var hangingConnects = [String: (NSError?) -> Void]()
  var holdSetupWrites = false
  var setupWrites = [(NSError?) -> Void]()

  private func record(_ call: String) { calls.append(call) }

  func adapterSnapshot(completion: @escaping (NSDictionary) -> Void) {
    workQueue.async { completion(self.snapshot) }
  }

  func restoredPeerSnapshots(completion: @escaping ([NSDictionary]) -> Void) {
    workQueue.async { completion(self.restored) }
  }

  func writeLimits(peerIdentifier: String, completion: @escaping (NSDictionary?, NSError?) -> Void) {
    workQueue.async {
      completion(["withResponse": 512, "withoutResponse": 182, "canSendWithoutResponse": self.canSendWithoutResponse], nil)
    }
  }

  func startScan(serviceUUIDs: [String], allowDuplicates: Bool, operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    workQueue.async {
      self.record("startScan services=\(serviceUUIDs.count) duplicates=\(allowDuplicates)")
      completion(nil)
    }
  }

  func stopScan(operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    workQueue.async {
      self.record("stopScan \(operationIdentifier)")
      completion(nil)
    }
  }

  func connect(peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    workQueue.async {
      self.record("connect \(peerIdentifier)")
      if peerIdentifier == "HANG" {
        self.hangingConnects[operationIdentifier] = completion
        return
      }
      completion(nil)
    }
  }

  func disconnect(peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    workQueue.async {
      self.record("disconnect \(peerIdentifier)")
      completion(nil)
    }
  }

  func discover(peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSDictionary?, NSError?) -> Void) {
    workQueue.async {
      self.record("discover \(peerIdentifier)")
      completion([
        "services": [[
          "uuid": "0000180D-0000-1000-8000-00805F9B34FB",
          "occurrence": 0,
          "characteristics": [[
            "uuid": "00002A37-0000-1000-8000-00805F9B34FB",
            "occurrence": 0,
            "readable": true,
            "writableWithResponse": true,
            "writableWithoutResponse": true,
            "notifiable": true,
            "indicatable": false,
            "descriptors": [["uuid": "00002902-0000-1000-8000-00805F9B34FB", "occurrence": 0]]
          ], [
            "uuid": "00002A99-0000-1000-8000-00805F9B34FB",
            "occurrence": 0,
            "readable": false,
            "writableWithResponse": false,
            "writableWithoutResponse": false,
            "notifiable": true,
            "indicatable": true,
            "descriptors": [["uuid": "00002902-0000-1000-8000-00805F9B34FB", "occurrence": 0]]
          ]]
        ]]
      ] as NSDictionary, nil)
    }
  }

  func readCharacteristic(
    peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int, characteristicUUID: String,
    characteristicOccurrence: Int, operationIdentifier: String,
    completion: @escaping (NSData?, OwnedCoreBluetoothReadProvenance, NSError?) -> Void
  ) {
    workQueue.async {
      self.record("read \(characteristicUUID)")
      completion(Data([0x64]) as NSData, self.readProvenance, nil)
    }
  }

  func readRssi(peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSNumber?, NSError?) -> Void) {
    workQueue.async {
      self.record("readRssi \(peerIdentifier)")
      completion(-60, nil)
    }
  }

  func write(
    peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int, characteristicUUID: String,
    characteristicOccurrence: Int, value: NSData, withResponse: Bool, operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  ) {
    workQueue.async {
      self.record("write \(value.length) response=\(withResponse)")
      if self.holdSetupWrites { self.setupWrites.append(completion); return }
      completion(nil)
    }
  }

  func subscribe(
    peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int, characteristicUUID: String,
    characteristicOccurrence: Int, subscriptionIdentifier: String, operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  ) {
    workQueue.async {
      self.record("subscribe \(subscriptionIdentifier)")
      self.subscriptionIdentifiers.append(subscriptionIdentifier)
      completion(nil)
    }
  }

  func unsubscribe(
    peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int, characteristicUUID: String,
    characteristicOccurrence: Int, subscriptionIdentifier: String, operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  ) {
    workQueue.async {
      self.record("unsubscribe \(subscriptionIdentifier)")
      completion(self.unsubscribeFailures.isEmpty ? nil : self.unsubscribeFailures.removeFirst())
    }
  }

  func readDescriptor(
    peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int, characteristicUUID: String,
    characteristicOccurrence: Int, descriptorUUID: String, descriptorOccurrence: Int, operationIdentifier: String,
    completion: @escaping (NSData?, NSError?) -> Void
  ) {
    workQueue.async { completion(Data([0x01, 0x00]) as NSData, nil) }
  }

  func writeDescriptor(
    peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int, characteristicUUID: String,
    characteristicOccurrence: Int, descriptorUUID: String, descriptorOccurrence: Int, value: NSData,
    operationIdentifier: String, completion: @escaping (NSError?) -> Void
  ) {
    workQueue.async { completion(nil) }
  }

  func cancelOperation(_ operationIdentifier: String, completion: @escaping (NSDictionary) -> Void) {
    workQueue.async {
      self.cancelled.append(operationIdentifier)
      self.hangingConnects.removeValue(forKey: operationIdentifier)
      completion(["state": "released", "failures": []])
    }
  }

  func onQueue<T>(_ body: @escaping () -> T) -> T {
    workQueue.sync(execute: body)
  }
}

final class Harness {
  let driver = ScriptedDriver()
  var adapter: UnifiedBleRustRadioAdapter!
  var host: MobileCoreHost!
  var sessions: UnifiedBleRustCoreSessions!
  let token = NSObject()
  let wakeLock = NSLock()
  var wakes = [String]()
  var sessionId = ""
  var records = [[String: Any]]()
  private let continuationLock = NSLock()
  private var continuationCallback: ((String?, String?) -> Void)?
  var continuationDone: ((String?, String?) -> Void)? {
    get { continuationLock.lock(); defer { continuationLock.unlock() }; return continuationCallback }
    set { continuationLock.lock(); defer { continuationLock.unlock() }; continuationCallback = newValue }
  }

  var admissions = [String: Int]()
  var nextAdmission = 0

  /// The wire rule (finding 109): an invoke naming an operation carries the
  /// session's next admission; `op.cancel` names its target's.
  func admitted(_ op: String, _ args: [String: Any]) -> [String: Any] {
    guard op != "scan.stop", args["admission"] == nil, let id = args["operationId"] as? String else { return args }
    let admission = admissions[id] ?? {
      nextAdmission += 1
      admissions[id] = nextAdmission
      return nextAdmission
    }()
    var admittedArgs = args
    admittedArgs["admission"] = admission
    return admittedArgs
  }

  func invoke(_ op: String, _ args: [String: Any]) -> [String: Any] {
    let args = admitted(op, args)
    let (envelope, failure): (String?, String?) = waitFor(op) { done in
      self.sessions.invoke(sessionId: self.sessionId, op: op, argsJson: jsonText(args)) { done(($0, $1)) }
    }
    check(failure == nil, "\(op) was refused: \(failure ?? "")")
    return json(envelope!)
  }

  func ok(_ op: String, _ args: [String: Any]) -> Any {
    let envelope = invoke(op, args)
    check(envelope["ok"] as? Bool == true, "\(op) failed: \(envelope)")
    return envelope["value"] as Any
  }

  func errorCode(_ envelope: [String: Any]) -> String? {
    (envelope["error"] as? [String: Any])?["code"] as? String
  }

  /// Drains until `predicate` holds over everything drained so far.
  func drainUntil(_ what: String, _ predicate: ([[String: Any]]) -> Bool) {
    let deadline = Date().addingTimeInterval(10)
    while !predicate(records) {
      check(Date() < deadline, "drain never produced \(what); records: \(records)")
      var more = true
      while more {
        let (text, failure): (String?, String?) = waitFor("drain") { done in
          self.sessions.drain(sessionId: self.sessionId, maxItems: 256, maxBytes: 65536) { done(($0, $1)) }
        }
        check(failure == nil, "drain refused: \(failure ?? "")")
        let page = json(text!)
        records.append(contentsOf: page["records"] as? [[String: Any]] ?? [])
        more = page["more"] as? Bool ?? false
      }
      Thread.sleep(forTimeInterval: 0.02)
    }
  }

  func records(_ type: String) -> [[String: Any]] { records.filter { $0["t"] as? String == type } }

  func onRadioQueue(_ body: @escaping () -> Void) {
    driver.workQueue.sync(execute: body)
  }

  func run() {
    translationChecks()
    restorationIdentityChecks()
    randomBytesChecks()

    driver.restored = [["peerIdentifier": "R", "name": "Polar H10 R", "connected": true]]
    sessions = UnifiedBleRustCoreSessions(installer: { wake in
      self.adapter = UnifiedBleRustRadioAdapter(driver: self.driver, onRestoredPeer: { peer in
        self.sessions.continueRestoredPeer(peer) { value, failure in
          self.continuationDone?(value, failure)
        }
      })
      self.host = try mobileHostInstall(
        radio: self.adapter, wake: wake, platform: "apple", owner: "apple-harness", adapterLabel: "corebluetooth"
      )
      self.adapter.bind(sink: self.host)
      return self.host
    })
    check(sessions.ensureHost() == nil, "host install failed")
    check(sessions.wireRevision() == mobileWireRevision(), "wire revision is not Rust's")
    check(sessions.contractRevision() == mobileContractRevision(), "contract revision is not Rust's")
    check(!json(sessions.nativeBuildIdentity()).isEmpty, "build identity is not Rust's JSON record")

    // Admission: a foreign wire revision is refused before a session exists.
    let refused: (String?, String?) = waitFor("foreign open") { done in
      self.sessions.openSession("js", expectedWireRevision: "ubm-mobile-wire/0", ownerToken: token, onWake: { _ in }) { done(($0, $1)) }
    }
    check(refused.0 == nil && json(refused.1 ?? "{}")["code"] as? String == "protocol.incompatible",
          "foreign wire revision was not refused as protocol.incompatible: \(refused)")
    check(Set(json(refused.1!).keys) == ["code", "domain", "operation", "detail"], "failure JSON keys are not exact")

    let admitted: (String?, String?) = waitFor("open") { done in
      self.sessions.openSession("js", expectedWireRevision: mobileWireRevision(), ownerToken: token, onWake: { id in
        self.wakeLock.lock()
        self.wakes.append(id)
        self.wakeLock.unlock()
      }) { done(($0, $1)) }
    }
    check(admitted.1 == nil, "open refused: \(admitted.1 ?? "")")
    let admission = json(admitted.0!)
    check(admission["sessionId"] is NSNumber, "admission sessionId is not a JSON number")
    sessionId = String((admission["sessionId"] as! NSNumber).uint64Value)
    check(admission["wireRevision"] as? String == mobileWireRevision(), "admission wire revision")

    // Adapter state is read from the platform, authorization included.
    let state = ok("adapter.state", [:]) as? [String: Any]
    check(state?["authorization"] as? String == "granted" && state?["power"] as? String == "on", "adapter.state: \(String(describing: state))")

    // Restoration: peers CoreBluetooth restored before the host existed.
    let restoredPeers = ok("peers.restored", [:]) as? [[String: Any]] ?? []
    check(restoredPeers.contains { $0["peerId"] as? String == "R" }, "restored peer R missing: \(restoredPeers)")
    onRadioQueue {
      self.adapter.protocolRadioDidRestorePeers([
        ["peerIdentifier": "R", "name": "Polar H10 R", "connected": true],
        ["peerIdentifier": "R2", "name": NSNull(), "connected": false],
      ])
    }
    // The host came up before this session opened, so the session may or
    // may not have seen R's record; either way every peer is announced once.
    let announcedPeers = { () -> [String] in
      self.records("restored").flatMap { ($0["peers"] as? [[String: Any]]) ?? [] }.compactMap { $0["peerId"] as? String }
    }
    drainUntil("restored record for R2") { _ in announcedPeers().contains("R2") }
    Thread.sleep(forTimeInterval: 0.2)
    drainUntil("quiet outbox") { _ in true }
    let announced = announcedPeers()
    check(announced.filter { $0 == "R2" }.count == 1 && announced.filter { $0 == "R" }.count <= 1,
          "restoration was re-announced: \(announced)")

    // PR210-52: restored peers are adopted once per process, across managers.
    let claimedPeers = { (value: Any) -> Set<String> in
      Set((((value as? [String: Any])?["peers"] as? [[String: Any]]) ?? []).compactMap { $0["peerId"] as? String })
    }
    let claimed = claimedPeers(ok("peers.claim-restored", ["maxPeers": 1023]))
    check(claimed == ["R", "R2"], "claim: \(claimed)")
    check(claimedPeers(ok("peers.claim-restored", ["maxPeers": 1023])).isEmpty, "the claimant claimed twice")
    let other: (String?, String?) = waitFor("second manager open") { done in
      self.sessions.openSession("js-2", expectedWireRevision: mobileWireRevision(), ownerToken: token, onWake: { _ in }) {
        done(($0, $1))
      }
    }
    check(other.1 == nil, "second manager open refused: \(other.1 ?? "")")
    let otherId = String((json(other.0!)["sessionId"] as! NSNumber).uint64Value)
    let otherClaim: (String?, String?) = waitFor("second manager claim") { done in
      self.sessions.invoke(sessionId: otherId, op: "peers.claim-restored", argsJson: jsonText(["maxPeers": 1023])) {
        done(($0, $1))
      }
    }
    let otherValue = json(otherClaim.0 ?? "{}")
    check(otherValue["ok"] as? Bool == true && claimedPeers(otherValue["value"] as Any).isEmpty,
          "a second manager re-adopted restored peers: \(otherClaim)")
    let otherClosed: String? = waitFor("second manager close") { done in self.sessions.closeSession(otherId) { done($0) } }
    check(otherClosed == nil, "second manager close failed: \(otherClosed ?? "")")

    // Scan + advertisements (manufacturer split, short section surfaced).
    _ = ok("scan.start", ["serviceUuids": [], "duplicatePolicy": "all", "operationId": "op-scan"])
    check(driver.onQueue { self.driver.calls.contains("startScan services=0 duplicates=true") }, "scan not started with duplicates")
    onRadioQueue {
      self.adapter.protocolRadioDidReceiveAdvertisement([
        "peerIdentifier": "P", "localName": "Polar H10 P", "rssi": -50,
        "serviceUUIDs": ["0000180D-0000-1000-8000-00805F9B34FB"],
        "manufacturerData": Data([0x6B, 0x00, 0x01, 0x02]), "connectable": true,
      ])
      self.adapter.protocolRadioDidReceiveAdvertisement([
        "peerIdentifier": "S", "rssi": 127, "manufacturerData": Data([0x01]),
      ])
      self.adapter.protocolRadioDidReceiveAdvertisement(["peerIdentifier": "HANG", "rssi": -70])
    }
    drainUntil("advertisements and the short-section drop") { records in
      let peers = records.filter { $0["t"] as? String == "adv" }.compactMap { $0["peerId"] as? String }
      return Set(peers).isSuperset(of: ["P", "S", "HANG"]) && records.contains { $0["t"] as? String == "ingress-drop" }
    }
    let advP = records("adv").first { $0["peerId"] as? String == "P" }!
    let manufacturer = advP["manufacturerData"] as? [[String: Any]] ?? []
    check(manufacturer.count == 1 && (manufacturer[0]["companyId"] as? NSNumber)?.intValue == 0x006B
          && manufacturer[0]["payloadB64"] as? String == "AQI=", "manufacturer split: \(advP)")
    check((advP["connectable"] as? Bool) == true, "connectable lost: \(advP)")
    let advS = records("adv").first { $0["peerId"] as? String == "S" }!
    check(advS["rssi"] is NSNull && advS["manufacturerData"] is NSNull, "RSSI 127 / short section: \(advS)")
    check(records("ingress-drop").contains { $0["class"] as? String == "advertisement" }, "short section not surfaced")

    // Connect, discover, subscribe, value, RSSI, read, writes.
    _ = ok("connection.connect", ["peerId": "P", "lease": "lease-p", "operationId": "op-connect"])
    // PR210-54: CoreBluetooth has no LE PHY control; refused before any effect.
    let connectsBefore = driver.onQueue { self.driver.calls.filter { $0.hasPrefix("connect ") }.count }
    let phy = invoke("connection.connect", ["peerId": "P", "lease": "lease-phy", "operationId": "op-phy", "preferredPhy": ["le-2m"]])
    check(errorCode(phy) == "capability.unsupported", "preferredPhy on Apple: \(phy)")
    check(driver.onQueue { self.driver.calls.filter { $0.hasPrefix("connect ") }.count } == connectsBefore,
          "a refused PHY preference reached CoreBluetooth")
    let discovered = ok("gatt.discover", ["peerId": "P", "lease": "lease-p", "operationId": "op-discover"]) as? [String: Any]
    check((discovered?["services"] as? [[String: Any]])?.count == 1, "discover: \(String(describing: discovered))")
    let selector: [String: Any] = [
      "serviceUuid": hrService, "serviceOccurrence": 0, "characteristicUuid": hrMeasurement, "characteristicOccurrence": 0,
    ]
    let subscribed = ok("gatt.subscribe", ["peerId": "P", "selector": selector, "consumer": "hr", "operationId": "op-sub"]) as? [String: Any]
    check(subscribed?["delivery"] as? String == "unknown", "Apple delivery must be unknown: \(String(describing: subscribed))")
    let subscriptionIdentifier = driver.onQueue { self.driver.subscriptionIdentifiers.last! }
    // Decision C: CoreBluetooth enables notifications on a notify+indicate
    // characteristic, so Rust refuses a hard indication requirement before
    // any request reaches the radio.
    var bothSelector = selector
    bothSelector["characteristicUuid"] = "00002a99-0000-1000-8000-00805f9b34fb"
    let requireIndication = invoke("gatt.subscribe", [
      "peerId": "P", "selector": bothSelector, "consumer": "both", "operationId": "op-sub-ind",
      "deliveryMode": "require-indication",
    ])
    check(errorCode(requireIndication) == "capability.limited", "require-indication on notify+indicate: \(requireIndication)")
    check(driver.onQueue { self.driver.subscriptionIdentifiers.count } == 1, "the refused requirement reached the radio")
    onRadioQueue {
      self.adapter.protocolRadioDidReceiveNotification(subscriptionIdentifier, value: Data([0x00, 0x48]) as NSData)
      self.adapter.protocolRadioDidReceiveNotification("ubm-rust-sub-unknown", value: Data([0x01]) as NSData)
    }
    drainUntil("heart-rate value and the unknown-subscription drop") { records in
      records.contains { $0["t"] as? String == "value" && $0["valueB64"] as? String == "AEg=" }
        && records.contains { $0["t"] as? String == "ingress-drop" && $0["class"] as? String == "notification" }
    }
    let rssi = ok("connection.rssi", ["peerId": "P", "lease": "lease-p", "operationId": "op-rssi"]) as? [String: Any]
    check((rssi?["rssi"] as? NSNumber)?.intValue == -60, "rssi: \(String(describing: rssi))")
    // gatt:maximum-write-length: CoreBluetooth's own maximumWriteValueLength(for:) per type.
    for (mode, expected) in [("with-response", 512), ("without-response", 182)] {
      let maximum = ok("connection.maximum-write-length", [
        "peerId": "P", "lease": "lease-p", "mode": mode, "operationId": "op-mwl-\(mode)",
      ]) as? [String: Any]
      check((maximum?["maximumWriteLength"] as? NSNumber)?.intValue == expected,
            "maximum write length \(mode): \(String(describing: maximum))")
    }
    let read = ok("gatt.read", ["peerId": "P", "selector": selector, "operationId": "op-read"]) as? [String: Any]
    check(read?["valueB64"] as? String == "ZA==", "read: \(String(describing: read))")
    check(read?["provenance"] as? String == "read-response", "read provenance: \(String(describing: read))")
    // CoreBluetooth reading a notifying characteristic: the radio says the
    // value may be a notification and the adapter carries that verbatim.
    driver.onQueue { self.driver.readProvenance = .readOrNotification }
    let fused = ok("gatt.read", ["peerId": "P", "selector": selector, "operationId": "op-read-fused"]) as? [String: Any]
    check(fused?["valueB64"] as? String == "ZA==" && fused?["provenance"] as? String == "read-or-notification",
          "read while notifying: \(String(describing: fused))")
    driver.onQueue { self.driver.readProvenance = .readResponse }
    let writeArgs: (String, String) -> [String: Any] = { id, value in
      ["peerId": "P", "selector": selector, "operationId": id, "valueB64": value, "mode": "without-response"]
    }
    driver.onQueue { self.driver.canSendWithoutResponse = false }
    let full = invoke("gatt.write", writeArgs("op-w1", "AQI="))
    check(full["ok"] as? Bool == false && full["commit"] as? String == "not-dispatched",
          "full WwR queue must be refused as not-dispatched: \(full)")
    driver.onQueue { self.driver.canSendWithoutResponse = true }
    let oversize = invoke("gatt.write", writeArgs("op-w2", Data(repeating: 7, count: 200).base64EncodedString()))
    check(errorCode(oversize) == "bytes.too-large" && oversize["commit"] as? String == "not-dispatched",
          "WwR beyond maximumWriteValueLength(.withoutResponse) is refused before CoreBluetooth: \(oversize)")
    let written = ok("gatt.write", writeArgs("op-w3", "AQI=")) as? [String: Any]
    check(written?["commitState"] as? String == "unknown", "WwR commit: \(String(describing: written))")
    // With response CoreBluetooth performs the long write itself, up to
    // maximumWriteValueLength(.withResponse).
    var longArgs = writeArgs("op-w4", Data(repeating: 7, count: 512).base64EncodedString())
    longArgs["mode"] = "with-response"
    let long = ok("gatt.write", longArgs) as? [String: Any]
    check(long?["commitState"] as? String == "confirmed", "long write with response: \(String(describing: long))")
    check(driver.onQueue { self.driver.calls.filter { $0.hasPrefix("write ") } } == ["write 2 response=false", "write 512 response=true"],
          "only the admissible writes reach CoreBluetooth")
    let mtu = invoke("connection.request-mtu", ["peerId": "P", "lease": "lease-p", "mtu": 185, "operationId": "op-mtu"])
    check(errorCode(mtu) == "capability.unsupported", "request-mtu on Apple: \(mtu)")

    // Finding 140 (I-1): the Swift adapter itself refuses the Android-only
    // foreground service and companion chooser, before CoreBluetooth, as
    // the legacy module refused them.
    let callsBeforeRefusals = driver.onQueue { self.driver.calls.count }
    let background = invoke("background.acquire", ["kind": "connected-device", "reason": "workout", "operationId": "op-bg"])
    check(errorCode(background) == "capability.unsupported", "background.acquire on Apple: \(background)")
    check(((background["error"] as? [String: Any])?["detail"] as? String ?? "").contains("Android-only"),
          "the adapter's own refusal: \(background)")
    // The owner refuses the companion chooser on Apple before the adapter;
    // the adapter's own refusal (below) stands behind it.
    let companion = invoke("companion.associate", ["name": "Polar", "operationId": "op-companion"])
    check(errorCode(companion) == "capability.unsupported", "companion.associate on Apple: \(companion)")
    check(driver.onQueue { self.driver.calls.count } == callsBeforeRefusals, "no refusal reached CoreBluetooth")
    androidOnlyRefusalsAtTheAdapter()

    // Cancellation of an in-flight connect: the OS work is withdrawn.
    let hanging = DispatchSemaphore(value: 0)
    var hangingEnvelope: String?
    sessions.invoke(sessionId: sessionId, op: "connection.connect",
                    argsJson: jsonText(self.admitted("connection.connect", ["peerId": "HANG", "lease": "lease-h", "operationId": "op-hang"]))) { envelope, _ in
      hangingEnvelope = envelope
      hanging.signal()
    }
    let deadline = Date().addingTimeInterval(10)
    while driver.onQueue({ self.driver.hangingConnects.isEmpty }) {
      check(Date() < deadline, "hanging connect never reached the driver")
      Thread.sleep(forTimeInterval: 0.01)
    }
    let cancel = ok("op.cancel", ["operationId": "op-hang"]) as? [String: Any]
    check(cancel?["state"] as? String == "cancellation-requested", "op.cancel: \(String(describing: cancel))")
    check(hanging.wait(timeout: .now() + timeout) == .success, "cancelled connect never answered")
    check(errorCode(json(hangingEnvelope!)) == "operation.aborted", "cancelled connect: \(hangingEnvelope!)")
    let cancelledOps = driver.onQueue { self.driver.cancelled }
    check(cancelledOps.count == 1 && cancelledOps[0].hasPrefix("ubm-rust-"), "driver cancel: \(cancelledOps)")

    // Link loss, then adoption of the restored, still-connected peer.
    onRadioQueue { self.adapter.protocolRadioDidDisconnectPeer("P", error: nil) }
    drainUntil("link loss") { records in
      records.contains { $0["t"] as? String == "link" && $0["peerId"] as? String == "P" }
    }
    _ = ok("connection.connect", ["peerId": "R", "lease": "lease-r", "operationId": "op-adopt"])
    check(driver.onQueue { self.driver.calls.contains("connect R") }, "restored peer was not adopted through connect")

    // Issue #212: presence observation is Android-only. Apple refuses it
    // before any effect; restoration arrives through willRestoreState.
    let observePresence = invoke("presence.observe", ["peerId": "R", "operationId": "op-presence"])
    check(errorCode(observePresence) == "capability.unsupported", "presence.observe on Apple: \(observePresence)")
    let unobservePresence = invoke("presence.unobserve", ["peerId": "R", "operationId": "op-unpresence"])
    check(errorCode(unobservePresence) == "capability.unsupported", "presence.unobserve on Apple: \(unobservePresence)")

    // Issue #212: subscription replay on the adopted restored link.
    let restoredSelector: [String: Any] = [
      "serviceUuid": hrService, "serviceOccurrence": 0,
      "characteristicUuid": hrMeasurement, "characteristicOccurrence": 0,
    ]
    _ = ok("gatt.discover", ["peerId": "R", "lease": "lease-r", "operationId": "op-discover-r"])
    let replayed = ok("gatt.subscribe", [
      "peerId": "R", "selector": restoredSelector, "consumer": "hr-restored", "operationId": "op-sub-r",
    ]) as? [String: Any]
    check(replayed?["delivery"] as? String == "unknown", "restored subscription delivery: \(String(describing: replayed))")
    let restoredSubscription = driver.onQueue { self.driver.subscriptionIdentifiers.last! }
    onRadioQueue {
      self.adapter.protocolRadioDidReceiveNotification(restoredSubscription, value: Data([0x01, 0x02]) as NSData)
    }
    drainUntil("restored subscription value") { records in
      records.contains {
        $0["t"] as? String == "value" && $0["consumer"] as? String == "hr-restored" && $0["valueB64"] as? String == "AQI="
      }
    }

    unsubscribeRefusalChecks()

    // Adapter power loss ends the scan with a reported source failure.
    onRadioQueue {
      self.driver.snapshot = ["availability": "available", "authorization": "granted", "power": "off", "safeReason": "off"]
      self.adapter.protocolRadioDidUpdateAdapterState(self.driver.snapshot)
    }
    drainUntil("adapter record and scan end") { records in
      records.contains { $0["t"] as? String == "adapter" }
        && records.contains { $0["t"] as? String == "scan-end" }
    }
    check(driver.onQueue { self.driver.calls.contains("stopScan ubm-rust-scan-lost") }, "lost scan was not cleared")
    let offConnect = invoke("connection.connect", ["peerId": "HANG", "lease": "lease-h2", "operationId": "op-off"])
    check(errorCode(offConnect) == "adapter.powered-off", "connect while off: \(offConnect)")

    wakeLock.lock()
    let wakeIds = Set(wakes)
    wakeLock.unlock()
    check(wakeIds == [sessionId], "wakes must name this session as a decimal string: \(wakeIds)")

    // Close: dispose through Rust, forget the lease, idempotent afterwards.
    let closed: String? = waitFor("close") { done in self.sessions.closeSession(self.sessionId) { done($0) } }
    check(closed == nil, "close failed: \(closed ?? "")")
    check(driver.onQueue { self.driver.calls.contains("disconnect R") }, "dispose did not release the adopted link")
    let again: String? = waitFor("close again") { done in self.sessions.closeSession(self.sessionId) { done($0) } }
    check(again == nil, "second close is not idempotent")
    let afterClose: (String?, String?) = waitFor("invoke after close") { done in
      self.sessions.invoke(sessionId: self.sessionId, op: "adapter.state", argsJson: "{}") { done(($0, $1)) }
    }
    check(json(afterClose.1 ?? "{}")["code"] as? String == "lifecycle.destroyed", "closed session: \(afterClose)")
    let badDrain: (String?, String?) = waitFor("bad drain") { done in
      self.sessions.drain(sessionId: self.sessionId, maxItems: 0.5, maxBytes: 10) { done(($0, $1)) }
    }
    check(json(badDrain.1 ?? "{}")["code"] as? String == "argument.invalid", "fractional drain: \(badDrain)")

    lifecycleCleanupChecks()
    coldWarmAdmissionChecks()
    queuedWarmReplacementChecks()
    nativeContinuationChecks()
    warmContinuationChecks()
    durableSetupContinuationChecks()

    // Process shutdown disables every CCCD the radio still holds.
    let cleanup = json(host.shutdown())
    check(cleanup["state"] != nil, "shutdown cleanup record: \(cleanup)")
    let counters: UnifiedBleRustRadioAdapterCounters = waitFor("adapter counters") { self.adapter.adapterCounters(completion: $0) }
    check(counters.mismatchedCompletions == 0, "Rust refused an adapter answer shape: \(counters)")
    check(counters.cancelledRequests >= 1, "cancel was not counted: \(counters)")
  }

  func unsubscribeRefusalChecks() {
    let selector: [String: Any] = ["serviceUuid": hrService, "serviceOccurrence": 0,
                                  "characteristicUuid": hrMeasurement, "characteristicOccurrence": 0]
    for parentCleanup in [false, true] {
      let peer = parentCleanup ? "UNSUB-PARENT" : "UNSUB-RETRY"
      let lease = "lease-\(peer)"
      let consumer = "consumer-\(peer)"
      _ = ok("connection.connect", ["peerId": peer, "lease": lease, "operationId": "connect-\(peer)"])
      _ = ok("gatt.discover", ["peerId": peer, "lease": lease, "operationId": "discover-\(peer)"])
      _ = ok("gatt.subscribe", ["peerId": peer, "selector": selector, "consumer": consumer, "operationId": "subscribe-\(peer)"])
      let identifier = driver.onQueue { self.driver.subscriptionIdentifiers.last! }
      driver.onQueue {
        self.driver.unsubscribeFailures = [NSError(domain: "CBErrorDomain", code: 0,
                                                   userInfo: [NSLocalizedDescriptionKey: "scripted unsubscribe refusal"])]
      }
      let refused = invoke("gatt.unsubscribe", ["peerId": peer, "selector": selector,
                                                 "consumer": consumer, "operationId": "remove-\(peer)"])
      check(errorCode(refused) == "platform.failure", "unsubscribe callback refusal must not become release: \(refused)")
      let error = refused["error"] as? [String: Any]
      let platform = error?["platform"] as? [String: Any]
      check(error?["operation"] as? String == "gatt.unsubscribe" && error?["domain"] as? String == "platform",
            "unsubscribe error operation/domain lost: \(refused)")
      check(platform?["domain"] as? String == "CBErrorDomain" && platform?["code"] as? String == "0",
            "unsubscribe native cause lost: \(refused)")
      check(refused["retryability"] as? String == "never"
              && (platform?["message"] as? String)?.contains("scripted unsubscribe refusal") == true,
            "unsubscribe retry advice or native message changed: \(refused)")
      check(driver.onQueue { self.driver.calls.filter { $0 == "unsubscribe \(identifier)" }.count } == 1,
            "first unsubscribe did not reach exact native subscription")
      if !parentCleanup {
        _ = ok("gatt.unsubscribe", ["peerId": peer, "selector": selector,
                                     "consumer": consumer, "operationId": "retry-remove-\(peer)"])
        check(driver.onQueue { self.driver.calls.filter { $0 == "unsubscribe \(identifier)" }.count } == 2,
              "failed unsubscribe ownership was not retained for native retry")
      }
      _ = ok("connection.disconnect", ["peerId": peer, "lease": lease, "operationId": "release-\(peer)"])
      check(driver.onQueue { self.driver.calls.contains("disconnect \(peer)") }, "parent release did not reach native disconnect")
      check(errorCode(refused) == "platform.failure", "later cleanup must not rewrite the prior failure")
    }
    print("[AppleRustRadioAdapterHarness] unsubscribe CBErrorDomain#0 preserved; exact-owner retry and parent disconnect passed (scripted callback only).")
  }

  func durableSetupContinuationChecks() {
    let peer = "9828347E-45DF-2EEB-E928-6E443F4065E3"
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent("ubm-swift-recording-\(UUID().uuidString)")
    try! FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
    check(json(mobileRecordingConfigureDirectory(path: directory.path))["ok"] as? Bool == true, "configure actual offline Swift registry")
    let selector: [String: Any] = ["serviceUuid": hrService, "serviceOccurrence": 1, "characteristicUuid": hrMeasurement, "characteristicOccurrence": 1]
    let order = jsonText(["onAppearance":"native", "peerId":peer, "resubscribe":[selector],
      "recording":["id":"swift-setup","maxBytes":1048576,"maxRecords":1000],
      "setup":[["selector":selector,"value":[2,0],"timeoutMs":10000,
        "response":["subscriptionIndex":0,"prefix":[240,2,0],"minLength":4,"maxLength":4,"status":["offset":3,"accepted":[0]]]]]])
    let reservation = json(host.continuationReserveDeclaration(declarationJson: order))["value"] as? [String: Any] ?? [:]
    let token = reservation["reservationToken"] as? String ?? ""
    check(!token.isEmpty && json(host.continuationCommitDeclaration(reservationToken: token))["ok"] as? Bool == true, "commit setup recording order")
    func cursor(_ operation: String, _ token: String = "") -> [String: Any] {
      let answer = json(mobileRecordingControl(operation: operation, id: "swift-setup", token: token, maxItems: 256, maxBytes: 65536))
      check(answer["ok"] as? Bool == true, "recording \(operation): \(answer)")
      return answer["value"] as? [String: Any] ?? [:]
    }
    func prefix(containing bytes: String) -> [String: Any] {
      let deadline = Date().addingTimeInterval(8)
      while Date() < deadline {
        let page = cursor("prepare")
        let records = page["records"] as? [[String: Any]] ?? []
        if records.contains(where: { ($0["record"] as? [String: Any])?["valueB64"] as? String == bytes }) { return page }
        if let token = page["token"] as? String { check(cursor("acknowledge", token)["acknowledged"] as? Bool == true, "consume inspected control prefix") }
        Thread.sleep(forTimeInterval: 0.005)
      }
      check(false, "missing durable bytes \(bytes)")
      return [:]
    }
    driver.onQueue { self.driver.holdSetupWrites = true }
    let firstCompleted = DispatchSemaphore(value: 0)
    host.continuationExecute(peerId: peer, declarationJson: order, completion: HarnessInvokeCompletion { envelope in
      check(json(envelope)["ok"] as? Bool == true, "setup execution refused: \(envelope)")
      firstCompleted.signal()
    })
    for generation in 0..<2 {
      let deadline = Date().addingTimeInterval(8)
      while driver.onQueue({ self.driver.setupWrites.isEmpty }) {
        check(Date() < deadline, "setup write never reached Swift driver")
        Thread.sleep(forTimeInterval: 0.005)
      }
      let subscription = driver.onQueue { self.driver.subscriptionIdentifiers.last! }
      onRadioQueue { self.adapter.protocolRadioDidReceiveNotification(subscription, value: Data([240,2,0,0]) as NSData) }
      let ack = prefix(containing: "8AIAAA==")
      if generation == 0 { check(firstCompleted.wait(timeout: .now()) == .timedOut, "ACK must not bypass held ATT") }
      check(NSDictionary(dictionary: cursor("prepare")) == NSDictionary(dictionary: ack), "early ACK prefix replays")
      check(cursor("acknowledge", ack["token"] as? String ?? "")["acknowledged"] as? Bool == true, "ACK inspected response prefix")
      driver.onQueue { self.driver.setupWrites.removeFirst()(nil) }
      if generation == 0 {
        check(firstCompleted.wait(timeout: .now() + timeout) == .success, "setup did not settle after ATT")
        onRadioQueue { self.adapter.protocolRadioDidDisconnectPeer(peer, error: nil) }
      }
    }
    driver.onQueue { self.driver.holdSetupWrites = false }
    let deadline = Date().addingTimeInterval(8)
    while true {
      let status: String = waitFor("setup recovery") { done in self.host.continuationDescribeBacklog(completion: HarnessInvokeCompletion(done)) }
      let value = json(status)["value"] as? [String: Any]
      if (value?["continuationOutcome"] as? [String: Any])?["event"] as? String == "continuation.completed" { break }
      check(Date() < deadline, "setup recovery incomplete: \(status)")
      Thread.sleep(forTimeInterval: 0.005)
    }
    let subscription = driver.onQueue { self.driver.subscriptionIdentifiers.last! }
    onRadioQueue { self.adapter.protocolRadioDidReceiveNotification(subscription, value: Data([0,73]) as NSData) }
    let retained = prefix(containing: "AEk=")
    let prepared: String = waitFor("durable radio claim") { done in self.host.continuationPrepareClaim(maxItems: 256, maxBytes: 65536, completion: HarnessInvokeCompletion(done)) }
    let claim = json(prepared)["value"] as? [String: Any] ?? [:]
    check((claim["recording"] as? [String: Any])?["id"] as? String == "swift-setup", "claim retains journal reference")
    check(claim["consumerCount"] as? Int == 2, "link recovery retains both subscription generations")
    let released: String = waitFor("durable radio release") { done in self.host.continuationAcknowledgeClaim(claimToken: claim["claimToken"] as? String ?? "", completion: HarnessInvokeCompletion(done)) }
    check((json(released)["value"] as? [String: Any])?["disposed"] as? Bool == true, "radio release failed: \(released)")
    check(NSDictionary(dictionary: cursor("prepare")) == NSDictionary(dictionary: retained), "offline prefix survives radio release unchanged")
    let receipt = cursor("acknowledge", retained["token"] as? String ?? "")
    check(receipt["acknowledged"] as? Bool == true, "offline ACK failed")
    check(NSDictionary(dictionary: cursor("acknowledge", retained["token"] as? String ?? "")) == NSDictionary(dictionary: receipt), "offline ACK replay differs")
    _ = cursor("stop")
    _ = cursor("clear")
    // Registry owns the open DB until process exit; no unlink of an open SQLite file.
  }

  func nativeContinuationChecks() {
    continuationStatusReadinessChecks()
    // No JS manager/session owns this order. The process host must receive
    // restored peers, reconnect, discover and subscribe on its own.
    let peer = "9828347E-45DF-2EEB-E928-6E443F4065E3"
    let declaration = jsonText([
      "onAppearance": "native", "peerId": peer,
      "resubscribe": [["serviceUuid": hrService, "serviceOccurrence": 1,
                       "characteristicUuid": hrMeasurement, "characteristicOccurrence": 1]],
    ])
    let declared: (String?, String?) = waitFor("native declaration") { done in
      self.sessions.declareBackgroundContinuation(declaration) { done(($0, $1)) }
    }
    check(declared.1 == nil, "Apple UUID declaration refused: \(String(describing: declared.1))")
    // A pending declaration transaction refuses seeding before native work.
    // Its failure is still the outcome of this OS wake, not a missing wake.
    let reserved = json(host.continuationReserveDeclaration(declarationJson: declaration))
    let reservation = (reserved["value"] as? [String: Any])?["reservationToken"] as? String
    check(reserved["ok"] as? Bool == true && reservation != nil, "reservation failed: \(reserved)")
    let expectedSeedError = json(host.continuationSeedDeclaration(declarationJson: declaration))["error"] as? [String: Any] ?? [:]
    var seedCompletions = 0
    sessions.continueRestoredPeer(peer) { value, failure in
      seedCompletions += 1
      check(value == nil && failure != nil, "reserved seed must fail before executing")
      check(NSDictionary(dictionary: json(failure ?? "{}")) == NSDictionary(dictionary: expectedSeedError), "seed failure changed: \(failure ?? "")")
      let wake = json(UserDefaults.standard.string(forKey: "com.sfourdrinier.unifiedblemanager.background-continuation.last-wake") ?? "{}")
      check(wake["event"] as? String == "continuation.failed" && wake["code"] as? String == expectedSeedError["code"] as? String
            && wake["reason"] as? String == expectedSeedError["detail"] as? String, "seed failure was not persisted before completion: \(wake)")
      check(wake["peerAddress"] as? String == peer && wake["strategy"] as? String == "native", "seed failure identity changed: \(wake)")
    }
    check(seedCompletions == 1, "seed refusal must complete exactly once")
    check(json(host.continuationCancelDeclaration(reservationToken: reservation!))["ok"] as? Bool == true, "could not cancel seed-test reservation")
    let staleDeclaration = declaration.replacingOccurrences(of: peer, with: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA")
    let stale: String = waitFor("stale captured wake") { done in
      self.host.continuationExecute(peerId: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA", declarationJson: staleDeclaration,
                                   completion: HarnessInvokeCompletion(done))
    }
    check(errorCode(json(stale)) == "lifecycle.invalid-state", "persisted replacement must fence stale captured wake: \(stale)")
    check(!driver.onQueue { self.driver.calls.contains("connect AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA") },
          "stale declaration must not reach radio admission")
    onRadioQueue {
      self.driver.snapshot = ["availability": "available", "authorization": "granted", "power": "on", "safeReason": NSNull()]
      self.adapter.protocolRadioDidUpdateAdapterState(self.driver.snapshot)
    }
    let completed: (String?, String?) = waitFor("native restoration continuation") { done in
      self.continuationDone = { done(($0, $1)) }
      self.onRadioQueue {
        self.adapter.protocolRadioDidRestorePeers([["peerIdentifier": peer, "name": "H10", "connected": true]])
      }
    }
    continuationDone = nil
    check(completed.1 == nil, "native continuation failed: \(String(describing: completed))")
    check(json(completed.0 ?? "{}")["event"] as? String == "continuation.completed", "native outcome: \(completed)")
    let (_, status) = readyContinuationStatus(deadline: Date().addingTimeInterval(10), beforeRead: { "" }) { done in
      self.sessions.continuationStatus { done(($0, $1)) }
    }
    check(status.1 == nil, "native wake status failed: \(String(describing: status))")
    let lastWake = json(status.0 ?? "{}")["lastWake"] as? [String: Any] ?? [:]
    check(Set(lastWake.keys) == ["observedAtMs", "event", "strategy", "peerAddress", "code", "reason"],
          "native wake must match the strict public status codec: \(lastWake)")
    check(lastWake["observedAtMs"] as? Int64 != nil, "wake timestamp must be integer milliseconds")
    check(lastWake["code"] is NSNull && lastWake["reason"] is NSNull, "success must not invent failure fields")
    check(driver.onQueue { self.driver.calls.contains("connect \(peer)") }, "wake never connected")
    check(driver.onQueue { self.driver.calls.contains("discover \(peer)") }, "wake never discovered")
    let replacement: (String?, String?) = waitFor("active declaration replacement") { done in
      self.sessions.declareBackgroundContinuation("{\"onAppearance\":\"record-only\"}") { done(($0, $1)) }
    }
    check(replacement.0 == nil && json(replacement.1 ?? "{}")["code"] as? String == "lifecycle.invalid-state",
          "a live native session must retain its declaration until its backlog is acknowledged")
    let previousSubscription = driver.onQueue { self.driver.subscriptionIdentifiers.last! }
    check(!completedRecovery(json(completed.0 ?? "{}"), previousSubscription: previousSubscription, currentSubscription: previousSubscription),
          "a completed outcome without a replacement native subscription cannot acknowledge service-change recovery")
    onRadioQueue { self.adapter.protocolRadioDidModifyServices(peer) }
    let recoveryDeadline = Date().addingTimeInterval(10)
    var recovered = false
    var recoveredSubscription: String?
    var lastRecoveryObservation: (String?, String?) = (nil, nil)
    while Date() < recoveryDeadline && !recovered {
      // Capture identity BEFORE each status read, including busy retries.
      // Status is not an atomic generation receipt: exact replacement-ID
      // positive intake below remains the proof that recovery is functional.
      let (candidateSubscription, observation) = readyContinuationStatus(deadline: recoveryDeadline, beforeRead: {
        self.driver.onQueue { self.driver.subscriptionIdentifiers.last! }
      }) { done in
        self.sessions.continuationStatus { done(($0, $1)) }
      }
      lastRecoveryObservation = observation
      check(observation.1 == nil, "native recovery status failed: \(String(describing: observation.1))")
      let observed = json(observation.0 ?? "{}")
      let recovery = observed["lastRecovery"] as? [String: Any]
      recovered = completedRecovery(recovery, previousSubscription: previousSubscription, currentSubscription: candidateSubscription)
      if recovered { recoveredSubscription = candidateSubscription }
      check((observed["lastWake"] as? [String: Any])?["observedAtMs"] as? Int64 == lastWake["observedAtMs"] as? Int64,
            "recovery must not overwrite the original OS wake: before=\(lastWake), after=\(String(describing: observed["lastWake"]))")
      if !recovered { Thread.sleep(forTimeInterval: 0.01) }
    }
    check(recovered, "autonomous service-change recovery must be visible in public Apple status: \(lastRecoveryObservation); calls=\(driver.onQueue { self.driver.calls })")
    let subscription = recoveredSubscription!
    check(subscription != previousSubscription, "recovery must publish a replacement native subscription before notification injection")
    onRadioQueue {
      self.adapter.protocolRadioDidReceiveNotification(subscription, value: Data([0, 72]) as NSData)
    }
    // Observe actual intake, without draining or assuming a sleep is a barrier.
    // No renderer/application manager pumps this native session.
    let intakeDeadline = Date().addingTimeInterval(10)
    while true {
      let envelope: String = waitFor("native intake counters") { done in
        self.host.continuationDescribeBacklog(completion: HarnessInvokeCompletion(done))
      }
      let snapshot = json(envelope)["value"] as? [String: Any]
      let counters = snapshot?["counters"] as? [String: Any]
      if (counters?["retainedByteBuffers"] as? NSNumber)?.intValue == 1 { break }
      check(Date() < intakeDeadline, "native notification never reached the owned backlog: \(envelope)")
      Thread.sleep(forTimeInterval: 0.005)
    }
    let prepared: (String?, String?) = waitFor("native prepare") { done in
      self.sessions.prepareContinuationClaim(maxItems: 256, maxBytes: 65536) { done(($0, $1)) }
    }
    check(prepared.1 == nil, "native prepare failed: \(String(describing: prepared))")
    let claim = json(prepared.0 ?? "{}")
    check(claim["consumerCount"] as? Int == 2, "handoff preserves both pre-recovery and replacement selector identities")
    let batches = claim["batches"] as? [String] ?? []
    let values = batches.flatMap { json($0)["records"] as? [[String: Any]] ?? [] }
    check(values.contains { $0["t"] as? String == "value" && $0["valueB64"] as? String == "AEg=" }, "no native HR value in claim: \(claim)")
    guard let token = claim["claimToken"] as? String, !token.isEmpty else {
      return check(false, "native prepare missing acknowledgement token")
    }
    let replay: (String?, String?) = waitFor("native prepare replay") { done in
      self.sessions.prepareContinuationClaim(maxItems: 256, maxBytes: 65536) { done(($0, $1)) }
    }
    check(replay.0 == prepared.0, "unacknowledged native backlog is not replayable")
    let acknowledged: (String?, String?) = waitFor("native acknowledgement") { done in
      self.sessions.acknowledgeContinuationClaim(token) { done(($0, $1)) }
    }
    check(acknowledged.1 == nil && json(acknowledged.0 ?? "{}")["disposed"] as? Bool == true,
          "native handoff did not release: \(acknowledged)")
    UserDefaults.standard.removeObject(forKey: "com.sfourdrinier.unifiedblemanager.background-continuation")
    UserDefaults.standard.removeObject(forKey: "com.sfourdrinier.unifiedblemanager.background-continuation.last-wake")
  }

  func coldWarmAdmissionChecks() {
    let peer = "9828347E-45DF-2EEB-E928-6E443F4065E3"
    let declaration = jsonText(["onAppearance": "native", "peerId": peer,
      "resubscribe": [["serviceUuid": hrService, "serviceOccurrence": 1,
                       "characteristicUuid": hrMeasurement, "characteristicOccurrence": 1]]])
    let key = "com.sfourdrinier.unifiedblemanager.background-continuation"
    let wakeKey = "com.sfourdrinier.unifiedblemanager.background-continuation.last-wake"
    UserDefaults.standard.set("actual-wake", forKey: wakeKey)
    let before = driver.onQueue { self.driver.calls.count }
    for standingOrder in [nil, "{\"onAppearance\":\"record-only\"}", declaration.replacingOccurrences(of: peer, with: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA")] {
      if let standingOrder { UserDefaults.standard.set(standingOrder, forKey: key) }
      else { UserDefaults.standard.removeObject(forKey: key) }
      let result: String = waitFor("cold warm authority refusal") { done in
        self.sessions.executeNativeContinuation(peer, declarationJson: declaration, completion: done)
      }
      check(errorCode(json(result)) == "lifecycle.invalid-state", "cold warm call created its own declaration authority: \(result)")
      check(UserDefaults.standard.string(forKey: key) == standingOrder, "cold warm call persisted an order")
    }
    check(driver.onQueue { self.driver.calls.count } == before, "cold refusal reached native radio")
    UserDefaults.standard.set(declaration, forKey: key)
    onRadioQueue {
      self.driver.snapshot = ["availability": "available", "authorization": "granted", "power": "on", "safeReason": NSNull()]
      self.adapter.protocolRadioDidUpdateAdapterState(self.driver.snapshot)
    }
    let matching: String = waitFor("cold matching standing order") { done in
      self.sessions.executeNativeContinuation(peer, declarationJson: declaration, completion: done)
    }
    check(json(matching)["ok"] as? Bool == true, "cold matching order refused: \(matching)")
    let prepared: String = waitFor("cold matching prepare") { done in
      self.sessions.prepareNativeContinuationClaim(maxItems: 256, maxBytes: 65536, completion: done)
    }
    let token = (json(prepared)["value"] as! [String: Any])["claimToken"] as! String
    let ack: String = waitFor("cold matching acknowledgement") { done in
      self.sessions.acknowledgeNativeContinuationClaim(token, completion: done)
    }
    check((json(ack)["value"] as? [String: Any])?["disposed"] as? Bool == true, "cold matching owner leaked")
    check(UserDefaults.standard.string(forKey: wakeKey) == "actual-wake", "cold warm call fabricated wake")
    UserDefaults.standard.removeObject(forKey: key)
    UserDefaults.standard.removeObject(forKey: wakeKey)
  }

  func queuedWarmReplacementChecks() {
    let queue = DispatchQueue(label: "test.warm.suspended-recording")
    queue.suspend()
    let wrapper = UnifiedBleRustCoreSessions(installer: { _ in
      check(false, "stale queued warm order installed a host")
      return self.host
    }, recordingQueue: queue)
    let peer = "9828347E-45DF-2EEB-E928-6E443F4065E3"
    let order = jsonText(["onAppearance": "native", "peerId": peer,
      "recording": ["id": "stale-warm", "maxBytes": 1048576, "maxRecords": 1000]])
    let key = "com.sfourdrinier.unifiedblemanager.background-continuation"
    UserDefaults.standard.set(order, forKey: key)
    check(Thread.isMainThread, "recording deferral test must originate on main")
    let result: String = waitFor("queued warm declaration replacement") { done in
      wrapper.executeNativeContinuation(peer, declarationJson: order, completion: done)
      UserDefaults.standard.set("{\"onAppearance\":\"record-only\"}", forKey: key)
      queue.resume()
    }
    check(errorCode(json(result)) == "lifecycle.invalid-state", "queued warm order bypassed replacement: \(result)")
    check(UserDefaults.standard.string(forKey: key) == "{\"onAppearance\":\"record-only\"}", "queued warm call overwrote replacement")
    UserDefaults.standard.removeObject(forKey: key)
  }

  func warmContinuationChecks() {
    let peer = "9828347E-45DF-2EEB-E928-6E443F4065E3"
    let declaration = jsonText(["onAppearance": "native", "peerId": peer,
      "resubscribe": [["serviceUuid": hrService, "serviceOccurrence": 1,
                       "characteristicUuid": hrMeasurement, "characteristicOccurrence": 1]]])
    let wakeKey = "com.sfourdrinier.unifiedblemanager.background-continuation.last-wake"
    let declarationKey = "com.sfourdrinier.unifiedblemanager.background-continuation"
    let declared: (String?, String?) = waitFor("warm declaration") { done in
      self.sessions.declareBackgroundContinuation(declaration) { done(($0, $1)) }
    }
    check(declared.1 == nil, "warm declaration refused: \(String(describing: declared))")
    UserDefaults.standard.set("prior-real-wake", forKey: wakeKey)
    let reservation = (json(host.continuationReserveDeclaration(declarationJson: declaration))["value"] as! [String: Any])["reservationToken"] as! String
    let expectedRefusal = host.continuationSeedDeclaration(declarationJson: declaration)
    let refused: String = waitFor("warm seed refusal") { done in
      self.sessions.executeNativeContinuation(peer, declarationJson: declaration, completion: done)
    }
    check(refused == expectedRefusal, "warm seed changed canonical native refusal: \(refused)")
    check(json(host.continuationCancelDeclaration(reservationToken: reservation))["ok"] as? Bool == true, "warm reservation cancellation failed")
    let conflicting = declaration.replacingOccurrences(of: peer, with: "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA")
    let conflict: String = waitFor("warm authority conflict") { done in
      self.sessions.executeNativeContinuation("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA", declarationJson: conflicting, completion: done)
    }
    check(errorCode(json(conflict)) == "lifecycle.invalid-state",
          "warm execution replaced committed declaration authority: \(conflict)")
    let malformed: String = waitFor("warm malformed declaration") { done in
      self.sessions.executeNativeContinuation(peer, declarationJson: "{\"unknown\":true}", completion: done)
    }
    check(errorCode(json(malformed)) == "argument.invalid", "warm declaration skipped validation: \(malformed)")
    let executed: String = waitFor("warm execution") { done in
      self.sessions.executeNativeContinuation(peer, declarationJson: declaration, completion: done)
    }
    check(json(executed)["ok"] as? Bool == true, "warm execution failed: \(executed)")
    check((json(executed)["value"] as? [String: Any])?["event"] as? String == "continuation.completed", "warm execution lost native outcome")
    let described: String = waitFor("warm raw backlog") { done in
      self.sessions.describeNativeContinuation(completion: done)
    }
    check(json(described)["ok"] as? Bool == true && (json(described)["value"] as? [String: Any])?["counters"] != nil,
          "warm status replaced native backlog with posture summary: \(described)")
    let invalidBounds: String = waitFor("warm invalid bounds") { done in
      self.sessions.prepareNativeContinuationClaim(maxItems: .nan, maxBytes: 100, completion: done)
    }
    check(errorCode(json(invalidBounds)) == "argument.invalid", "warm invalid claim bounds accepted")
    let prepared: String = waitFor("warm raw prepare") { done in
      self.sessions.prepareNativeContinuationClaim(maxItems: 256, maxBytes: 65536, completion: done)
    }
    let claim = json(prepared)["value"] as? [String: Any] ?? [:]
    guard let token = claim["claimToken"] as? String else { return check(false, "warm claim lost token: \(prepared)") }
    let replay: String = waitFor("warm raw prepare replay") { done in
      self.host.continuationPrepareClaim(maxItems: 256, maxBytes: 65536, completion: HarnessInvokeCompletion(done))
    }
    check(prepared == replay, "warm prepare changed canonical replay envelope")
    let acknowledged: String = waitFor("warm raw acknowledgement") { done in
      self.sessions.acknowledgeNativeContinuationClaim(token, completion: done)
    }
    check((json(acknowledged)["value"] as? [String: Any])?["disposed"] as? Bool == true, "warm acknowledgement lost disposal: \(acknowledged)")
    check(UserDefaults.standard.string(forKey: wakeKey) == "prior-real-wake", "warm controls fabricated an OS wake")
    check(UserDefaults.standard.string(forKey: declarationKey) == declaration, "warm controls rewrote declaration")
    UserDefaults.standard.removeObject(forKey: wakeKey)
    UserDefaults.standard.removeObject(forKey: declarationKey)
    print("[AppleRustRadioAdapterHarness] warm canonical controls preserve seed refusal, claim replay, disposal and actual wake history.")
  }

  func lifecycleCleanupChecks() {
    let gate = DisposalGate()
    let lifecycle = UnifiedBleRustCoreSessions(
      installer: { _ in self.host },
      disposeInvoker: gate.invoke,
      retryDelay: .milliseconds(20)
    )
    let ownerA = NSObject()
    let ownerB = NSObject()
    func open(_ owner: NSObject) -> String {
      let result: (String?, String?) = waitFor("lifecycle open") { done in
        lifecycle.openSession("lifecycle", expectedWireRevision: mobileWireRevision(), ownerToken: owner, onWake: { _ in }) {
          done(($0, $1))
        }
      }
      check(result.1 == nil, "lifecycle open failed: \(String(describing: result.1))")
      return String((json(result.0!)["sessionId"] as! NSNumber).uint64Value)
    }
    func waitUntilClosed(_ wrapper: UnifiedBleRustCoreSessions, _ id: String) {
      let deadline = Date().addingTimeInterval(10)
      while Date() < deadline {
        let result: (String?, String?) = waitFor("closed session probe") { done in
          wrapper.invoke(sessionId: id, op: "adapter.state", argsJson: "{}") { done(($0, $1)) }
        }
        if json(result.1 ?? "{}")["code"] as? String == "lifecycle.destroyed" { return }
        Thread.sleep(forTimeInterval: 0.01)
      }
      check(false, "session \(id) remained addressable after retry")
    }
    let first = open(ownerA)
    let unaffected = open(ownerB)
    gate.failOnce(first)
    lifecycle.closeSessions(ownedBy: ownerA)
    check(gate.waitForAttempts(first, 2), "failed module cleanup was not retried without another module event")
    waitUntilClosed(lifecycle, first)
    let other: (String?, String?) = waitFor("unrelated owner") { done in
      lifecycle.invoke(sessionId: unaffected, op: "adapter.state", argsJson: "{}") { done(($0, $1)) }
    }
    check(json(other.0 ?? "{}")["ok"] as? Bool == true, "another module's session was disposed")
    let otherClose: String? = waitFor("unrelated close") { done in lifecycle.closeSession(unaffected) { done($0) } }
    check(otherClose == nil, "unrelated close failed")

    let ownerC = NSObject()
    let racing = open(ownerC)
    gate.holdOnce(racing)
    let explicitDone = DispatchSemaphore(value: 0)
    lifecycle.closeSession(racing) { _ in explicitDone.signal() }
    lifecycle.closeSessions(ownedBy: ownerC)
    check(gate.attempts(racing) == 1, "explicit close and invalidation overlapped native disposal")
    gate.releaseHeldFailure(racing)
    check(explicitDone.wait(timeout: .now() + timeout) == .success, "explicit close did not settle")
    check(gate.waitForAttempts(racing, 2), "racing failed disposal was not retried")
    waitUntilClosed(lifecycle, racing)
    gate.repeatHeldFailure(racing)
    Thread.sleep(forTimeInterval: 0.05)
    check(gate.attempts(racing) == 2, "stale disposal callback scheduled another retry")

    let admissionEntered = DispatchSemaphore(value: 0)
    let allowAdmission = DispatchSemaphore(value: 0)
    let admittedIdLock = NSLock()
    var admittedId: String?
    let admissionOwner = NSObject()
    let admission = UnifiedBleRustCoreSessions(
      installer: { _ in self.host },
      openInvoker: { host, name, revision in
        admissionEntered.signal()
        check(allowAdmission.wait(timeout: .now() + timeout) == .success, "admission gate timed out")
        let session = try host.openSession(owner: name, expectedWireRevision: revision)
        admittedIdLock.lock()
        admittedId = String(session.sessionId())
        admittedIdLock.unlock()
        return session
      },
      disposeInvoker: gate.invoke,
      retryDelay: .milliseconds(20)
    )
    let openDone = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
      admission.openSession("late", expectedWireRevision: mobileWireRevision(), ownerToken: admissionOwner, onWake: { _ in }) { value, failure in
        check(value == nil && failure != nil, "late admission was delivered after module invalidation")
        openDone.signal()
      }
    }
    check(admissionEntered.wait(timeout: .now() + timeout) == .success, "open did not reach native admission")
    admission.closeSessions(ownedBy: admissionOwner)
    allowAdmission.signal()
    check(openDone.wait(timeout: .now() + timeout) == .success, "late admission did not settle")
    admittedIdLock.lock()
    let lateId = admittedId
    admittedIdLock.unlock()
    check(lateId != nil, "native admission did not create a session")
    check(gate.waitForAttempts(lateId!, 1), "late native session was not handed to process cleanup")
    waitUntilClosed(admission, lateId!)
  }

  func translationChecks() {
    let snapshot = UnifiedBleRustRadioAdapter.adapterSnapshot(
      ["availability": "available", "authorization": "notDetermined", "power": "unknown", "safeReason": "not yet"]
    )
    check(snapshot.authorization == "not-determined", "authorization vocabulary: \(snapshot)")
    func readinessKind(_ authorization: String, _ power: String, _ availability: String = "available") -> String? {
      let adapter = MobileAdapterSnapshot(availability: availability, authorization: authorization, power: power, safeReason: nil)
      guard case let .failed(kind, _, _, _, _, dispatched)? = UnifiedBleRustRadioAdapter.readinessFailure(adapter) else { return nil }
      check(!dispatched, "a readiness refusal never reaches CoreBluetooth")
      return kind
    }
    check(readinessKind("not-determined", "unknown") == "permission-not-determined", "not-determined")
    check(readinessKind("restricted", "on") == "permission-restricted", "restricted")
    check(readinessKind("denied", "on") == "permission-denied", "denied")
    check(readinessKind("granted", "resetting") == "adapter-resetting", "resetting")
    check(readinessKind("granted", "unknown") == "adapter-unavailable", "state unknown")
    check(readinessKind("granted", "unsupported", "unsupported") == "adapter-unavailable", "unsupported")
    check(readinessKind("granted", "off") == "adapter-off", "off")
    check(readinessKind("granted", "on") == nil, "ready")
    let attError = NSError(domain: CBATTErrorDomain, code: 5)
    guard case let .failed(kind, status, nativeDomain, nativeCode, _, dispatched) = UnifiedBleRustRadioAdapter.failure(attError, verb: .read),
          kind == "gatt-status", status == 5, dispatched else {
      return check(false, "ATT error must travel as gatt-status with its code")
    }
    check(nativeDomain == CBATTErrorDomain && nativeCode == 5, "the NSError identity travels (113): \(String(describing: nativeDomain)) \(String(describing: nativeCode))")
    let owned = NSError(domain: UnifiedBleRustRadioAdapter.ownedDomain, code: 1010)
    guard case let .failed(_, _, ownedDomain, ownedCode, _, _) = UnifiedBleRustRadioAdapter.failure(owned, verb: .read),
          ownedDomain == UnifiedBleRustRadioAdapter.ownedDomain, ownedCode == 1010 else {
      return check(false, "the owned radio's code travels with its domain")
    }
    check(UnifiedBleRustRadioAdapter.ownedKind(1005, verb: .connect) == "peer-unknown", "1005")
    check(UnifiedBleRustRadioAdapter.ownedKind(1026, verb: .discover) == "path-stale", "1026 discover")
    check(UnifiedBleRustRadioAdapter.ownedKind(1026, verb: .readDescriptor) == "busy", "1026 descriptor")
    check(UnifiedBleRustRadioAdapter.coreBluetoothKind(7) == "not-connected", "CBError.peripheralDisconnected")
    check(UnifiedBleRustRadioAdapter.rssi(127) == nil && UnifiedBleRustRadioAdapter.rssi(-40) == -40, "rssi 127")
  }

  /// Finding 140 (I-1): every Android-only verb submitted straight to a
  /// real Swift adapter is refused `unsupported`, never sent, and never
  /// reaches CoreBluetooth.
  func androidOnlyRefusalsAtTheAdapter() {
    final class RecordingSink: UnifiedBleRustRadioSink {
      let lock = NSLock()
      let answered = DispatchSemaphore(value: 0)
      var completions = [UInt64: MobileRadioCompletion]()
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock()
        completions[requestId] = completion
        lock.unlock()
        answered.signal()
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let isolatedDriver = ScriptedDriver()
    let isolated = UnifiedBleRustRadioAdapter(driver: isolatedDriver)
    let sink = RecordingSink()
    isolated.bind(sink: sink)
    let requests: [MobileRadioRequest] = [
      .acquireBackground(id: 901, kind: "connected-device", reason: "workout"),
      .releaseBackground(id: 902, leaseId: "lease"),
      .updateBackgroundNotification(id: 903, leaseId: "lease", title: "Recording", body: nil),
      .associateCompanion(id: 904, name: "Polar", serviceUuid: nil),
    ]
    for request in requests { isolated.submit(request: request) }
    for _ in requests {
      check(sink.answered.wait(timeout: .now() + timeout) == .success, "an Android-only verb was never answered")
    }
    for id in UInt64(901)...UInt64(904) {
      sink.lock.lock()
      let completion = sink.completions[id]
      sink.lock.unlock()
      guard case let .failed(kind, _, _, _, detail, dispatched)? = completion else {
        return check(false, "request \(id) was not refused: \(String(describing: completion))")
      }
      check(kind == "unsupported" && !dispatched && detail.contains("Android-only"), "request \(id): \(kind) \(detail)")
    }
    check(isolatedDriver.onQueue { isolatedDriver.calls.isEmpty }, "no Android-only verb reached CoreBluetooth")
  }

  func restorationIdentityChecks() {
    let derived = UnifiedBleRustRestorationIdentity.derive(
      applicationId: "com.example.ubm", restorationId: "polar-h10", generation: "1"
    )
    // Vector computed independently (Node crypto) from the legacy algorithm.
    check(derived["restoreIdentifier"] == "com.example.ubm.ubm.g724M4r3EkMsbopT-viWE7", "restoreIdentifier: \(derived)")
    check(derived["namespaceValue"] == "ubm-ns:GhfflGiIDVkYzZxNY1FE5zyLLR_oAW0tlW7NLtgLSBg", "namespaceValue")
    check(derived["clientId"] == "ubm-client:um_4FoT8oRK4NNEcoTjcJDaZR0dzOszsUbelmCh3GoE", "clientId")
    check(derived["hostSessionScope"] == "ubm-host:5p410UYhu5fl7V5eMAPRdm7nt2IJ5ARR88O0X9BpeAY", "hostSessionScope")
    check(!UnifiedBleRustRestorationIdentity.validToken("-bad", maximumBytes: 128), "token must start alphanumeric")
    check(!UnifiedBleRustRestorationIdentity.validToken(String(repeating: "a", count: 65), maximumBytes: 64), "token byte bound")

    let directory = FileManager.default.temporaryDirectory.appendingPathComponent("ubm-restoration-\(UUID().uuidString).bundle")
    let contents = directory.appendingPathComponent("Contents")
    try! FileManager.default.createDirectory(at: contents, withIntermediateDirectories: true)
    let plist: [String: Any] = [
      "CFBundleIdentifier": "com.example.ubm",
      "UnifiedBleProtocolRestorationId": "polar-h10",
      "UnifiedBleProtocolRestorationGeneration": "1",
    ]
    try! PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
      .write(to: contents.appendingPathComponent("Info.plist"))
    defer { try? FileManager.default.removeItem(at: directory) }
    let bundle = Bundle(path: directory.path)!
    check(UnifiedBleRustRestorationIdentity.configuredRestoreIdentifier(bundle: bundle) == derived["restoreIdentifier"],
          "configured restore identifier")
    // Finding 140 (I-2): the production host acquires the process central
    // with exactly the bundle's restore identifier and power-alert choice,
    // as the legacy module did. (Creating the real CBCentralManager needs a
    // Bluetooth-entitled app; the owner and radio are byte-identical to 4.x.)
    let production = UnifiedBleRustCoreSessions.productionRadioConfiguration(bundle: bundle)
    check(production.restoreIdentifierKey == derived["restoreIdentifier"], "production restore identifier")
    check(production.showPowerAlert == nil, "no power-alert key → the central's default")
    switch UnifiedBleRustRestorationIdentity.bootstrap(requestJson: "{\"restorationId\":\"polar-h10\",\"generation\":\"1\"}", bundle: bundle) {
    case let .success(text):
      let identity = json(text)
      check(Set(identity.keys) == [
        "applicationId", "restorationId", "generation", "restoreIdentifier", "namespaceValue", "clientId", "hostSessionScope",
      ], "identity keys: \(identity.keys)")
      check(identity["restoreIdentifier"] as? String == derived["restoreIdentifier"], "bootstrap identity")
    case let .failure(failure):
      check(false, "bootstrap refused: \(failure)")
    }
    guard case let .failure(mismatch) = UnifiedBleRustRestorationIdentity.bootstrap(
      requestJson: "{\"restorationId\":\"other\",\"generation\":\"1\"}", bundle: bundle
    ) else {
      return check(false, "a foreign restoration id was accepted")
    }
    check(json(mismatch.json)["code"] as? String == "platform.failure", "mismatch failure: \(mismatch.json)")

    // PR210-72: an Info.plist-only app gets its configured identity from `{}`;
    // an unconfigured bundle answers `null`.
    switch UnifiedBleRustRestorationIdentity.bootstrap(requestJson: "{}", bundle: bundle) {
    case let .success(text):
      check(json(text)["clientId"] as? String == derived["clientId"], "configured identity: \(text)")
    case let .failure(failure):
      check(false, "configured identity refused: \(failure)")
    }
    let bare = FileManager.default.temporaryDirectory.appendingPathComponent("ubm-bare-\(UUID().uuidString).bundle")
    try! FileManager.default.createDirectory(at: bare.appendingPathComponent("Contents"), withIntermediateDirectories: true)
    try! PropertyListSerialization.data(fromPropertyList: ["CFBundleIdentifier": "com.example.bare"], format: .xml, options: 0)
      .write(to: bare.appendingPathComponent("Contents/Info.plist"))
    defer { try? FileManager.default.removeItem(at: bare) }
    let alert = FileManager.default.temporaryDirectory.appendingPathComponent("ubm-alert-\(UUID().uuidString).bundle")
    try! FileManager.default.createDirectory(at: alert.appendingPathComponent("Contents"), withIntermediateDirectories: true)
    try! PropertyListSerialization.data(
      fromPropertyList: ["CFBundleIdentifier": "com.example.alert", "UnifiedBleProtocolShowPowerAlert": false],
      format: .xml, options: 0
    ).write(to: alert.appendingPathComponent("Contents/Info.plist"))
    defer { try? FileManager.default.removeItem(at: alert) }
    let alertConfiguration = UnifiedBleRustCoreSessions.productionRadioConfiguration(bundle: Bundle(path: alert.path)!)
    check(alertConfiguration.restoreIdentifierKey == nil, "no restoration configured → no restore identifier")
    check(alertConfiguration.showPowerAlert == false, "the Info.plist power-alert choice reaches the central")
    guard case .success("null") = UnifiedBleRustRestorationIdentity.bootstrap(
      requestJson: "{}", bundle: Bundle(path: bare.path)!
    ) else {
      return check(false, "an unconfigured bundle must answer null")
    }
  }

  func randomBytesChecks() {
    let sessions = UnifiedBleRustCoreSessions(installer: { _ in throw MobileCoreError.Failed(code: "x", domain: "y", operation: "z", detail: nil) })
    var produced: (String?, String?) = (nil, nil)
    sessions.randomBytes(32) { produced = ($0, $1) }
    check(produced.1 == nil && Data(base64Encoded: produced.0 ?? "")?.count == 32, "randomBytes(32): \(produced)")
    sessions.randomBytes(1025) { produced = ($0, $1) }
    check(json(produced.1 ?? "{}")["code"] as? String == "argument.invalid", "randomBytes(1025) must be refused")
    sessions.randomBytes(1.5) { produced = ($0, $1) }
    check(produced.1 != nil, "randomBytes(1.5) must be refused")
    check(sessions.ensureHost().map { json($0)["code"] as? String } == "x", "install failure must be reported as data")
  }
}

@main
enum AppleRustRadioAdapterHarness {
  static func main() {
    Harness().run()
    print("[AppleRustRadioAdapterHarness] Rust host ↔ Swift adapter scripted exchange passed (deterministic; no physical radio).")
  }
}
