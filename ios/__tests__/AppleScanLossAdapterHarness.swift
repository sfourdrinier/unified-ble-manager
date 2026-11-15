// Production CoreBluetooth radio and Swift adapter with deterministic sinks; no BLE.
import CoreBluetooth
import Foundation
import ObjectiveC.runtime

private final class ScanCentral: CBCentralManager {
  override var state: CBManagerState { .poweredOn }
  override func scanForPeripherals(withServices serviceUUIDs: [CBUUID]?, options: [String: Any]? = nil) {}
  override func stopScan() {}
}
private final class ScanPeer: CBPeripheral {
  override var identifier: UUID { UUID(uuidString: "00000000-0000-0000-0000-000000000522")! }
  override var delegate: CBPeripheralDelegate? { get { nil } set {} }
}
private class LegacySink: UnifiedBleRustRadioSink {
  var acceptedAdvertisements = 0
  var rejectAdvertisements = false
  func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String { "delivered" }
  func ingest(ingress: MobileRadioIngress) -> String {
    if case .advertisement = ingress {
      if rejectAdvertisements { return "dropped-advertisement" }
      acceptedAdvertisements += 1
    }
    return "accepted"
  }
}
private final class LossSink: LegacySink, UnifiedBleRustScanLossSink {
  var acceptLoss = false
  var attempts: [OwnedScanLoss] = []
  let acceptedLoss = DispatchSemaphore(value: 0)
  func ingestScanLoss(_ loss: OwnedScanLoss) -> String {
    attempts.append(loss)
    if acceptLoss { acceptedLoss.signal(); return "accepted" }
    return "dropped-control"
  }
}

@main enum AppleScanLossAdapterHarness {
  static func main() {
    guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
          let peer = class_createInstance(ScanPeer.self, 0) as? ScanPeer else { fatalError("fixture") }
    _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peer)
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let adapter = UnifiedBleRustRadioAdapter(driver: radio)
    let sink = LossSink()
    precondition(adapter.bind(sink: sink))
    radio.queue.sync {
      radio.central = central; radio.delegate = adapter
      var failure: NSError?
      radio.startScanOnQueue(owner: adapter, route: adapter, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") { failure = $0 }
      precondition(failure == nil)
      let generation = radio.scanLeases[ObjectIdentifier(adapter)]!.generation
      for _ in 0..<1026 { radio.centralManager(central, didDiscover: peer, advertisementData: [:], rssi: -40) }
      radio.drainScanCallbacks()
      precondition(sink.acceptedAdvertisements == 1024 && sink.attempts.last?.exactCount == 2)
      precondition(sink.attempts.last?.generation == generation && radio.pendingScanLosses.count == 1)
      sink.acceptLoss = true
    }
    precondition(sink.acceptedLoss.wait(timeout: .now() + 5) == .success, "loss must retry without another advertisement")
    radio.queue.sync {
      var failure: NSError?
      precondition(radio.pendingScanLosses.isEmpty && sink.attempts.last?.exactCount == 2)
      sink.rejectAdvertisements = true
      radio.centralManager(central, didDiscover: peer, advertisementData: [:], rssi: -40)
      radio.drainScanCallbacks()
      precondition(sink.attempts.last?.exactCount == 1 && radio.pendingScanLosses.isEmpty)
      radio.stopScanOnQueue(owner: adapter) { failure = $0 }
      precondition(failure == nil)
    }

    let legacyRadio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let legacyAdapter = UnifiedBleRustRadioAdapter(driver: legacyRadio)
    let legacy = LegacySink()
    precondition(legacyAdapter.bind(sink: legacy))
    legacyRadio.queue.sync {
      legacyRadio.central = central; legacyRadio.delegate = legacyAdapter
      var failure: NSError?
      legacyRadio.startScanOnQueue(owner: legacyAdapter, route: legacyAdapter, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "legacy") { failure = $0 }
      precondition(failure == nil)
      for _ in 0..<1025 { legacyRadio.centralManager(central, didDiscover: peer, advertisementData: [:], rssi: -40) }
      legacyRadio.drainScanCallbacks()
      precondition(legacy.acceptedAdvertisements == 1024)
      precondition(legacyRadio.pendingScanLosses[ObjectIdentifier(legacyAdapter)]?.exactCount == 1,
        "legacy binding must retain loss rather than claim delivery")
    }
    AppleBorrowerScanAdapterHarness.main()
    print("Apple scan-loss adapter ingress passed (deterministic; no physical radio)")
  }
}

private var starts = 0
private var stops = 0
private final class BorrowerScanCentral: CBCentralManager {
  override var state: CBManagerState { .poweredOn }
  override func scanForPeripherals(withServices: [CBUUID]?, options: [String: Any]? = nil) { starts += 1 }
  override func stopScan() { stops += 1 }
}
private final class BorrowerScanPeer: CBPeripheral {
  override var identifier: UUID { UUID(uuidString: "00000000-0000-0000-0000-000000000522")! }
  override var delegate: CBPeripheralDelegate? { get { nil } set {} }
}
private final class BorrowerScanSink: UnifiedBleRustScanLossSink {
  let done = DispatchSemaphore(value: 0)
  var answers: [UInt64: MobileRadioCompletion] = [:]
  var advertisements = 0
  var acceptLoss = true
  func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String {
    answers[requestId] = completion; done.signal(); return "delivered"
  }
  func ingest(ingress: MobileRadioIngress) -> String {
    if case .advertisement = ingress { advertisements += 1 }
    return "accepted"
  }
  func ingestScanLoss(_ loss: OwnedScanLoss) -> String { acceptLoss ? "accepted" : "dropped-control" }
  func wait(_ id: UInt64) -> MobileRadioCompletion {
    precondition(done.wait(timeout: .now() + 10) == .success, "missing adapter completion")
    return answers[id]!
  }
}

