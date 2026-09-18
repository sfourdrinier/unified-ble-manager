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
  var hangingConnects = [String: (NSError?) -> Void]()

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
      completion(nil)
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
      self.adapter = UnifiedBleRustRadioAdapter(driver: self.driver)
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

    // Process shutdown disables every CCCD the radio still holds.
    let cleanup = json(host.shutdown())
    check(cleanup["state"] != nil, "shutdown cleanup record: \(cleanup)")
    let counters: UnifiedBleRustRadioAdapterCounters = waitFor("adapter counters") { self.adapter.adapterCounters(completion: $0) }
    check(counters.mismatchedCompletions == 0, "Rust refused an adapter answer shape: \(counters)")
    check(counters.cancelledRequests >= 1, "cancel was not counted: \(counters)")
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
