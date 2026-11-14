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

/// UniFFI deliberately generates subclassable `noHandle` objects for native
/// harnesses.  These fixtures therefore exercise the real Sessions -> adapter
/// -> authority boundary while making the otherwise non-deterministic cleanup
/// and capacity outcomes controllable.
private final class LifecycleRecoveryOwner: MobileScanFailureRecoveryOwner, @unchecked Sendable {
  private let identifier: String

  init(identifier: String) {
    self.identifier = identifier
    super.init(noHandle: .init())
  }

  required init(unsafeFromHandle handle: UInt64) {
    identifier = "unexpected-ffi-owner"
    super.init(unsafeFromHandle: handle)
  }

  override func info() throws -> MobileScanFailureRecoveryOwnerInfo {
    MobileScanFailureRecoveryOwnerInfo(ownerId: identifier, prepared: false,
      producerFinished: true, rawRetired: false, rawReleased: false,
      memberCount: 0, released: false)
  }

  override func rawRetire() throws {}
}

private final class LifecycleCleanupReceipt: MobileScanFailureCleanupReceipt, @unchecked Sendable {
  private let lock = NSLock()
  private(set) var retireCalls = 0

  override init(noHandle: NoHandle) { super.init(noHandle: noHandle) }

  required init(unsafeFromHandle handle: UInt64) { super.init(unsafeFromHandle: handle) }

  convenience init() { self.init(noHandle: .init()) }

  override func retire() throws {
    lock.lock()
    retireCalls += 1
    lock.unlock()
  }
}

private final class LifecycleRecovery: MobileScanFailureRecovery, @unchecked Sendable {
  private let lock = NSLock()
  private var currentOwners: [MobileScanFailureRecoveryOwner]
  private let cleanup: [MobileScanFailureCleanupReceipt]

  init(owners: [MobileScanFailureRecoveryOwner], cleanup: [MobileScanFailureCleanupReceipt]) {
    currentOwners = owners
    self.cleanup = cleanup
    super.init(noHandle: .init())
  }

  required init(unsafeFromHandle handle: UInt64) {
    currentOwners = []
    cleanup = []
    super.init(unsafeFromHandle: handle)
  }

  func replaceOwners(_ owners: [MobileScanFailureRecoveryOwner]) {
    lock.lock()
    currentOwners = owners
    lock.unlock()
  }

  override func owners() -> [MobileScanFailureRecoveryOwner] {
    lock.lock()
    defer { lock.unlock() }
    return currentOwners
  }

  override func pendingCleanupReceipts() throws -> [MobileScanFailureCleanupReceipt] { cleanup }
}

private final class LifecycleRecoveryAuthority: MobileScanFailureRecoveryConfigurationAuthority, @unchecked Sendable {
  private let lock = NSLock()
  private let recovery: MobileScanFailureRecovery
  private(set) var attachedRecordingIds = [String]()

  init(recovery: MobileScanFailureRecovery) {
    self.recovery = recovery
    super.init(noHandle: .init())
  }

  required init(unsafeFromHandle handle: UInt64) {
    recovery = LifecycleRecovery(owners: [], cleanup: [])
    super.init(unsafeFromHandle: handle)
  }

  override func attachRecordingV2(recordingId: String) throws -> MobileScanFailureRecovery {
    lock.lock()
    attachedRecordingIds.append(recordingId)
    lock.unlock()
    return recovery
  }

  func attachedIds() -> [String] {
    lock.lock()
    defer { lock.unlock() }
    return attachedRecordingIds
  }
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
  var orderedTestGate: OwnedOrderedGraphPermitGate?
  func orderedGraphGate(peer: String) -> OwnedOrderedGraphPermitGate? { orderedTestGate }
  var snapshot: NSDictionary = ["availability": "available", "authorization": "granted", "power": "on", "safeReason": NSNull()]
  var restored: [NSDictionary] = []
  var restoredSnapshotRequests = 0
  var holdRestoredSnapshots = false
  var restoredCompletion: (([NSDictionary]) -> Void)?
  var canSendWithoutResponse = true
  var readProvenance = OwnedCoreBluetoothReadProvenance.readResponse
  var calls = [String]()
  var cancelled = [String]()
  var holdCancellation = false
  var cancellationCompletions = [String: (NSDictionary) -> Void]()
  var subscriptionIdentifiers = [String]()
  var holdSubscribes = false
  var subscribeCompletions = [String: (NSError?) -> Void]()
  var unsubscribeFailures = [NSError]()
  var hangingConnects = [String: (NSError?) -> Void]()
  var holdConnects = false
  var connectCompletions = [String: (NSError?) -> Void]()
  var holdSetupWrites = false
  var setupWrites = [(NSError?) -> Void]()
  var holdPreparation = false
  var preparations = [String: (NSDictionary?, NSError?) -> Void]()
  var holdReads = false
  var readCompletions = [String: (NSData?, OwnedCoreBluetoothReadProvenance, NSError?) -> Void]()
  let preparationAdmitted = DispatchSemaphore(value: 0)

  func connectIdentifier(_ request: Int) -> String {
    connectCompletions.keys.first { $0.hasSuffix("-\(request)") } ?? "missing-connect-\(request)"
  }
  func subscriptionIdentifier(_ request: Int) -> String {
    subscriptionIdentifiers.first { $0.hasSuffix("-\(request)") } ?? "missing-subscription-\(request)"
  }
  func preparationIdentifier(_ request: Int) -> String {
    preparations.keys.first { $0.hasSuffix("-\(request)") } ?? "missing-preparation-\(request)"
  }

  private func record(_ call: String) { calls.append(call) }

  func adapterSnapshot(completion: @escaping (NSDictionary) -> Void) {
    workQueue.async { completion(self.snapshot) }
  }

  func prepareForOperation(operationIdentifier: String, completion: @escaping (NSDictionary?, NSError?) -> Void) {
    workQueue.async {
      if self.holdPreparation {
        self.preparations[operationIdentifier] = completion
        self.preparationAdmitted.signal()
      } else { completion(self.snapshot, nil) }
    }
  }

  var connectedInventory = [MobilePeerName(peerId: "FOREIGN", name: "other app link")]
  var connectedQueries = [[String]]()
  func resolveDirectoryPeer(peerIdentifier: String, completion: @escaping (MobilePeerName?, NSError?) -> Void) {
    workQueue.async { completion(self.connectedInventory.first { $0.peerId == peerIdentifier }, nil) }
  }
  func connectedPeerSnapshots(services: [String], completion: @escaping ([MobilePeerName]?, NSError?) -> Void) {
    workQueue.async { self.connectedQueries.append(services); completion(self.connectedInventory, nil) }
  }

  func restoredPeerSnapshots(completion: @escaping ([NSDictionary]) -> Void) {
    restoredSnapshotRequests += 1
    if holdRestoredSnapshots { restoredCompletion = completion }
    else { workQueue.async { completion(self.restored) } }
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
      if self.holdConnects {
        self.connectCompletions[operationIdentifier] = completion
      } else if peerIdentifier == "HANG" {
        self.hangingConnects[operationIdentifier] = completion
        return
      } else { completion(nil) }
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
        "nativeSnapshotOwner": "scripted-owner",
        "nativeSnapshotAttachment": NSNumber(value: 1),
        "nativeSnapshotRevision": NSNumber(value: 1),
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
      if self.holdReads { self.readCompletions[operationIdentifier] = completion; return }
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
      if self.holdSubscribes { self.subscribeCompletions[subscriptionIdentifier] = completion; return }
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
      if self.holdCancellation { self.cancellationCompletions[operationIdentifier] = completion; return }
      completion(["state": "released", "failures": []])
    }
  }

  func onQueue<T>(_ body: @escaping () -> T) -> T {
    workQueue.sync(execute: body)
  }
}

private func jsonNullable<T>(_ value: T?) -> Any {
  if let value { return value }
  return NSNull()
}

final class Harness {
  /// Mirrors the production binding's ownership sequence without opening a
  /// physical CoreBluetooth central. The test must exercise the real UniFFI
  /// host boundary: a raw host handle cannot configure or later claim the
  /// adapter-owned spool authority.
  private final class AdapterHostBinding: MobileHostBinding {
    let adapter: UnifiedBleRustRadioAdapter
    init(_ adapter: UnifiedBleRustRadioAdapter) { self.adapter = adapter }
    func bind(host: MobileCoreHost) -> Bool {
      guard let authority = host.claimNotificationFailureSpoolConfigurationAuthorityV3(),
            let scanRecoveryAuthority = host.claimScanFailureRecoveryConfigurationAuthorityV1(),
            adapter.installNotificationFailureSpoolConfigurationAuthority(authority),
            adapter.installScanFailureRecoveryConfigurationAuthority(scanRecoveryAuthority),
            adapter.bind(sink: host) else { return false }
      adapter.installNotificationFailureSpool(host)
      return true
    }
  }

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

  func generationScopedConnectionChecks() {
    final class Sink: UnifiedBleRustAcknowledgedFailureSink {
      var completions = [UInt64: MobileRadioCompletion]()
      var ingresses = [MobileRadioIngress]()

      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String {
        ingest(ingress: .notificationSourceFailed(version: 1, instance: instance, epoch: epoch, failure: failure))
      }

      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        completions[requestId] = completion
        return "delivered"
      }

      func ingest(ingress: MobileRadioIngress) -> String {
        ingresses.append(ingress)
        return "accepted"
      }
    }

    let sink = Sink()
    let generationDriver = ScriptedDriver()
    let scopedAdapter = UnifiedBleRustRadioAdapter(driver: generationDriver)
    scopedAdapter.bind(sink: sink)
    scopedAdapter.submit(request: .connect(id: 99, peerId: "EMPTY", autoConnect: false, preferredPhy: [], expectedGeneration: ""))
    let emptyDeadline = Date().addingTimeInterval(10)
    while Date() < emptyDeadline && sink.completions[99] == nil { Thread.sleep(forTimeInterval: 0.01) }
    guard case let .failed(kind, _, _, _, _, dispatched)? = sink.completions[99] else {
      return check(false, "empty generation must be refused structurally")
    }
    check(kind == "platform" && !dispatched, "empty generation refusal must precede CoreBluetooth")
    generationDriver.holdConnects = true
    func waitForConnect(_ peer: String) {
      let deadline = Date().addingTimeInterval(10)
      while Date() < deadline {
        if generationDriver.onQueue({ generationDriver.calls.contains("connect \(peer)") }) { return }
        Thread.sleep(forTimeInterval: 0.01)
      }
      check(false, "connect dispatch for (peer) was not observed")
    }

    scopedAdapter.submit(request: .connect(id: 1, peerId: "A", autoConnect: false, preferredPhy: [], expectedGeneration: "gen-A"))
    waitForConnect("A")
    generationDriver.onQueue { generationDriver.connectCompletions.removeValue(forKey: generationDriver.connectIdentifier(1))?(nil) }
    scopedAdapter.submit(request: .connect(id: 2, peerId: "B", autoConnect: false, preferredPhy: [], expectedGeneration: "gen-B"))
    waitForConnect("B")
    generationDriver.onQueue { scopedAdapter.protocolRadioDidDisconnectPeer("A", error: nil) }
    check(sink.ingresses.contains {
      if case let .connectionScoped(peerId, expectedGeneration, connected, _) = $0 {
        return peerId == "A" && expectedGeneration == "gen-A" && !connected
      }
      return false
    }, "A loss must retain A's established generation")
    generationDriver.onQueue { generationDriver.connectCompletions.removeValue(forKey: generationDriver.connectIdentifier(2))?(nil) }
    generationDriver.onQueue { scopedAdapter.protocolRadioDidDisconnectPeer("B", error: nil) }
    check(sink.ingresses.contains {
      if case let .connectionScoped(peerId, expectedGeneration, connected, _) = $0 {
        return peerId == "B" && expectedGeneration == "gen-B" && !connected
      }
      return false
    }, "B loss must use B's generation after replacement admission")