private enum AppleBorrowerScanAdapterHarness {
  static func main() {
    guard let central = class_createInstance(BorrowerScanCentral.self, 0) as? BorrowerScanCentral,
          let peer = class_createInstance(BorrowerScanPeer.self, 0) as? BorrowerScanPeer else { fatalError("fixture") }
    _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peer)
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let first = UnifiedBleRustRadioAdapter(driver: radio)
    let second = UnifiedBleRustRadioAdapter(driver: radio)
    let a = BorrowerScanSink(), b = BorrowerScanSink()
    radio.queue.sync { radio.central = central }
    precondition(first.bind(sink: a) && second.bind(sink: b))
    precondition(radio.attachDelegateIfAvailable(first))
    precondition(!radio.attachDelegateIfAvailable(second), "public multi-borrower guard must remain closed")
    // Exercise the internal foundation without opening the public guard.
    radio.queue.sync { precondition(radio.borrowerRelease.attach(second)) }
    let operationDone = DispatchSemaphore(value: 0)
    var operationFailure: NSError?
    func start(_ adapter: UnifiedBleRustRadioAdapter, _ id: String) {
      adapter.startOwnedScan(serviceUUIDs: [], operationIdentifier: id) { operationFailure = $0; operationDone.signal() }
      precondition(operationDone.wait(timeout: .now() + 10) == .success && operationFailure == nil, "scan start failed")
    }
    func stop(_ adapter: UnifiedBleRustRadioAdapter, _ id: String) {
      adapter.stopOwnedScan(operationIdentifier: id) { operationFailure = $0; operationDone.signal() }
      precondition(operationDone.wait(timeout: .now() + 10) == .success && operationFailure == nil, "scan stop failed")
    }
    start(first, "1"); start(second, "2")
    radio.queue.sync {
      precondition(radio.scanLeases[ObjectIdentifier(first)] != nil && radio.scanLeases[ObjectIdentifier(second)] != nil)
      radio.centralManager(central, didDiscover: peer, advertisementData: [:], rssi: -40)
      radio.drainScanCallbacks()
      precondition(a.advertisements == 1 && b.advertisements == 1)
    }
    let cancellationDone = DispatchSemaphore(value: 0)
    radio.cancelOperation("2", borrower: second) { _ in cancellationDone.signal() }
    precondition(cancellationDone.wait(timeout: .now() + 10) == .success)
    radio.queue.sync {
      precondition(radio.scanLeases[ObjectIdentifier(first)] != nil && radio.scanLeases[ObjectIdentifier(second)] == nil,
        "cancellation must use the requesting borrower, not the legacy delegate")
    }
    start(second, "2-retry")
    stop(first, "3")
    radio.queue.sync {
      precondition(radio.scanLeases[ObjectIdentifier(first)] == nil && radio.scanLeases[ObjectIdentifier(second)] != nil && stops == 0)
    }
    start(first, "4")
    let done = DispatchSemaphore(value: 0)
    var failure: NSError?
    radio.queue.sync {
      a.acceptLoss = false
      radio.centralManager(central, didDiscover: peer,
        advertisementData: [CBAdvertisementDataManufacturerDataKey: Data(repeating: 1, count: OwnedCoreBluetoothProtocolRadio.scanCallbackByteLimit + 1)], rssi: -40)
    }
    radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
    precondition(done.wait(timeout: .now() + 10) == .success && failure?.code == 1014)
    radio.queue.sync {
      precondition(radio.pendingScanLosses[ObjectIdentifier(first)] != nil)
      precondition(radio.scanLeases[ObjectIdentifier(second)] != nil && stops == 0)
      a.acceptLoss = true
    }
    // Hold the production queue while release, a late stop and a late start
    // are admitted in that order. Neither late request may affect B.
    let held = DispatchSemaphore(value: 0), resume = DispatchSemaphore(value: 0)
    radio.queue.async { held.signal(); resume.wait() }
    precondition(held.wait(timeout: .now() + 10) == .success)
    radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
    var heldStopFailure: NSError?
    let heldStopDone = DispatchSemaphore(value: 0)
    first.stopOwnedScan(operationIdentifier: "held-stop") { heldStopFailure = $0; heldStopDone.signal() }
    first.startOwnedScan(serviceUUIDs: [], operationIdentifier: "held-start") { operationFailure = $0; operationDone.signal() }
    resume.signal()
    precondition(done.wait(timeout: .now() + 10) == .success && failure?.code == 1014,
      "non-scan teardown must fail closed while multiple borrowers exist")
    precondition(heldStopDone.wait(timeout: .now() + 10) == .success && heldStopFailure?.code == 1004)
    precondition(operationDone.wait(timeout: .now() + 10) == .success && operationFailure?.code == 1014)
    radio.queue.sync {
      precondition(radio.scanLeases[ObjectIdentifier(first)] == nil && radio.scanLeases[ObjectIdentifier(second)] != nil)
      precondition(radio.pendingScanLosses[ObjectIdentifier(first)] == nil && stops == 0)
      radio.centralManager(central, didDiscover: peer, advertisementData: [:], rssi: -40)
      radio.drainScanCallbacks()
      precondition(a.advertisements == 1 && b.advertisements == 2)
    }
    first.startOwnedScan(serviceUUIDs: [], operationIdentifier: "5") { operationFailure = $0; operationDone.signal() }
    precondition(operationDone.wait(timeout: .now() + 10) == .success && operationFailure?.code == 1014,
      "retiring borrower started a scan")
    stop(second, "6")
    radio.queue.sync { precondition(stops == 1) }
    print("Production adapter borrower scan routing and release retry passed (synthetic; no BLE)")
  }
}