    scopedAdapter.submit(request: .connect(id: 3, peerId: "C", autoConnect: false, preferredPhy: [], expectedGeneration: "gen-C"))
    waitForConnect("C")
    scopedAdapter.cancel(requestId: 3)
    generationDriver.onQueue { generationDriver.connectCompletions.removeValue(forKey: generationDriver.connectIdentifier(3))?(nil) }
    generationDriver.onQueue { scopedAdapter.protocolRadioDidDisconnectPeer("C", error: nil) }
    let counters = waitFor("adapter counters") { done in scopedAdapter.adapterCounters(completion: done) }
    check(counters.unknownConnectionLosses == 1, "cancelled late connect must not leave a generation owner")
    let instance = MobileInstance(peerId: "P", serviceUuid: "180D", serviceOccurrence: 0,
                                  characteristicUuid: "2A37", characteristicOccurrence: 0)
    func waitForCompletion(_ id: UInt64) {
      let deadline = Date().addingTimeInterval(10)
      while Date() < deadline {
        if generationDriver.onQueue({ sink.completions[id] != nil }) { return }
        Thread.sleep(forTimeInterval: 0.01)
      }
      check(false, "notification request did not complete")
    }
    scopedAdapter.submit(request: .enableNotifications(id: 10, instance: instance, epoch: 1, requested: nil, preferred: nil))
    waitForCompletion(10)
    scopedAdapter.submit(request: .enableNotifications(id: 11, instance: instance, epoch: 2, requested: nil, preferred: nil))
    waitForCompletion(11)
    generationDriver.onQueue {
      scopedAdapter.protocolRadioDidFailNotification(generationDriver.subscriptionIdentifier(10), error: NSError(domain: "old-source", code: 19))
    }
    check(generationDriver.onQueue { sink.ingresses.contains {
      if case let .notificationSourceFailed(version, observed, epoch, failure) = $0 {
        return version == 1 && observed == instance && epoch == 1 && failure.nativeDomain == "old-source" && failure.nativeCode == 19
      }
      return false
    } }, "source failure must carry the callback owner's exact instance and epoch")
    scopedAdapter.submit(request: .disableNotifications(id: 12, instance: instance, epoch: 1))
    waitForCompletion(12)
    generationDriver.onQueue { scopedAdapter.protocolRadioDidFailNotification(generationDriver.subscriptionIdentifier(10), error: NSError(domain: "late", code: 20)) }
    check(generationDriver.onQueue { sink.ingresses.contains {
      if case let .dropped(ingressClass, detail) = $0 { return ingressClass == "control" && detail.contains(generationDriver.subscriptionIdentifier(10)) }
      return false
    } }, "retired callback must be reported without targeting the replacement")
    scopedAdapter.submit(request: .disableNotifications(id: 13, instance: instance, epoch: 2))
    waitForCompletion(13)
    generationDriver.onQueue { generationDriver.holdSubscribes = true }
    scopedAdapter.submit(request: .enableNotifications(id: 14, instance: instance, epoch: 3, requested: nil, preferred: nil))
    let subscribeDeadline = Date().addingTimeInterval(10)
    while Date() < subscribeDeadline && !generationDriver.onQueue({ generationDriver.subscribeCompletions[generationDriver.subscriptionIdentifier(14)] != nil }) {
      Thread.sleep(forTimeInterval: 0.01)
    }
    scopedAdapter.cancel(requestId: 14)
    waitForCompletion(14)
    generationDriver.onQueue {
      scopedAdapter.protocolRadioDidReceiveNotification(generationDriver.subscriptionIdentifier(14), value: Data([1]) as NSData)
      generationDriver.subscribeCompletions.removeValue(forKey: generationDriver.subscriptionIdentifier(14))?(nil)
    }
    scopedAdapter.submit(request: .disableNotifications(id: 15, instance: instance, epoch: 3))
    waitForCompletion(15)
    guard case .failed? = generationDriver.onQueue({ sink.completions[15] }) else {
      return check(false, "late cancelled enable must not create a subscription owner")
    }
    let notificationCalls = generationDriver.onQueue { generationDriver.calls.filter { $0.hasPrefix("unsubscribe ") } }
    check(notificationCalls == ["unsubscribe " + generationDriver.subscriptionIdentifier(10), "unsubscribe " + generationDriver.subscriptionIdentifier(11)],
          "old cleanup must address its own claim before the replacement")
    generationDriver.onQueue {
      generationDriver.holdConnects = false
      generationDriver.connectCompletions.removeAll()
      generationDriver.calls.removeAll()
    }
  }

  func orderedCaptureRetirementCompletionChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      private let lock = NSLock()
      private var values: [UInt64: [MobileRadioCompletion]] = [:]
      let completed = DispatchSemaphore(value: 0)
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock(); values[requestId, default: []].append(completion); lock.unlock()
        completed.signal()
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func get(_ id: UInt64) -> [MobileRadioCompletion] {
        lock.lock(); defer { lock.unlock() }; return values[id] ?? []
      }
    }

    for reportFailure in [false, true] {
      let driver = ScriptedDriver()
      let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      let source = OwnedOrderedNotificationSource(queue: driver.workQueue, generation: "async-retire")
      let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
        let ingress = try OwnedOrderedNotificationIngress(queue: driver.workQueue, generation: source.generation,
          members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
        precondition(source.add(ingress))
        return (source, ingress)
      }, wake: { _, _ in })
      let adapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
      let sink = Sink()
      check(adapter.bind(sink: sink), "bind retirement regression sink")
      let setup = driver.onQueue { () -> (UnifiedBleRustOrderedGroupBridge.RegisteredMember, AppleOrderedGroupCaptureOwner.Capture) in
        let member = try! bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
        let capture = try! bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 42))
        return (member, capture)
      }
      let registration = MobileOrderedRegistration(memberId: setup.0.id, sourceMember: setup.0.sourceMember, epoch: setup.0.epoch)
      adapter.submit(request: .orderedRetire(id: 42, generation: .opaque(value: source.generation), groupId: 1, member: registration))
      driver.onQueue {}
      check(sink.get(42).isEmpty, "retirement must remain pending until the earlier capture settles")
      driver.onQueue {
        if reportFailure {
          try! bridge.replayCapturedFailure(groupID: 1, capture: setup.1)
        } else if case let .captured(ticket) = setup.1 {
          try! bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
        } else {
          check(false, "expected an owned capture")
        }
      }
      check(sink.completed.wait(timeout: .now() + timeout) == .success, "retirement must complete without caller retry")
      let answers = sink.get(42)
      check(answers.count == 1, "retirement must complete exactly once")
      if reportFailure {
        guard case .failed? = answers.first else { return check(false, "retirement must report the earlier source failure") }
      } else {
        check(answers.first == .unit, "retirement must succeed after capture cancellation")
      }
    }
  }

  func orderedV2ControlChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      private let lock = NSLock()
      private var values: [UInt64: MobileRadioCompletion] = [:]
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock(); defer { lock.unlock() }; values[requestId] = completion; return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func get(_ id: UInt64) -> MobileRadioCompletion? { lock.lock(); defer { lock.unlock() }; return values[id] }
    }
    let driver = ScriptedDriver()
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      fatalError("fenced open must not install")
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let sink = Sink()
    adapter.bind(sink: sink)
    driver.onQueue {}
    let owner = adapter.orderedAdmissionOwner(version: 3)
    check(UUID(uuidString: owner) != nil, "v2 owner negotiation")
    check(adapter.orderedAdmissionOwner(version: 4).isEmpty, "unproven owner retirement must not be advertised")
    let otherSink = Sink()
    check(!adapter.bind(sink: otherSink), "a replacement must not take the control route")
    let instance = MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0, characteristicUuid: "c", characteristicOccurrence: 0)
    let spec = MobileOrderedOpenSpec(connectionGeneration: "conn", databaseGeneration: "db",
      snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0),
      members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8)
    let identity = MobileOrderedOpenIdentity(version: 2, ownerIncarnation: owner, slot: 0, slotGeneration: 1,
      originalRequestId: 5000, nativeGroupId: 5001, peerId: "p", spec: spec)
    let entered = DispatchSemaphore(value: 0)
    let release = DispatchSemaphore(value: 0)
    driver.workQueue.async { entered.signal(); release.wait() }
    check(entered.wait(timeout: .now() + timeout) == .success, "hold radio queue")
    adapter.submit(request: .orderedOpenV2(id: 5000, identity: identity))
    adapter.submit(request: .orderedRecoverOpenV2(id: 5002, identity: identity))
    guard case let .orderedOpenResultV2(slot, .failed(_, .notCreated))? = sink.get(5002),
          slot.slot == 0, slot.slotGeneration == 1, slot.ownerIncarnation == owner else {
      release.signal(); return check(false, "v2 fence must answer while radio queue is blocked")
    }
    check(otherSink.get(5002) == nil, "fence completion stays on the original control route")
    adapter.submit(request: .orderedAcknowledgeOpenV2(id: 5003, identity: identity))
    guard case .orderedOpenRetiredV2(slot)? = sink.get(5003), slot.slotGeneration == 1 else {
      release.signal(); return check(false, "v2 acknowledgement must report exact retirement")
    }
    adapter.submit(request: .orderedAcknowledgeOpenV2(id: 5004, identity: identity))
    check(sink.get(5003) == sink.get(5004), "lost retirement reply can be retried")
    release.signal()
    driver.onQueue {}
    var next = identity
    next.slotGeneration = 2
    next.originalRequestId = 5010
    next.nativeGroupId = 5011
    adapter.submit(request: .orderedRecoverOpenV2(id: 5012, identity: next))
    guard case .orderedOpenResultV2(_, .failed(_, .notCreated))? = sink.get(5012) else {
      return check(false, "fence before open must bind and fence next generation")
    }
    adapter.submit(request: .orderedAcknowledgeOpenV2(id: 5013, identity: next))
    adapter.submit(request: .orderedAcknowledgeOpenV2(id: 5014, identity: identity))
    guard case .orderedOpenRetiredV2? = sink.get(5013), case .orderedOpenRetiredV2? = sink.get(5014) else {
      return check(false, "retirement and stale acknowledgement")
    }
    adapter.submit(request: .orderedOpenV2(id: 5010, identity: next))
    driver.onQueue {}
    guard case .failed? = sink.get(5010) else { return check(false, "retired generation was readmitted") }

    let gate = OwnedOrderedGraphPermitGate(queue: driver.workQueue, maxItems: 4, maxBytes: 16, overflow: {})
    let gateKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "conn",
      snapshot: .init(owner: "test", attachment: 0, revision: 0), id: 4)
    driver.onQueue { driver.orderedTestGate = gate; check(gate.acquire(gateKey, valid: true) == .acquired, "v2 graph permit") }
    var deferred = identity
    deferred.slotGeneration = 3
    deferred.originalRequestId = 5015
    deferred.nativeGroupId = 5016
    adapter.submit(request: .orderedOpenV2(id: 5015, identity: deferred))
    driver.onQueue {}
    adapter.submit(request: .orderedRecoverOpenV2(id: 5017, identity: deferred))
    adapter.submit(request: .orderedAcknowledgeOpenV2(id: 5018, identity: deferred))
    guard case .orderedOpenResultV2(_, .failed(_, .notCreated))? = sink.get(5017),
          case .orderedOpenRetiredV2? = sink.get(5018) else { return check(false, "graph-deferred open fence and retirement") }
    driver.onQueue { _ = gate.release(gateKey); driver.orderedTestGate = nil }
    driver.onQueue {}
    guard case .failed? = sink.get(5015) else { return check(false, "retired graph closure must stay fenced") }

    var cancelled = identity
    cancelled.slotGeneration = 4
    cancelled.originalRequestId = 5050
    cancelled.nativeGroupId = 5051
    let cancelEntered = DispatchSemaphore(value: 0)
    let cancelRelease = DispatchSemaphore(value: 0)
    driver.workQueue.async { cancelEntered.signal(); cancelRelease.wait() }
    check(cancelEntered.wait(timeout: .now() + timeout) == .success, "hold queue before cancellation")
    adapter.submit(request: .orderedOpenV2(id: 5050, identity: cancelled))
    adapter.cancel(requestId: 5050)
    adapter.submit(request: .orderedRecoverOpenV2(id: 5052, identity: cancelled))
    guard case let .orderedOpenResultV2(_, .failed(cancelFailure, .notCreated))? = sink.get(5052),
          cancelFailure.kind == "cancelled" else { cancelRelease.signal(); return check(false, "cancelled v2 reservation") }
    adapter.submit(request: .orderedAcknowledgeOpenV2(id: 5053, identity: cancelled))
    cancelRelease.signal()
    driver.onQueue {}
    guard case .orderedOpenRetiredV2? = sink.get(5053) else { return check(false, "cancelled receipt retirement") }

    let started = DispatchSemaphore(value: 0)
    let finish = DispatchSemaphore(value: 0)
    let failure = NSError(domain: "original-v2", code: 42, userInfo: [NSLocalizedDescriptionKey: "original refusal"])
    let blockedBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      started.signal(); finish.wait()
      throw UnifiedBleRustOrderedGroupBridge.FactoryNoEffect(underlying: failure)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let blocked = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: blockedBridge)
    blocked.bind(sink: sink)
    driver.onQueue {}
    var executing = identity
    executing.ownerIncarnation = blocked.orderedAdmissionOwner(version: 3)
    executing.originalRequestId = 5020
    executing.nativeGroupId = 5021
    blocked.submit(request: .orderedOpenV2(id: 5020, identity: executing))
    check(started.wait(timeout: .now() + timeout) == .success, "executing v2 open")
    blocked.submit(request: .orderedRecoverOpenV2(id: 5022, identity: executing))
    guard case .orderedOpenResultV2(_, .pending)? = sink.get(5022) else {
      finish.signal(); return check(false, "executing open must report pending without blocking")
    }
    blocked.submit(request: .orderedAcknowledgeOpenV2(id: 5023, identity: executing))
    guard case .failed? = sink.get(5023) else { finish.signal(); return check(false, "executing open cannot retire") }
    finish.signal()
    driver.onQueue {}
    guard case let .orderedOpenResultV2(_, .failed(original, .notCreated))? = sink.get(5020),
          original.nativeDomain == "original-v2", original.nativeCode == 42, original.detail == "original refusal" else {
      return check(false, "no-effect failure must preserve its original cause")
    }
    blocked.submit(request: .orderedRecoverOpenV2(id: 5024, identity: executing))
    check(sink.get(5020) == sink.get(5024), "recovery must replay exact failure")
    blocked.submit(request: .orderedAcknowledgeOpenV2(id: 5025, identity: executing))
    guard case .orderedOpenRetiredV2? = sink.get(5025) else { return check(false, "terminal open retirement") }

    let source = OwnedOrderedNotificationSource(queue: driver.workQueue, generation: "v2-partial")
    var allowRemoval = false
    let partialBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: driver.workQueue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard source.add(ingress) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      source.invalidate(.connectionLost)
      return (source, ingress)
    }, validateSnapshot: { _, _ in true }, removeSourceGroup: { source, ingress in
      allowRemoval && source.remove(ingress)
    }, wake: { _, _ in })
    let partialAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: partialBridge)
    partialAdapter.bind(sink: sink)
    driver.onQueue {}
    var partial = identity
    partial.ownerIncarnation = partialAdapter.orderedAdmissionOwner(version: 3)
    partial.originalRequestId = 5030
    partial.nativeGroupId = 5031
    partialAdapter.submit(request: .orderedOpenV2(id: 5030, identity: partial))
    driver.onQueue {}
    guard case let .orderedOpenResultV2(_, .failed(original, .created(generation, members)))? = sink.get(5030),
          generation == .opaque(value: "v2-partial"), members.isEmpty, original.dispatched else {
      return check(false, "partial failure must retain installed ownership and original failure")
    }
    partialAdapter.submit(request: .orderedRecoverOpenV2(id: 5032, identity: partial))
    check(sink.get(5030) == sink.get(5032), "partial failure recovery must be exact")
    partialAdapter.submit(request: .orderedAcknowledgeOpenV2(id: 5033, identity: partial))
    guard case .orderedOpenRetiredV2? = sink.get(5033) else { return check(false, "partial receipt retirement") }
    driver.onQueue {
      do {
        allowRemoval = true
        guard let batch = try partialBridge.drain(groupID: 5031) else { return check(false, "partial source retained after admission retirement") }
        try partialBridge.acknowledge(groupID: 5031, batchID: batch.id)
        try partialBridge.seal(groupID: 5031)
        try partialBridge.remove(groupID: 5031, opaqueSourceGeneration: "v2-partial")
        try partialBridge.acknowledgeRemoval(groupID: 5031, opaqueSourceGeneration: "v2-partial")
      } catch { check(false, "partial source cleanup: \(error)") }
    }

    let liveSource = OwnedOrderedNotificationSource(queue: driver.workQueue, generation: "v2-live")
    var permitLiveRemoval = false
    let liveBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: driver.workQueue, generation: liveSource.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard liveSource.add(ingress) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      return (liveSource, ingress)
    }, validateSnapshot: { _, _ in true }, removeSourceGroup: { source, ingress in
      permitLiveRemoval && source.remove(ingress)
    }, wake: { _, _ in })
    let liveAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: liveBridge)
    liveAdapter.bind(sink: sink)
    driver.onQueue {}
    var live = identity
    live.ownerIncarnation = liveAdapter.orderedAdmissionOwner(version: 3)
    live.originalRequestId = 5040
    live.nativeGroupId = 5041
    liveAdapter.submit(request: .orderedOpenV2(id: 5040, identity: live))
    driver.onQueue {}
    guard case let .orderedOpenResultV2(_, .opened(generation, members))? = sink.get(5040),
          generation == .opaque(value: "v2-live"), members.count == 1 else { return check(false, "v2 source open") }
    liveAdapter.submit(request: .orderedRecoverOpenV2(id: 5042, identity: live))
    check(sink.get(5040) == sink.get(5042), "lost successful callback must replay exact descriptor")
    liveAdapter.submit(request: .orderedAcknowledgeOpenV2(id: 5043, identity: live))
    let retirement = UUID().uuidString.lowercased()
    liveAdapter.submit(request: .ownerFence(id: 5044, ownerIncarnation: live.ownerIncarnation, retirementId: retirement))
    liveAdapter.submit(request: .orderedRetire(id: 5045, generation: generation, groupId: 5041, member: members[0]))
    driver.onQueue {}
    check(sink.get(5045) == .unit, "fenced owner must allow exact source retirement")
    liveAdapter.submit(request: .orderedClose(id: 5046, generation: generation, groupId: 5041))
    driver.onQueue {}
    check(sink.get(5046) == .unit, "fenced owner must allow source close")
    liveAdapter.submit(request: .orderedDrain(id: 5047, generation: generation, groupId: 5041))
    driver.onQueue {}
    guard case let .orderedRecords(batchID?, _)? = sink.get(5047) else { return check(false, "fenced source must drain") }
    liveAdapter.submit(request: .orderedAcknowledgeBatch(id: 5048, generation: generation, groupId: 5041, batchId: batchID))
    liveAdapter.submit(request: .orderedSeal(id: 5049, generation: generation, groupId: 5041))
    liveAdapter.submit(request: .orderedRemove(id: 5054, generation: generation, groupId: 5041))
    driver.onQueue {}
    guard case .failed? = sink.get(5054) else { return check(false, "failed removal must retain source debt") }
    liveAdapter.submit(request: .ownerStatus(id: 5056, ownerIncarnation: live.ownerIncarnation, retirementId: retirement))
    guard case .ownerRetirement(_, _, .fencedPending)? = sink.get(5056) else { return check(false, "failed source cleanup cannot prove quiescence") }
    driver.onQueue { permitLiveRemoval = true }
    liveAdapter.submit(request: .orderedRemove(id: 5057, generation: generation, groupId: 5041))
    liveAdapter.submit(request: .orderedRemove(id: 5058, generation: generation, groupId: 5041))
    liveAdapter.submit(request: .orderedForgetRemovalReceipt(id: 5055, generation: generation, groupId: 5041))
    driver.onQueue {}
    check(sink.get(5048) == .unit && sink.get(5049) == .unit && sink.get(5057) == .unit && sink.get(5058) == .unit && sink.get(5055) == .unit,
      "fenced source cleanup must complete through the adapter")
  }

  func ownerFenceControlChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      private let lock = NSLock()
      private var values: [UInt64: MobileRadioCompletion] = [:]
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock(); defer { lock.unlock() }; values[requestId] = completion; return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func get(_ id: UInt64) -> MobileRadioCompletion? { lock.lock(); defer { lock.unlock() }; return values[id] }
    }
    let driver = ScriptedDriver()
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      fatalError("fenced owner must not create a source")
    }, wake: { _, _ in })
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let sink = Sink()
    adapter.bind(sink: sink)
    driver.onQueue {}
    let owner = adapter.orderedAdmissionOwner(version: 3)
    let retirement = UUID().uuidString.lowercased()
    adapter.submit(request: .ownerFence(id: 9109, ownerIncarnation: UUID().uuidString.lowercased(), retirementId: retirement))
    guard case .ownerRetirement(_, _, .rejected(.wrongOwner))? = sink.get(9109) else {
      return check(false, "a different owner must not be fenced")
    }
    adapter.submit(request: .ownerFence(id: 9110, ownerIncarnation: owner, retirementId: "invalid"))
    guard case .ownerRetirement(_, _, .rejected(.malformed))? = sink.get(9110) else {
      return check(false, "malformed retirement identity must be rejected")
    }
    let entered = DispatchSemaphore(value: 0)
    let release = DispatchSemaphore(value: 0)
    driver.workQueue.async { entered.signal(); release.wait() }
    check(entered.wait(timeout: .now() + timeout) == .success, "hold owner queue")
    adapter.submit(request: .adapterState(id: 9101))
    adapter.submit(request: .ownerFence(id: 9102, ownerIncarnation: owner, retirementId: retirement))
    let lateBorrower = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    check(!lateBorrower.bind(sink: Sink()), "a fenced owner must refuse a new callback attachment")
    guard case let .ownerRetirement(actualOwner, actualRetirement, .fencedPending(classes, _))? = sink.get(9102),
          actualOwner == owner, actualRetirement == retirement, classes.contains(.nativeWork), classes.contains(.callbacks) else {
      release.signal(); return check(false, "fence must return retained pending state while queued work is held")
    }
    adapter.submit(request: .ownerStatus(id: 9103, ownerIncarnation: owner, retirementId: retirement))
    check(sink.get(9102) == sink.get(9103), "lost fence reply must be recoverable by exact status")
    adapter.submit(request: .ownerFence(id: 9104, ownerIncarnation: owner, retirementId: UUID().uuidString.lowercased()))
    guard case .ownerRetirement(_, _, .rejected(.conflictingRetirement))? = sink.get(9104) else {
      release.signal(); return check(false, "different retirement must be rejected")
    }
    adapter.submit(request: .adapterState(id: 9105))
    guard case .failed? = sink.get(9105) else { release.signal(); return check(false, "fenced owner must refuse new work") }
    let instance = MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0, characteristicUuid: "c", characteristicOccurrence: 0)
    let spec = MobileOrderedOpenSpec(connectionGeneration: "c", databaseGeneration: "d",
      snapshotIdentity: MobileGattSnapshotIdentity(owner: "snapshot", attachment: 0, revision: 0),
      members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8)
    let identity = MobileOrderedOpenIdentity(version: 2, ownerIncarnation: owner, slot: 0, slotGeneration: 1,
      originalRequestId: 9106, nativeGroupId: 9107, peerId: "p", spec: spec)
    adapter.submit(request: .orderedOpenV2(id: 9106, identity: identity))
    guard case .orderedOpenResultV2(_, .failed(_, .notCreated))? = sink.get(9106) else {
      release.signal(); return check(false, "fenced v2 open must retain its typed no-effect receipt")
    }
    adapter.submit(request: .orderedRecoverOpenV2(id: 9108, identity: identity))
    check(sink.get(9106) == sink.get(9108), "fenced open receipt must survive a lost reply")
    release.signal()
    driver.onQueue {}
    guard case .failed? = sink.get(9101) else { return check(false, "queued work must not execute after fence") }
    check(adapter.orderedAdmissionOwner(version: 4).isEmpty, "incomplete retirement must remain unadvertised")
  }

  func ownerFenceQueuedBindingChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      private let lock = NSLock()
      private var values: [UInt64: MobileRadioCompletion] = [:]
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock(); defer { lock.unlock() }; values[requestId] = completion; return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func get(_ id: UInt64) -> MobileRadioCompletion? { lock.lock(); defer { lock.unlock() }; return values[id] }
    }
    let driver = ScriptedDriver()
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      fatalError("no source expected")
    }, wake: { _, _ in })
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
    driver.workQueue.async { entered.signal(); release.wait() }
    check(entered.wait(timeout: .now() + timeout) == .success, "hold binding queue")
    let sink = Sink()
    check(adapter.bind(sink: sink), "binding admitted before fence")
    adapter.submit(request: .adapterState(id: 9149))
    adapter.submit(request: .ownerFence(id: 9150, ownerIncarnation: adapter.orderedAdmissionOwner(version: 3),
                                        retirementId: UUID().uuidString.lowercased()))
    release.signal()
    driver.onQueue {}
    check(driver.onQueue { driver.restoredSnapshotRequests } == 0,
          "queued binding must not install or request restored snapshots after fence")
    guard case .failed? = sink.get(9149) else { return check(false, "fenced queued request must retain its completion route") }
  }

  func ownerFenceAttachmentCallbackChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let driver = ScriptedDriver()
    driver.holdRestoredSnapshots = true
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      fatalError("no source expected")
    }, wake: { _, _ in })
    var announced = [String]()
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, onRestoredPeer: { announced.append($0) }, orderedGroupBridge: bridge)
    check(adapter.bind(sink: Sink()), "binding accepted")
    driver.onQueue {}
    adapter.submit(request: .ownerFence(id: 9151, ownerIncarnation: adapter.orderedAdmissionOwner(version: 3),
                                        retirementId: UUID().uuidString.lowercased()))
    driver.onQueue {
      check(driver.restoredCompletion != nil, "executing attachment retains its callback")
      driver.restoredCompletion?([["peerIdentifier": "original"]])
      driver.restoredCompletion?([["peerIdentifier": "late-duplicate"]])
    }
    check(driver.onQueue { announced } == ["original"], "late duplicate callback cannot reenter the old route")
  }

  func ownerFenceCachedRadioBorrowerChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    // The cached radio is constructed without allocating a central or using BLE.
    let key = "harness-" + UUID().uuidString
    let radio = OwnedCoreBluetoothProtocolRadioOwner.acquire(restoreIdentifierKey: key, showPowerAlert: nil)
    check(OwnedCoreBluetoothProtocolRadioOwner.acquire(restoreIdentifierKey: key, showPowerAlert: nil) === radio,
          "configuration must resolve to the cached radio")
    let old = UnifiedBleRustRadioAdapter(driver: radio)
    check(old.bind(sink: Sink()), "old adapter route binds")
    radio.queue.sync {}
    old.submit(request: .ownerFence(id: 9160, ownerIncarnation: old.orderedAdmissionOwner(version: 3),
                                    retirementId: UUID().uuidString.lowercased()))
    check(!radio.attachDelegateIfAvailable(old), "fenced owner must not claim cached radio borrower")
    let replacement = UnifiedBleRustRadioAdapter(driver: radio)
    check(radio.attachDelegateIfAvailable(replacement), "independent replacement may claim unowned radio")
    check(replacement.bind(sink: Sink()), "borrower callback route binds")
    let replacementBridge = radio.queue.sync { replacement.orderedGroupsOnRadioQueue()! }
    replacement.submit(request: .ownerFence(id: 9161, ownerIncarnation: replacement.orderedAdmissionOwner(version: 3),
                                            retirementId: UUID().uuidString.lowercased()))
    radio.queue.sync { radio.destroyed = true }
    let failure: NSError? = waitFor("failed cached borrower release") { radio.releaseBorrowerIfCurrent(replacement, completion: $0) }
    check(failure != nil, "failed physical cleanup must report failure")
    let next = UnifiedBleRustRadioAdapter(driver: radio)
    check(!radio.attachDelegateIfAvailable(next), "failed cleanup retains the exact borrower claim")
    radio.queue.sync { radio.destroyed = false }
    let released: NSError? = waitFor("cached borrower release retry") { radio.releaseBorrowerIfCurrent(replacement, completion: $0) }
    check(released == nil, "same borrower can retry cleanup")
    let fencedPeer = UnifiedBleRustRadioAdapter(driver: radio, orderedGroupBridge: replacementBridge)
    check(!radio.attachDelegateIfAvailable(fencedPeer), "cleanup must not reopen the retired owner's borrower admission")
    check(radio.attachDelegateIfAvailable(next), "replacement may attach only after cleanup succeeds")
    let final: NSError? = waitFor("replacement release") { radio.releaseBorrowerIfCurrent(next, completion: $0) }
    check(final == nil, "replacement borrower releases without allocating a central")
    var retained: UnifiedBleRustRadioAdapter? = UnifiedBleRustRadioAdapter(driver: radio)
    weak let weakBorrower = retained
    check(radio.attachDelegateIfAvailable(retained!), "another borrower attaches")
    retained = nil
    guard let claimed = weakBorrower else { return check(false, "cached borrower claim must retain its exact adapter") }
    let retainedRelease: NSError? = waitFor("retained borrower release") { radio.releaseBorrowerIfCurrent(claimed, completion: $0) }
    check(retainedRelease == nil, "retained borrower can be released")
  }

  func ownerFenceGraphCleanupChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      private let lock = NSLock()
      private var values: [UInt64: MobileRadioCompletion] = [:]
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock(); defer { lock.unlock() }; values[requestId] = completion; return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func get(_ id: UInt64) -> MobileRadioCompletion? { lock.lock(); defer { lock.unlock() }; return values[id] }
    }
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil, showPowerAlert: nil)
    let adapter = UnifiedBleRustRadioAdapter(driver: radio), sink = Sink()
    check(adapter.bind(sink: sink), "graph adapter binds")
    radio.queue.sync {}
    adapter.submit(request: .orderedGraphAuthorityReserve(id: 9170, request: 771))
    radio.queue.sync {}
    guard case let .orderedGraphAuthorityReserved(receipt)? = sink.get(9170) else { return check(false, "reserve authority") }
    adapter.submit(request: .orderedGraphAuthorityOpen(id: 9171, receiptId: receipt))
    radio.queue.sync {}
    guard case let .orderedGraphAuthorityOpened(authority)? = sink.get(9171) else { return check(false, "open authority") }
    adapter.submit(request: .orderedGraphAuthorityReserve(id: 9182, request: 773))
    radio.queue.sync {}
    guard case let .orderedGraphAuthorityReserved(queuedReceipt)? = sink.get(9182) else { return check(false, "reserve queued authority") }
    let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
    radio.queue.async { entered.signal(); release.wait() }
    check(entered.wait(timeout: .now() + timeout) == .success, "hold queued authority open")
    adapter.submit(request: .orderedGraphAuthorityOpen(id: 9183, receiptId: queuedReceipt))
    adapter.submit(request: .ownerFence(id: 9172, ownerIncarnation: adapter.orderedAdmissionOwner(version: 3),
                                        retirementId: UUID().uuidString.lowercased()))
    release.signal()
    radio.queue.sync {}
    adapter.submit(request: .orderedGraphAuthorityStatus(id: 9184, receiptId: queuedReceipt))
    radio.queue.sync {}
    guard case .orderedGraphAuthorityStatus(.notCreated)? = sink.get(9184) else {
      return check(false, "queued authority open must retain no-effect settlement after fence")
    }
    adapter.submit(request: .orderedGraphAuthorityForgetOpen(id: 9185, receiptId: queuedReceipt))
    radio.queue.sync {}
    check(sink.get(9185) == .unit, "queued authority no-effect receipt can be retired")
    adapter.submit(request: .orderedGraphAuthorityReserve(id: 9180, request: 772))
    guard case .failed? = sink.get(9180) else { return check(false, "fence refuses a new authority reservation") }
    adapter.submit(request: .orderedGraphAuthorityReserveFence(id: 9181, request: 772))
    radio.queue.sync {}
    guard case .orderedGraphReserveStatus(.notCreated)? = sink.get(9181) else {
      return check(false, "fenced reservation must have a truthful no-effect result")
    }
    adapter.submit(request: .orderedGraphAuthorityStatus(id: 9173, receiptId: receipt))
    radio.queue.sync {}
    guard case let .orderedGraphAuthorityStatus(.opened(actual))? = sink.get(9173), actual == authority else {
      return check(false, "fenced owner must expose the exact authority receipt")
    }
    adapter.submit(request: .orderedGraphAuthorityClose(id: 9174, authorityId: authority))
    radio.queue.sync {}
    check(sink.get(9174) == .unit, "fenced authority must close")
    adapter.submit(request: .orderedGraphAuthorityClose(id: 9178, authorityId: authority))
    radio.queue.sync {}
    check(sink.get(9178) == .unit, "retry of the exact close replays success")
    adapter.submit(request: .orderedGraphAuthorityForget(id: 9175, authorityId: authority))
    radio.queue.sync {}
    check(sink.get(9175) == .unit, "fenced authority must forget its exact close")
    adapter.submit(request: .orderedGraphAuthorityForgetOpen(id: 9176, receiptId: receipt))
    radio.queue.sync {}
    check(sink.get(9176) == .unit, "fenced authority must forget its exact open receipt")
    adapter.submit(request: .orderedGraphAuthorityForgetOpen(id: 9179, receiptId: receipt))
    radio.queue.sync {}
    check(sink.get(9179) == .unit, "retry of exact receipt retirement replays success")
    adapter.submit(request: .orderedGraphAuthorityReserveForget(id: 9177, request: 771))
    radio.queue.sync {}
    check(sink.get(9177) == .unit, "fenced reserve cleanup remains available")
  }

  func ownerFencePhysicalCleanupChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      private let lock = NSLock()
      private var values: [UInt64: MobileRadioCompletion] = [:]
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock(); defer { lock.unlock() }; values[requestId] = completion; return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func get(_ id: UInt64) -> MobileRadioCompletion? { lock.lock(); defer { lock.unlock() }; return values[id] }
    }
    let driver = ScriptedDriver()
    let source = OwnedOrderedNotificationSource(queue: driver.workQueue, generation: "fenced-physical")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: driver.workQueue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard source.add(ingress) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      return (source, ingress)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge), sink = Sink()
    adapter.bind(sink: sink)
    func answer(_ id: UInt64) -> MobileRadioCompletion? {
      let deadline = Date().addingTimeInterval(10)
      while Date() < deadline {
        if let value = sink.get(id) { return value }
        Thread.sleep(forTimeInterval: 0.001)
      }
      return nil
    }
    let instance = MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0,
      characteristicUuid: "c", characteristicOccurrence: 0)
    adapter.submit(request: .orderedOpen(id: 9401, groupId: 9402, connectionGeneration: "conn", databaseGeneration: "db",
      snapshotIdentity: .init(owner: "test", attachment: 0, revision: 0),
      members: [.init(memberId: 1, instance: instance)], itemCapacity: 4, byteCapacity: 32))
    guard case let .orderedOpened(generation, members)? = answer(9401), let member = members.first else { return check(false, "physical cleanup source") }
    adapter.submit(request: .connect(id: 9403, peerId: "p", autoConnect: false, preferredPhy: [], expectedGeneration: "conn"))
    check(answer(9403) == .unit, "physical cleanup connection")
    let activation = MobileOrderedActivation(instance: instance, connectionGeneration: "conn", sourceGeneration: generation,
      groupId: 9402, member: member, activationEpoch: 1)
    driver.onQueue { driver.holdSubscribes = true }
    adapter.submit(request: .orderedEnableNotifications(id: 9404, activation: activation, requested: nil, preferred: nil))
    let deadline = Date().addingTimeInterval(10)
    while !driver.onQueue({ driver.subscribeCompletions[driver.subscriptionIdentifier(9404)] != nil }) {
      check(Date() < deadline, "physical enable must start"); Thread.sleep(forTimeInterval: 0.001)
    }
    let owner = adapter.orderedAdmissionOwner(version: 3), retirement = UUID().uuidString.lowercased()
    adapter.submit(request: .ownerFence(id: 9405, ownerIncarnation: owner, retirementId: retirement))
    adapter.submit(request: .orderedEnableSettlement(id: 9406, activation: activation))
    check(answer(9406) == .orderedEnableStatus(status: .pending), "fenced owner must expose pending physical enable")
    driver.onQueue { driver.subscribeCompletions.removeValue(forKey: driver.subscriptionIdentifier(9404))?(nil) }
    check(answer(9404) == .notifyEnabled(delivery: "unknown"), "late physical enable is retained")
    driver.onQueue { driver.unsubscribeFailures = [NSError(domain: UnifiedBleRustRadioAdapter.ownedDomain, code: 1017)] }
    adapter.submit(request: .orderedDisableNotifications(id: 9407, activation: activation))
    guard case .failed? = answer(9407) else { return check(false, "failed physical cleanup must stay owned") }
    adapter.submit(request: .orderedEnableSettlement(id: 9408, activation: activation))
    check(answer(9408) == .orderedEnableStatus(status: .enabled(delivery: "unknown")), "failed cleanup retains enable receipt")
    adapter.submit(request: .orderedDisableNotifications(id: 9409, activation: activation))
    check(answer(9409) == .unit, "fenced physical cleanup can retry")
    adapter.submit(request: .orderedForgetEnableReceipt(id: 9410, activation: activation))
    check(answer(9410) == .unit, "released physical receipt can be forgotten")
  }

  func lateConnectCompletionCannotFinishReusedRequest() {
    final class Sink: UnifiedBleRustRadioSink {
      var answers = [MobileRadioCompletion]()
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        answers.append(completion); return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let driver = ScriptedDriver()
    driver.holdConnects = true
    let adapter = UnifiedBleRustRadioAdapter(driver: driver)
    let sink = Sink()
    check(adapter.bind(sink: sink), "bind reused request test")
    let request = MobileRadioRequest.connect(id: 4244, peerId: "p", autoConnect: false,
      preferredPhy: [], expectedGeneration: "c")
    adapter.submit(request: request)
    let deadline = Date().addingTimeInterval(2)
    while driver.onQueue({ driver.connectCompletions.count }) < 1 {
      check(Date() < deadline, "first connect admitted"); Thread.sleep(forTimeInterval: 0.001)
    }
    let old = driver.onQueue { driver.connectCompletions.keys.first! }
    adapter.cancel(requestId: 4244)
    driver.onQueue {}
    adapter.submit(request: request)
    while driver.onQueue({ driver.connectCompletions.count }) < 2 {
      check(Date() < deadline, "replacement connect admitted"); Thread.sleep(forTimeInterval: 0.001)
    }
    driver.onQueue {
      driver.connectCompletions.removeValue(forKey: old)?(nil)
      check(sink.answers.count == 1, "stale completion must not finish replacement request")
      let current = driver.connectCompletions.keys.first!
      driver.connectCompletions.removeValue(forKey: current)?(nil)
      check(sink.answers.count == 2 && sink.answers.last == .unit, "replacement completes from its own callback")
    }
  }

  func lateReadCompletionCannotFinishReusedRequest() {
    final class Sink: UnifiedBleRustRadioSink {
      var answers = [MobileRadioCompletion]()
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        answers.append(completion); return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let driver = ScriptedDriver()
    driver.holdReads = true
    let adapter = UnifiedBleRustRadioAdapter(driver: driver)
    let sink = Sink()
    check(adapter.bind(sink: sink), "bind reused read test")
    let instance = MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0,
      characteristicUuid: "c", characteristicOccurrence: 0)
    let request = MobileRadioRequest.read(id: 4245, instance: instance)
    adapter.submit(request: request)
    let deadline = Date().addingTimeInterval(2)
    while driver.onQueue({ driver.readCompletions.count }) < 1 {
      check(Date() < deadline, "first read admitted"); Thread.sleep(forTimeInterval: 0.001)
    }
    let old = driver.onQueue { driver.readCompletions.keys.first! }
    adapter.cancel(requestId: 4245)
    driver.onQueue {}
    adapter.submit(request: request)
    while driver.onQueue({ driver.readCompletions.count }) < 2 {
      check(Date() < deadline, "replacement read admitted"); Thread.sleep(forTimeInterval: 0.001)
    }
    driver.onQueue {
      driver.readCompletions.removeValue(forKey: old)?(Data([1]) as NSData, .readResponse, nil)
      check(sink.answers.count == 1, "stale read must not finish replacement request")
      let current = driver.readCompletions.keys.first!
      driver.readCompletions.removeValue(forKey: current)?(Data([2]) as NSData, .readResponse, nil)
      check(sink.answers.count == 2 && sink.answers.last == .read(value: Data([2]), provenance: "read-response"),
        "replacement read completes from its own callback")
    }
  }

  func cancelledDeferredRequestCannotRunReplacement() {
    final class Sink: UnifiedBleRustRadioSink {
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let driver = ScriptedDriver()
    let adapter = UnifiedBleRustRadioAdapter(driver: driver)
    let sink = Sink()
    check(adapter.bind(sink: sink), "bind deferred reuse test")
    let gate = OwnedOrderedGraphPermitGate(queue: driver.workQueue, maxItems: 4, maxBytes: 1024, overflow: {})
    let key = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "conn",
      snapshot: .init(owner: "test", attachment: 0, revision: 0), id: 9)
    driver.onQueue {
      driver.orderedTestGate = gate
      check(gate.acquire(key, valid: true) == .acquired, "hold graph permit")
    }
    func instance(_ characteristic: String) -> MobileInstance {
      MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0,
        characteristicUuid: characteristic, characteristicOccurrence: 0)
    }
    adapter.submit(request: .read(id: 4246, instance: instance("old")))
    driver.onQueue {}
    adapter.cancel(requestId: 4246)
    driver.onQueue {}
    adapter.submit(request: .read(id: 4246, instance: instance("new")))
    driver.onQueue {}
    driver.onQueue { _ = gate.release(key); driver.orderedTestGate = nil }
    driver.onQueue {}
    driver.onQueue {}
    check(driver.onQueue { !driver.calls.contains("read old") && driver.calls.contains("read new") },
      "cancelled deferred work must not claim the replacement request")
  }

  func duplicateRequestNumbersHaveDistinctRadioIdentities() {
    final class Sink: UnifiedBleRustRadioSink {
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let driver = ScriptedDriver()
    driver.holdConnects = true
    let first = UnifiedBleRustRadioAdapter(driver: driver)
    let second = UnifiedBleRustRadioAdapter(driver: driver)
    let firstSink = Sink(), secondSink = Sink()
    check(first.bind(sink: firstSink) && second.bind(sink: secondSink), "both adapters bind")
    let request = MobileRadioRequest.connect(id: 4242, peerId: "p", autoConnect: false,
      preferredPhy: [], expectedGeneration: "c")
    first.submit(request: request)
    second.submit(request: request)
    let deadline = Date().addingTimeInterval(2)
    while Date() < deadline && driver.onQueue({ driver.connectCompletions.count }) < 2 {
      Thread.sleep(forTimeInterval: 0.001)
    }
    check(driver.onQueue { driver.connectCompletions.count } == 2,
      "independent borrowers with the same request number must own distinct radio operations")
    driver.onQueue { driver.holdSubscribes = true }
    let instance = MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0,
      characteristicUuid: "c", characteristicOccurrence: 0)
    let enable = MobileRadioRequest.enableNotifications(id: 4243, instance: instance, epoch: 1,
      requested: nil, preferred: nil)
    first.submit(request: enable)
    second.submit(request: enable)
    let subscribeDeadline = Date().addingTimeInterval(2)
    while Date() < subscribeDeadline && driver.onQueue({ driver.subscribeCompletions.count }) < 2 {
      Thread.sleep(forTimeInterval: 0.001)
    }
    check(driver.onQueue { driver.subscribeCompletions.count } == 2,
      "independent borrowers with the same request number must own distinct subscriptions")
  }

  func sharedBridgeKeepsEachAdaptersWakeRoute() {
    final class Sink: UnifiedBleRustRadioSink {
      var completions: [UInt64: MobileRadioCompletion] = [:]
      var wakes: [UInt64] = []
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        completions[requestId] = completion; return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String {
        if case let .orderedWake(_, groupId) = ingress { wakes.append(groupId) }
        return "accepted"
      }
    }
    let driver = ScriptedDriver()
    let source = OwnedOrderedNotificationSource(queue: driver.workQueue, generation: "shared-source")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: driver.workQueue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress))
      return (source, ingress)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let first = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let second = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let firstSink = Sink(), secondSink = Sink()
    check(first.bind(sink: firstSink) && second.bind(sink: secondSink), "both routes bind")
    let snapshot = MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0)
    func instance(_ characteristic: String) -> MobileInstance {
      MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0,
        characteristicUuid: characteristic, characteristicOccurrence: 0)
    }
    func open(_ id: UInt64, _ characteristic: String) -> MobileRadioRequest {
      .orderedOpen(id: id, groupId: id, connectionGeneration: "c", databaseGeneration: "d",
        snapshotIdentity: snapshot, members: [.init(memberId: 1, instance: instance(characteristic))],
        itemCapacity: 4, byteCapacity: 16)
    }
    first.submit(request: open(7001, "a")); second.submit(request: open(7002, "b"))
    driver.onQueue {
      guard case .orderedOpened? = firstSink.completions[7001], case .orderedOpened? = secondSink.completions[7002] else {
        return check(false, "both adapters must install their groups")
      }
      for characteristic in ["a", "b"] {
        source.admit(route: .init(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
          characteristicUUID: characteristic, characteristicOccurrence: 0), bytes: Data([1]), provenance: .notification)
      }
    }
    driver.onQueue {
      check(firstSink.wakes == [7001] && secondSink.wakes == [7002], "shared source wakes must reach their exact adapters")
    }
  }

  func bindingReplacementDoesNotStealCompletion() {
    final class Sink: UnifiedBleRustRadioSink {
      private let lock = NSLock()
      private var ids = [UInt64]()
      let answered = DispatchSemaphore(value: 0)
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock(); ids.append(requestId); lock.unlock(); answered.signal(); return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func contains(_ id: UInt64) -> Bool { lock.lock(); defer { lock.unlock() }; return ids.contains(id) }
    }
    let driver = ScriptedDriver()
    driver.holdConnects = true
    let adapter = UnifiedBleRustRadioAdapter(driver: driver)
    let original = Sink()
    let replacement = Sink()
    check(adapter.bind(sink: original), "first binding must succeed")
    check(adapter.bind(sink: original), "same binding must be idempotent")
    adapter.submit(request: .connect(id: 9001, peerId: "held", autoConnect: false, preferredPhy: [], expectedGeneration: "generation"))
    let deadline = Date().addingTimeInterval(10)
    while !driver.onQueue({ driver.connectCompletions[driver.connectIdentifier(9001)] != nil }) {
      check(Date() < deadline, "held connection not admitted")
      Thread.sleep(forTimeInterval: 0.001)
    }
    check(!adapter.bind(sink: replacement), "replacement must be refused while the original route is live")
    driver.onQueue { driver.connectCompletions.removeValue(forKey: driver.connectIdentifier(9001))?(nil) }
    check(original.answered.wait(timeout: .now() + timeout) == .success, "old binding must receive its completion")
    check(original.contains(9001) && !replacement.contains(9001), "late callback must not enter replacement binding")
  }

  func ownerFenceHeldCallbackAndIsolationChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      private let lock = NSLock()
      private var values: [UInt64: MobileRadioCompletion] = [:]
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        lock.lock(); defer { lock.unlock() }; values[requestId] = completion; return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func get(_ id: UInt64) -> MobileRadioCompletion? { lock.lock(); defer { lock.unlock() }; return values[id] }
    }
    let driver = ScriptedDriver()
    driver.holdConnects = true
    func makeBridge() -> UnifiedBleRustOrderedGroupBridge {
      UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
        fatalError("test does not open a source")
      }, wake: { _, _ in })
    }
    let bridge = makeBridge()
    let first = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let borrower = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let other = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: makeBridge())
    let firstSink = Sink(), borrowerSink = Sink(), otherSink = Sink()
    first.bind(sink: firstSink); borrower.bind(sink: borrowerSink); other.bind(sink: otherSink)
    driver.onQueue {}
    first.submit(request: .connect(id: 9201, peerId: "held", autoConnect: false, preferredPhy: [], expectedGeneration: "g"))
    let deadline = Date().addingTimeInterval(10)
    while !driver.onQueue({ driver.connectCompletions[driver.connectIdentifier(9201)] != nil }) {
      check(Date() < deadline, "connection callback must be held")
      Thread.sleep(forTimeInterval: 0.001)
    }
    let owner = first.orderedAdmissionOwner(version: 3)
    let retirement = UUID().uuidString.lowercased()
    first.submit(request: .ownerFence(id: 9202, ownerIncarnation: owner, retirementId: retirement))
    guard case .ownerRetirement(_, _, .fencedPending)? = firstSink.get(9202) else {
      return check(false, "executing work must keep retirement pending")
    }
    borrower.submit(request: .ownerStatus(id: 9203, ownerIncarnation: owner, retirementId: retirement))
    check(firstSink.get(9202) == borrowerSink.get(9203), "borrowers of one owner must share the retained receipt")
    borrower.submit(request: .adapterState(id: 9204))
    guard case .failed? = borrowerSink.get(9204) else { return check(false, "shared borrower must observe the same fence") }
    other.submit(request: .adapterState(id: 9205))
    let otherDeadline = Date().addingTimeInterval(10)
    while otherSink.get(9205) == nil {
      check(Date() < otherDeadline, "separate owner must remain usable")
      Thread.sleep(forTimeInterval: 0.001)
    }
    guard case .adapter? = otherSink.get(9205) else { return check(false, "separate owner must not be fenced") }
    driver.onQueue { driver.connectCompletions.removeValue(forKey: driver.connectIdentifier(9201))?(nil) }
    guard firstSink.get(9201) != nil, borrowerSink.get(9201) == nil, otherSink.get(9201) == nil else {
      return check(false, "delayed callback must return to its original sink")
    }
    first.submit(request: .ownerStatus(id: 9206, ownerIncarnation: owner, retirementId: retirement))
    guard case .ownerRetirement(_, _, .fencedPending)? = firstSink.get(9206) else {
      return check(false, "callback alone does not prove cleanup")
    }
  }

  func ownerFenceDuringCallbackRelayChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      let entered = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
      private let lock = NSLock()
      private var values: [UInt64: MobileRadioCompletion] = [:]
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        if requestId == 9301 { entered.signal(); release.wait() }
        lock.lock(); values[requestId] = completion; lock.unlock()
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func get(_ id: UInt64) -> MobileRadioCompletion? { lock.lock(); defer { lock.unlock() }; return values[id] }
    }
    let driver = ScriptedDriver()
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      fatalError("test does not open a source")
    }, wake: { _, _ in })
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let sink = Sink()
    adapter.bind(sink: sink)
    driver.onQueue {}
    adapter.submit(request: .adapterState(id: 9301))
    check(sink.entered.wait(timeout: .now() + timeout) == .success, "completion relay must be held")
    let owner = adapter.orderedAdmissionOwner(version: 3), retirement = UUID().uuidString.lowercased()
    adapter.submit(request: .ownerFence(id: 9302, ownerIncarnation: owner, retirementId: retirement))
    guard case let .ownerRetirement(_, _, .fencedPending(classes, _))? = sink.get(9302), classes.contains(.callbacks) else {
      sink.release.signal(); return check(false, "held relay must never be reported quiescent")
    }
    sink.release.signal()
    driver.onQueue {}
    adapter.submit(request: .ownerStatus(id: 9303, ownerIncarnation: owner, retirementId: retirement))
    guard case .ownerRetirement(_, _, .fencedPending)? = sink.get(9303) else {
      return check(false, "relay return alone does not prove owner cleanup")
    }
  }

  func orderedDeferredOpenFenceChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      var completions: [UInt64: MobileRadioCompletion] = [:]
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { completions[requestId] = completion; return "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let driver = ScriptedDriver()
    let gate = OwnedOrderedGraphPermitGate(queue: driver.workQueue, maxItems: 8, maxBytes: 32, overflow: {})
    let identity = OwnedGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0)
    let gateKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "conn", snapshot: identity, id: 1)
    driver.onQueue { driver.orderedTestGate = gate; check(gate.acquire(gateKey, valid: true) == .acquired, "graph permit") }
    var creations = 0
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      creations += 1
      throw NSError(domain: "unexpected", code: 1)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let sink = Sink()
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let sameOwnerAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    let replacementBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      throw NSError(domain: "unused", code: 1)
    }, wake: { _, _ in })
    let replacementAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: replacementBridge)
    check(adapter.orderedAdmissionOwner(version: 3) == sameOwnerAdapter.orderedAdmissionOwner(version: 3), "adapters share admission owner")
    check(adapter.orderedAdmissionOwner(version: 3) != replacementAdapter.orderedAdmissionOwner(version: 3), "replacement admission owner is fresh")
    check(!adapter.orderedAdmissionOwner(version: 3).isEmpty, "failure-summary admission is supported")
    check(adapter.orderedAdmissionOwner(version: 2).isEmpty && adapter.orderedAdmissionOwner(version: 1).isEmpty, "legacy adapters are refused")
    adapter.bind(sink: sink)
    let instance = MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0, characteristicUuid: "c", characteristicOccurrence: 0)
    let open = MobileRadioRequest.orderedOpen(id: 900, groupId: 901, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8)
    adapter.submit(request: open)
    driver.onQueue {} // Original open is now deferred behind the held graph permit.
    adapter.submit(request: .orderedRecoverOpen(id: 902, groupId: 901, openRequestId: 999, peerId: "p"))
    driver.onQueue {
      guard case .failed? = sink.completions[902] else { return check(false, "wrong request must not fence") }
    }
    adapter.submit(request: .orderedRecoverOpen(id: 902, groupId: 901, openRequestId: 900, peerId: "p"))
    driver.onQueue {
      guard case .orderedOpenFailed(_, .notCreated)? = sink.completions[902] else { return check(false, "recovery must overtake the held open") }
      check(sink.completions[900] == nil, "open remains deferred")
    }
    guard bridge.openReceiptIdentity(groupID: 901) != nil else { return check(false, "fenced receipt missing") }
    adapter.submit(request: .orderedAcknowledgeOpen(id: 911, version: 2, groupId: 901, openRequestId: 900, peerId: "p"))
    driver.onQueue { guard case .failed? = sink.completions[911] else { return check(false, "unknown ack version") } }
    adapter.submit(request: .orderedAcknowledgeOpen(id: 912, version: 1, groupId: 901, openRequestId: 900, peerId: "p"))
    driver.onQueue { guard case .unit? = sink.completions[912] else { return check(false, "exact ack") } }
    let replacement = MobileRadioRequest.orderedOpen(id: 909, groupId: 901, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8)
    adapter.submit(request: replacement)
    driver.onQueue {}
    adapter.submit(request: .orderedAcknowledgeOpen(id: 913, version: 1, groupId: 901, openRequestId: 900, peerId: "p"))
    driver.onQueue { guard case .unit? = sink.completions[913] else { return check(false, "lost ack retry") } }
    adapter.submit(request: .orderedRecoverOpen(id: 914, groupId: 901, openRequestId: 900, peerId: "p"))
    adapter.submit(request: .orderedAcknowledgeOpen(id: 915, version: 1, groupId: 901, openRequestId: 909, peerId: "other"))
    driver.onQueue {
      guard case .failed? = sink.completions[914], case .failed? = sink.completions[915] else { return check(false, "stale recovery or mismatched ack") }
    }
    adapter.submit(request: .orderedRecoverOpen(id: 910, groupId: 901, openRequestId: 909, peerId: "p"))
    driver.onQueue {
      guard case .orderedOpenFailed(_, .notCreated)? = sink.completions[910] else { return check(false, "replacement fence") }
      _ = gate.release(gateKey)
    }
    driver.onQueue {
      guard case .orderedOpenFailed(_, .notCreated)? = sink.completions[900] else { return check(false, "late open must be fenced") }
      guard case .orderedOpenFailed(_, .notCreated)? = sink.completions[909] else { return check(false, "replacement must remain fenced") }
      check(creations == 0, "fenced closure cannot create")
    }
    guard let replacementReceipt = bridge.openReceiptIdentity(groupID: 901) else { return check(false, "replacement receipt missing") }
    do { try bridge.acknowledgeOpen(replacementReceipt.request, token: replacementReceipt.token) }
    catch { return check(false, "replacement receipt acknowledgement") }

    let cancelKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "conn", snapshot: identity, id: 2)
    driver.onQueue { check(gate.acquire(cancelKey, valid: true) == .acquired, "second graph permit") }
    let cancelledOpen = MobileRadioRequest.orderedOpen(id: 903, groupId: 904, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8)
    adapter.submit(request: cancelledOpen)
    driver.onQueue {}
    adapter.cancel(requestId: 903)
    adapter.submit(request: .orderedRecoverOpen(id: 905, groupId: 904, openRequestId: 903, peerId: "p"))
    driver.onQueue {
      guard case .orderedOpenFailed(_, .notCreated)? = sink.completions[905] else { return check(false, "cancelled deferred open recovery") }
      check(sink.completions[903] == sink.completions[905], "cancelled open recovery must replay the original answer")
      _ = gate.release(cancelKey)
      check(creations == 0, "cancelled deferred closure cannot create")
    }
    guard let cancelledReceipt = bridge.openReceiptIdentity(groupID: 904) else { return check(false, "cancelled receipt missing") }
    do { try bridge.acknowledgeOpen(cancelledReceipt.request, token: cancelledReceipt.token) }
    catch { return check(false, "cancelled receipt acknowledgement") }

    let overflowGate = OwnedOrderedGraphPermitGate(queue: driver.workQueue, maxItems: 1, maxBytes: 1, overflow: {})
    let overflowKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "conn", snapshot: identity, id: 3)
    driver.onQueue {
      driver.orderedTestGate = overflowGate
      check(overflowGate.acquire(overflowKey, valid: true) == .acquired, "overflow graph permit")
      check(overflowGate.enqueue(reject: { check(false, "first deferred request rejected") }) {}, "fill deferred queue")
    }
    adapter.submit(request: .orderedOpen(id: 940, groupId: 941, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8))
    driver.onQueue { guard case .orderedOpenFailed(_, .notCreated)? = sink.completions[940] else { return check(false, "deferred admission refusal") } }
    adapter.submit(request: .orderedRecoverOpen(id: 942, groupId: 941, openRequestId: 940, peerId: "p"))
    driver.onQueue {
      check(sink.completions[940] == sink.completions[942], "deferred refusal must replay exactly")
      _ = overflowGate.release(overflowKey)
      driver.orderedTestGate = gate
    }

    let capacityBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      creations += 1
      throw NSError(domain: "unexpected", code: 2)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    var heldReceipts: [(UnifiedBleRustOrderedGroupBridge.OpenRequest, UnifiedBleRustOrderedGroupBridge.OpenToken)] = []
    do {
      for id in UInt64(10000)..<UInt64(10064) {
        let request = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: id, groupID: id, peer: "p", fingerprint: id)
        heldReceipts.append((request, try capacityBridge.reserveOpen(request)))
      }
    } catch { return check(false, "could not fill receipt table: \(error)") }
    let capacityAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: capacityBridge)
    capacityAdapter.bind(sink: sink)
    let refusedOpen = MobileRadioRequest.orderedOpen(id: 11000, groupId: 11001, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8)
    capacityAdapter.submit(request: refusedOpen)
    driver.onQueue {
      guard case .orderedOpenFailed(_, .notCreated)? = sink.completions[11000] else { return check(false, "capacity refusal") }
    }
    capacityAdapter.submit(request: .orderedRecoverOpen(id: 11002, groupId: 11001, openRequestId: 11000, peerId: "p"))
    driver.onQueue {
      check(sink.completions[11002] == sink.completions[11000], "lost capacity refusal must preserve exact failure")
    }
    guard let refusalReceipt = capacityBridge.openReceiptIdentity(groupID: 11001), let held = heldReceipts.first else { return check(false, "capacity receipt missing") }
    capacityBridge.cancelOpen(held.0, token: held.1)
    do {
      try capacityBridge.acknowledgeOpen(held.0, token: held.1)
      try capacityBridge.acknowledgeOpen(refusalReceipt.request, token: refusalReceipt.token)
      try capacityBridge.acknowledgeOpen(refusalReceipt.request, token: refusalReceipt.token)
    } catch { return check(false, "capacity acknowledgement: \(error)") }
    capacityAdapter.submit(request: refusedOpen)
    driver.onQueue {
      guard case let .failed(_, _, _, _, _, dispatched)? = sink.completions[11000], dispatched else { return check(false, "late refused submission must stay fenced") }
      check(creations == 0, "acknowledged refusal was admitted")
    }
    do {
      _ = try capacityBridge.reserveOpen(.init(requestID: 12000, groupID: 12000, peer: "p", fingerprint: 12000))
      for id in UInt64(12100)..<UInt64(12162) {
        let request = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: id, groupID: id, peer: "p", fingerprint: id)
        do { _ = try capacityBridge.reserveOpen(request); return check(false, "receipt table should be full") }
        catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionUnavailable {}
        guard let receipt = capacityBridge.openReceiptIdentity(groupID: id) else { return check(false, "bounded refusal receipt missing") }
        try capacityBridge.acknowledgeOpen(request, token: receipt.token)
      }
    } catch { return check(false, "could not exhaust refusal identities: \(error)") }
    capacityAdapter.submit(request: .orderedOpen(id: 13000, groupId: 13001, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8))
    driver.onQueue {
      guard case let .orderedOpenFailed(failure, .indeterminate(recoveryKey))? = sink.completions[13000], failure.dispatched, recoveryKey == 13001 else {
        return check(false, "untracked admission cannot be reported as not created")
      }
    }

    let partialSource = OwnedOrderedNotificationSource(queue: driver.workQueue, generation: "partial-recovery")
    let successfulSource = OwnedOrderedNotificationSource(queue: driver.workQueue, generation: "successful-recovery")
    let successfulBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: driver.workQueue, generation: successfulSource.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard successfulSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      return (successfulSource, group)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let successfulAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: successfulBridge)
    successfulAdapter.bind(sink: sink)
    successfulAdapter.submit(request: .orderedOpen(id: 920, groupId: 921, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8))
    driver.onQueue { guard case .orderedOpened? = sink.completions[920] else { return check(false, "successful open") } }
    successfulAdapter.submit(request: .orderedRecoverOpen(id: 922, groupId: 921, openRequestId: 920, peerId: "p"))
    driver.onQueue { check(sink.completions[920] == sink.completions[922], "lost success must replay exact descriptor") }
    successfulAdapter.submit(request: .orderedAcknowledgeOpen(id: 923, version: 1, groupId: 921, openRequestId: 920, peerId: "p"))
    successfulAdapter.submit(request: .orderedAcknowledgeOpen(id: 924, version: 1, groupId: 921, openRequestId: 920, peerId: "p"))
    driver.onQueue { check(sink.completions[923] == .unit && sink.completions[924] == .unit, "successful receipt ack and retry") }
    successfulAdapter.submit(request: .orderedOpen(id: 920, groupId: 921, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8))
    driver.onQueue { guard case .failed? = sink.completions[920] else { return check(false, "retired successful request was readmitted") } }

    let refusedBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      throw UnifiedBleRustOrderedGroupBridge.FactoryNoEffect(underlying: NSError(domain: "open-test", code: 42, userInfo: [NSLocalizedDescriptionKey: "original refusal"]))
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let refusedAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: refusedBridge)
    refusedAdapter.bind(sink: sink)
    refusedAdapter.submit(request: .orderedOpen(id: 930, groupId: 931, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8))
    driver.onQueue {
      guard case let .orderedOpenFailed(failure, .notCreated)? = sink.completions[930], failure.nativeDomain == "open-test", failure.nativeCode == 42 else { return check(false, "original refusal") }
    }
    refusedAdapter.submit(request: .orderedRecoverOpen(id: 932, groupId: 931, openRequestId: 930, peerId: "p"))
    driver.onQueue { check(sink.completions[930] == sink.completions[932], "lost failure must replay exact native cause") }

    let uncertainBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { _, _, _, _ in
      throw NSError(domain: "unknown-effect", code: 43)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let uncertainAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: uncertainBridge)
    uncertainAdapter.bind(sink: sink)
    uncertainAdapter.submit(request: .orderedOpen(id: 950, groupId: 951, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8))
    driver.onQueue {
      guard case let .orderedOpenFailed(failure, .indeterminate)? = sink.completions[950],
            failure.nativeDomain == "unknown-effect", failure.nativeCode == 43 else { return check(false, "uncertain open must retain native error") }
    }
    uncertainAdapter.submit(request: .orderedRecoverOpen(id: 952, groupId: 951, openRequestId: 950, peerId: "p"))
    uncertainAdapter.submit(request: .orderedAcknowledgeOpen(id: 953, version: 1, groupId: 951, openRequestId: 950, peerId: "p"))
    driver.onQueue {
      check(sink.completions[950] == sink.completions[952], "uncertain result must retain its original failure")
      guard case .failed? = sink.completions[953] else { return check(false, "uncertain receipt cannot be retired") }
    }

    var allowPartialRemoval = false
    let partialBridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: driver.workQueue, generation: partialSource.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard partialSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      partialSource.invalidate(.connectionLost)
      return (partialSource, group)
    }, validateSnapshot: { _, _ in true }, removeSourceGroup: { source, group in allowPartialRemoval && source.remove(group) }, wake: { _, _ in })
    let partialAdapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: partialBridge)
    partialAdapter.bind(sink: sink)
    partialAdapter.submit(request: .orderedOpen(id: 906, groupId: 907, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8))
    driver.onQueue {
      guard case let .orderedOpenFailed(_, .created(generation, members))? = sink.completions[906], generation == .opaque(value: "partial-recovery"), members.isEmpty else { return check(false, "partial open identity") }
    }
    partialAdapter.submit(request: .orderedRecoverOpen(id: 908, groupId: 907, openRequestId: 906, peerId: "p"))
    driver.onQueue {
      check(sink.completions[908] == sink.completions[906], "partial recovery must preserve the exact failure and descriptor")
      do {
        allowPartialRemoval = true
        guard let batch = try partialBridge.drain(groupID: 907) else { return check(false, "partial terminal missing") }
        try partialBridge.acknowledge(groupID: 907, batchID: batch.id)
        try partialBridge.seal(groupID: 907)
        try partialBridge.remove(groupID: 907, opaqueSourceGeneration: "partial-recovery")
        try partialBridge.acknowledgeRemoval(groupID: 907, opaqueSourceGeneration: "partial-recovery")
        guard case .completed = try partialBridge.fenceOpen(groupID: 907) else { return check(false, "open receipt lost on source removal") }
      } catch { return check(false, "partial cleanup failed: \(error)") }
    }
    guard let partialReceipt = partialBridge.openReceiptIdentity(groupID: 907) else { return check(false, "partial receipt missing") }
    do {
      try partialBridge.acknowledgeOpen(partialReceipt.request, token: partialReceipt.token)
      try partialBridge.acknowledgeOpen(partialReceipt.request, token: partialReceipt.token)
    } catch { return check(false, "partial receipt acknowledgement") }
  }

  func notificationRouteAdmissionReceiptChecks() {
    final class Sink: UnifiedBleRustAcknowledgedFailureSink {
      var completions: [UInt64: MobileRadioCompletion] = [:]
      var receiptStatus = "full"
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { completions[requestId] = completion; return "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String { receiptStatus }
    }
    let driver = ScriptedDriver(), sink = Sink(), adapter = UnifiedBleRustRadioAdapter(driver: driver)
    check(adapter.bind(sink: sink), "bind route admission sink")
    func instance(_ n: Int) -> MobileInstance {
      MobileInstance(peerId: "p", serviceUuid: "180D", serviceOccurrence: 0,
        characteristicUuid: "2A37", characteristicOccurrence: UInt64(n))
    }
    func wait(_ id: UInt64) -> MobileRadioCompletion? {
      let deadline = Date().addingTimeInterval(10)
      while Date() < deadline {
        if let result = driver.onQueue({ sink.completions[id] }) { return result }
        Thread.sleep(forTimeInterval: 0.001)
      }
      return nil
    }
    for n in 0..<64 {
      let id = UInt64(10000 + n)
      adapter.submit(request: .enableNotifications(id: id, instance: instance(n), epoch: 1, requested: nil, preferred: nil))
      check(wait(id) == .notifyEnabled(delivery: "unknown"), "route \(n) admitted")
    }
    adapter.submit(request: .enableNotifications(id: 10128, instance: instance(128), epoch: 1, requested: nil, preferred: nil))
    guard case .failed? = wait(10128) else { return check(false, "receipt exhaustion must refuse route before subscribe") }
    check(driver.onQueue { driver.subscriptionIdentifiers.count } == 64, "exhausted admission must not reach driver")
    let firstIdentifier = driver.onQueue { driver.subscriptionIdentifier(10000) }
    var captured = driver.onQueue { adapter.captureNotificationFailure(firstIdentifier) }
    check(captured != nil, "admitted route owns a receipt")
    adapter.submit(request: .disableNotifications(id: 10129, instance: instance(0), epoch: 1))
    check(wait(10129) == .unit, "retire first route")
    driver.onQueue {
      captured?(NSError(domain: "retired-route", code: 42))
      captured = nil
    }
    adapter.submit(request: .enableNotifications(id: 10130, instance: instance(128), epoch: 1, requested: nil, preferred: nil))
    guard case .failed? = wait(10130) else { return check(false, "unacknowledged retired receipt must retain admission capacity") }
    driver.onQueue { sink.receiptStatus = "accepted" }
    let deadline = Date().addingTimeInterval(5)
    while Date() < deadline && driver.onQueue({ adapter.hasPendingNotificationFailureRetention() }) { Thread.sleep(forTimeInterval: 0.01) }
    check(!driver.onQueue { adapter.hasPendingNotificationFailureRetention() }, "retired receipt must settle")
    driver.onQueue { driver.holdSubscribes = true }
    adapter.submit(request: .enableNotifications(id: 10131, instance: instance(128), epoch: 1, requested: nil, preferred: nil))
    let subscribeDeadline = Date().addingTimeInterval(5)
    while Date() < subscribeDeadline && !driver.onQueue({ driver.subscribeCompletions[driver.subscriptionIdentifier(10131)] != nil }) { Thread.sleep(forTimeInterval: 0.01) }
    driver.onQueue {
      let identifier = driver.subscriptionIdentifier(10131)
      check(driver.subscribeCompletions[identifier] != nil, "retry must reach driver after acknowledgment")
      var pendingCapture = adapter.captureNotificationFailure(identifier)
      check(pendingCapture != nil, "receipt must already exist while physical enable is pending")
      pendingCapture = nil
      driver.subscribeCompletions.removeValue(forKey: identifier)?(NSError(domain: "enable", code: 1))
      driver.holdSubscribes = false
    }
    guard case .failed? = wait(10131) else { return check(false, "scripted enable failure") }
    let failedIdentifier = driver.onQueue { driver.subscriptionIdentifier(10131) }
    driver.onQueue {
      let lateCapture = adapter.captureNotificationFailure(failedIdentifier)
      check(lateCapture != nil, "failed enable without authoritative release must retain its exact route for late callbacks")
    }
    adapter.submit(request: .enableNotifications(id: 10132, instance: instance(128), epoch: 1, requested: nil, preferred: nil))
    guard case let .failed(kind, _, _, _, _, dispatched)? = wait(10132) else {
      return check(false, "uncertain enable cleanup must retain its receipt capacity")
    }
    check(kind == "busy" && !dispatched, "capacity refusal must name the local cause and occur before dispatch")

    let cancelDriver = ScriptedDriver(), cancelSink = Sink(), cancelAdapter = UnifiedBleRustRadioAdapter(driver: cancelDriver)
    check(cancelAdapter.bind(sink: cancelSink), "bind cancellation sink")
    cancelDriver.onQueue { cancelDriver.holdSubscribes = true; cancelDriver.holdCancellation = true }
    cancelAdapter.submit(request: .enableNotifications(id: 10200, instance: instance(200), epoch: 9, requested: nil, preferred: nil))
    let cancelDeadline = Date().addingTimeInterval(10)
    while Date() < cancelDeadline && cancelDriver.onQueue({ cancelDriver.subscriptionIdentifiers.isEmpty }) { Thread.sleep(forTimeInterval: 0.001) }
    let cancelIdentifier = cancelDriver.onQueue { cancelDriver.subscriptionIdentifier(10200) }
    cancelAdapter.cancel(requestId: 10200)
    while Date() < cancelDeadline && cancelDriver.onQueue({ cancelDriver.cancellationCompletions.isEmpty }) { Thread.sleep(forTimeInterval: 0.001) }
    cancelDriver.onQueue {
      check(cancelAdapter.captureNotificationFailure(cancelIdentifier) != nil, "cancel must retain route until native cleanup answers")
      let completion = cancelDriver.cancellationCompletions.removeValue(forKey: cancelDriver.cancelled.last!)!
      completion(["state": "retryable", "failures": []])
      cancelDriver.subscribeCompletions.removeValue(forKey: cancelIdentifier)?(NSError(domain: "late-enable", code: 2))
      check(cancelAdapter.captureNotificationFailure(cancelIdentifier) != nil, "failed cleanup and late enable callback must retain the original route")
    }
  }

  func directFifthFailureReceiptCheck() {
    final class Sink: UnifiedBleRustAcknowledgedFailureSink {
      var completion: MobileRadioCompletion?
      var blocked = true
      var accepted: [Int64] = []
      var coalescedOverflowDetails: [String] = []
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { self.completion = completion; return "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String {
        if case let .dropped(ingressClass, detail) = ingress, ingressClass == "notification" {
          coalescedOverflowDetails.append(detail)
        }
        return blocked ? "full" : "accepted"
      }
      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String {
        if blocked { return "full" }
        if let code = failure.nativeCode { accepted.append(code) }
        return "accepted"
      }
    }
    let driver = ScriptedDriver(), sink = Sink(), adapter = UnifiedBleRustRadioAdapter(driver: driver)
    check(adapter.bind(sink: sink), "bind fifth-failure sink")
    let instance = MobileInstance(peerId: "p", serviceUuid: "180D", serviceOccurrence: 0,
      characteristicUuid: "2A37", characteristicOccurrence: 0)
    adapter.submit(request: .enableNotifications(id: 10300, instance: instance, epoch: 1, requested: nil, preferred: nil))
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline && driver.onQueue({ sink.completion == nil }) { Thread.sleep(forTimeInterval: 0.001) }
    check(driver.onQueue { sink.completion } == .notifyEnabled(delivery: "unknown"), "enable fifth-failure route")
    driver.onQueue {
      let identifier = driver.subscriptionIdentifier(10300)
      for code in 1...7 { adapter.protocolRadioDidFailNotification(identifier, error: NSError(domain: "fifth", code: code)) }
      sink.blocked = false
    }
    while Date() < deadline && driver.onQueue({ adapter.hasPendingNotificationFailureRetention() }) { Thread.sleep(forTimeInterval: 0.01) }
    check(driver.onQueue { sink.accepted } == [1, 2, 3, 4, 5], "five actual failures must reach the original sink in FIFO order")
    check(driver.onQueue { sink.coalescedOverflowDetails.count } == 1,
      "later failures must remain as one durable coalesced overflow record")
    check(driver.onQueue { sink.coalescedOverflowDetails[0].contains("count=2")
      && sink.coalescedOverflowDetails[0].contains("first=fifth (6)")
      && sink.coalescedOverflowDetails[0].contains("last=fifth (7)") },
      "coalesced overflow must truthfully retain its count and first/last native evidence")
  }

  func durableFailureOverflowSpoolCheck() {
    final class Sink: UnifiedBleRustAcknowledgedFailureSink {
      var completion: MobileRadioCompletion?
      var blocked = true
      var accepted: [Int64] = []
      var coalescedOverflowDetails: [String] = []
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { self.completion = completion; return "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String {
        if case let .dropped(ingressClass, detail) = ingress, ingressClass == "notification" {
          coalescedOverflowDetails.append(detail)
        }
        return "accepted"
      }
      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String {
        if blocked { return "full" }
        if let code = failure.nativeCode { accepted.append(code) }
        return "accepted"
      }
    }
    final class Spool: UnifiedBleRustNotificationFailureSpool {
      var records: [(UInt64, MobileInstance, UInt64, MobilePlatformFailure)] = []
      var status = "committed"
      var resume = "100"
      // A V4 acknowledgement records a capability before Swift may attempt
      // retirement.  Keeping this separate from `records` makes the fake
      // exercise the same crash-safe two-phase ownership boundary as the
      // sidecar: an ACK never removes a source fact.
      var retirementCapabilities: [UInt64: (String, UInt64, MobileInstance, UInt64, MobilePlatformFailure)] = [:]
      var loseFirstConfirmationReply = true
      func appendNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String {
        guard status == "committed" else { return status }
        records.append((sequence, instance, epoch, failure))
        return "committed"
      }
      func notificationFailureSpoolResumeSequenceV1(instance: MobileInstance, epoch: UInt64) -> String { resume }
      func notificationFailureSpoolPageV1(instance: MobileInstance, epoch: UInt64,
                                          afterSequence: UInt64, maxItems: UInt32) -> String {
        let page = records.filter { $0.0 > afterSequence }.prefix(Int(maxItems)).map { record -> [String: Any] in
          [
            "epoch": String(record.2),
            "sequence": String(record.0),
            "failure": [
              "instance": [
                "peerId": record.1.peerId,
                "serviceUuid": record.1.serviceUuid,
                "serviceOccurrence": record.1.serviceOccurrence,
                "characteristicUuid": record.1.characteristicUuid,
                "characteristicOccurrence": record.1.characteristicOccurrence,
              ],
              "kind": record.3.kind,
              "gattStatus": jsonNullable(record.3.gattStatus),
              "nativeDomain": jsonNullable(record.3.nativeDomain),
              "nativeCode": jsonNullable(record.3.nativeCode),
              "detail": record.3.detail,
              "dispatched": record.3.dispatched,
            ],
          ]
        }
        let body: [String: Any] = ["version": 1, "records": page]
        return String(decoding: try! JSONSerialization.data(withJSONObject: body, options: [.sortedKeys]), as: UTF8.self)
      }
      func acknowledgeNotificationFailureSpoolV1(sequence: UInt64, instance: MobileInstance,
                                                 epoch: UInt64, failure: MobilePlatformFailure) -> String {
        guard let index = records.firstIndex(where: { record in
          record.0 == sequence && record.1 == instance && record.2 == epoch && record.3 == failure
        }) else { return "invalid" }
        records.remove(at: index)
        return "acknowledged"
      }
      func notificationFailureSpoolResumeGlobalV2() -> String {
        let highest = records.map { $0.0 }.max() ?? 0
        return "{\"version\":2,\"highestCaptureOrdinal\":\"\(highest)\"}"
      }
      func notificationFailureSpoolPageGlobalV2(afterCaptureOrdinal: UInt64, maxItems: UInt32) -> String {
        let page = records.filter { $0.0 > afterCaptureOrdinal }.prefix(Int(maxItems)).map { record -> [String: Any] in
          [
            "captureOrdinal": String(record.0), "route": record.1.peerId, "epoch": String(record.2),
            "sourceSequence": String(record.0),
            "failure": [
              "instance": ["peerId": record.1.peerId, "serviceUuid": record.1.serviceUuid,
                           "serviceOccurrence": record.1.serviceOccurrence, "characteristicUuid": record.1.characteristicUuid,
                           "characteristicOccurrence": record.1.characteristicOccurrence],
              "kind": record.3.kind, "gattStatus": jsonNullable(record.3.gattStatus),
              "nativeDomain": jsonNullable(record.3.nativeDomain), "nativeCode": jsonNullable(record.3.nativeCode),
              "detail": record.3.detail, "dispatched": record.3.dispatched,
            ],
          ]
        }
        return String(decoding: try! JSONSerialization.data(withJSONObject: ["version": 2, "records": page], options: [.sortedKeys]), as: UTF8.self)
      }
      func acknowledgeNotificationFailureSpoolGlobalV2(captureOrdinal: UInt64) -> String {
        // V2 lacks the exact source identity and cannot retire a V4 record.
        "unsupported"
      }
      func acknowledgeNotificationFailureSpoolGlobalV3(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                        instance: MobileInstance, epoch: UInt64,
                                                        failure: MobilePlatformFailure) -> String { "unsupported" }
      func retireNotificationFailureSpoolAcknowledgementV3(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                           instance: MobileInstance, epoch: UInt64,
                                                           failure: MobilePlatformFailure) -> String { "unsupported" }
      func acknowledgeNotificationFailureSpoolGlobalV4(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                        instance: MobileInstance, epoch: UInt64,
                                                        failure: MobilePlatformFailure) -> String {
        guard let record = records.first,
              record.0 == captureOrdinal, record.0 == sourceSequence,
              record.1 == instance, record.2 == epoch, record.3 == failure else { return "storage.identity" }
        let capability: String
        if let retained = retirementCapabilities[captureOrdinal] {
          guard retained.1 == sourceSequence, retained.2 == instance,
                retained.3 == epoch, retained.4 == failure else { return "storage.identity" }
          capability = retained.0
        } else {
          capability = "durable-v4-\(captureOrdinal)-\(sourceSequence)"
          retirementCapabilities[captureOrdinal] = (capability, sourceSequence, instance, epoch, failure)
        }
        return jsonText(["version": 4, "status": "acknowledged", "retirementCapability": capability])
      }
      func confirmNotificationFailureSpoolAcknowledgementRetirementV4(captureOrdinal: UInt64,
                                                                        sourceSequence: UInt64,
                                                                        instance: MobileInstance,
                                                                        epoch: UInt64,
                                                                        failure: MobilePlatformFailure,
                                                                        retirementCapability: String) -> String {
        guard let retained = retirementCapabilities[captureOrdinal],
              retained.0 == retirementCapability, retained.1 == sourceSequence,
              retained.2 == instance, retained.3 == epoch, retained.4 == failure,
              let record = records.first, record.0 == captureOrdinal,
              record.0 == sourceSequence, record.1 == instance,
              record.2 == epoch, record.3 == failure else { return "storage.identity" }
        if loseFirstConfirmationReply {
          loseFirstConfirmationReply = false
          return "ambiguous"
        }
        retirementCapabilities.removeValue(forKey: captureOrdinal)
        records.removeFirst()
        return "confirmed"
      }
      func notificationFailureSpoolPendingRetirementsV4(maxItems: UInt32) -> String {
        let pending = records.compactMap { record -> [String: Any]? in
          guard let retained = retirementCapabilities[record.0] else { return nil }
          return [
            "captureOrdinal": String(record.0), "route": record.1.peerId,
            "epoch": String(record.2), "sourceSequence": String(record.0),
            "retirementCapability": retained.0,
            "failure": [
              "instance": ["peerId": record.1.peerId, "serviceUuid": record.1.serviceUuid,
                           "serviceOccurrence": record.1.serviceOccurrence, "characteristicUuid": record.1.characteristicUuid,
                           "characteristicOccurrence": record.1.characteristicOccurrence],
              "kind": record.3.kind, "gattStatus": jsonNullable(record.3.gattStatus),
              "nativeDomain": jsonNullable(record.3.nativeDomain), "nativeCode": jsonNullable(record.3.nativeCode),
              "detail": record.3.detail, "dispatched": record.3.dispatched,
            ],
          ]
        }.prefix(Int(maxItems))
        return jsonText(["version": 4, "records": Array(pending)])
      }
    }
    let driver = ScriptedDriver(), sink = Sink(), spool = Spool()
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, notificationFailureSpool: spool)
    check(adapter.bind(sink: sink), "bind durable overflow spool sink")
    let instance = MobileInstance(peerId: "spooled", serviceUuid: "180D", serviceOccurrence: 0,
      characteristicUuid: "2A37", characteristicOccurrence: 0)
    adapter.submit(request: .enableNotifications(id: 10350, instance: instance, epoch: 51, requested: nil, preferred: nil))
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline && driver.onQueue({ sink.completion == nil }) { Thread.sleep(forTimeInterval: 0.001) }
    driver.onQueue {
      let identifier = driver.subscriptionIdentifier(10350)
      for code in 1...7 { adapter.protocolRadioDidFailNotification(identifier, error: NSError(domain: "spooled", code: code)) }
      sink.blocked = false
    }
    while Date() < deadline && driver.onQueue({ adapter.hasPendingNotificationFailureRetention() }) { Thread.sleep(forTimeInterval: 0.01) }
    driver.onQueue {
      check(sink.accepted == [1, 2, 3, 4, 5, 6, 7],
        "in-memory prefix must reach Rust before the exact durable spool suffix")
      check(spool.records.isEmpty,
        "each exact durable record must be acknowledged only after V1 ingress accepts it")
    }
    let fallbackDriver = ScriptedDriver(), fallbackSink = Sink()
    let fallbackAdapter = UnifiedBleRustRadioAdapter(driver: fallbackDriver, notificationFailureSpool: spool)
    check(fallbackAdapter.bind(sink: fallbackSink), "bind fallback spool sink")
    fallbackAdapter.submit(request: .enableNotifications(id: 10351, instance: instance, epoch: 52, requested: nil, preferred: nil))
    while Date() < deadline && fallbackDriver.onQueue({ fallbackSink.completion == nil }) { Thread.sleep(forTimeInterval: 0.001) }
    fallbackDriver.onQueue {
      spool.status = "full"
      let identifier = fallbackDriver.subscriptionIdentifier(10351)
      for code in 11...16 { fallbackAdapter.protocolRadioDidFailNotification(identifier, error: NSError(domain: "fallback", code: code)) }
      spool.status = "committed"
      fallbackSink.blocked = false
    }
    while Date() < deadline && fallbackDriver.onQueue({ fallbackAdapter.hasPendingNotificationFailureRetention() }) { Thread.sleep(forTimeInterval: 0.01) }
    fallbackDriver.onQueue {
      check(!spool.records.contains { $0.3.nativeCode == 16 },
        "a later failure must remain behind an earlier non-durable fallback, not leap into the spool")
      check(fallbackSink.accepted == [11, 12, 13, 14, 15]
        && fallbackSink.coalescedOverflowDetails.count == 1
        && fallbackSink.coalescedOverflowDetails[0].contains("first=fallback (16)"),
        "non-durable overflow must remain retry-owned until the V1 prefix settles")
    }
  }

  func globalDurableFailureSpoolInterleavingCheck() {
    final class StrictSink: UnifiedBleRustAcknowledgedFailureSink {
      var completions: [UInt64: MobileRadioCompletion] = [:]
      var expectedSequence: UInt64 = 1
      var accepted: [(UInt64, String, Int64)] = []
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        completions[requestId] = completion
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64,
                                      failure: MobilePlatformFailure) -> String {
        guard sequence == expectedSequence, let code = failure.nativeCode else { return "invalid" }
        expectedSequence += 1
        accepted.append((sequence, instance.peerId, code))
        return "accepted"
      }
    }
    final class GlobalSpool: UnifiedBleRustNotificationFailureSpool {
      struct Record {
        let captureOrdinal: UInt64
        let instance: MobileInstance
        let epoch: UInt64
        let sourceSequence: UInt64
        let failure: MobilePlatformFailure
      }
      var records: [Record]
      // V4 keeps the durable receipt until a separate capability-bound final
      // confirmation. The first confirmation reply is deliberately lost so
      // the adapter must retry the same head rather than advance to B7.
      var lostConfirmationReply = true
      var retirementCapabilities: [UInt64: (String, Record)] = [:]
      init(_ records: [Record]) { self.records = records }
      func appendNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64,
                                       failure: MobilePlatformFailure) -> String { "unsupported" }
      func notificationFailureSpoolResumeSequenceV1(instance: MobileInstance, epoch: UInt64) -> String { "unsupported" }
      func notificationFailureSpoolPageV1(instance: MobileInstance, epoch: UInt64,
                                          afterSequence: UInt64, maxItems: UInt32) -> String { "unsupported" }
      func acknowledgeNotificationFailureSpoolV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64,
                                                 failure: MobilePlatformFailure) -> String { "unsupported" }
      func notificationFailureSpoolResumeGlobalV2() -> String {
        let highest = records.map(\.captureOrdinal).max() ?? 0
        return "{\"version\":2,\"highestCaptureOrdinal\":\"\(highest)\"}"
      }
      func notificationFailureSpoolPageGlobalV2(afterCaptureOrdinal: UInt64, maxItems: UInt32) -> String {
        let page = records.filter { $0.captureOrdinal > afterCaptureOrdinal }.prefix(Int(maxItems)).map { record -> [String: Any] in
          [
            "captureOrdinal": String(record.captureOrdinal),
            "route": record.instance.peerId,
            "epoch": String(record.epoch),
            "sourceSequence": String(record.sourceSequence),
            "failure": [
              "instance": ["peerId": record.instance.peerId, "serviceUuid": record.instance.serviceUuid,
                           "serviceOccurrence": record.instance.serviceOccurrence,
                           "characteristicUuid": record.instance.characteristicUuid,
                           "characteristicOccurrence": record.instance.characteristicOccurrence],
              "kind": record.failure.kind, "gattStatus": jsonNullable(record.failure.gattStatus),
              "nativeDomain": jsonNullable(record.failure.nativeDomain),
              "nativeCode": jsonNullable(record.failure.nativeCode), "detail": record.failure.detail,
              "dispatched": record.failure.dispatched,
            ],
          ]
        }
        let body: [String: Any] = ["version": 2, "records": page]
        return String(decoding: try! JSONSerialization.data(withJSONObject: body, options: [.sortedKeys]), as: UTF8.self)
      }
      func acknowledgeNotificationFailureSpoolGlobalV4(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                        instance: MobileInstance, epoch: UInt64,
                                                        failure: MobilePlatformFailure) -> String {
        guard let record = records.first,
              record.captureOrdinal == captureOrdinal,
              record.sourceSequence == sourceSequence,
              record.instance == instance,
              record.epoch == epoch,
              record.failure == failure else { return "storage.identity" }
        let capability: String
        if let retained = retirementCapabilities[captureOrdinal] {
          guard retained.1.captureOrdinal == captureOrdinal,
                retained.1.sourceSequence == sourceSequence,
                retained.1.instance == instance, retained.1.epoch == epoch,
                retained.1.failure == failure else { return "storage.identity" }
          capability = retained.0
        } else {
          capability = "v4-\(captureOrdinal)-\(sourceSequence)-capability"
          retirementCapabilities[captureOrdinal] = (capability, record)
        }
        return jsonText(["version": 4, "status": "acknowledged",
                         "retirementCapability": capability])
      }
      func confirmNotificationFailureSpoolAcknowledgementRetirementV4(captureOrdinal: UInt64,
                                                                       sourceSequence: UInt64,
                                                                       instance: MobileInstance,
                                                                       epoch: UInt64,
                                                                       failure: MobilePlatformFailure,
                                                                       retirementCapability: String) -> String {
        guard let retained = retirementCapabilities[captureOrdinal],
              retained.0 == retirementCapability,
              retained.1.captureOrdinal == captureOrdinal,
              retained.1.sourceSequence == sourceSequence,
              retained.1.instance == instance,
              retained.1.epoch == epoch,
              retained.1.failure == failure else { return "storage.identity" }
        guard let record = records.first,
              record.captureOrdinal == captureOrdinal,
              record.sourceSequence == sourceSequence,
              record.instance == instance,
              record.epoch == epoch,
              record.failure == failure else { return "storage.identity" }
        if lostConfirmationReply {
          lostConfirmationReply = false
          return "ambiguous"
        }
        retirementCapabilities.removeValue(forKey: captureOrdinal)
        records.removeFirst()
        return "confirmed"
      }
      func notificationFailureSpoolPendingRetirementsV4(maxItems: UInt32) -> String {
        let rows = records.compactMap { record -> [String: Any]? in
          guard let retained = retirementCapabilities[record.captureOrdinal] else { return nil }
          return [
            "captureOrdinal": String(record.captureOrdinal), "route": record.instance.peerId,
            "epoch": String(record.epoch), "sourceSequence": String(record.sourceSequence),
            "retirementCapability": retained.0,
            "failure": [
              "instance": ["peerId": record.instance.peerId, "serviceUuid": record.instance.serviceUuid,
                           "serviceOccurrence": record.instance.serviceOccurrence,
                           "characteristicUuid": record.instance.characteristicUuid,
                           "characteristicOccurrence": record.instance.characteristicOccurrence],
              "kind": record.failure.kind, "gattStatus": jsonNullable(record.failure.gattStatus),
              "nativeDomain": jsonNullable(record.failure.nativeDomain),
              "nativeCode": jsonNullable(record.failure.nativeCode), "detail": record.failure.detail,
              "dispatched": record.failure.dispatched,
            ],
          ]
        }.prefix(Int(maxItems))
        return jsonText(["version": 4, "records": Array(rows)])
      }
    }
    let a = MobileInstance(peerId: "A", serviceUuid: "180D", serviceOccurrence: 0,
      characteristicUuid: "2A37", characteristicOccurrence: 0)
    let b = MobileInstance(peerId: "B", serviceUuid: "180D", serviceOccurrence: 0,
      characteristicUuid: "2A37", characteristicOccurrence: 0)
    let spool = GlobalSpool([
      .init(captureOrdinal: 6, instance: a, epoch: 1, sourceSequence: 6,
            failure: MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: "A", nativeCode: 6, detail: "A6", dispatched: true)),
      .init(captureOrdinal: 7, instance: b, epoch: 1, sourceSequence: 7,
            failure: MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: "B", nativeCode: 7, detail: "B7", dispatched: true)),
    ])
    let driver = ScriptedDriver(), sink = StrictSink()
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, notificationFailureSpool: spool)
    check(adapter.bind(sink: sink), "bind strict global spool sink")
    adapter.submit(request: .enableNotifications(id: 10901, instance: a, epoch: 1, requested: nil, preferred: nil))
    adapter.submit(request: .enableNotifications(id: 10902, instance: b, epoch: 1, requested: nil, preferred: nil))
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline && driver.onQueue({ !spool.records.isEmpty }) { Thread.sleep(forTimeInterval: 0.01) }
    driver.onQueue {
      check(sink.accepted.count == 2
          && sink.accepted[0].0 == 1 && sink.accepted[0].1 == "A" && sink.accepted[0].2 == 6
          && sink.accepted[1].0 == 2 && sink.accepted[1].1 == "B" && sink.accepted[1].2 == 7,
        "global capture FIFO must replay A6/B7 with fresh contiguous V1 delivery sequences")
      check(spool.records.isEmpty, "a lost V4 final-confirmation reply must retry the exact global head before B7")
    }
  }

  func v4RestartReplaysPendingReceiptBeforeFinalConfirmationCheck() {
    final class Sink: UnifiedBleRustAcknowledgedFailureSink {
      var completion: MobileRadioCompletion?
      var accepted: [Int64] = []
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        self.completion = completion
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64,
                                      failure: MobilePlatformFailure) -> String {
        guard let code = failure.nativeCode else { return "invalid" }
        accepted.append(code)
        return "accepted"
      }
    }
    final class RestartSpool: UnifiedBleRustNotificationFailureSpool {
      let record: (capture: UInt64, instance: MobileInstance, epoch: UInt64,
                   source: UInt64, failure: MobilePlatformFailure, capability: String)
      var pending = true
      var confirmationAttempts = 0
      var loseFirstConfirmationReply = true

      init(record: (capture: UInt64, instance: MobileInstance, epoch: UInt64,
                    source: UInt64, failure: MobilePlatformFailure, capability: String)) {
        self.record = record
      }

      func appendNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64,
                                       failure: MobilePlatformFailure) -> String { "unsupported" }
      func notificationFailureSpoolResumeGlobalV2() -> String {
        jsonText(["version": 2, "highestCaptureOrdinal": String(record.capture)])
      }
      func notificationFailureSpoolPageGlobalV2(afterCaptureOrdinal: UInt64, maxItems: UInt32) -> String {
        jsonText(["version": 2, "records": []])
      }
      func notificationFailureSpoolPendingRetirementsV4(maxItems: UInt32) -> String {
        guard pending else { return jsonText(["version": 4, "records": []]) }
        return jsonText(["version": 4, "records": [[
          "captureOrdinal": String(record.capture), "route": record.instance.peerId,
          "epoch": String(record.epoch), "sourceSequence": String(record.source),
          "retirementCapability": record.capability,
          "failure": [
            "instance": ["peerId": record.instance.peerId, "serviceUuid": record.instance.serviceUuid,
                         "serviceOccurrence": record.instance.serviceOccurrence,
                         "characteristicUuid": record.instance.characteristicUuid,
                         "characteristicOccurrence": record.instance.characteristicOccurrence],
            "kind": record.failure.kind, "gattStatus": jsonNullable(record.failure.gattStatus),
            "nativeDomain": jsonNullable(record.failure.nativeDomain),
            "nativeCode": jsonNullable(record.failure.nativeCode), "detail": record.failure.detail,
            "dispatched": record.failure.dispatched,
          ],
        ]]])
      }
      func confirmNotificationFailureSpoolAcknowledgementRetirementV4(captureOrdinal: UInt64,
                                                                       sourceSequence: UInt64,
                                                                       instance: MobileInstance,
                                                                       epoch: UInt64,
                                                                       failure: MobilePlatformFailure,
                                                                       retirementCapability: String) -> String {
        guard pending, captureOrdinal == record.capture, sourceSequence == record.source,
              instance == record.instance, epoch == record.epoch, failure == record.failure,
              retirementCapability == record.capability else { return "storage.identity" }
        confirmationAttempts += 1
        if loseFirstConfirmationReply {
          loseFirstConfirmationReply = false
          return "ambiguous"
        }
        pending = false
        return "confirmed"
      }
    }
    let instance = MobileInstance(peerId: "restart", serviceUuid: "180D", serviceOccurrence: 0,
      characteristicUuid: "2A37", characteristicOccurrence: 0)
    let record = (capture: UInt64(41), instance: instance, epoch: UInt64(9), source: UInt64(7),
      failure: MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: "restart",
        nativeCode: 41, detail: "ack committed before process exit", dispatched: true), capability: "restart-v4-capability")
    let driver = ScriptedDriver(), sink = Sink(), spool = RestartSpool(record: record)
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, notificationFailureSpool: spool)
    check(adapter.bind(sink: sink), "bind restart spool sink")
    adapter.submit(request: .enableNotifications(id: 10950, instance: instance, epoch: 10, requested: nil, preferred: nil))
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline && driver.onQueue({ sink.completion == nil || adapter.hasPendingNotificationFailureRetention() }) {
      Thread.sleep(forTimeInterval: 0.001)
    }
    driver.onQueue {
      check(sink.accepted == [41],
        "a V4 ACK committed before process exit must be redelivered to the fresh sink before retirement")
      check(spool.confirmationAttempts == 2 && !spool.pending,
        "a lost final-confirmation reply must retry after fresh sink acceptance without duplicate ingress")
      check(!adapter.hasPendingNotificationFailureRetention(),
        "the V4 restart receipt must fully settle after its confirmation retry")
    }
  }

  func notificationFailureSpoolReconfigurationFenceCheck() {
    final class Sink: UnifiedBleRustRadioSink {
      var completion: MobileRadioCompletion?
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        self.completion = completion
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let driver = ScriptedDriver(), sink = Sink()
    let adapter = UnifiedBleRustRadioAdapter(driver: driver)
    check(adapter.bind(sink: sink), "bind spool reconfiguration fence sink")
    let initial = adapter.withNotificationFailureSpoolReconfiguration { ("configured-A", true) }
    check(initial.admitted && initial.result == "configured-A", "a clean adapter must admit initial spool A configuration")
    let instance = MobileInstance(peerId: "spool-fence", serviceUuid: "180D", serviceOccurrence: 0,
      characteristicUuid: "2A37", characteristicOccurrence: 0)
    adapter.submit(request: .enableNotifications(id: 10991, instance: instance, epoch: 1, requested: nil, preferred: nil))
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline && driver.onQueue({ sink.completion == nil }) { Thread.sleep(forTimeInterval: 0.001) }
    check(!adapter.withNotificationFailureSpoolReconfiguration { ("configured", true) }.admitted,
      "an active notification route must fence spool A to B reconfiguration")
    driver.onQueue {
      let identifier = driver.subscriptionIdentifier(10991)
      let capture = adapter.captureNotificationFailure(identifier)
      check(capture != nil, "active route must retain a callback before reconfiguration")
      capture?(NSError(domain: "spool-fence", code: 1))
    }
    check(!adapter.withNotificationFailureSpoolReconfiguration { ("configured", true) }.admitted,
      "an in-memory callback receipt must fence spool A to B reconfiguration")
  }

  func fifthFailureRetirementRetryCheck() {
    final class Sink: UnifiedBleRustAcknowledgedFailureSink {
      var completions: [UInt64: MobileRadioCompletion] = [:]
      var mode = "full"
      var attempts: [(UInt64, MobileInstance, UInt64, String?, Int64)] = []
      var admitted: [UInt64: (MobileInstance, UInt64, String?, Int64)] = [:]
      var coalescedOverflowDetails: [String] = []
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        completions[requestId] = completion
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String {
        if case let .dropped(ingressClass, detail) = ingress, ingressClass == "notification" {
          coalescedOverflowDetails.append(detail)
        }
        return "accepted"
      }
      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String {
        guard let code = failure.nativeCode else { return "invalid" }
        attempts.append((sequence, instance, epoch, failure.nativeDomain, code))
        if mode == "full" { return "full" }
        if let prior = admitted[sequence] {
          check(prior.0 == instance && prior.1 == epoch && prior.2 == failure.nativeDomain && prior.3 == code,
            "retry must preserve receipt identity and failure")
          return "accepted"
        }
        admitted[sequence] = (instance, epoch, failure.nativeDomain, code)
        if mode == "ambiguous" { mode = "accepted"; return "ambiguous" }
        return "accepted"
      }
    }
    let driver = ScriptedDriver(), sink = Sink(), adapter = UnifiedBleRustRadioAdapter(driver: driver)
    check(adapter.bind(sink: sink), "bind retirement retry sink")
    let instance = MobileInstance(peerId: "p", serviceUuid: "180D", serviceOccurrence: 0,
      characteristicUuid: "2A37", characteristicOccurrence: 0)
    func wait(_ id: UInt64) -> MobileRadioCompletion? {
      let deadline = Date().addingTimeInterval(10)
      while Date() < deadline {
        if let completion = driver.onQueue({ sink.completions[id] }) { return completion }
        Thread.sleep(forTimeInterval: 0.001)
      }
      return nil
    }
    adapter.submit(request: .enableNotifications(id: 10400, instance: instance, epoch: 11, requested: nil, preferred: nil))
    check(wait(10400) == .notifyEnabled(delivery: "unknown"), "enable original route")
    driver.onQueue {
      let identifier = driver.subscriptionIdentifier(10400)
      for code in 1...7 { adapter.protocolRadioDidFailNotification(identifier, error: NSError(domain: "original", code: code)) }
    }
    adapter.submit(request: .disableNotifications(id: 10401, instance: instance, epoch: 11))
    check(wait(10401) == .unit, "retire original route while sink is blocked")
    adapter.submit(request: .enableNotifications(id: 10402, instance: instance, epoch: 12, requested: nil, preferred: nil))
    check(wait(10402) == .notifyEnabled(delivery: "unknown"), "enable replacement route")
    driver.onQueue {
      adapter.protocolRadioDidFailNotification(driver.subscriptionIdentifier(10402), error: NSError(domain: "replacement", code: 6))
      check(sink.attempts.allSatisfy { $0.1 == instance && $0.2 == 11 && $0.3 == "original" && $0.4 == 1 },
        "a blocked receipt must stop later routes overtaking")
      sink.mode = "ambiguous"
    }
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline && driver.onQueue({ adapter.hasPendingNotificationFailureRetention() }) { Thread.sleep(forTimeInterval: 0.01) }
    driver.onQueue {
      check(!adapter.hasPendingNotificationFailureRetention(), "retired and replacement receipts must settle")
      let ordered = sink.admitted.keys.sorted().compactMap { sink.admitted[$0] }
      check(ordered.map { $0.3 } == [1, 2, 3, 4, 5, 6], "all old-route failures must precede replacement")
      check(ordered.allSatisfy { $0.0 == instance }, "receipts must retain their original instance")
      check(ordered.map { $0.1 } == [11, 11, 11, 11, 11, 12], "receipts must retain their original epochs")
      check(ordered.map { $0.2 } == ["original", "original", "original", "original", "original", "replacement"],
        "receipts must retain their original domains")
      check(sink.attempts.filter { $0.0 == 1 }.count >= 3, "ambiguous ACK must retry the original sequence")
      check(sink.coalescedOverflowDetails.count == 1
        && sink.coalescedOverflowDetails[0].contains("count=2")
        && sink.coalescedOverflowDetails[0].contains("first=original (6)")
        && sink.coalescedOverflowDetails[0].contains("last=original (7)"),
        "retirement must retain and eventually deliver coalesced saturation evidence after the original FIFO")
    }
  }

  func acknowledgedFailureReceiptChecks() {
    final class Sink: UnifiedBleRustAcknowledgedFailureSink {
      var completions: [UInt64: MobileRadioCompletion] = [:]
      var attempts: [(UInt64, MobileInstance, UInt64, MobilePlatformFailure)] = []
      var admitted: [UInt64: MobilePlatformFailure] = [:]
      var mode = "unavailable"
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { completions[requestId] = completion; return "delivered" }
      func ingest(ingress: MobileRadioIngress) -> String { preconditionFailure("failure used legacy ingress") }
      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { mode != "unavailable" && offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String {
        attempts.append((sequence, instance, epoch, failure))
        if mode == "full" || mode == "closed" { return mode }
        if let prior = admitted[sequence] { precondition(prior == failure); return "accepted" }
        admitted[sequence] = failure
        if mode == "ambiguous" { mode = "accepted"; return "ambiguous" }
        return "accepted"
      }
    }
    let driver = ScriptedDriver(), sink = Sink(), adapter = UnifiedBleRustRadioAdapter(driver: driver)
    check(adapter.bind(sink: sink), "bind acknowledged sink")
    let instance = MobileInstance(peerId: "p", serviceUuid: "180D", serviceOccurrence: 0, characteristicUuid: "2A37", characteristicOccurrence: 0)
    adapter.submit(request: .enableNotifications(id: 9901, instance: instance, epoch: 71, requested: nil, preferred: nil))
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline && driver.onQueue({ sink.completions[9901] == nil }) { Thread.sleep(forTimeInterval: 0.01) }
    check(driver.onQueue { sink.completions[9901] } == .notifyEnabled(delivery: "unknown"), "enable receipt test")
    let identifier = driver.onQueue { driver.subscriptionIdentifier(9901) }
    var captures = driver.onQueue { [adapter.captureNotificationFailure(identifier)!, adapter.captureNotificationFailure(identifier)!] }
    adapter.submit(request: .disableNotifications(id: 9902, instance: instance, epoch: 71))
    let disableDeadline = Date().addingTimeInterval(10)
    while Date() < disableDeadline && driver.onQueue({ sink.completions[9902] == nil }) { Thread.sleep(forTimeInterval: 0.01) }
    check(driver.onQueue { sink.completions[9902] } == .unit, "route retired before delayed callbacks")
    driver.onQueue {
      let first = captures.removeFirst()
      let second = captures.removeFirst()
      first(NSError(domain: "original", code: 42, userInfo: [NSLocalizedDescriptionKey: "first"]))
      second(NSError(domain: "original", code: 43, userInfo: [NSLocalizedDescriptionKey: "second"]))
      check(sink.attempts.isEmpty && adapter.hasPendingNotificationFailureRetention(), "unadvertised ingress must retain failures without calling it")
      sink.mode = "full"
    }
    Thread.sleep(forTimeInterval: 0.4)
    driver.onQueue {
      check(!sink.attempts.isEmpty && sink.attempts.allSatisfy { $0.0 == 1 }, "full must serialize later receipts")
      sink.mode = "closed"
    }
    Thread.sleep(forTimeInterval: 0.7)
    driver.onQueue {
      check(adapter.hasPendingNotificationFailureRetention(), "closed must retain receipt")
      check(sink.attempts.allSatisfy { $0.0 == 1 }, "closed must not admit later receipts")
      sink.mode = "ambiguous"
    }
    let recovery = Date().addingTimeInterval(6)
    while Date() < recovery && driver.onQueue({ adapter.hasPendingNotificationFailureRetention() }) { Thread.sleep(forTimeInterval: 0.01) }
    driver.onQueue {
      check(!adapter.hasPendingNotificationFailureRetention(), "ambiguous reply must reconcile")
      check(sink.admitted.count == 2 && sink.admitted[1]?.nativeCode == 42 && sink.admitted[2]?.nativeCode == 43, "exact failures accepted once in order")
      check(sink.attempts.allSatisfy { $0.1 == instance && $0.2 == 71 && $0.3.nativeDomain == "original" }, "original instance epoch and cause retained")
      check(sink.attempts.filter { $0.0 == 1 }.count >= 3, "ambiguous acknowledgment must replay exact receipt")
    }
  }

  func notificationFailureRetentionChecks() {
    final class Sink: UnifiedBleRustAcknowledgedFailureSink {
      var completions: [UInt64: MobileRadioCompletion] = [:]
      var status = "dropped-control"
      var attempts: [MobileRadioIngress] = []
      var acceptAfterFirstFailure = false
      var acceptedCodes: [Int64] = []
      var onAccepted42: (() -> Void)?
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { completions[requestId] = completion; return "delivered" }
      func notificationFailureIngressVersion(offered: UInt32) -> UInt32 { offered == 1 ? 1 : 0 }
      func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String {
        ingest(ingress: .notificationSourceFailed(version: 1, instance: instance, epoch: epoch, failure: failure))
      }
      func ingest(ingress: MobileRadioIngress) -> String {
        attempts.append(ingress)
        if acceptAfterFirstFailure, case let .notificationSourceFailed(_, _, _, failure) = ingress,
           let code = failure.nativeCode {
          let result = code == 42 ? status : "accepted"
          if result == "accepted" {
            acceptedCodes.append(code)
            if code == 42 {
              let action = onAccepted42
              onAccepted42 = nil
              action?()
            }
          }
          return result
        }
        return status
      }
    }
    for initial in ["dropped-control", "closed"] {
      let driver = ScriptedDriver(), sink = Sink()
      sink.status = initial
      let adapter = UnifiedBleRustRadioAdapter(driver: driver)
      check(adapter.bind(sink: sink), "bind failure owner")
      let instance = MobileInstance(peerId: "p", serviceUuid: "180D", serviceOccurrence: 0, characteristicUuid: "2A37", characteristicOccurrence: 0)
      func wait(_ id: UInt64) -> MobileRadioCompletion? {
        let deadline = Date().addingTimeInterval(10)
        while Date() < deadline {
          if let answer = driver.onQueue({ sink.completions[id] }) { return answer }
          Thread.sleep(forTimeInterval: 0.01)
        }
        return nil
      }
      adapter.submit(request: .enableNotifications(id: 9001, instance: instance, epoch: 7, requested: nil, preferred: nil))
      check(wait(9001) == .notifyEnabled(delivery: "unknown"), "ordinary enable")
      let identifier = driver.onQueue { driver.subscriptionIdentifier(9001) }
      driver.onQueue {
        var captures: [(NSError) -> Void] = []
        for _ in 0..<4 { captures.append(adapter.captureNotificationFailure(identifier)!) }
        check(adapter.captureNotificationFailure(identifier, allowOverflow: false) == nil,
          "ordinary failure receipts must be bounded before the terminal receipt")
        captures.append(adapter.captureNotificationFailure(identifier)!)
        check(adapter.captureNotificationFailure(identifier) == nil,
          "route's separately reserved terminal receipt must also be bounded")
        captures.removeAll()
      }
      driver.onQueue {}
      if initial == "dropped-control" {
        driver.onQueue {
          sink.acceptAfterFirstFailure = true
          let first = adapter.captureNotificationFailure(identifier)!
          let second = adapter.captureNotificationFailure(identifier)!
          let nested = adapter.captureNotificationFailure(identifier)!
          sink.onAccepted42 = { nested(NSError(domain: "fifo", code: 44)) }
          first(NSError(domain: "fifo", code: 42))
          second(NSError(domain: "fifo", code: 43))
          check(sink.acceptedCodes.isEmpty, "a later captured failure must not overtake a refused failure at the same sink")
          sink.status = "accepted"
        }
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline && driver.onQueue({ adapter.hasPendingNotificationFailureRetention() }) { Thread.sleep(forTimeInterval: 0.01) }
        driver.onQueue {
          check(!adapter.hasPendingNotificationFailureRetention(), "FIFO failures must settle")
          check(sink.acceptedCodes == [42, 43, 44], "accepted failures must preserve order through reentrant delivery")
          sink.acceptAfterFirstFailure = false
          sink.status = initial
          sink.attempts.removeAll()
        }
      }
      var captured = driver.onQueue { adapter.captureNotificationFailure(identifier) }
      check(captured != nil, "capture original subscription")
      driver.onQueue { driver.unsubscribeFailures = [1017, 1019].map { NSError(domain: UnifiedBleRustRadioAdapter.ownedDomain, code: $0) } }
      adapter.submit(request: .disableNotifications(id: 9002, instance: instance, epoch: 7))
      guard case .failed? = wait(9002) else { return check(false, "1017 before the disconnect fence must not release ordinary ownership") }
      adapter.submit(request: .disableNotifications(id: 9003, instance: instance, epoch: 7))
      guard case .failed? = wait(9003) else { return check(false, "1019 must not release ordinary ownership") }
      adapter.submit(request: .disableNotifications(id: 9004, instance: instance, epoch: 7))
      check(wait(9004) == .unit, "ordinary disable must retain the exact owner for retry")
      driver.onQueue {
        captured?(NSError(domain: "departed-native", code: 42, userInfo: [NSLocalizedDescriptionKey: "original failure"]))
        captured = nil
      }
      check(driver.onQueue { adapter.hasPendingNotificationFailureRetention() }, "refused failure must retain its borrower")
      driver.onQueue { sink.status = "accepted" }
      let recoveryDeadline = Date().addingTimeInterval(5)
      while Date() < recoveryDeadline && driver.onQueue({ adapter.hasPendingNotificationFailureRetention() }) { Thread.sleep(forTimeInterval: 0.01) }
      driver.onQueue {
        let failures = sink.attempts.compactMap { ingress -> MobilePlatformFailure? in
          if case let .notificationSourceFailed(_, observed, epoch, failure) = ingress, observed == instance, epoch == 7 { return failure }
          return nil
        }
        check(failures.count >= 2 && failures.allSatisfy { $0.nativeDomain == "departed-native" && $0.nativeCode == 42 && $0.detail == "original failure" }, "refused or closed sink must recover the exact captured failure")
        check(!adapter.hasPendingNotificationFailureRetention(), "accepted failure must release its bounded receipt")
      }
    }
  }

  func orderedAdapterWakeChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      var completions: [UInt64: MobileRadioCompletion] = [:]
      var ingresses: [MobileRadioIngress] = []
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        completions[requestId] = completion
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String {
        ingresses.append(ingress)
        if case .orderedWakeFailure = ingress { return "accepted" }
        return "dropped-control"
      }
    }
    let driver = ScriptedDriver()
    let source = OwnedOrderedNotificationSource(queue: driver.workQueue, generation: "wake-uuid")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: driver.workQueue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard source.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      return (source, group)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let sink = Sink()
    let adapter = UnifiedBleRustRadioAdapter(driver: driver, orderedGroupBridge: bridge)
    adapter.bind(sink: sink)
    let instance = MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0,
      characteristicUuid: "c", characteristicOccurrence: 0)
    adapter.submit(request: .orderedOpen(id: 70, groupId: 77, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0),
      members: [MobileOrderedMember(memberId: 5, instance: instance)], itemCapacity: 4, byteCapacity: 32))
    func waitForCompletion(_ id: UInt64) -> MobileRadioCompletion? {
      let deadline = Date().addingTimeInterval(10)
      while Date() < deadline {
        if let answer = driver.onQueue({ sink.completions[id] }) { return answer }
        Thread.sleep(forTimeInterval: 0.01)
      }
      return nil
    }
    guard case let .orderedOpened(generation, members)? = waitForCompletion(70), members.count == 1 else {
      return check(false, "adapter ordered open")
    }
    adapter.submit(request: .connect(id: 74, peerId: "p", autoConnect: false, preferredPhy: [], expectedGeneration: "conn"))
    guard waitForCompletion(74) == .unit else { return check(false, "ordered enable connection") }
    let activation = MobileOrderedActivation(instance: instance, connectionGeneration: "conn", sourceGeneration: generation, groupId: 77, member: members[0], activationEpoch: 9)
    driver.onQueue { driver.holdSubscribes = true }
    adapter.submit(request: .orderedEnableNotifications(id: 75, activation: activation, requested: nil, preferred: nil))
    let subscribeDeadline = Date().addingTimeInterval(10)
    while Date() < subscribeDeadline && !driver.onQueue({ driver.subscribeCompletions[driver.subscriptionIdentifier(75)] != nil }) { Thread.sleep(forTimeInterval: 0.01) }
    adapter.cancel(requestId: 75)
    adapter.submit(request: .orderedEnableSettlement(id: 76, activation: activation))
    check(waitForCompletion(76) == .orderedEnableStatus(status: .pending), "cancelled waiter preserves pending native enable")
    driver.onQueue { driver.subscribeCompletions.removeValue(forKey: driver.subscriptionIdentifier(75))?(nil) }
    check(waitForCompletion(75) == .notifyEnabled(delivery: "unknown"), "late enable is reported")
    adapter.submit(request: .orderedEnableSettlement(id: 78, activation: activation))
    check(waitForCompletion(78) == .orderedEnableStatus(status: .enabled(delivery: "unknown")), "late enable receipt remains exact")
    driver.onQueue { driver.unsubscribeFailures = [NSError(domain: UnifiedBleRustRadioAdapter.ownedDomain, code: 1017)] }
    adapter.submit(request: .orderedDisableNotifications(id: 84, activation: activation))
    guard case .failed? = waitForCompletion(84) else { return check(false, "stale native path must not prove release") }
    adapter.submit(request: .orderedEnableSettlement(id: 85, activation: activation))
    check(waitForCompletion(85) == .orderedEnableStatus(status: .enabled(delivery: "unknown")), "stale cleanup retains physical debt")
    adapter.submit(request: .orderedDisableNotifications(id: 79, activation: activation))
    check(waitForCompletion(79) == .unit, "exact ordered cleanup")
    adapter.submit(request: .orderedEnableSettlement(id: 80, activation: activation))
    check(waitForCompletion(80) == .orderedEnableStatus(status: .released(failure: nil)), "cleanup produces released receipt")
    adapter.submit(request: .orderedForgetEnableReceipt(id: 81, activation: activation))
    check(waitForCompletion(81) == .unit, "released receipt can be forgotten")
    let retiredActivation = MobileOrderedActivation(instance: instance, connectionGeneration: "conn", sourceGeneration: generation, groupId: 77, member: members[0], activationEpoch: 10)
    adapter.submit(request: .orderedEnableNotifications(id: 82, activation: retiredActivation, requested: nil, preferred: nil))
    let secondDeadline = Date().addingTimeInterval(10)
    while Date() < secondDeadline && !driver.onQueue({ driver.subscribeCompletions[driver.subscriptionIdentifier(82)] != nil }) { Thread.sleep(forTimeInterval: 0.01) }
    // Preserve the value admitted before disconnect; a later value below must
    // not cross the departing borrower's terminal fence.
    driver.onQueue {
      source.admit(route: OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0), bytes: Data([9]), provenance: .notification)
    }
    driver.onQueue { adapter.protocolRadioDidDisconnectPeer("p", error: nil) }
    driver.onQueue { driver.subscribeCompletions.removeValue(forKey: driver.subscriptionIdentifier(82))?(nil) }
    guard case .orderedEnableReleased? = waitForCompletion(82) else { return check(false, "late callback after connection retirement") }
    adapter.submit(request: .orderedEnableSettlement(id: 83, activation: retiredActivation))
    check(waitForCompletion(83) == .orderedEnableStatus(status: .released(failure: nil)), "retirement cannot be undone by a late callback")
    driver.onQueue {
      source.admit(route: OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0), bytes: Data([9]), provenance: .notification)
    }
    let deadline = Date().addingTimeInterval(10)
    while Date() < deadline && !driver.onQueue({ sink.ingresses.contains { if case .orderedWakeFailure = $0 { return true }; return false } }) { Thread.sleep(forTimeInterval: 0.01) }
    check(driver.onQueue { sink.ingresses.contains { if case let .orderedWake(actual, id) = $0 { return actual == generation && id == 77 }; return false } },
      "wake must carry the exact source generation and group")
    check(driver.onQueue { sink.ingresses.contains { if case let .orderedWakeFailure(actual, id) = $0 { return actual == generation && id == 77 }; return false } },
      "refused wake must report failure to the host")
    adapter.submit(request: .orderedDrain(id: 71, generation: generation, groupId: 77))
    guard case let .orderedRecords(batchID?, records)? = waitForCompletion(71), records.count == 2 else {
      return check(false, "wake refusal must retain the value and terminal")
    }
    check(records[0] == .value(member: members[0], ordinal: 0, value: Data([9]), delivery: "unknown", provenance: "notification"), "value survives refused wake")
    check(records[1] == .terminal(reason: "wake-failed", uncertainTail: true, nativeDomain: nil,
      nativeCode: nil, failureMember: nil, rejected: nil), "wake refusal ends explicitly")
    adapter.submit(request: .orderedAcknowledgeBatch(id: 72, generation: generation, groupId: 77, batchId: batchID))
    check(waitForCompletion(72) == .unit, "wake-failure batch ack")
    adapter.submit(request: .orderedSeal(id: 74, generation: generation, groupId: 77))
    check(waitForCompletion(74) == .unit, "seal wake-failure source")
    adapter.submit(request: .orderedRemove(id: 73, generation: generation, groupId: 77))
    check(waitForCompletion(73) == .unit, "wake-failure group cleanup")
  }

  func productionOrderedFactoryNoEffectChecks() {
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let adapter = UnifiedBleRustRadioAdapter(driver: radio)
    radio.workQueue.sync {
      let peer = "factory-refusal-peer"
      check(radio.orderedSnapshotIdentities.attach(peer), "fixture attachment")
      guard let snapshot = radio.orderedSnapshotIdentities.publish(peer),
            let bridge = adapter.orderedGroupsOnRadioQueue() else {
        return check(false, "production bridge and snapshot must exist")
      }
      let route = OrderedNotificationRoute(peer: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0)
      do {
        _ = try bridge.open(groupID: 499, members: [.init(id: 1, route: route)], capacity: 2, byteCapacity: 8, snapshot: snapshot)
        return check(false, "a snapshot without this adapter's connection claim must be stale")
      } catch UnifiedBleRustOrderedGroupBridge.Failure.staleGeneration {
        // Exercise the factory independently once admission is established.
      } catch { return check(false, "expected an unowned graph refusal, got \(error)") }
      bridge.setSnapshotValidator { peer, identity in radio.orderedSnapshotIdentities.matches(peer, identity) }
      do {
        _ = try bridge.open(groupID: 500, members: [.init(id: 1, route: route)], capacity: 2, byteCapacity: 8, snapshot: snapshot)
        return check(false, "unresolved production characteristic must be refused")
      } catch let error as UnifiedBleRustOrderedGroupBridge.FactoryNoEffect {
        check((error.underlying as? OwnedOrderedNotificationIngress.Failure) == .invalidMembers, "bridge must preserve the original factory error")
      } catch {
        return check(false, "expected original factory error, got \(error)")
      }
      check(radio.orderedSources[peer] == nil, "refusal must not publish a source")
      let instance = MobileInstance(peerId: peer, serviceUuid: "180D", serviceOccurrence: 0,
        characteristicUuid: "2A37", characteristicOccurrence: 0)
      let identity = MobileGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let result = UnifiedBleRustOrderedRadioAdapter.handle(.orderedOpen(id: 501, groupId: 501,
        connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: identity,
        members: [MobileOrderedMember(memberId: 1, instance: instance)], itemCapacity: 2, byteCapacity: 8), bridge: bridge)
      let original = OwnedOrderedNotificationIngress.Failure.invalidMembers as NSError
      guard case let .orderedOpenFailed(failure, .notCreated)? = result,
            !failure.dispatched, failure.nativeDomain == original.domain, failure.nativeCode == Int64(original.code),
            failure.detail.contains(original.localizedDescription) else {
        return check(false, "production refusal must report no effect and the original error")
      }
      check(radio.orderedSources[peer] == nil, "typed refusal must not publish a source")
    }
  }

  func orderedTypedRadioChecks() {
    let orderedQueue = DispatchQueue(label: "apple.ordered.typed.harness")
    let source = OwnedOrderedNotificationSource(queue: orderedQueue, generation: "source-uuid")
    var ingress: OwnedOrderedNotificationIngress?
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: orderedQueue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: orderedQueue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard source.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      ingress = group
      return (source, group)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let uncertainSource = OwnedOrderedNotificationSource(queue: orderedQueue, generation: "uncertain-source")
    let refusedBridge = UnifiedBleRustOrderedGroupBridge(queue: orderedQueue, factory: { _, _, _, _ in
      throw UnifiedBleRustOrderedGroupBridge.FactoryNoEffect(underlying: NSError(domain: "pre-install", code: 42, userInfo: [NSLocalizedDescriptionKey: "characteristic unavailable"]))
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let uncertainBridge = UnifiedBleRustOrderedGroupBridge(queue: orderedQueue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: orderedQueue, generation: uncertainSource.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard uncertainSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      throw NSError(domain: "post-install", code: 1)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let partialSource = OwnedOrderedNotificationSource(queue: orderedQueue, generation: "partial-source")
    let partialBridge = UnifiedBleRustOrderedGroupBridge(queue: orderedQueue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: orderedQueue, generation: partialSource.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard partialSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      partialSource.invalidate(.connectionLost)
      return (partialSource, group)
    }, validateSnapshot: { _, _ in true }, removeSourceGroup: { _, _ in false }, wake: { _, _ in })
    orderedQueue.sync {
      let instance = MobileInstance(peerId: "p", serviceUuid: "s", serviceOccurrence: 0,
        characteristicUuid: "c", characteristicOccurrence: 0)
      let refused = UnifiedBleRustOrderedRadioAdapter.handle(.orderedOpen(id: 97, groupId: 97, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 5, instance: instance)], itemCapacity: 4, byteCapacity: 32), bridge: refusedBridge)
      guard case let .orderedOpenFailed(refusal, .notCreated)? = refused,
            !refusal.dispatched, refusal.nativeDomain == "pre-install", refusal.nativeCode == 42,
            refusal.detail.contains("characteristic unavailable") else {
        return check(false, "known factory refusal must retain its original error and no-effect outcome")
      }
      let uncertain = UnifiedBleRustOrderedRadioAdapter.handle(.orderedOpen(id: 99, groupId: 99, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 5, instance: instance)], itemCapacity: 4, byteCapacity: 32), bridge: uncertainBridge)
      guard case let .orderedOpenFailed(failure, .indeterminate(recoveryKey))? = uncertain,
            recoveryKey == 99, failure.dispatched else { return check(false, "factory failure must preserve uncertain ownership") }
      let partial = UnifiedBleRustOrderedRadioAdapter.handle(.orderedOpen(id: 98, groupId: 98, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 5, instance: instance)], itemCapacity: 4, byteCapacity: 32), bridge: partialBridge)
      guard case let .orderedOpenFailed(partialFailure, .created(partialGeneration, partialMembers))? = partial,
            partialFailure.dispatched, partialGeneration == .opaque(value: "partial-source"), partialMembers.isEmpty else {
        return check(false, "failed rollback must return its exact partial source identity")
      }
      let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      let opened = UnifiedBleRustOrderedRadioAdapter.handle(.orderedOpen(id: 1, groupId: 10, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0),
        members: [MobileOrderedMember(memberId: 5, instance: instance)], itemCapacity: 4, byteCapacity: 32), bridge: bridge)
      guard case let .orderedOpened(generation, members)? = opened, members.count == 1 else {
        return check(false, "ordered open must return source identity and membership")
      }
      check(generation == .opaque(value: "source-uuid"), "Apple source identity is opaque")
      let registration = members[0]
      let retained = UnifiedBleRustOrderedRadioAdapter.handle(.orderedRetainClaim(id: 2, generation: generation, groupId: 10, member: registration), bridge: bridge)
      guard case let .orderedClaim(claimGeneration, claimMember)? = retained else { return check(false, "claim retain") }
      check(claimGeneration == generation && claimMember == registration, "claim has exact source epoch")
      source.admit(route: route, bytes: Data([1, 2]), provenance: .readOrNotification)
      ingress?.reportLoss(member: route, count: UInt64.max)
      let request = MobileRadioRequest.orderedDrain(id: 3, generation: generation, groupId: 10)
      let first = UnifiedBleRustOrderedRadioAdapter.handle(request, bridge: bridge)
      guard case let .orderedRecords(batchID?, records)? = first else { return check(false, "nonempty batch") }
      check(records == [.value(member: registration, ordinal: 0, value: Data([1, 2]), delivery: "unknown", provenance: "read-or-notification"),
        .loss(scope: "member", memberId: 5, unit: "source-records", count: UInt64.max, byteCount: nil)],
        "typed records preserve provenance, ordinal and full-width source count")
      check(UnifiedBleRustOrderedRadioAdapter.handle(request, bridge: bridge) == first, "drain replay must be exact")
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 4, generation: generation, groupId: 10, batchId: batchID + 1), bridge: bridge) else { return check(false, "wrong batch ack") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 5, generation: generation, groupId: 10, batchId: batchID), bridge: bridge) == .unit, "ack exact batch")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 50, generation: generation, groupId: 10, batchId: batchID), bridge: bridge) == .unit, "retry committed ack")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedRetire(id: 51, generation: generation, groupId: 10, member: registration), bridge: bridge) == .unit, "retire member")
      guard case let .orderedRecords(fenceID?, fenceRecords)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedDrain(id: 52, generation: generation, groupId: 10), bridge: bridge),
            fenceRecords == [.fence(member: registration, uncertainTail: true)] else { return check(false, "typed exact fence") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 53, generation: generation, groupId: 10, batchId: fenceID), bridge: bridge) == .unit, "ack fence")
      guard case let .orderedRegistered(replacement)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedRegister(id: 54, generation: generation, groupId: 10, member: registration), bridge: bridge) else { return check(false, "register replacement") }
      check(replacement != registration, "replacement has distinct native membership")
      guard case let .orderedClaim(_, replacementClaim)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedRetainClaim(id: 55, generation: generation, groupId: 10, member: replacement), bridge: bridge), replacementClaim == replacement else { return check(false, "retain replacement") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedClose(id: 6, generation: .counter(value: 1), groupId: 10), bridge: bridge) else { return check(false, "wrong generation") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedClose(id: 7, generation: generation, groupId: 10), bridge: bridge) == .unit, "close source")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedSeal(id: 71, generation: generation, groupId: 10), bridge: bridge) == .unit, "seal source")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedSeal(id: 72, generation: generation, groupId: 10), bridge: bridge) == .unit, "retry seal")
      guard case let .orderedRecords(terminalID?, terminalRecords)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedDrain(id: 8, generation: generation, groupId: 10), bridge: bridge),
            case .terminal("closed", true, nil, nil, nil, nil) = terminalRecords.first else { return check(false, "typed terminal") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 9, generation: generation, groupId: 10, batchId: terminalID), bridge: bridge) == .unit, "ack terminal")
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedRemove(id: 10, generation: generation, groupId: 10), bridge: bridge) else { return check(false, "remove must retain claim") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedConfirmCleanup(id: 11, generation: generation, groupId: 10, member: registration), bridge: bridge) == .unit, "confirm exact claim")
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedRemove(id: 56, generation: generation, groupId: 10), bridge: bridge) else { return check(false, "replacement retains cleanup") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedConfirmCleanup(id: 57, generation: generation, groupId: 10, member: replacement), bridge: bridge) == .unit, "confirm replacement claim")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedRemove(id: 12, generation: generation, groupId: 10), bridge: bridge) == .unit, "remove released group")
      guard case let .orderedOpened(overflowGeneration, overflowMembers)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedOpen(id: 80, groupId: 11, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 6, instance: instance)], itemCapacity: 1, byteCapacity: 32), bridge: bridge), overflowMembers.count == 1 else { return check(false, "overflow open") }
      source.admit(route: route, bytes: Data([3]), provenance: .notification)
      ingress?.reportLoss(member: route, count: UInt64.max)
      guard case let .orderedRecords(overflowID?, overflowRecords)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedDrain(id: 81, generation: overflowGeneration, groupId: 11), bridge: bridge), overflowRecords.count == 2 else { return check(false, "typed overflow batch") }
      check(overflowRecords[1] == .terminal(reason: "overflow", uncertainTail: true, nativeDomain: nil, nativeCode: nil,
        failureMember: nil, rejected: .loss(scope: "member", memberId: 6, unit: "source-records", count: UInt64.max, byteCount: nil)), "overflow retains full-width rejected source loss")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 82, generation: overflowGeneration, groupId: 11, batchId: overflowID), bridge: bridge) == .unit, "ack overflow")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedSeal(id: 89, generation: overflowGeneration, groupId: 11), bridge: bridge) == .unit, "seal overflow source")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedRemove(id: 83, generation: overflowGeneration, groupId: 11), bridge: bridge) == .unit, "remove overflow group")
      guard case let .orderedOpened(ownerGeneration, _)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedOpen(id: 830, groupId: 13, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 9, instance: instance)], itemCapacity: 4, byteCapacity: 32), bridge: bridge) else { return check(false, "protocol owner open") }
      source.admit(route: route, bytes: Data([11]), provenance: .notification)
      let v2OwnerDrain = MobileRadioRequest.orderedDrainV2(id: 831, version: 2, generation: ownerGeneration, groupId: 13)
      guard case let .orderedRecords(v2OwnerID?, v2OwnerRecords)? = UnifiedBleRustOrderedRadioAdapter.handle(v2OwnerDrain, bridge: bridge) else { return check(false, "V2 owner drain") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedDrain(id: 832, generation: ownerGeneration, groupId: 13), bridge: bridge) else { return check(false, "legacy drain cannot steal V2 batch") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 833, generation: ownerGeneration, groupId: 13, batchId: v2OwnerID), bridge: bridge) else { return check(false, "legacy ack cannot consume representable V2 batch") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(v2OwnerDrain, bridge: bridge) == .orderedRecords(batchId: v2OwnerID, records: v2OwnerRecords), "V2 lost reply replays")
      let v2OwnerAck = MobileRadioRequest.orderedAcknowledgeBatchV2(id: 834, version: 2, generation: ownerGeneration, groupId: 13, batchId: v2OwnerID)
      check(UnifiedBleRustOrderedRadioAdapter.handle(v2OwnerAck, bridge: bridge) == .unit && UnifiedBleRustOrderedRadioAdapter.handle(v2OwnerAck, bridge: bridge) == .unit, "V2 lost ack reply retries")
      source.admit(route: route, bytes: Data([12]), provenance: .notification)
      let legacyOwnerDrain = MobileRadioRequest.orderedDrain(id: 835, generation: ownerGeneration, groupId: 13)
      guard case let .orderedRecords(legacyOwnerID?, legacyOwnerRecords)? = UnifiedBleRustOrderedRadioAdapter.handle(legacyOwnerDrain, bridge: bridge) else { return check(false, "legacy owner drain") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(v2OwnerDrain, bridge: bridge) else { return check(false, "V2 drain cannot steal legacy batch") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatchV2(id: 836, version: 2, generation: ownerGeneration, groupId: 13, batchId: legacyOwnerID), bridge: bridge) else { return check(false, "V2 ack cannot consume legacy batch") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(legacyOwnerDrain, bridge: bridge) == .orderedRecords(batchId: legacyOwnerID, records: legacyOwnerRecords), "legacy lost reply replays")
      let legacyOwnerAck = MobileRadioRequest.orderedAcknowledgeBatch(id: 837, generation: ownerGeneration, groupId: 13, batchId: legacyOwnerID)
      check(UnifiedBleRustOrderedRadioAdapter.handle(legacyOwnerAck, bridge: bridge) == .unit && UnifiedBleRustOrderedRadioAdapter.handle(legacyOwnerAck, bridge: bridge) == .unit, "legacy lost ack reply retries")
      guard case let .orderedOpened(failureGeneration, failureMembers)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedOpen(id: 84, groupId: 12, connectionGeneration: "conn", databaseGeneration: "db", snapshotIdentity: MobileGattSnapshotIdentity(owner: "test", attachment: 0, revision: 0), members: [MobileOrderedMember(memberId: 7, instance: instance)], itemCapacity: 2, byteCapacity: 32), bridge: bridge), failureMembers.count == 1 else { return check(false, "failure open") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedBarrier(id: 840, generation: failureGeneration, groupId: 12, barrierId: 840), bridge: bridge) == .unit, "admit prior barrier")
      guard case let .orderedRecords(priorID?, _)? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedDrain(id: 841, generation: failureGeneration, groupId: 12), bridge: bridge) else { return check(false, "drain prior batch") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 842, generation: failureGeneration, groupId: 12, batchId: priorID), bridge: bridge) == .unit, "ack prior batch")
      source.fail(route: route, failure: .init(domain: "CBFixture", code: 42))
      source.fail(route: route, failure: .init(error: NSError(domain: "CBFixture", code: 43, userInfo: [NSLocalizedDescriptionKey: "specific notification error", NSUnderlyingErrorKey: NSError(domain: "cause", code: 7, userInfo: [NSLocalizedDescriptionKey: "specific cause"])])))
      source.fail(route: route, failure: .init(error: NSError(domain: "CBFixture", code: 43, userInfo: [NSLocalizedDescriptionKey: "different notification error"])))
      source.fail(route: route, failure: .init(domain: "CBFixture", code: 45))
      let failureRequest = MobileRadioRequest.orderedDrain(id: 85, generation: failureGeneration, groupId: 12)
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(failureRequest, bridge: bridge) else {
        return check(false, "legacy wire must refuse a batch that cannot carry late failure identity")
      }
      guard let retainedBatch = try? bridge.drain(groupID: 12) else { return check(false, "failure batch remains retained") }
      let originalClaim = OrderedNotificationClaim(generation: "source-uuid", route: route, token: failureMembers[0].sourceMember)
      check(retainedBatch.records.contains(.subsequentSourceFailure(
        OwnedOrderedNotificationIngress.NativeFailure(error: NSError(domain: "CBFixture", code: 43, userInfo: [NSLocalizedDescriptionKey: "specific notification error", NSUnderlyingErrorKey: NSError(domain: "cause", code: 7, userInfo: [NSLocalizedDescriptionKey: "specific cause"])] )).attributed(to: originalClaim, epoch: failureMembers[0].epoch))),
        "retained failure keeps original claim and epoch")
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(failureRequest, bridge: bridge) else {
        return check(false, "retry must remain unresolved")
      }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 843, generation: failureGeneration, groupId: 12, batchId: priorID), bridge: bridge) == .unit, "older exact ack remains idempotent")
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 844, generation: failureGeneration, groupId: 12, batchId: retainedBatch.id + 1), bridge: bridge) else { return check(false, "wrong batch id") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 86, generation: failureGeneration, groupId: 12, batchId: retainedBatch.id), bridge: bridge) else {
        return check(false, "legacy wire cannot acknowledge an unrepresentable batch")
      }
      check((try? bridge.drain(groupID: 12)) == retainedBatch, "refused acknowledgement retains exact batch")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedSeal(id: 90, generation: failureGeneration, groupId: 12), bridge: bridge) == .unit, "seal failed source")
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedRemove(id: 87, generation: failureGeneration, groupId: 12), bridge: bridge) else {
        return check(false, "unresolved batch must prevent cleanup")
      }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatchV2(id: 88, version: 2, generation: failureGeneration, groupId: 12, batchId: retainedBatch.id), bridge: bridge) else { return check(false, "a batch cannot be acknowledged before its versioned delivery") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedDrainV2(id: 89, version: 3, generation: failureGeneration, groupId: 12), bridge: bridge) else { return check(false, "unknown drain version must retain the batch") }
      let v2 = MobileRadioRequest.orderedDrainV2(id: 91, version: 2, generation: failureGeneration, groupId: 12)
      guard case let .orderedRecords(v2ID?, records)? = UnifiedBleRustOrderedRadioAdapter.handle(v2, bridge: bridge), v2ID == retainedBatch.id else {
        return check(false, "versioned drain must replay the retained batch")
      }
      let late = records.compactMap { record -> (MobilePlatformFailure, MobileInstance, String, UInt64, UInt64, UInt64, UInt32)? in
        guard case let .subsequentSourceFailureV2(failure, instance, sourceGeneration, sourceMember, epoch, batch, index) = record else { return nil }
        return (failure, instance, sourceGeneration, sourceMember, epoch, batch, index)
      }
      check(late.map { $0.0.nativeCode } == [43, 43] && records.contains(.failureObservationsOmitted), "retained late failures and overflow evidence are forwarded in order")
      check(late.first?.0.detail.contains("specific notification error") == true && late.first?.0.detail.contains("cause:7") == true && late.first?.0.detail.contains("specific cause") == true, "versioned wire retains native description and underlying error")
      check(late.count == 2 && late[1].0.detail == "different notification error", "same-domain and code errors retain distinct descriptions")
      for item in late {
        check(item.0.nativeDomain == "CBFixture" && item.1 == instance && item.2 == originalClaim.generation && item.3 == originalClaim.token && item.4 == failureMembers[0].epoch && item.5 == v2ID && Int(item.6) < records.count,
          "late failure retains original route, generation, claim, epoch and batch position")
      }
      check(UnifiedBleRustOrderedRadioAdapter.handle(v2, bridge: bridge) == .orderedRecords(batchId: v2ID, records: records), "versioned retry replays exact batch")
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatchV2(id: 95, version: 3, generation: failureGeneration, groupId: 12, batchId: v2ID), bridge: bridge) else { return check(false, "unknown version must not acknowledge") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatchV2(id: 96, version: 2, generation: failureGeneration, groupId: 12, batchId: v2ID + 1), bridge: bridge) else { return check(false, "wrong versioned batch must not acknowledge") }
      guard case .failed? = UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatch(id: 92, generation: failureGeneration, groupId: 12, batchId: v2ID), bridge: bridge) else { return check(false, "legacy ack must not consume versioned batch") }
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatchV2(id: 93, version: 2, generation: failureGeneration, groupId: 12, batchId: v2ID), bridge: bridge) == .unit, "exact versioned acknowledgement")
      check(UnifiedBleRustOrderedRadioAdapter.handle(.orderedAcknowledgeBatchV2(id: 94, version: 2, generation: failureGeneration, groupId: 12, batchId: v2ID), bridge: bridge) == .unit, "lost acknowledgement retry")
    }
  }

  func orderedVersionedReplacementChecks() {
    let queue = DispatchQueue(label: "apple.ordered.v2.replacement")
    let routes = ["a", "b"].map { OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: $0, characteristicOccurrence: 0) }
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "original-source")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { members, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: members, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard source.add(ingress) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      return (source, ingress)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    queue.sync {
      do {
        let descriptor = try bridge.open(groupID: 70, members: routes.enumerated().map { .init(id: UInt64($0.offset + 1), route: $0.element) }, capacity: 8, byteCapacity: 32)
        var claims: [OrderedNotificationClaim] = []
        var receipts: [(Bool) -> Void] = []
        for (index, member) in descriptor.members.enumerated() {
          let claim = OrderedNotificationClaim(generation: source.generation, route: routes[index], token: member.sourceMember)
          claims.append(claim)
          guard let receipt = source.captureDeferredFailure(claim: claim, failure: .init(domain: "original", code: 100 + index)) else { return check(false, "capture original recipient") }
          receipts.append(receipt)
          try bridge.retire(groupID: 70, expected: member)
          let replacement = try bridge.register(groupID: 70, expected: member)
          check(replacement.epoch != member.epoch, "replacement must have a new epoch")
        }
        receipts.forEach { $0(true) }
        let generation = MobileOrderedGeneration.opaque(value: source.generation)
        let drain = MobileRadioRequest.orderedDrainV2(id: 1, version: 2, generation: generation, groupId: 70)
        guard case let .orderedRecords(batchID?, records)? = UnifiedBleRustOrderedRadioAdapter.handle(drain, bridge: bridge) else { return check(false, "versioned multi-claim drain") }
        let failures = records.compactMap { value -> (MobilePlatformFailure, MobileInstance, String, UInt64, UInt64, UInt64, UInt32)? in
          guard case let .subsequentSourceFailureV2(failure, instance, generation, token, epoch, batch, index) = value else { return nil }
          return (failure, instance, generation, token, epoch, batch, index)
        }
        check(failures.count == 2, "both original claims survive replacement")
        for (i, value) in failures.enumerated() {
          check(value.0.nativeCode == Int64(100 + i) && value.0.nativeDomain == "original" && value.1.characteristicUuid == claims[i].route.characteristicUUID && value.2 == claims[i].generation && value.3 == claims[i].token && value.4 == descriptor.members[i].epoch && value.5 == batchID && records[Int(value.6)] == .subsequentSourceFailureV2(failure: value.0, instance: value.1, sourceGeneration: value.2, sourceMember: value.3, epoch: value.4, batchId: value.5, recordIndex: value.6), "each failure keeps its own original claim and batch position")
        }
        check(UnifiedBleRustOrderedRadioAdapter.handle(drain, bridge: bridge) == .orderedRecords(batchId: batchID, records: records), "multi-claim batch retry")
      } catch { check(false, "multi-claim setup failed: \(error)") }
    }
  }

  func run() {
    var reentrantInstall: UnifiedBleRustCoreSessions!
    reentrantInstall = UnifiedBleRustCoreSessions(installer: { wake in
      // A native callback may arrive synchronously during installation.
      // It must be able to enter the session owner without deadlocking.
      wake.wake(sessionId: 1)
      let nestedFailure = reentrantInstall.ensureHost()
      check(json(nestedFailure ?? "{}")["code"] as? String == "lifecycle.invalid-state",
            "reentrant host installation must fail promptly")
      throw MobileCoreError.Failed(code: "test.refused", domain: "test", operation: "install", detail: nil)
    })
    check(reentrantInstall.ensureHost() != nil, "scripted install should fail after its reentrant wake")
    orderedDeferredOpenFenceChecks()
    orderedAdapterWakeChecks()
    productionOrderedFactoryNoEffectChecks()
    orderedTypedRadioChecks()
    orderedVersionedReplacementChecks()
    translationChecks()
    explicitRadioAdmissionChecks()
    generationScopedConnectionChecks()
    restorationIdentityChecks()
    randomBytesChecks()

    driver.restored = [["peerIdentifier": "R", "name": "Polar H10 R", "connected": true]]
    sessions = UnifiedBleRustCoreSessions(installer: { wake in
      let bridge = UnifiedBleRustOrderedGroupBridge(queue: self.driver.workQueue, factory: { routes, capacity, bytes, notify in
        let source = OwnedOrderedNotificationSource(queue: self.driver.workQueue, generation: UUID().uuidString)
        let group = try OwnedOrderedNotificationIngress(queue: self.driver.workQueue, generation: source.generation,
          members: routes, capacity: capacity, byteCapacity: bytes, wake: notify)
        guard source.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
        return (source, group)
      }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
      self.adapter = UnifiedBleRustRadioAdapter(driver: self.driver, onRestoredPeer: { peer in
        self.sessions.continueRestoredPeer(peer) { value, failure in
          self.continuationDone?(value, failure)
        }
      }, orderedGroupBridge: bridge)
      self.host = try mobileHostInstallBound(
        radio: self.adapter, wake: wake, binding: AdapterHostBinding(self.adapter),
        platform: "apple", owner: "apple-harness", adapterLabel: "corebluetooth"
      )
      return self.host
    })
    check(sessions.ensureHost() == nil, "host install failed")
    check(sessions.wireRevision() == mobileWireRevision(), "wire revision is not Rust's")
    check(sessions.contractRevision() == mobileContractRevision(), "contract revision is not Rust's")
    check(!json(sessions.nativeBuildIdentity()).isEmpty, "build identity is not Rust's JSON record")
    check(host.configureNotificationFailureSpoolV1(recordingId: "raw-bypass") == "unsupported",
          "raw V1 host configuration must not bypass the adapter fence")
    check(host.configureNotificationFailureSpoolV2(recordingId: "raw-bypass", maxBytes: "1", maxRecords: "1")
            .contains("capability.unsupported"),
          "raw V2 host configuration must not bypass the adapter fence")
    check(host.claimNotificationFailureSpoolConfigurationAuthorityV3() == nil,
          "a published host must not hand the adapter authority to a public caller")
    check(host.claimScanFailureRecoveryConfigurationAuthorityV1() == nil,
          "a published host must not hand scan recovery authority to a public caller")

    // The recovery selection is a Swift-native embedding operation. It has no
    // TurboModule/ObjC method and a raw host cannot bypass the adapter-held
    // authority. Exercise the real UniFFI attach path with a missing recording:
    // Rust must reject it without returning a recovery holder.
    do {
      _ = try sessions.openSelectedScanFailureRecovery()
      check(false, "scan recovery opened without a native selection")
    } catch let error as MobileCoreError {
      guard case let .Failed(code, _, operation, _) = error else { check(false, "unexpected no-selection error") ; return }
      check(code == "capability.unsupported" && operation == "scan.failure-recovery.open", "missing native selection is not explicit")
    } catch { check(false, "unexpected no-selection error \(error)") }
    do {
      try sessions.selectScanFailureRecoveryRecording("bad/name")
      check(false, "a path-like scan recovery recording name was accepted")
    } catch let error as MobileCoreError {
      guard case let .Failed(code, _, operation, _) = error else { check(false, "unexpected name error") ; return }
      check(code == "argument.invalid" && operation == "scan.failure-recovery.select", "invalid native recording name is not rejected")
    } catch { check(false, "unexpected name error \(error)") }
    do {
      try sessions.selectScanFailureRecoveryRecording("apple-harness-missing-recording")
      _ = try sessions.openSelectedScanFailureRecovery()
      check(false, "missing recording produced a recovery holder")
    } catch let error as MobileCoreError {
      guard case .Failed = error else { check(false, "unexpected attach error") ; return }
    } catch { check(false, "unexpected attach error \(error)") }

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

    let foreignVersion: (String?, String?) = waitFor("foreign scan version") { done in
      self.sessions.negotiateScanEndVersion(sessionId: self.sessionId, offeredJson: "[1,2]", ownerToken: NSObject()) { done(($0, $1)) }
    }
    check(foreignVersion.0 == nil && json(foreignVersion.1 ?? "{}" )["code"] as? String == "ownership.denied", "foreign module negotiated scan version")
    let unsupportedVersion: (String?, String?) = waitFor("unqualified scan version") { done in
      self.sessions.negotiateScanEndVersion(sessionId: self.sessionId, offeredJson: "[2]", ownerToken: token) { done(($0, $1)) }
    }
    check(unsupportedVersion.0 == nil && json(unsupportedVersion.1 ?? "{}" )["code"] as? String == "protocol.incompatible", "unqualified scan version selected")
    let qualifiedVersion: (String?, String?) = waitFor("qualified scan version") { done in
      self.sessions.negotiateScanEndVersion(sessionId: self.sessionId, offeredJson: "[1,2]", ownerToken: token) { done(($0, $1)) }
    }
    check(qualifiedVersion.1 == nil && json(qualifiedVersion.0 ?? "{}")["version"] as? Int == 1, "qualified scan version was not v1")

    let foreignPeers = ok("peers.connected", ["services": ["180d"], "operationId": "foreign-directory"]) as? [[String: Any]] ?? []
    check(foreignPeers.count == 1 && foreignPeers[0]["peerId"] as? String == "FOREIGN", "system inventory must include a foreign link")
    check(foreignPeers[0]["connection"] as? String == "connected", "native inventory connection fact")
    check(foreignPeers[0]["source"] as? String == "system-connected", "native inventory source")
    check(!(ok("peers.known", [:]) as? [[String: Any]] ?? []).contains { $0["peerId"] as? String == "FOREIGN" }, "directory read must not add owner cache state")

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

    onRadioQueue {
      self.adapter.protocolRadioDidFailNotification(restoredSubscription,
        error: NSError(domain: "CBErrorDomain", code: 17, userInfo: [NSLocalizedDescriptionKey: "scripted source failure"]))
    }
    drainUntil("ordinary source failure") { records in
      records.contains { $0["t"] as? String == "stream-end" && $0["consumer"] as? String == "hr-restored" && $0["reason"] as? String == "source-failed" }
    }
    let end = records("stream-end").first { $0["consumer"] as? String == "hr-restored" }
    let error = end?["error"] as? [String: Any]
    let platform = error?["platform"] as? [String: Any]
    check(platform?["domain"] as? String == "CBErrorDomain" && platform?["code"] as? String == "17",
      "ordinary terminal must retain the native cause: \(String(describing: end))")

    if ProcessInfo.processInfo.environment["UBM_APPLE_522_ONLY"] == "1" {
      print("[AppleRustRadioAdapterHarness] exact open recovery and ordinary source failure passed (scripted radio).")
      return
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
    check(driver.onQueue { self.driver.calls.contains { $0.hasPrefix("stopScan ubm-rust-") } }, "lost scan was not cleared")
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

    // Native owner proof is not implemented yet. Shutdown must retain the
    // route and report that exact debt, including on a repeated shutdown.
    func checkPendingShutdown(_ cleanup: [String: Any]) {
      guard cleanup["state"] as? String == "release-failed",
            let failures = cleanup["failures"] as? [[String: Any]], failures.count == 1,
            failures[0]["resourceKind"] as? String == "ordered-owner",
            failures[0]["code"] as? String == "lifecycle.invalid-state",
            failures[0]["domain"] as? String == "cleanup",
            failures[0]["operation"] as? String == "host.shutdown.ordered-owner",
            failures[0]["detail"] as? String == "native owner quiescence has not been confirmed; callback routing remains retained" else {
        return check(false, "shutdown must retain exactly the unproven owner debt: \(cleanup)")
      }
    }
    checkPendingShutdown(json(host.shutdown()))
    checkPendingShutdown(json(host.shutdown()))
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
    let configured = mobileRecordingConfigureDirectory(path: directory.path)
    check(json(configured)["ok"] as? Bool == true, "configure actual offline Swift registry: \(configured)")
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

  /// Minimal production-bound host bootstrap for the isolated durable setup
  /// child. It deliberately does not run the main harness, whose recorded
  /// scenarios can legitimately configure the process-global registry first.
  func installDurableSetupHost() {
    sessions = UnifiedBleRustCoreSessions(installer: { wake in
      let bridge = UnifiedBleRustOrderedGroupBridge(queue: self.driver.workQueue, factory: { routes, capacity, bytes, notify in
        let source = OwnedOrderedNotificationSource(queue: self.driver.workQueue, generation: UUID().uuidString)
        let group = try OwnedOrderedNotificationIngress(queue: self.driver.workQueue, generation: source.generation,
          members: routes, capacity: capacity, byteCapacity: bytes, wake: notify)
        guard source.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
        return (source, group)
      }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
      self.adapter = UnifiedBleRustRadioAdapter(driver: self.driver, onRestoredPeer: { _ in }, orderedGroupBridge: bridge)
      self.host = try mobileHostInstallBound(
        radio: self.adapter, wake: wake, binding: AdapterHostBinding(self.adapter),
        platform: "apple", owner: "apple-harness-durable", adapterLabel: "corebluetooth"
      )
      self.sessions.registerProductionRecoveryAdapter(self.adapter, for: self.host)
      return self.host
    })
    if let root = ProcessInfo.processInfo.environment["UBM_APPLE_SCAN_RECOVERY_DIRECTORY"] {
      do { try sessions.installTestingRecordingStorageDirectory(URL(fileURLWithPath: root, isDirectory: true)) }
      catch { check(false, "could not install seeded recovery directory: \(error)") }
    }
    check(sessions.ensureHost() == nil, "isolated durable setup host install failed")
  }

  /// This runs in a fresh process after the Rust fixture writer has committed
  /// real recovery sidecars into this process' private Application Support
  /// root.  The fixture is deliberately created before Swift starts: the
  /// bridge may only reopen an existing opaque recording, never manufacture
  /// recovery state for a test.
  func seededScanRecoveryLifecycleChecks() {
    installDurableSetupHost()

    func open(_ recordingId: String) -> String {
      do { try sessions.selectScanFailureRecoveryRecording(recordingId) }
      catch { check(false, "could not select seeded recovery recording \(recordingId): \(error)") }
      let result: (String?, String?) = waitFor("seeded scan recovery open") { done in
        sessions.scanFailureRecoveryOpen { value, failure in done((value, failure)) }
      }
      check(result.1 == nil, "could not open seeded recovery \(recordingId): \(result.1 ?? "nil")")
      guard let leaseId = json(result.0 ?? "{}")["leaseId"] as? String else {
        check(false, "seeded recovery did not return an opaque lease id")
        return ""
      }
      return leaseId
    }

    func owners(_ leaseId: String) -> [[String: Any]] {
      let result: (String?, String?) = waitFor("seeded scan recovery owners") { done in
        sessions.scanFailureRecoveryOwners(leaseId) { value, failure in done((value, failure)) }
      }
      check(result.1 == nil, "seeded recovery owner inventory failed: \(result.1 ?? "nil")")
      return json(result.0 ?? "{}")["owners"] as? [[String: Any]] ?? []
    }

    let rawLease = open("apple-recovery-raw")
    let rawInventory = owners(rawLease)
    check(rawInventory.count == 1 && rawInventory.first?["prepared"] as? Bool == false && rawInventory.first?["producerFinished"] as? Bool == true,
          "raw fixture must reopen as one completed, unprepared durable owner")
    guard let rawOwner = rawInventory.first?["handleId"] as? String else {
      return check(false, "raw fixture owner has no opaque handle")
    }
    // The TurboModule returns this JSON directly to the shared React Native
    // facade, which parses identity and counts at the root. Keep the real
    // seeded sidecar fixture on that exact wire shape rather than accepting a
    // host-specific wrapper.
    let rawInfoResult: (String?, String?) = waitFor("seeded raw info") { done in
      sessions.scanFailureRecoveryRawInfo(ownerHandle: rawOwner) { value, failure in done((value, failure)) }
    }
    check(rawInfoResult.1 == nil, "seeded raw info failed: \(rawInfoResult.1 ?? "nil")")
    let rawInfo = json(rawInfoResult.0 ?? "{}")
    check((rawInfo["identity"] as? [String: Any])?["host"] as? String != nil && rawInfo["memberCount"] as? NSNumber != nil && rawInfo["tokenCount"] as? NSNumber != nil,
          "raw info must expose identity and counts at the shared React Native root")
    let rawBeforeHead = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 10, offset: "0", maxBytes: "65536")
    check(rawBeforeHead.kind == "bytes" && rawBeforeHead.total == "65536" && rawBeforeHead.bytes?.length == 64 * 1024,
          "unprepared raw source must retain the exact 64 KiB byte bound")
    let null = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 2, offset: "0", maxBytes: "1")
    let boolean = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 3, offset: "0", maxBytes: "1")
    let signed = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 4, offset: "0", maxBytes: "1")
    let unsigned = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 5, offset: "0", maxBytes: "1")
    let float = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 6, offset: "0", maxBytes: "1")
    let map = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 7, offset: "0", maxBytes: "1")
    let array = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 8, offset: "0", maxBytes: "1")
    let text = try! sessions.readReactNativeScanFailureRecoveryRawToken(rawOwner, tokenIndex: 9, offset: "0", maxBytes: "5")
    check(null.kind == "null" && boolean.kind == "bool" && (boolean.value as? NSNumber)?.boolValue == true && signed.value as? String == "-2" && unsigned.value as? String == "3" && float.value as? String == "4607182418800017408" && map.count == "0" && array.count == "0" && text.total == "13" && text.bytes?.length == 5,
          "raw token kinds and bounded metadata changed")

    let lease = open("apple-recovery-two")
    let firstInventory = owners(lease)
    check(firstInventory.count == 2, "fixture must expose two persisted recovery owners")
    guard let firstOwner = firstInventory.first?["handleId"] as? String else {
      return check(false, "first recovery owner has no opaque handle")
    }
    let secondInventory = owners(lease)
    check(secondInventory.count == 2,
          "repeated owner inventory changed the persisted owner count")
    check(secondInventory.first?["handleId"] as? String == firstOwner,
          "repeated owner inventory replaced a live owner alias")

    let head: (String?, String?) = waitFor("seeded recovery head") { done in
      sessions.scanFailureRecoveryHead(lease, handleId: firstOwner, index: 0) { value, failure in done((value, failure)) }
    }
    check(head.1 == nil, "active seeded recovery head was not available: \(head.1 ?? "nil")")
    guard let headId = (json(head.0 ?? "{}")["head"] as? [String: Any])?["handleId"] as? String else {
      return check(false, "active seeded recovery head has no opaque handle")
    }
    _ = owners(lease)
    let page = try! sessions.readReactNativeScanFailureRecoveryPage(headId, offset: "0")
    check(page.bytes.count == 64 * 1024 || page.done || page.nextOffset != "0",
          "active head must remain readable after repeated owner inventory")
    // Selecting another recording revokes every alias from the prior native
    // selection. Even an open callback that races the selection can never
    // use its old handles afterwards.
    do { try sessions.selectScanFailureRecoveryRecording("apple-recovery-empty") }
    catch { return check(false, "could not replace native recovery selection: \(error)") }
    let stale: (String?, String?) = waitFor("stale recovery owner inventory") { done in
      sessions.scanFailureRecoveryOwners(lease) { value, failure in done((value, failure)) }
    }
    check(stale.0 == nil && json(stale.1 ?? "{}")["code"] as? String == "ownership.denied",
          "selection replacement must revoke old recovery lease aliases")

    // Return to the original fixture and retire its active head. The retired
    // head must be reclaimed, while the other owner alias stays usable.
    let reclaimedLease = open("apple-recovery-two")
    let recoveredOwners = owners(reclaimedLease)
    guard let recoveredOwner = recoveredOwners.first?["handleId"] as? String else {
      return check(false, "reopened owner missing after selection replacement")
    }
    let recoveredHead: (String?, String?) = waitFor("reclaimed recovery head") { done in
      sessions.scanFailureRecoveryHead(reclaimedLease, handleId: recoveredOwner, index: 0) { value, failure in done((value, failure)) }
    }
    guard let recoveredHeadId = (json(recoveredHead.0 ?? "{}")["head"] as? [String: Any])?["handleId"] as? String else {
      return check(false, "reopened active head missing")
    }
    var offset = "0"
    while true {
      let recoveredPage = try! sessions.readReactNativeScanFailureRecoveryPage(recoveredHeadId, offset: offset)
      if recoveredPage.done { break }
      check(recoveredPage.nextOffset != offset, "recovered head page did not advance")
      offset = recoveredPage.nextOffset
    }
    let acknowledged: (String?, String?) = waitFor("acknowledge recovered recovery head") { done in
      sessions.scanFailureRecoveryControl(reclaimedLease, handleId: recoveredHeadId, operation: "acknowledge") { value, failure in done((value, failure)) }
    }
    check(acknowledged.1 == nil, "acknowledging drained head failed: \(acknowledged.1 ?? "nil")")
    let retired: (String?, String?) = waitFor("retire recovery head") { done in
      sessions.scanFailureRecoveryControl(reclaimedLease, handleId: recoveredHeadId, operation: "retire") { value, failure in done((value, failure)) }
    }
    check(retired.1 == nil, "retiring active head failed: \(retired.1 ?? "nil")")
    let retiredAgain: (String?, String?) = waitFor("retired recovery head") { done in
      sessions.scanFailureRecoveryControl(reclaimedLease, handleId: recoveredHeadId, operation: "retire") { value, failure in done((value, failure)) }
    }
    check(retiredAgain.0 == nil && json(retiredAgain.1 ?? "{}")["code"] as? String == "ownership.denied",
          "terminal head control must reclaim only its own alias")
    let survivor = owners(reclaimedLease)
    check(survivor.count == 2 && survivor.contains { $0["handleId"] as? String != recoveredOwner },
          "terminal head reclamation must not evict another owner alias")
  }

  /// Exercises the lifecycle paths whose production failures are intentionally
  /// hard to manufacture from a persisted sidecar.  The sessions, adapter,
  /// native selection lock, and opaque alias registry are all real; only the
  /// UniFFI authority response is deterministic.
  func forcedScanRecoveryLifecycleChecks() {
    final class LifecycleBinding: MobileHostBinding {
      let adapter: UnifiedBleRustRadioAdapter
      let authority: MobileScanFailureRecoveryConfigurationAuthority

      init(adapter: UnifiedBleRustRadioAdapter, authority: MobileScanFailureRecoveryConfigurationAuthority) {
        self.adapter = adapter
        self.authority = authority
      }

      func bind(host: MobileCoreHost) -> Bool {
        // Claim both production capabilities so the real host accepts this
        // binding, but deliberately substitute only the recovery authority
        // before publication. The harness never exposes either authority.
        guard let spoolAuthority = host.claimNotificationFailureSpoolConfigurationAuthorityV3(),
              host.claimScanFailureRecoveryConfigurationAuthorityV1() != nil,
              adapter.installNotificationFailureSpoolConfigurationAuthority(spoolAuthority),
              adapter.installScanFailureRecoveryConfigurationAuthority(authority),
              adapter.bind(sink: host) else { return false }
        adapter.installNotificationFailureSpool(host)
        return true
      }
    }

    let cleanupReceipt = LifecycleCleanupReceipt()
    let recovery = LifecycleRecovery(owners: [], cleanup: [cleanupReceipt])
    let authority = LifecycleRecoveryAuthority(recovery: recovery)
    let lifecycleDriver = ScriptedDriver()
    let lifecycleBridge = UnifiedBleRustOrderedGroupBridge(queue: lifecycleDriver.workQueue, factory: { routes, capacity, bytes, notify in
      let source = OwnedOrderedNotificationSource(queue: lifecycleDriver.workQueue, generation: UUID().uuidString)
      let group = try OwnedOrderedNotificationIngress(queue: lifecycleDriver.workQueue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: notify)
      guard source.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      return (source, group)
    }, validateSnapshot: { _, _ in true }, wake: { _, _ in })
    let lifecycleAdapter = UnifiedBleRustRadioAdapter(driver: lifecycleDriver, onRestoredPeer: { _ in }, orderedGroupBridge: lifecycleBridge)
    var lifecycleSessions: UnifiedBleRustCoreSessions!
    lifecycleSessions = UnifiedBleRustCoreSessions(installer: { wake in
      let host = try mobileHostInstallBound(radio: lifecycleAdapter, wake: wake,
        binding: LifecycleBinding(adapter: lifecycleAdapter, authority: authority),
        platform: "apple", owner: "apple-harness-forced-recovery", adapterLabel: "corebluetooth")
      lifecycleSessions.registerProductionRecoveryAdapter(lifecycleAdapter, for: host)
      return host
    })
    guard let root = ProcessInfo.processInfo.environment["UBM_APPLE_SCAN_RECOVERY_DIRECTORY"] else {
      return check(false, "forced lifecycle child has no private recording root")
    }
    do { try lifecycleSessions.installTestingRecordingStorageDirectory(URL(fileURLWithPath: root, isDirectory: true)) }
    catch { return check(false, "could not install forced recovery directory: \(error)") }
    check(lifecycleSessions.ensureHost() == nil, "forced recovery host install failed")

    // The barrier is entered while the production selection lock is held. A
    // concurrent selection must remain blocked through the actual adapter
    // attach; otherwise an A lease could be returned after B is selected.
    let enteredOpen = DispatchSemaphore(value: 0)
    let releaseOpen = DispatchSemaphore(value: 0)
    lifecycleSessions.installTestingScanRecoveryHooks(beforeOpen: {
      enteredOpen.signal()
      check(releaseOpen.wait(timeout: .now() + timeout) == .success,
            "blocked scan recovery open was not released")
    })
    try! lifecycleSessions.selectScanFailureRecoveryRecording("forced-selection-a")
    let openDone = DispatchSemaphore(value: 0)
    let openLock = NSLock()
    var openFailure: Error?
    DispatchQueue.global().async {
      do { _ = try lifecycleSessions.openSelectedScanFailureRecovery() }
      catch { openLock.lock(); openFailure = error; openLock.unlock() }
      openDone.signal()
    }
    check(enteredOpen.wait(timeout: .now() + timeout) == .success, "forced open did not reach selection barrier")
    let selectionDone = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
      do { try lifecycleSessions.selectScanFailureRecoveryRecording("forced-selection-b") }
      catch { check(false, "replacement selection failed: \(error)") }
      selectionDone.signal()
    }
    check(selectionDone.wait(timeout: .now() + .milliseconds(100)) == .timedOut,
          "selection changed while a production open still held its lock")
    releaseOpen.signal()
    check(openDone.wait(timeout: .now() + timeout) == .success, "forced open did not complete")
    openLock.lock(); let observedOpenFailure = openFailure; openLock.unlock()
    check(observedOpenFailure == nil, "forced open failed: \(String(describing: observedOpenFailure))")
    check(selectionDone.wait(timeout: .now() + timeout) == .success, "replacement selection did not resume")
    check(authority.attachedIds() == ["forced-selection-a"],
          "open must attach A before the blocked replacement can select B: \(authority.attachedIds())")
    lifecycleSessions.installTestingScanRecoveryHooks()

    func openLease(_ recordingId: String) -> String {
      do { try lifecycleSessions.selectScanFailureRecoveryRecording(recordingId) }
      catch { check(false, "could not select \(recordingId): \(error)") }
      let result: (String?, String?) = waitFor("forced recovery open") { done in
        lifecycleSessions.scanFailureRecoveryOpen { value, failure in done((value, failure)) }
      }
      check(result.1 == nil, "forced recovery open refused: \(result.1 ?? "nil")")
      guard let lease = json(result.0 ?? "{}")["leaseId"] as? String else {
        check(false, "forced recovery did not return a lease")
        return ""
      }
      return lease
    }

    let cleanupLease = openLease("forced-cleanup")
    let cleanupInventory: (String?, String?) = waitFor("forced cleanup inventory") { done in
      lifecycleSessions.scanFailureRecoveryPendingCleanup(cleanupLease) { value, failure in done((value, failure)) }
    }
    guard let cleanupId = (json(cleanupInventory.0 ?? "{}")["cleanupReceiptIds"] as? [String])?.first else {
      return check(false, "forced cleanup receipt was not exposed")
    }
    var injectCleanupFailure = true
    lifecycleSessions.installTestingScanRecoveryHooks(beforeCleanupRetire: {
      if injectCleanupFailure {
        injectCleanupFailure = false
        throw MobileCoreError.Failed(code: "test.refused", domain: "test",
          operation: "scan.failure-recovery.retire-cleanup", detail: "forced first retirement failure")
      }
    })
    let firstRetire: String? = waitFor("forced first cleanup retirement") { done in
      lifecycleSessions.scanFailureRecoveryRetireCleanup(cleanupLease, handleId: cleanupId, completion: done)
    }
    check(firstRetire != nil && json(firstRetire ?? "{}")["code"] as? String == "test.refused",
          "injected cleanup failure was not surfaced")
    let retryRetire: String? = waitFor("forced cleanup retry") { done in
      lifecycleSessions.scanFailureRecoveryRetireCleanup(cleanupLease, handleId: cleanupId, completion: done)
    }
    check(retryRetire == nil && cleanupReceipt.retireCalls == 1,
          "failed cleanup must retain the exact receipt for one successful retry")
    let terminalRetire: String? = waitFor("terminal cleanup retirement") { done in
      lifecycleSessions.scanFailureRecoveryRetireCleanup(cleanupLease, handleId: cleanupId, completion: done)
    }
    check(terminalRetire != nil && json(terminalRetire ?? "{}")["code"] as? String == "ownership.denied",
          "successful cleanup retry must retire its opaque alias")
    lifecycleSessions.installTestingScanRecoveryHooks()

    // Exercise more than the alias cap across a single lease lifetime.  The
    // first 1024 distinct terminal raw owners must release both the handle
    // and durable-identity entry, allowing the 1025th owner to be inventoried.
    let cappedOwners = (0..<1024).map { LifecycleRecoveryOwner(identifier: "terminal-owner-\($0)") }
    recovery.replaceOwners(cappedOwners)
    let capLease = openLease("forced-cap")
    let firstOwners: (String?, String?) = waitFor("cap owner inventory") { done in
      lifecycleSessions.scanFailureRecoveryOwners(capLease) { value, failure in done((value, failure)) }
    }
    let firstHandles = (json(firstOwners.0 ?? "{}")["owners"] as? [[String: Any]] ?? []).compactMap { $0["handleId"] as? String }
    check(firstOwners.1 == nil && firstHandles.count == 1024, "cap inventory did not expose exactly the alias limit")
    for handle in firstHandles {
      let retired: (String?, String?) = waitFor("terminal raw owner") { done in
        lifecycleSessions.scanFailureRecoveryRawControl(ownerHandle: handle, operation: "retire") { value, failure in done((value, failure)) }
      }
      check(retired.1 == nil, "terminal raw owner was not reclaimed: \(retired.1 ?? "nil")")
    }
    recovery.replaceOwners([LifecycleRecoveryOwner(identifier: "terminal-owner-1024")])
    let nextOwners: (String?, String?) = waitFor("post-cap owner inventory") { done in
      lifecycleSessions.scanFailureRecoveryOwners(capLease) { value, failure in done((value, failure)) }
    }
    let nextHandles = (json(nextOwners.0 ?? "{}")["owners"] as? [[String: Any]] ?? []).compactMap { $0["handleId"] as? String }
    check(nextOwners.1 == nil && nextHandles.count == 1,
          "1025th distinct terminal owner could not reuse reclaimed aliases: \(nextOwners.1 ?? "nil")")
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
    let observerOwner = NSObject()
    let hintLock = NSLock()
    var hints = 0
    lifecycle.addRecoveryObserver(observerOwner) {
      hintLock.lock(); hints += 1; hintLock.unlock()
    }
    func hintCount() -> Int { hintLock.lock(); defer { hintLock.unlock() }; return hints }
    func waitForHints(_ expected: Int) {
      let deadline = Date().addingTimeInterval(10)
      while hintCount() < expected && Date() < deadline { Thread.sleep(forTimeInterval: 0.01) }
      check(hintCount() == expected, "expected \(expected) recovery hints, got \(hintCount())")
    }
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
    let deniedOrdered: (String?, String?) = waitFor("foreign ordered recording receipt") { done in
      lifecycle.orderedRecordingReceiptControl(sessionId: first, id: "foreign", operation: "reserve", ownerToken: ownerB) { done(($0, $1)) }
    }
    check(deniedOrdered.0 == nil && json(deniedOrdered.1 ?? "{}")["code"] as? String == "ownership.denied", "foreign module accessed ordered recording session")
    let ownReserve: (String?, String?) = waitFor("owned ordered recording reserve") { done in
      lifecycle.orderedRecordingReceiptControl(sessionId: first, id: "[\"module-receipt\",\"apple-harness\"]", operation: "reserve", ownerToken: ownerA) { done(($0, $1)) }
    }
    check(ownReserve.1 == nil && json(ownReserve.0 ?? "{}")["ok"] as? Bool == true, "owner could not reserve ordered recording receipt")
    let receipt = json(ownReserve.0 ?? "{}")["value"] as? [String: Any]
    guard let receiptHandle = receipt?["handle"] as? String else { fatalError("reservation omitted its handle") }
    let ownRetire: (String?, String?) = waitFor("owned ordered recording retire") { done in
      lifecycle.orderedRecordingReceiptControl(sessionId: first, id: receiptHandle, operation: "retire", ownerToken: ownerA) { done(($0, $1)) }
    }
    check(ownRetire.1 == nil && json(ownRetire.0 ?? "{}")["ok"] as? Bool == true, "owner could not retire ordered recording receipt")
    gate.failOnce(first)
    lifecycle.closeSessions(ownedBy: ownerA)
    check(gate.waitForAttempts(first, 2), "failed module cleanup was not retried without another module event")
    waitUntilClosed(lifecycle, first)
    waitForHints(1)
    let other: (String?, String?) = waitFor("unrelated owner") { done in
      lifecycle.invoke(sessionId: unaffected, op: "adapter.state", argsJson: "{}") { done(($0, $1)) }
    }
    check(json(other.0 ?? "{}")["ok"] as? Bool == true, "another module's session was disposed")
    let otherClose: String? = waitFor("unrelated close") { done in lifecycle.closeSession(unaffected) { done($0) } }
    check(otherClose == nil, "unrelated close failed")
    check(hintCount() == 2, "explicit successful close must emit a recovery hint")

    let ownerC = NSObject()
    let racing = open(ownerC)
    gate.holdOnce(racing)
    let explicitDone = DispatchSemaphore(value: 0)
    var explicitFailure: String?
    lifecycle.closeSession(racing) { failure in explicitFailure = failure; explicitDone.signal() }
    lifecycle.closeSessions(ownedBy: ownerC)
    check(gate.attempts(racing) == 1, "explicit close and invalidation overlapped native disposal")
    check(hintCount() == 2, "pending disposal emitted a false recovery hint")
    gate.releaseHeldFailure(racing)
    check(explicitDone.wait(timeout: .now() + timeout) == .success, "explicit close did not settle")
    check(json(explicitFailure ?? "{}") ["code"] as? String == "platform.failure", "injected disposal failure was not returned: \(String(describing: explicitFailure))")
    check(gate.waitForAttempts(racing, 2), "racing failed disposal was not retried")
    waitUntilClosed(lifecycle, racing)
    waitForHints(3)
    gate.repeatHeldFailure(racing)
    Thread.sleep(forTimeInterval: 0.05)
    check(gate.attempts(racing) == 2, "stale disposal callback scheduled another retry")
    lifecycle.removeRecoveryObserver(observerOwner)
    let detachedOwner = NSObject()
    let detached = open(detachedOwner)
    let detachedClose: String? = waitFor("detached observer close") { done in lifecycle.closeSession(detached) { done($0) } }
    check(detachedClose == nil && hintCount() == 3, "detached observer received a recovery hint")

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
    admission.addRecoveryObserver(observerOwner) {
      hintLock.lock(); hints += 1; hintLock.unlock()
    }
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
    waitForHints(4)
    admission.removeRecoveryObserver(observerOwner)
  }

  func translationChecks() {
    let snapshot = UnifiedBleRustRadioAdapter.adapterSnapshot(
      ["availability": "available", "authorization": "notDetermined", "power": "unknown", "safeReason": "not yet"]
    )
    check(snapshot.authorization == "not-determined", "authorization vocabulary: \(snapshot)")
    // Native callback reservations survive cancellation until the exact
    // objects finish; duplicate/foreign callbacks cannot retire another slot.
    let discoveryService = CBMutableService(type: CBUUID(string: "180D"), primary: true)
    let foreignService = CBMutableService(type: CBUUID(string: "180D"), primary: false)
    var discovery = PendingDiscovery(operationIdentifier: "old-discovery", completion: { _, _ in })
    discovery.awaitingServices = false
    discovery.includeCallbacks.insert(ObjectIdentifier(discoveryService))
    discovery.characteristicCallbacks.insert(ObjectIdentifier(discoveryService))
    discovery.cancelled = true
    check(!discovery.consumeIncludes(foreignService), "foreign inclusion callback has no admission")
    check(discovery.consumeIncludes(discoveryService), "exact canceled inclusion callback retires")
    check(!discovery.consumeIncludes(discoveryService), "duplicate callback cannot retire twice")
    check(!discovery.isDrained, "characteristic callback still owns discovery")
    check(discovery.consumeCharacteristics(discoveryService), "exact characteristic callback retires")
    check(discovery.isDrained, "canceled discovery releases only after all callback owners finish")
    // Invalidated child objects need not deliver another CoreBluetooth callback.
    // Retire only their reservations; valid-service work still owns its slot.
    let staleCharacteristic = CBMutableCharacteristic(type: CBUUID(string: "2A37"), properties: [.read], value: nil, permissions: [.readable])
    var invalidated = PendingDiscovery(operationIdentifier: "invalidated-discovery", completion: { _, _ in })
    invalidated.awaitingServices = false
    invalidated.includeCallbacks.insert(ObjectIdentifier(discoveryService))
    invalidated.characteristicCallbacks.insert(ObjectIdentifier(discoveryService))
    invalidated.descriptorCallbacks[ObjectIdentifier(staleCharacteristic)] = ObjectIdentifier(discoveryService)
    invalidated.characteristicCallbacks.insert(ObjectIdentifier(foreignService))
    invalidated.retireInvalidatedServices([discoveryService])
    check(!invalidated.consumeIncludes(discoveryService), "late invalidated inclusion has no reservation")
    check(!invalidated.consumeCharacteristics(discoveryService), "late invalidated characteristics have no reservation")
    check(!invalidated.consumeDescriptors(staleCharacteristic), "late invalidated descriptors have no reservation")
    check(!invalidated.isDrained, "unaffected service callback remains owned")
    check(invalidated.consumeCharacteristics(foreignService), "unaffected callback can drain")
    check(invalidated.isDrained, "invalidation drains without missing child callbacks")
    var replacement = PendingDiscovery(operationIdentifier: "replacement-discovery", completion: { _, _ in })
    replacement.awaitingServices = false
    replacement.characteristicCallbacks.insert(ObjectIdentifier(foreignService))
    check(!replacement.consumeCharacteristics(discoveryService), "old same-UUID callback cannot consume replacement admission")
    check(replacement.consumeCharacteristics(foreignService), "replacement exact object remains admitted")
    let invalidationRadio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    var invalidationCompletions = 0
    var pendingInvalidation = PendingDiscovery(operationIdentifier: "invalidation", completion: { value, failure in
      check(value == nil && failure?.code == 1026, "invalidation reports original service-change failure")
      invalidationCompletions += 1
    })
    pendingInvalidation.awaitingServices = false
    pendingInvalidation.includeCallbacks.insert(ObjectIdentifier(discoveryService))
    pendingInvalidation.characteristicCallbacks.insert(ObjectIdentifier(discoveryService))
    pendingInvalidation.descriptorCallbacks[ObjectIdentifier(staleCharacteristic)] = ObjectIdentifier(discoveryService)
    invalidationRadio.pendingDiscovery["invalidation-peer"] = pendingInvalidation
    var cancellationDebt = PendingCancellationCleanup()
    cancellationDebt.discoveryPeers.insert("invalidation-peer")
    invalidationRadio.pendingCancellationCleanup["invalidation"] = cancellationDebt
    invalidationRadio.invalidateDiscovery("invalidation-peer", services: [discoveryService])
    check(invalidationCompletions == 1, "invalidation completion delivered once")
    check(invalidationRadio.pendingDiscovery["invalidation-peer"] == nil, "invalidated callbacks cannot block rediscovery")
    check(invalidationRadio.pendingCancellationCleanup["invalidation"] == nil, "invalidated callback cleanup debt retires")
    invalidationRadio.invalidateDiscovery("invalidation-peer", services: [discoveryService])
    check(invalidationCompletions == 1, "duplicate invalidation cannot complete twice")
    let graph = UnifiedBleRustRadioAdapter.services(["services": [
      ["uuid": hrService, "occurrence": 0, "primary": true,
       "includedServices": [["uuid": hrService, "occurrence": 1]], "characteristics": []],
      ["uuid": hrService, "occurrence": 1, "primary": false,
       "includedServices": [], "characteristics": []],
      ["uuid": "0000180f-0000-1000-8000-00805f9b34fb", "occurrence": 0,
       "primary": NSNull(), "includedServices": NSNull(), "characteristics": []]
    ]] as NSDictionary)
    check(graph?.count == 3, "graph service occurrences")
    check(graph?[0].primary == true, "primary service observed")
    check(graph?[0].includedServices?.first?.occurrence == 1, "duplicate UUID inclusion identity")
    check(graph?[1].primary == false && graph?[1].includedServices?.count == 0, "secondary and observed empty")
    check(graph?[2].primary == nil && graph?[2].includedServices == nil, "unknown graph facts")
    func readinessKind(_ authorization: String, _ power: String, _ availability: String = "available") -> String? {
      let adapter = MobileAdapterSnapshot(availability: availability, authorization: authorization, power: power, safeReason: nil)
      guard case let .failed(kind, _, _, _, _, dispatched)? = UnifiedBleRustRadioAdapter.readinessFailure(adapter) else { return nil }
      check(!dispatched, "a readiness refusal never reaches CoreBluetooth")
      return kind
    }
    check(readinessKind("not-determined", "on") == nil, "powered-on scoped authorization must not invent a global refusal")
    check(readinessKind("unknown", "on") == nil, "unknown authorization is not a denial")
    check(readinessKind("not-determined", "unknown") == "permission-not-determined", "legacy uninitialized permission preparation remains explicit")
    check(readinessKind("not-determined", "off") == "adapter-off", "nonblocking global authorization does not hide actual powered-off")
    check(readinessKind("not-determined", "unsupported") == "adapter-unavailable", "nonblocking global authorization does not hide actual unsupported")
    check(readinessKind("restricted", "on") == "permission-restricted", "restricted")
    check(readinessKind("denied", "on") == "permission-denied", "denied")
    check(readinessKind("granted", "resetting") == "adapter-resetting", "resetting")
    check(readinessKind("granted", "unknown") == "adapter-unavailable", "state unknown")
    check(readinessKind("granted", "unsupported", "unsupported") == "adapter-unavailable", "unsupported")
    check(readinessKind("granted", "off") == "adapter-off", "off")
    check(readinessKind("granted", "on") == nil, "ready")
    let scopedRefusal = OwnedCoreBluetoothProtocolRadioSupport.operationReadinessFailure(state: .unauthorized)!
    guard case let .failed(scopedKind, _, scopedDomain, scopedCode, _, scopedDispatched) = UnifiedBleRustRadioAdapter.failure(scopedRefusal, verb: .connect) else {
      return check(false, "measured scoped unauthorized state must refuse")
    }
    check(scopedKind == "permission-denied" && scopedDomain == "CoreBluetooth.CBManagerState"
          && scopedCode == 3 && !scopedDispatched, "scoped state identity and admission phase must survive")
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

  func explicitRadioAdmissionChecks() {
    final class Sink: UnifiedBleRustRadioSink {
      let answered = DispatchSemaphore(value: 0)
      func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
        answered.signal()
        return "delivered"
      }
      func ingest(ingress: MobileRadioIngress) -> String { "accepted" }
    }
    let driver = ScriptedDriver()
    driver.holdPreparation = true
    driver.snapshot = ["availability": "available", "authorization": "notDetermined", "power": "on"]
    let adapter = UnifiedBleRustRadioAdapter(driver: driver)
    let sink = Sink()
    adapter.bind(sink: sink)
    adapter.submit(request: .connect(id: 990, peerId: "ASK", autoConnect: false, preferredPhy: [], expectedGeneration: "apple-harness-generation"))
    check(driver.preparationAdmitted.wait(timeout: .now() + timeout) == .success, "connect preparation missing")
    check(driver.onQueue { driver.calls.isEmpty }, "connect must not run before observed initial state")
    driver.onQueue { driver.preparations.removeValue(forKey: driver.preparationIdentifier(990))?(driver.snapshot, nil) }
    check(sink.answered.wait(timeout: .now() + timeout) == .success, "scoped ready connect not answered")
    check(driver.onQueue { driver.calls == ["connect ASK"] }, "actual poweredOn permits nonblocking global authorization")
    adapter.submit(request: .connect(id: 991, peerId: "LATE", autoConnect: false, preferredPhy: [], expectedGeneration: "apple-harness-generation"))
    check(driver.preparationAdmitted.wait(timeout: .now() + timeout) == .success, "cancel preparation missing")
    adapter.cancel(requestId: 991)
    check(sink.answered.wait(timeout: .now() + timeout) == .success, "cancel did not answer original request")
    driver.onQueue { driver.preparations.removeValue(forKey: driver.preparationIdentifier(991))?(driver.snapshot, nil) }
    check(driver.onQueue { driver.calls == ["connect ASK"] }, "late state must not connect after Rust cancellation")
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
      .associateCompanion(id: 904, name: "Polar", serviceUuid: nil, filtersJson: nil),
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
    if ProcessInfo.processInfo.environment["UBM_APPLE_CAPTURE_RETIRE_ASYNC_ONLY"] == "1" {
      Harness().orderedCaptureRetirementCompletionChecks()
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_FIFTH_FAILURE_ONLY"] == "1" {
      Harness().directFifthFailureReceiptCheck()
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_DURABLE_FAILURE_SPOOL_ONLY"] == "1" {
      Harness().durableFailureOverflowSpoolCheck()
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_GLOBAL_DURABLE_FAILURE_SPOOL_ONLY"] == "1" {
      Harness().globalDurableFailureSpoolInterleavingCheck()
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_V4_RESTART_FAILURE_SPOOL_ONLY"] == "1" {
      Harness().v4RestartReplaysPendingReceiptBeforeFinalConfirmationCheck()
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_SPOOL_RECONFIGURATION_ONLY"] == "1" {
      Harness().notificationFailureSpoolReconfigurationFenceCheck()
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_FIFTH_RETIREMENT_ONLY"] == "1" {
      Harness().fifthFailureRetirementRetryCheck()
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_ROUTE_RECEIPT_ONLY"] == "1" {
      Harness().notificationRouteAdmissionReceiptChecks()
      print("[AppleRustRadioAdapterHarness] route receipt admission checks passed (scripted radio).")
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_ORDERED_TYPED_ONLY"] == "1" {
      Harness().orderedTypedRadioChecks()
      Harness().orderedVersionedReplacementChecks()
      print("[AppleRustRadioAdapterHarness] ordered typed adapter checks passed (scripted radio).")
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_SCAN_RECOVERY_ONLY"] == "1" {
      Harness().seededScanRecoveryLifecycleChecks()
      print("[AppleRustRadioAdapterHarness] seeded scan recovery lifecycle checks passed (durable fixture; scripted radio).")
      return
    }
    if ProcessInfo.processInfo.environment["UBM_APPLE_SCAN_RECOVERY_LIFECYCLE_ONLY"] == "1" {
      Harness().forcedScanRecoveryLifecycleChecks()
      print("[AppleRustRadioAdapterHarness] forced scan recovery lifecycle checks passed (scripted authority; production session and adapter boundaries).")
      return
    }
    // JournalRegistry has one immutable private directory per process. The
    // runner invokes this scenario in a fresh child so it can own its temp
    // directory without weakening that production invariant.
    if ProcessInfo.processInfo.environment["UBM_APPLE_DURABLE_SETUP_ONLY"] == "1" {
      let harness = Harness()
      harness.installDurableSetupHost()
      harness.durableSetupContinuationChecks()
      print("[AppleRustRadioAdapterHarness] durable setup recording checks passed (fresh process; scripted radio).")
      return
    }
    Harness().notificationRouteAdmissionReceiptChecks()
    Harness().durableFailureOverflowSpoolCheck()
    Harness().globalDurableFailureSpoolInterleavingCheck()
    Harness().v4RestartReplaysPendingReceiptBeforeFinalConfirmationCheck()
    Harness().notificationFailureSpoolReconfigurationFenceCheck()
    Harness().acknowledgedFailureReceiptChecks()
    Harness().notificationFailureRetentionChecks()
    if ProcessInfo.processInfo.environment["UBM_APPLE_FAILURE_ONLY"] == "1" {
      print("[AppleRustRadioAdapterHarness] captured failure retention checks passed (scripted radio).")
      return
    }
    Harness().lateConnectCompletionCannotFinishReusedRequest()
    Harness().lateReadCompletionCannotFinishReusedRequest()
    Harness().cancelledDeferredRequestCannotRunReplacement()
    Harness().duplicateRequestNumbersHaveDistinctRadioIdentities()
    Harness().orderedV2ControlChecks()
    Harness().ownerFenceControlChecks()
    Harness().ownerFenceQueuedBindingChecks()
    Harness().ownerFenceAttachmentCallbackChecks()
    Harness().ownerFenceCachedRadioBorrowerChecks()
    Harness().ownerFenceGraphCleanupChecks()
    Harness().ownerFencePhysicalCleanupChecks()
    Harness().ownerFenceHeldCallbackAndIsolationChecks()
    Harness().ownerFenceDuringCallbackRelayChecks()
    Harness().sharedBridgeKeepsEachAdaptersWakeRoute()
    Harness().bindingReplacementDoesNotStealCompletion()
    if ProcessInfo.processInfo.environment["UBM_APPLE_SLOT_ONLY"] == "1" {
      print("[AppleRustRadioAdapterHarness] v2 slot adapter checks passed (scripted radio).")
      return
    }
    // Cancellation from another thread must win while admission waits in the
    // driver queue; a claimed admission remains claimed on repeated cancel.
    let gate = UnifiedBleRustRadioAdapter.MarkerAdmissionGate()
    let queue = DispatchQueue(label: "marker.cancel.held")
    let entered = DispatchSemaphore(value: 0)
    let release = DispatchSemaphore(value: 0)
    let done = DispatchSemaphore(value: 0)
    var writes = 0
    queue.async {
      entered.signal()
      release.wait()
      do {
        _ = try OwnedOrderedWriteDispatch.start(queue: queue, admit: {
          guard gate.claim() else { throw NSError(domain: "test.cancel", code: 1) }
          return 0
        }, write: { _ in writes += 1 }, completion: { _ in })
        preconditionFailure("cancelled marker was admitted")
      } catch {
        precondition(writes == 0)
      }
      done.signal()
    }
    entered.wait()
    gate.cancel()
    gate.cancel()
    release.signal()
    done.wait()
    let admitted = UnifiedBleRustRadioAdapter.MarkerAdmissionGate()
    precondition(admitted.claim())
    admitted.cancel()
    precondition(admitted.claim())
    Harness().run()
    if ProcessInfo.processInfo.environment["UBM_APPLE_522_ONLY"] != "1" {
      print("[AppleRustRadioAdapterHarness] Rust host ↔ Swift adapter scripted exchange passed (deterministic; no physical radio).")
    }
  }
}
