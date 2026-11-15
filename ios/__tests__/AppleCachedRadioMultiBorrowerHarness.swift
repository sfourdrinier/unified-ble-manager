// Deterministic production radio boundary; does not allocate a central or use BLE.
import CoreBluetooth
import Foundation
import ObjectiveC.runtime

private let peerID = UUID(uuidString: "00000000-0000-0000-0000-000000000522")!
private let otherPeerID = UUID(uuidString: "00000000-0000-0000-0000-000000000523")!
private var fixtureService: CBService?
private var notifying: [ObjectIdentifier: Bool] = [:]
private var physicalWrites: [(ObjectIdentifier, Bool)] = []
private var scanStarts: [[String]?] = []
private var scanStops = 0
private var cancelledConnections = 0
private var startedConnections = 0
private var discoveryStarts = 0
private var connectionState: CBPeripheralState = .connected
private var otherConnectionState: CBPeripheralState = .connected
private var cancelledPeers: [UUID] = []
private var scanState: CBManagerState = .poweredOn
private var restoredScanActive = false
private var writeReady = true
private var characteristicReads = 0
private final class ScanCentral: CBCentralManager {
  override var state: CBManagerState { scanState }
  override var isScanning: Bool { restoredScanActive }
  override func scanForPeripherals(withServices serviceUUIDs: [CBUUID]?, options: [String: Any]? = nil) { scanStarts.append(serviceUUIDs?.map { $0.uuidString }.sorted()) }
  override func stopScan() { scanStops += 1 }
  override func connect(_ peripheral: CBPeripheral, options: [String: Any]? = nil) { startedConnections += 1 }
  override func cancelPeripheralConnection(_ peripheral: CBPeripheral) { cancelledConnections += 1; cancelledPeers.append(peripheral.identifier) }
}
private final class Peripheral: CBPeripheral {
  override var identifier: UUID { peerID }
  override var state: CBPeripheralState { connectionState }
  override var delegate: CBPeripheralDelegate? { get { nil } set {} }
  override var services: [CBService]? { [] }
  override func discoverServices(_ serviceUUIDs: [CBUUID]?) { discoveryStarts += 1 }
  override var canSendWriteWithoutResponse: Bool { writeReady }
  override func maximumWriteValueLength(for type: CBCharacteristicWriteType) -> Int { 2 }
  override func readRSSI() {}
  override func readValue(for characteristic: CBCharacteristic) { characteristicReads += 1 }
  override func readValue(for descriptor: CBDescriptor) {}
  override func writeValue(_ data: Data, for descriptor: CBDescriptor) {}
  override func writeValue(_ data: Data, for characteristic: CBCharacteristic, type: CBCharacteristicWriteType) {}
  override func setNotifyValue(_ enabled: Bool, for characteristic: CBCharacteristic) {
    physicalWrites.append((ObjectIdentifier(characteristic), enabled))
  }
}
private final class OtherPeripheral: CBPeripheral {
  override var identifier: UUID { otherPeerID }
  override var state: CBPeripheralState { otherConnectionState }
  override var delegate: CBPeripheralDelegate? { get { nil } set {} }
  override var services: [CBService]? { [] }
  override func discoverServices(_ serviceUUIDs: [CBUUID]?) { discoveryStarts += 1 }
}
private final class Characteristic: CBMutableCharacteristic {
  override var service: CBService? { fixtureService }
  override var isNotifying: Bool { notifying[ObjectIdentifier(self)] ?? false }
}

private final class Borrower: NSObject, OwnedCoreBluetoothProtocolRadioDelegate, OwnedScanLossReceiver, OwnedScanAdvertisementReceiver, OwnedDeferredNotificationFailureReceiver, OwnedNotificationFailureRetention {
  var values: [String] = []
  var disconnects: [String] = []
  var serviceChanges: [String] = []
  var failures: [String] = []
  var failureErrors: [NSError] = []
  var advertisements = 0
  var records: [NSDictionary] = []
  var states = 0
  var readiness: [String] = []
  var lostAdvertisements = 0
  var refuseLoss = false
  var lossGenerations: [UUID] = []
  var refusedLossAttempts = 0
  var rejectAdvertisements = false
  var unknownLosses = 0
  var onLoss: (() -> Void)?
  var onState: (() -> Void)?
  var refuseFailureCapture = false
  var failureCaptures = 0
  var onFailure: (() -> Void)?
  var pendingFailureRetention = false
  func hasPendingNotificationFailureRetention() -> Bool { pendingFailureRetention }
  func receiveScanAdvertisement(_ advertisement: NSDictionary, generation: UUID) -> Bool {
    if rejectAdvertisements { return false }
    protocolRadioDidReceiveAdvertisement(advertisement)
    return true
  }
  func receiveScanLoss(_ loss: OwnedScanLoss) -> Bool {
    if refuseLoss { refusedLossAttempts += 1; return false }
    lossGenerations.append(loss.generation)
    if loss.exactCount == nil { unknownLosses += 1 }
    lostAdvertisements += Int(loss.exactCount ?? 0)
    onLoss?()
    return true
  }
  func protocolRadioDidUpdateAdapterState(_ snapshot: NSDictionary) { states += 1; onState?() }
  func protocolRadioDidReceiveAdvertisement(_ advertisement: NSDictionary) { advertisements += 1; records.append(advertisement) }
  func protocolRadioDidDisconnectPeer(_ peerIdentifier: String, error: NSError?) { disconnects.append(peerIdentifier) }
  func protocolRadioDidModifyServices(_ peerIdentifier: String) { serviceChanges.append(peerIdentifier) }
  func protocolRadioDidReceiveNotification(_ subscriptionIdentifier: String, value: NSData) { values.append(subscriptionIdentifier) }
  @objc func protocolRadioDidFailNotification(_ subscriptionIdentifier: String, error: NSError) { failures.append(subscriptionIdentifier); failureErrors.append(error); onFailure?() }
  func captureNotificationFailure(_ identifier: String) -> ((NSError) -> Void)? {
    failureCaptures += 1
    if refuseFailureCapture { return nil }
    return { [self] error in protocolRadioDidFailNotification(identifier, error: error) }
  }
  @objc func protocolRadioDidBecomeReadyToSendWriteWithoutResponse(_ peerIdentifier: String) { readiness.append(peerIdentifier) }
}

// Deliberately does not implement the deferred-capture protocol.
private final class LegacyFailureBorrower: NSObject, OwnedCoreBluetoothProtocolRadioDelegate {
  var errors: [NSError] = []
  func protocolRadioDidUpdateAdapterState(_ snapshot: NSDictionary) {}
  func protocolRadioDidReceiveAdvertisement(_ advertisement: NSDictionary) {}
  func protocolRadioDidDisconnectPeer(_ peerIdentifier: String, error: NSError?) {}
  func protocolRadioDidModifyServices(_ peerIdentifier: String) {}
  func protocolRadioDidReceiveNotification(_ subscriptionIdentifier: String, value: NSData) {}
  @objc func protocolRadioDidFailNotification(_ subscriptionIdentifier: String, error: NSError) { errors.append(error) }
}

@main enum AppleCachedRadioMultiBorrowerHarness {
  static func main() {
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let first = Borrower()
    let second = Borrower()
    if CommandLine.arguments.contains("--notification-join-stale-claim") ||
       CommandLine.arguments.contains("--notification-join-stale-route") ||
       CommandLine.arguments.contains("--notification-first-stale-join-current") {
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0)
      var firstAnswer: NSError??, secondAnswer: NSError??
      radio.queue.sync {
        radio.connectionClaims[peer] = [
          ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)
        ]
        precondition(radio.installNotificationRoute("first", borrower: first))
        precondition(radio.installNotificationRoute("second", borrower: second))
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "first",
          enabled: true, completion: { firstAnswer = .some($0) }, borrower: first,
          claimIdentity: radio.connectionClaims[peer]![ObjectIdentifier(first)]!.identity)
        precondition(radio.queueNotificationJoin(address: address, subscriptionIdentifier: "second",
          operationIdentifier: "same", completion: { secondAnswer = .some($0) }))
        let staleFirst = CommandLine.arguments.contains("--notification-first-stale-join-current")
        if staleFirst {
          radio.connectionClaims[peer]?[ObjectIdentifier(first)] = .init(owner: first, route: first, pending: nil)
        } else if CommandLine.arguments.contains("--notification-join-stale-route") {
          let originalIdentity = radio.notificationRoutes["second"]!.identity
          radio.removeNotificationRoute("second", borrower: second)
          precondition(radio.installNotificationRoute("second", borrower: second))
          precondition(radio.notificationRoutes["second"]!.identity != originalIdentity,
            "a replacement route must have a new lifetime identity even for the same borrower and identifier")
        } else {
          radio.connectionClaims[peer]?[ObjectIdentifier(second)] = .init(owner: second, route: second, pending: nil)
        }
        radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nil)
        if staleFirst {
          precondition(firstAnswer != nil && firstAnswer! != nil && secondAnswer != nil && secondAnswer! == nil)
          precondition(radio.subscriptions[address] == "second", "the current joined claim must retain the shared enable")
        } else {
          precondition(firstAnswer != nil && firstAnswer! == nil)
          precondition(secondAnswer != nil && secondAnswer! != nil, "stale join must fail its own claim")
          precondition(radio.subscriptions[address] == "first" && radio.sharedSubscriptions[address] == nil,
            "stale join must not acquire the replacement generation")
        }
      }
      print("Notification stale join fenced"); return
    }
    if CommandLine.arguments.contains("--notification-cancel-stale-claim") {
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0)
      radio.queue.sync {
        let old = OwnedCoreBluetoothProtocolRadio.ConnectionClaim(owner: first, route: first, pending: nil)
        radio.connectionClaims[peer] = [ObjectIdentifier(first): old]
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "old", enabled: true,
          completion: { _ in }, borrower: first, claimIdentity: old.identity)
        radio.connectionClaims[peer]?[ObjectIdentifier(first)] = .init(owner: first, route: first, pending: nil)
        radio.cancelPendingOperation("same", scanOwner: first, cleanupIdentifier: "new-cleanup")
        precondition(radio.pendingNotify[address]?.claimIdentity == old.identity,
          "a replacement claim cannot cancel a prior claim's notification transition")
        precondition(radio.pendingCancellationCleanup["new-cleanup"] == nil)
      }
      print("Notification cancellation claim fenced"); return
    }
    if CommandLine.arguments.contains("--ordered-native-claim-fence") {
      let peer = peerID.uuidString
      radio.queue.sync {
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        let admitted = radio.connectionClaims[peer]![ObjectIdentifier(first)]!.identity
        precondition(radio.connectionClaimIsCurrent(owner: first, peer: peer, identity: admitted))
        precondition(!radio.connectionClaimIsCurrent(owner: second, peer: peer, identity: admitted))
        radio.connectionClaims[peer]?[ObjectIdentifier(first)] = .init(owner: first, route: first, pending: nil)
        precondition(!radio.connectionClaimIsCurrent(owner: first, peer: peer, identity: admitted),
          "a replacement claim cannot inherit ordered admission")
      }
      print("Ordered native claim fenced"); return
    }
    if CommandLine.arguments.contains("--notification-failure-cleanup-collision") ||
       CommandLine.arguments.contains("--notification-service-cleanup-collision") {
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0)
      let other = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A38", characteristicOccurrence: 0)
      radio.queue.sync {
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        var existing = PendingCancellationCleanup()
        existing.notificationDesiredStates[other] = true
        radio.pendingCancellationCleanup["same"] = existing
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "first",
          enabled: true, completion: { _ in }, borrower: first,
          claimIdentity: radio.connectionClaims[peer]![ObjectIdentifier(first)]!.identity)
        radio.notificationStateWriter = { _, _ in false }
        if CommandLine.arguments.contains("--notification-service-cleanup-collision") {
          radio.clearNotificationOwnership(forPeerIdentifier: peer)
        } else {
          radio.handleNotificationStateUpdate(address: address, isNotifying: false, error: NSError(domain: "fixture", code: 42))
        }
        precondition(radio.pendingCancellationCleanup["same"]?.notificationDesiredStates == [other: true],
          "another borrower's cleanup must not absorb this transition")
        precondition(radio.pendingCancellationCleanup.values.contains { $0.notificationDesiredStates[address] == false },
          "the failed transition must retain its own cleanup")
      }
      print("Notification cleanup ownership isolated"); return
    }
    if CommandLine.arguments.contains("--notification-deferred-original-owner") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        precondition(radio.installNotificationRoute("same", borrower: first))
        radio.subscriptions[address] = "same"
        let source = OwnedOrderedNotificationSource(queue: radio.queue, generation: "original")
        let route = radio.orderedRoute(for: address)
        let group = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation, members: [route], capacity: 4, byteCapacity: 32)
        precondition(source.add(group))
        let claim = source.makeClaim(route: route)!
        try! group.register(claim)
        radio.orderedSources[peer] = source
        // The source may have a registered group before this ordinary route
        // has an ordered activation token of its own.
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        let failure = NSError(domain: "original", code: 42)
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: failure)
        radio.removeNotificationRoute("same", borrower: first)
        precondition(radio.installNotificationRoute("same", borrower: second))
        precondition(gate.release(key) == .released)
        precondition(first.failureErrors.first === failure && second.failureErrors.isEmpty,
          "a deferred failure belongs to the callback-time owner")
        guard case let .terminal(.sourceFailed(observed, _, _, _), _, _) = group.drain().first else {
          fatalError("the original ordered claim must receive the deferred failure")
        }
        precondition(observed.domain == failure.domain && observed.code == failure.code)
      }
      print("Deferred failure retained original owner"); return
    }
    if CommandLine.arguments.contains("--ordered-value-before-saturated-notification-failure") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        precondition(radio.installNotificationRoute("established", borrower: first))
        radio.subscriptions[address] = "established"
        let source = OwnedOrderedNotificationSource(queue: radio.queue, generation: "value-before-failure")
        let route = radio.orderedRoute(for: address)
        let group = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation, members: [route], capacity: 4, byteCapacity: 32)
        precondition(source.add(group))
        let claim = source.makeClaim(route: route)!
        let epoch = try! group.register(claim)
        radio.orderedSources[peer] = source
        radio.notificationRoutes["established"]?.orderedClaim = claim
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        radio.admitOrderedCharacteristicValue(address: address, value: Data([7]))
        for _ in 0..<1023 { precondition(gate.enqueue {}) }
        let failure = NSError(domain: "native", code: 42)
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: failure)
        precondition(group.drain().isEmpty, "saturated failure must not overtake the previously admitted value")
        precondition(gate.release(key) == .released)
        let events = group.drain()
        guard events.count >= 2,
              case let .value(observedRoute, observedEpoch, ordinal, bytes, .notification) = events[0],
              case let .terminal(.sourceFailed(observedFailure, failedRoute, token, failedEpoch), _, _) = events[1] else {
          fatalError("the admitted value must precede the exact notification failure: \(events)")
        }
        precondition(observedRoute == route && observedEpoch == epoch && ordinal == 0 && bytes == Data([7]))
        precondition(failedRoute == route && token == claim.token && failedEpoch == epoch &&
          observedFailure.domain == failure.domain && observedFailure.code == failure.code)
      }
      print("Ordered value preceded saturated notification failure"); return
    }
    if CommandLine.arguments.contains("--notification-reentrant-gate-order") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        precondition(radio.installNotificationRoute("established", borrower: first))
        radio.subscriptions[address] = "established"
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        var order: [String] = []
        first.onFailure = { order.append("failure") }
        gate.enqueue {
          order.append("first")
          radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: NSError(domain: "native", code: 42))
        }
        gate.enqueue { order.append("second") }
        precondition(gate.release(key) == .released)
        precondition(order == ["first", "second", "failure"], "reentrant callback overtook queued work: \(order)")
      }
      print("Reentrant notification callback ordered"); return
    }
    if CommandLine.arguments.contains("--notification-full-gate-error") || CommandLine.arguments.contains("--notification-full-gate-off") ||
       CommandLine.arguments.contains("--notification-full-gate-repeated-failures") ||
       CommandLine.arguments.contains("--notification-full-gate-admitted-repeated-failures") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      let admittedRepeated = CommandLine.arguments.contains("--notification-full-gate-admitted-repeated-failures")
      if admittedRepeated {
        radio.queue.sync {
          radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
          radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
            ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
          precondition(radio.installNotificationRoute("established", borrower: first))
          precondition(radio.installNotificationRoute("shared", borrower: second))
        }
        var firstAnswer: NSError??
        radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
          characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "established",
          operationIdentifier: "first-enable") { firstAnswer = .some($0) }
        radio.queue.sync {
          notifying[ObjectIdentifier(characteristic)] = true
          radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nil)
          precondition(firstAnswer != nil && firstAnswer! == nil, "first route must pass production admission")
        }
        var secondAnswer: NSError??
        radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
          characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "shared",
          operationIdentifier: "shared-enable") { secondAnswer = .some($0) }
        radio.queue.sync {
          precondition(secondAnswer != nil && secondAnswer! == nil, "shared route must pass production admission")
          precondition(radio.subscriptions[address] == "established" && radio.sharedSubscriptions[address] == ["shared"])
          precondition(physicalWrites.map { $0.1 } == [true])
        }
      }
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        if !admittedRepeated {
          precondition(radio.installNotificationRoute("established", borrower: first))
          precondition(radio.installNotificationRoute("shared", borrower: second))
          radio.subscriptions[address] = "established"
          radio.sharedSubscriptions[address] = ["shared"]
        }
        let source = OwnedOrderedNotificationSource(queue: radio.queue, generation: "full")
        let route = radio.orderedRoute(for: address)
        let group = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation, members: [route], capacity: 4, byteCapacity: 32)
        let sharedGroup = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation, members: [route], capacity: 4, byteCapacity: 32)
        precondition(source.add(group) && source.add(sharedGroup))
        let claim = source.makeClaim(route: route)!, sharedClaim = source.makeClaim(route: route)!
        try! group.register(claim); try! sharedGroup.register(sharedClaim)
        radio.orderedSources[peer] = source; radio.notificationRoutes["established"]?.orderedClaim = claim
        radio.notificationRoutes["shared"]?.orderedClaim = sharedClaim
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        let repeated = admittedRepeated || CommandLine.arguments.contains("--notification-full-gate-repeated-failures")
        var order: [String] = []
        var prefixDelivered = false
        for index in 0..<1024 { precondition(gate.enqueue { if index == 1023 { prefixDelivered = true; order.append("prefix") } }) }
        first.onFailure = {
          if repeated {
            precondition(radio.orderedSources[peer] === source, "overflow overtook an original failure")
            order.append("first:\(first.failureErrors.last!.code)"); return
          }
          precondition(prefixDelivered, "failure overtook the accepted callback prefix")
          precondition(radio.orderedSources[peer] === source, "overflow overtook the original failure")
        }
        second.onFailure = {
          if repeated {
            precondition(radio.orderedSources[peer] === source, "overflow overtook a shared failure")
            order.append("second:\(second.failureErrors.last!.code)"); return
          }
          precondition(prefixDelivered, "shared failure overtook the accepted callback prefix")
          precondition(radio.orderedSources[peer] === source, "overflow overtook the shared failure")
        }
        let isError = repeated || CommandLine.arguments.contains("--notification-full-gate-error")
        let originalError = NSError(domain: "full-native", code: 42)
        let secondError = NSError(domain: "full-native", code: 43)
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: isError ? originalError : nil)
        var resumeBarrier: (() -> Void)?
        if repeated {
          precondition(gate.enqueueBarrier(invalidating: false) { resume in
            order.append("barrier")
            resumeBarrier = resume
          })
          radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: secondError)
        } else {
          precondition(first.failures.isEmpty && second.failures.isEmpty, "failure must wait for the accepted callback prefix")
        }
        let replacement = Borrower()
        radio.removeNotificationRoute("established", borrower: first)
        precondition(radio.installNotificationRoute("established", borrower: replacement))
        precondition(gate.release(key) == .released)
        if repeated {
          precondition(order.first == "prefix", "distinct failures overtook the accepted prefix: \(order)")
          precondition(order.last == "barrier" && first.failureErrors.map(\.code) == [42] && second.failureErrors.map(\.code) == [42],
            "the barrier must pause failure 43 after both original owners receive failure 42: \(order)")
          precondition(radio.orderedSources[peer] === source, "fail-stop must wait behind the paused barrier")
          resumeBarrier?()
          resumeBarrier?()
          precondition(first.failureErrors.map(\.code) == [42, 43] && second.failureErrors.map(\.code) == [42, 43],
            "each original owner must retain both distinct failures")
          precondition(first.failureErrors[0] === originalError && second.failureErrors[0] === originalError &&
            first.failureErrors[1] === secondError && second.failureErrors[1] === secondError,
            "each delivery must retain the exact observed error object")
          precondition(replacement.failures.isEmpty, "replacement must not inherit captured failures")
          precondition(radio.orderedSources[peer] == nil, "overflow must follow the retained failures")
          for originalGroup in [group, sharedGroup] {
            let events = originalGroup.drain()
            guard events.count >= 2,
                  case let .terminal(.sourceFailed(firstFailure, _, _, _), _, _) = events[0],
                  case let .subsequentSourceFailure(secondFailure) = events[1] else {
              fatalError("both original ordered claims must retain the two failures")
            }
            precondition(firstFailure.code == 42 && secondFailure.code == 43,
              "ordered failure evidence must retain callback order")
          }
          return
        }
        precondition(radio.orderedSources[peer] == nil, "overflow must follow the retained failures")
        precondition(first.failures == ["established"], "saturated gate must report the established owner's failure")
        precondition(second.failures == ["shared"], "saturated gate must report the shared owner's failure")
        precondition(replacement.failures.isEmpty, "replacement borrower must not receive the original failure")
        if isError { precondition(first.failureErrors.first === originalError && second.failureErrors.first === originalError) }
        precondition(first.failureErrors.first?.domain == (isError ? "full-native" : "com.sfourdrinier.unifiedblemanager.corebluetooth"))
        guard case let .terminal(.sourceFailed(failure, _, _, _), _, _) = group.drain().first else { fatalError("ordered owner must fail-stop") }
        precondition(failure.code == (isError ? 42 : 1039))
        guard case let .terminal(.sourceFailed(sharedFailure, _, _, _), _, _) = sharedGroup.drain().first else { fatalError("shared ordered owner must fail-stop") }
        precondition(sharedFailure.code == failure.code)
      }
      print("Saturated notification gate retained failure"); return
    }
    if CommandLine.arguments.contains("--notification-immediate-failure-capture") {
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0)
      radio.queue.sync {
        precondition(radio.installNotificationRoute("same", borrower: first))
        radio.subscriptions[address] = "same"
        radio.reportNotificationFailure(address: address, error: NSError(domain: "immediate", code: 42))
        precondition(first.failureCaptures == 1 && first.failureErrors.first?.domain == "immediate",
          "ordinary failure must pass through the retained receiver")
      }
      print("Immediate failure captured"); return
    }
    if CommandLine.arguments.contains("--notification-deferred-successful-disable") ||
       CommandLine.arguments.contains("--notification-full-gate-successful-disable") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        precondition(radio.installNotificationRoute("same", borrower: first))
        radio.subscriptions[address] = "same"
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "disable", subscriptionIdentifier: "same", enabled: false,
          completion: { _ in }, borrower: first, claimIdentity: radio.connectionClaims[peer]![ObjectIdentifier(first)]!.identity)
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        if CommandLine.arguments.contains("--notification-full-gate-successful-disable") {
          for _ in 0..<1024 { precondition(gate.enqueue({})) }
        }
        radio.notificationTransitionTickets[address] = UUID()
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: nil)
        radio.notificationTransitionTickets[address] = UUID()
        precondition(gate.release(key) == .released)
        precondition(first.failureErrors.isEmpty, "a successful disable is not an unexpected off even after the ticket changes")
      }
      print("Deferred successful disable classified"); return
    }
    if CommandLine.arguments.contains("--notification-deferred-departed-legacy-error") {
      let legacy = LegacyFailureBorrower()
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(legacy): .init(owner: legacy, route: legacy, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.installNotificationRoute("same", borrower: legacy))
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        radio.notificationTransitionTickets[address] = UUID()
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "old", subscriptionIdentifier: "same", enabled: true,
          completion: { _ in }, borrower: legacy, claimIdentity: radio.connectionClaims[peer]![ObjectIdentifier(legacy)]!.identity,
          routeIdentity: radio.notificationRoutes["same"]!.identity)
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: NSError(domain: "legacy-old", code: 43))
        radio.removeNotificationRoute("same", borrower: legacy)
        precondition(radio.installNotificationRoute("same", borrower: second))
        radio.notificationTransitionTickets[address] = UUID()
        precondition(gate.release(key) == .released)
        precondition(legacy.errors.first?.domain == "legacy-old" && legacy.errors.first?.code == 43,
          "the departed notification owner must receive its original native failure")
        precondition(second.failures.isEmpty, "replacement route must not receive the departed owner's error")
      }
      print("Deferred legacy error retained"); return
    }
    if CommandLine.arguments.contains("--notification-deferred-departed-error") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.installNotificationRoute("old", borrower: first))
        let source = OwnedOrderedNotificationSource(queue: radio.queue, generation: "old-source")
        let route = radio.orderedRoute(for: address)
        let oldGroup = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation, members: [route], capacity: 4, byteCapacity: 32)
        let newGroup = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation, members: [route], capacity: 4, byteCapacity: 32)
        precondition(source.add(oldGroup) && source.add(newGroup))
        let oldClaim = source.makeClaim(route: route)!, newClaim = source.makeClaim(route: route)!
        try! oldGroup.register(oldClaim); try! newGroup.register(newClaim)
        radio.orderedSources[peer] = source
        radio.notificationRoutes["old"]?.orderedClaim = oldClaim
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        radio.notificationTransitionTickets[address] = UUID()
        radio.notificationTransitionObjects[address] = ObjectIdentifier(characteristic)
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "old", subscriptionIdentifier: "old", enabled: true,
          completion: { _ in }, borrower: first, claimIdentity: radio.connectionClaims[peer]![ObjectIdentifier(first)]!.identity)
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: NSError(domain: "native-old", code: 42))
        try! oldGroup.retire(route)
        oldGroup.close()
        do { try oldGroup.seal(); preconditionFailure("deferred error must retain its owner") }
        catch {}
        radio.removeNotificationRoute("old", borrower: first)
        radio.notificationTransitionTickets[address] = UUID()
        precondition(gate.release(key) == .released)
        let events = oldGroup.drain()
        precondition(events.contains(.subsequentSourceFailure(OwnedOrderedNotificationIngress.NativeFailure(error: NSError(domain: "native-old", code: 42)).attributed(to: oldClaim, epoch: 1))),
          "the departed ordered owner must retain the native failure")
        precondition(newGroup.drain().isEmpty, "the replacement claimant must not inherit the old failure")
        try! oldGroup.seal()
      }
      print("Deferred departed error retained"); return
    }
    if CommandLine.arguments.contains("--notification-deferred-old-callback") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      var replacementAnswered = false
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.installNotificationRoute("old", borrower: first))
        precondition(radio.installNotificationRoute("new", borrower: second))
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        radio.notificationTransitionTickets[address] = UUID()
        radio.notificationTransitionObjects[address] = ObjectIdentifier(characteristic)
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "old",
          enabled: true, completion: { _ in }, borrower: first,
          claimIdentity: radio.connectionClaims[peer]![ObjectIdentifier(first)]!.identity)
        notifying[ObjectIdentifier(characteristic)] = true
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: nil)
        radio.notificationTransitionTickets[address] = UUID()
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "new",
          enabled: true, completion: { _ in replacementAnswered = true }, borrower: second,
          claimIdentity: radio.connectionClaims[peer]![ObjectIdentifier(second)]!.identity)
        precondition(gate.release(key) == .released)
        precondition(!replacementAnswered && radio.subscriptions[address] == nil,
          "a deferred old callback must not settle a replacement physical transition")
      }
      print("Deferred notification callback fenced"); return
    }
    if CommandLine.arguments.contains("--notification-teardown-replaced-route") {
      let peer = peerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var answer: NSError?
      radio.queue.sync {
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.installNotificationRoute("same", borrower: first))
        let route = radio.notificationRoutes["same"]!
        let claim = radio.connectionClaims[peer]![ObjectIdentifier(first)]!.identity
        radio.setNotify(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
          characteristicUUID: "2A37", characteristicOccurrence: 0, enabled: false,
          subscriptionIdentifier: "same", operationIdentifier: "same",
          expectedRelease: (owner: first, claim: claim, route: route)) { answer = $0; done.signal() }
        radio.removeNotificationRoute("same", borrower: first)
        precondition(radio.installNotificationRoute("same", borrower: second))
      }
      precondition(done.wait(timeout: .now() + 2) == .success && answer?.code == 1020)
      radio.queue.sync {
        precondition(radio.notificationRoutes["same"]?.borrower === second && physicalWrites.isEmpty,
          "queued cleanup must not touch a replacement route")
      }
      print("Notification queued teardown route fenced"); return
    }
    if CommandLine.arguments.contains("--notification-teardown-replaced-claim") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      let done = DispatchSemaphore(value: 0)
      var answer: NSError?
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.installNotificationRoute("old", borrower: first))
        radio.subscriptions[address] = "old"; notifying[ObjectIdentifier(characteristic)] = true
      }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "release") { answer = $0; done.signal() }
      radio.queue.sync {}
      radio.queue.sync {
        precondition(physicalWrites.count == 1)
        radio.connectionClaims[peer]?[ObjectIdentifier(first)] = .init(owner: first, route: first, pending: nil)
        notifying[ObjectIdentifier(characteristic)] = false
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: nil)
      }
      precondition(done.wait(timeout: .now() + 2) == .success && answer != nil)
      radio.queue.sync {
        precondition(radio.connectionClaims[peer]?[ObjectIdentifier(first)] != nil, "old teardown must not retire replacement claim")
        precondition(radio.connectionClaims[peer]?[ObjectIdentifier(second)] != nil)
        precondition(radio.notificationRoutes["old"]?.borrower === first, "old teardown must not remove the route")
      }
      print("Notification teardown replacement fenced"); return
    }
    if CommandLine.arguments.contains("--notification-stale-disable") {
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0)
      var answer: NSError??
      radio.queue.sync {
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.installNotificationRoute("old", borrower: first))
        precondition(radio.installNotificationRoute("replacement", borrower: second))
        radio.subscriptions[address] = "old"
        radio.pendingNotify[address] = PendingNotify(operationIdentifier: "stop", subscriptionIdentifier: "old",
          enabled: false, completion: { answer = .some($0) }, borrower: first,
          claimIdentity: radio.connectionClaims[peer]![ObjectIdentifier(first)]!.identity)
        radio.connectionClaims[peer]?[ObjectIdentifier(first)] = .init(owner: first, route: first, pending: nil)
        radio.subscriptions[address] = "replacement"
        radio.notificationStateWriter = { _, enabled in physicalWrites.append((ObjectIdentifier(first), enabled)); return true }
        radio.handleNotificationStateUpdate(address: address, isNotifying: false, error: nil)
        precondition(answer != nil && answer! != nil, "old disable must report its stale claim")
        precondition(radio.subscriptions[address] == "replacement", "old disable must not remove a replacement")
        precondition(second.failures == ["replacement"], "a physical notification gap must reach the surviving claimant")
      }
      print("Notification stale disable fenced"); return
    }
    if CommandLine.arguments.contains("--notification-late-group-claim-identity") {
      radio.queue.sync {
        let route = OrderedNotificationRoute(peer: peerID.uuidString, serviceUUID: "s", serviceOccurrence: 0,
          characteristicUUID: "c", characteristicOccurrence: 0)
        let source = OwnedOrderedNotificationSource(queue: radio.queue, generation: "late-claims")
        let firstGroup = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation,
          members: [route], capacity: 4, byteCapacity: 16)
        let secondGroup = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation,
          members: [route], capacity: 4, byteCapacity: 16)
        precondition(source.add(firstGroup) && source.add(secondGroup))
        let firstClaim = source.makeClaim(route: route)!, secondClaim = source.makeClaim(route: route)!
        try! firstGroup.register(firstClaim); try! secondGroup.register(secondClaim)
        let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
        let firstReceipt = source.captureDeferredFailure(claim: firstClaim, failure: failure)!
        let secondReceipt = source.captureDeferredFailure(claim: secondClaim, failure: failure)!
        try! firstGroup.retire(route); try! secondGroup.retire(route)
        _ = firstGroup.drain(); _ = secondGroup.drain()
        firstReceipt(true); secondReceipt(true)
        let firstRecords = firstGroup.drain(), secondRecords = secondGroup.drain()
        precondition(firstRecords == [.subsequentSourceFailure(failure.attributed(to: firstClaim, epoch: 1))])
        precondition(secondRecords == [.subsequentSourceFailure(failure.attributed(to: secondClaim, epoch: 1))])
        precondition(firstRecords != secondRecords,
          "late failure records must retain their distinct original group claim identities")
      }
      print("Late notification failures retain group claim identities"); return
    }
    if CommandLine.arguments.contains("--notification-retention-exhausted-before-enable") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      var answer: NSError??
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        first.refuseFailureCapture = true
        precondition(radio.installNotificationRoute("sub", borrower: first))
      }
      radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "sub",
        operationIdentifier: "enable") { answer = .some($0) }
      radio.queue.sync {
        precondition(physicalWrites.isEmpty, "an exhausted failure recipient must refuse admission before the physical enable")
        precondition(answer != nil && answer! != nil, "retention exhaustion must be reported to the caller")
        precondition(radio.pendingNotify.isEmpty && radio.subscriptions.isEmpty,
          "refused admission must leave no pending or active notification ownership")
        radio.removeNotificationRoute("sub", borrower: first)
        first.refuseFailureCapture = false
        precondition(radio.installNotificationRoute("retry", borrower: first))
      }
      radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "retry",
        operationIdentifier: "retry-enable") { _ in }
      radio.queue.sync {
        precondition(physicalWrites.map { $0.1 } == [true] && radio.pendingNotify.count == 1,
          "a later admitted route must be able to enable after refusal and rollback")
      }
      print("Notification retention exhaustion refused before enable"); return
    }
    if CommandLine.arguments.contains("--notification-retention-exhausted-before-join") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      var answer: NSError??
      var initialAnswer: NSError??
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.installNotificationRoute("existing", borrower: first))
        precondition(radio.installNotificationRoute("joining", borrower: second))
      }
      radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "existing",
        operationIdentifier: "initial-enable") { initialAnswer = .some($0) }
      radio.queue.sync {
        notifying[ObjectIdentifier(characteristic)] = true
        radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nil)
        precondition(initialAnswer != nil && initialAnswer! == nil && radio.subscriptions[address] == "existing")
        precondition(physicalWrites.map { $0.1 } == [true])
        second.refuseFailureCapture = true
      }
      radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "joining",
        operationIdentifier: "join") { answer = .some($0) }
      radio.queue.sync {
        precondition(answer != nil && answer! != nil, "an exhausted recipient must be refused before joining an active route")
        precondition(radio.subscriptions[address] == "existing" && (radio.sharedSubscriptions[address] ?? []).isEmpty,
          "refusal must preserve the existing borrower and install no joining owner")
        precondition(physicalWrites.map { $0.1 } == [true], "a refused shared join must not change physical notification state")
        radio.removeNotificationRoute("joining", borrower: second)
        second.refuseFailureCapture = false
        precondition(radio.installNotificationRoute("retry-joining", borrower: second))
        answer = nil
      }
      radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "retry-joining",
        operationIdentifier: "retry-join") { answer = .some($0) }
      radio.queue.sync {
        precondition(answer != nil && answer! == nil && radio.sharedSubscriptions[address] == ["retry-joining"],
          "a retry must join once failure retention is available")
        precondition(radio.subscriptions[address] == "existing" && physicalWrites.map { $0.1 } == [true],
          "retry must preserve the existing owner without another physical enable")
      }
      print("Notification retention exhaustion refused before join"); return
    }
    if CommandLine.arguments.contains("--notification-stale-claim-callback") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      let done = DispatchSemaphore(value: 0)
      var answer: NSError?
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        precondition(radio.installNotificationRoute("sub", borrower: first))
        radio.notificationStateWriter = { _, enabled in physicalWrites.append((ObjectIdentifier(characteristic), enabled)); return true }
      }
      radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "sub",
        operationIdentifier: "enable") { answer = $0; done.signal() }
      radio.queue.sync {
        precondition(radio.pendingNotify[address] != nil)
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nil)
      }
      precondition(done.wait(timeout: .now() + 2) == .success && answer != nil,
        "a callback from an old claim cannot enable a replacement claim")
      radio.queue.sync {
        precondition(radio.subscriptions[address] == nil && physicalWrites.map { $0.1 } == [true, false],
          "the stale enable must retain cleanup and disable its physical registration")
      }
      print("Notification stale claim callback fenced"); return
    }
    if CommandLine.arguments.contains("--notification-pending-claim") {
      let peer = peerID.uuidString
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let done = DispatchSemaphore(value: 0)
      var answer: NSError?
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first,
          pending: PendingVoid(operationIdentifier: "connect", completion: { _ in }))]
        precondition(radio.installNotificationRoute("sub", borrower: first))
      }
      radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "sub",
        operationIdentifier: "enable") { answer = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 2) == .success && answer != nil,
        "a pending connection claim cannot admit notification enable")
      precondition(physicalWrites.isEmpty, "no CCCD transition is dispatched for a pending claim")
      print("Notification pending claim rejected"); return
    }
    if CommandLine.arguments.contains("--stale-cleanup-operation-reuse") {
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "service", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      radio.queue.sync {
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        var cleanup = PendingCancellationCleanup()
        cleanup.notificationDesiredStates[address] = false
        radio.pendingCancellationCleanup["old-physical"] = cleanup
        radio.fallbackCancellationKeys[ObjectIdentifier(first)] = ["same": .init(owner: first, physicalOperation: "old-physical")]
        precondition(radio.registerScopedGatt(owner: first, peer: peer, operation: "same") == nil,
          "an operation identity with retained cleanup must not be reused")
        radio.pendingCancellationCleanup.removeValue(forKey: "old-physical")
        radio.pruneFallbackCancellationKeys()
        precondition(radio.registerScopedGatt(owner: first, peer: peer, operation: "same") != nil,
          "the operation identity may be reused after cleanup is settled")
      }
      print("Stale cleanup operation reuse passed"); return
    }
    if CommandLine.arguments.contains("--preparation-borrower-collision") {
      let done = DispatchSemaphore(value: 0)
      var firstResult: NSError?
      var firstCalls = 0
      var secondCalls = 0
      radio.queue.sync {
        radio.radioPreparation.start("same", owner: first, snapshot: ["power": "unknown"], waitForInitialState: true) { _, error in
          firstCalls += 1; firstResult = error
        }
        radio.radioPreparation.start("same", owner: second, snapshot: ["power": "unknown"], waitForInitialState: true) { _, _ in
          secondCalls += 1
        }
      }
      radio.cancelOperation("same", borrower: first) { _ in done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(firstCalls == 1 && firstResult != nil && secondCalls == 0,
          "cancellation must target only its borrower's preparation")
        radio.radioPreparation.update(["power": "poweredOn"])
        precondition(secondCalls == 1, "the other preparation must survive")
      }
      print("Preparation borrower collision passed"); return
    }
    if CommandLine.arguments.contains("--fallback-cleanup-receipt-collision") {
      let a = CharacteristicAddress(peerIdentifier: "a", serviceUUID: "service", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      let b = CharacteristicAddress(peerIdentifier: "b", serviceUUID: "service", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      let done = DispatchSemaphore(value: 0)
      var receipt: NSDictionary?
      radio.queue.sync {
        radio.pendingNotify[a] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "a", enabled: true, completion: { _ in }, borrower: first)
        radio.pendingNotify[b] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "b", enabled: true, completion: { _ in }, borrower: second)
      }
      radio.cancelOperation("same", borrower: first) { _ in
        radio.cancelOperation("same", borrower: second) { _ in done.signal() }
      }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync { radio.clearCancellationCleanup(forNotificationAddress: a) }
      radio.retryCancellationCleanup("same", borrower: first) { receipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "released",
        "one borrower's completed cleanup must not include another borrower's debt")
      radio.retryCancellationCleanup("same", borrower: second) { receipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "retryable",
        "the other borrower's cleanup debt must remain reachable")
      print("Fallback cleanup receipts are independent"); return
    }
    if CommandLine.arguments.contains("--fallback-notification-collision") {
      let a = CharacteristicAddress(peerIdentifier: "peer", serviceUUID: "service", serviceOccurrence: 0,
        characteristicUUID: "a", characteristicOccurrence: 0)
      let b = CharacteristicAddress(peerIdentifier: "peer", serviceUUID: "service", serviceOccurrence: 0,
        characteristicUUID: "b", characteristicOccurrence: 0)
      let done = DispatchSemaphore(value: 0)
      radio.queue.sync {
        precondition(radio.installNotificationRoute("a", borrower: first))
        precondition(radio.installNotificationRoute("b", borrower: second))
        radio.pendingNotify[a] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "a", enabled: true, completion: { _ in }, borrower: first)
        radio.pendingNotify[b] = PendingNotify(operationIdentifier: "same", subscriptionIdentifier: "b", enabled: true, completion: { _ in }, borrower: second)
        radio.removeNotificationRoute("a", borrower: first)
      }
      radio.cancelOperation("same", borrower: first) { _ in done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(radio.pendingNotify[a] == nil && radio.pendingNotify[b] != nil,
          "a cancellation must retire only the requesting borrower's notification")
      }
      print("Fallback cancellation preserves another notification"); return
    }
    if CommandLine.arguments.contains("--fallback-rssi-collision") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var answers = 0
      radio.queue.sync { radio.peripheralByIdentifier[peer] = peripheral }
      radio.readRssi(peerIdentifier: peer, operationIdentifier: "same") { _, failure in
        precondition(failure == nil); answers += 1
      }
      radio.cancelOperation("same", borrower: first) { _ in done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        radio.peripheral(peripheral, didReadRSSI: -42, error: nil)
        precondition(answers == 1, "an unrelated borrower cannot cancel a legacy RSSI operation")
      }
      print("Fallback cancellation preserves another RSSI operation"); return
    }
    if CommandLine.arguments.contains("--fallback-connection-collision") {
      let peer = peerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var receipt: NSDictionary?
      radio.queue.sync {
        radio.pendingConnect[peer] = PendingVoid(operationIdentifier: "same", completion: { _ in })
        radio.connectionClaims[peer] = [ObjectIdentifier(second): .init(owner: second, route: second,
          pending: PendingVoid(operationIdentifier: "other", completion: { _ in }))]
      }
      radio.cancelOperation("same", borrower: first) { receipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(radio.pendingConnect[peer]?.operationIdentifier == "same",
          "an unrelated borrower's cancellation must not retire the physical connection")
        precondition(receipt?["state"] as? String == "released")
      }
      print("Fallback cancellation preserves another connection"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-queued-cancel") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.read], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let done = DispatchSemaphore(value: 0)
      var firstAnswers = 0, secondAnswers = 0
      var receipt: NSDictionary?
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.readCharacteristic(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, operationIdentifier: "same") { _, _, _ in firstAnswers += 1 }
      radio.readCharacteristic(borrower: second, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, operationIdentifier: "same") { _, _, _ in secondAnswers += 1 }
      radio.cancelOperation("same", borrower: second) { receipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "released")
      radio.queue.sync {
        precondition(characteristicReads == 1)
        radio.peripheral(peripheral, didUpdateValueFor: characteristic, error: nil)
        precondition(firstAnswers == 1 && secondAnswers == 0 && characteristicReads == 1, "queued cancellation must not dispatch or disturb the in-flight read")
      }
      print("Queued read cancellation passed"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-write-readiness") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.writeWithoutResponse], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let done = DispatchSemaphore(value: 0); var answer: NSError?
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]; writeReady = false
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
      }
      radio.write(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, value: NSData(), withResponse: false,
        operationIdentifier: "write") { answer = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && answer?.code == 1014)
      radio.queue.sync { precondition(radio.scopedGattOperations.isEmpty) }
      print("Write readiness refusal passed"); return
    }
    if CommandLine.arguments.contains("--claim-rssi-descriptor-cancel-collision") || CommandLine.arguments.contains("--claim-descriptor-rssi-cancel-collision") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.read], value: nil, permissions: [])
      let descriptor = CBMutableDescriptor(type: CBUUID(string: "1234"), value: Data([1]))
      characteristic.descriptors = [descriptor]
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let done = DispatchSemaphore(value: 0); var receipt: NSDictionary?; var firstAnswers = 0, secondAnswers = 0
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.readRssi(borrower: first, peerIdentifier: peer, operationIdentifier: "same") { _, error in precondition(error == nil); firstAnswers += 1 }
      radio.readDescriptor(borrower: second, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, descriptorUUID: "1234", descriptorOccurrence: 0,
        operationIdentifier: "same") { _, error in precondition(error == nil); secondAnswers += 1 }
      let cancelDescriptor = CommandLine.arguments.contains("--claim-descriptor-rssi-cancel-collision")
      let cancelledOwner = cancelDescriptor ? second : first
      radio.cancelOperation("same", borrower: cancelledOwner) { receipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "retryable")
      radio.queue.sync {
        radio.peripheral(peripheral, didUpdateValueFor: descriptor, error: nil)
        radio.peripheral(peripheral, didReadRSSI: -42, error: nil)
        precondition(firstAnswers == (cancelDescriptor ? 1 : 0) && secondAnswers == (cancelDescriptor ? 0 : 1),
          "cancellation cannot consume the other borrower's callback")
      }
      radio.retryCancellationCleanup("same", borrower: cancelledOwner) { receipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "released")
      print("RSSI and descriptor cancellation collision passed"); return
    }
    if CommandLine.arguments.contains("--claim-descriptor-retirement") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.read], value: nil, permissions: [])
      let descriptors = ["1234", "1235"].map { CBMutableDescriptor(type: CBUUID(string: $0), value: Data([1])) }
      characteristic.descriptors = descriptors
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let done = DispatchSemaphore(value: 0); var firstAnswers = [NSError?](), secondAnswers = [NSError?]()
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.readDescriptor(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, descriptorUUID: "1234", descriptorOccurrence: 0,
        operationIdentifier: "same") { _, error in firstAnswers.append(error) }
      radio.writeDescriptor(borrower: second, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, descriptorUUID: "1235", descriptorOccurrence: 0,
        value: NSData(), operationIdentifier: "same") { secondAnswers.append($0) }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "disconnect") { error in precondition(error == nil); done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(firstAnswers.count == 1 && firstAnswers[0] != nil && secondAnswers.isEmpty)
        radio.peripheral(peripheral, didUpdateValueFor: descriptors[0], error: nil)
        radio.peripheral(peripheral, didWriteValueFor: descriptors[1], error: nil)
        precondition(firstAnswers.count == 1 && secondAnswers.count == 1 && secondAnswers[0] == nil, "late callback must not cross claims")
      }
      print("Descriptor retirement and collision passed"); return
    }
    if CommandLine.arguments.contains("--claim-descriptor-admission") {
      let done = DispatchSemaphore(value: 0)
      var failures = 0
      radio.readDescriptor(borrower: first, peerIdentifier: peerID.uuidString, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, descriptorUUID: "2901", descriptorOccurrence: 0,
        operationIdentifier: "read") { _, error in if error != nil { failures += 1 }; done.signal() }
      radio.writeDescriptor(borrower: first, peerIdentifier: peerID.uuidString, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, descriptorUUID: "2901", descriptorOccurrence: 0,
        value: NSData(), operationIdentifier: "write") { error in if error != nil { failures += 1 }; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && done.wait(timeout: .now() + 5) == .success)
      precondition(failures == 2, "descriptor work requires an active claim")
      print("Claim descriptor admission passed"); return
    }
    if CommandLine.arguments.contains("--claim-rssi-retirement") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString; let done = DispatchSemaphore(value: 0); var answers = [NSError?]()
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.readRssi(borrower: first, peerIdentifier: peer, operationIdentifier: "rssi") { _, error in answers.append(error) }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "disconnect") { error in precondition(error == nil); done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(answers.count == 1 && answers[0] != nil)
        radio.peripheral(peripheral, didReadRSSI: -50, error: nil)
        precondition(answers.count == 1, "late RSSI must not settle the retired owner twice")
      }
      print("RSSI retirement passed"); return
    }
    if CommandLine.arguments.contains("--claim-rssi-generation") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      var answers = [NSError?]()
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
      }
      radio.readRssi(borrower: first, peerIdentifier: peer, operationIdentifier: "same") { _, error in answers.append(error) }
      radio.queue.sync {
        radio.connectionClaims[peer]?[ObjectIdentifier(first)] = .init(owner: first, route: first, pending: nil)
        radio.peripheral(peripheral, didReadRSSI: -55, error: nil)
        precondition(answers.count == 1 && answers[0] != nil, "old generation must not receive a successful RSSI")
      }
      print("Claim RSSI generation passed"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-write-limit") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.writeWithoutResponse], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let done = DispatchSemaphore(value: 0)
      var answer: NSError?
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
      }
      radio.write(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, value: Data([1, 2, 3]) as NSData, withResponse: false,
        operationIdentifier: "write") { answer = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && answer?.code == 1012, "oversize is a platform refusal, not a busy queue")
      print("Claim GATT write limit passed"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-admission") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.read, .write], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      var failures = 0
      radio.queue.sync { radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service] }
      radio.readCharacteristic(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, operationIdentifier: "read") { _, _, error in if error != nil { failures += 1 } }
      radio.queue.sync {
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        radio.releasingConnectionClaims[peer] = [ObjectIdentifier(first)]
      }
      radio.write(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, value: NSData(), withResponse: true,
        operationIdentifier: "write") { error in if error != nil { failures += 1 } }
      radio.queue.sync {
        precondition(failures == 2 && radio.scopedGattOperations.isEmpty, "detached or releasing claims cannot admit GATT")
      }
      print("Claim GATT admission passed"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-cancel-write") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let chars = ["2A37", "2A38"].map { Characteristic(type: CBUUID(string: $0), properties: [.write], value: nil, permissions: []) }
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = chars; fixtureService = service
      var firstAnswers = 0, secondAnswers = 0
      var receipt: NSDictionary?
      let done = DispatchSemaphore(value: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      for (borrower, uuid) in [(first, "2A37"), (second, "2A38")] {
        radio.write(borrower: borrower, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
          characteristicUUID: uuid, characteristicOccurrence: 0, value: NSData(), withResponse: true, operationIdentifier: "same") { error in
            if borrower === first { firstAnswers += 1 } else { precondition(error == nil); secondAnswers += 1 }
          }
      }
      radio.cancelOperation("same", borrower: first) { receipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "retryable")
      radio.queue.sync {
        radio.peripheral(peripheral, didWriteValueFor: chars[1], error: nil)
        radio.peripheral(peripheral, didWriteValueFor: chars[0], error: nil)
        precondition(firstAnswers == 0 && secondAnswers == 1)
      }
      radio.retryCancellationCleanup("same", borrower: first) { receipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "released")
      print("Claim GATT write cancellation passed"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-write-fence") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.write], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let done = DispatchSemaphore(value: 0)
      var old: [NSError?] = [], attempted: [NSError?] = [], replacement: [NSError?] = []
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      func write(_ borrower: Borrower, _ id: String, _ completion: @escaping (NSError?) -> Void) {
        radio.write(borrower: borrower, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
          characteristicUUID: "2A37", characteristicOccurrence: 0, value: NSData(), withResponse: true,
          operationIdentifier: id, completion: completion)
      }
      write(first, "old") { old.append($0) }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "disconnect") { _ in done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      write(second, "new") { attempted.append($0) }
      radio.queue.sync {
        precondition(old.count == 1 && old[0] != nil && attempted.count == 1 && attempted[0] != nil,
          "old physical write must fence the replacement")
        radio.peripheral(peripheral, didWriteValueFor: characteristic, error: nil)
      }
      write(second, "new") { replacement.append($0) }
      radio.queue.sync {
        radio.peripheral(peripheral, didWriteValueFor: characteristic, error: nil)
        precondition(old.count == 1 && replacement.count == 1 && replacement[0] == nil)
      }
      print("Claim GATT write fence passed"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-generation") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.read, .write], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      var answer: NSError?
      var readAnswer: NSError?
      var answered = false, readAnswered = false
      let done = DispatchSemaphore(value: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
      }
      radio.readCharacteristic(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, operationIdentifier: "read") { _, _, error in
          readAnswer = error; readAnswered = true
        }
      radio.write(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, value: NSData(), withResponse: true, operationIdentifier: "write") { error in
          answer = error; answered = true; done.signal()
        }
      radio.queue.sync {
        radio.connectionClaims[peer]?[ObjectIdentifier(first)] = .init(owner: first, route: first, pending: nil)
        radio.peripheral(peripheral, didUpdateValueFor: characteristic, error: nil)
        radio.peripheral(peripheral, didWriteValueFor: characteristic, error: nil)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && answered && answer != nil && readAnswered && readAnswer != nil, "old callbacks must not succeed for a replaced claim")
      print("Claim GATT generation passed"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-cancel-collision") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let chars = ["2A37", "2A38"].map { Characteristic(type: CBUUID(string: $0), properties: [.read, .write], value: nil, permissions: []) }
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = chars; fixtureService = service
      var firstAnswers = 0, secondAnswers = 0
      let cancelled = DispatchSemaphore(value: 0)
      var receipt: NSDictionary?
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.readCharacteristic(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, operationIdentifier: "same") { _, _, _ in firstAnswers += 1 }
      radio.write(borrower: second, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A38", characteristicOccurrence: 0, value: NSData(), withResponse: true, operationIdentifier: "same") { error in
          precondition(error == nil); secondAnswers += 1
        }
      radio.cancelOperation("same", borrower: first) { receipt = $0; cancelled.signal() }
      precondition(cancelled.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "retryable")
      radio.queue.sync {
        radio.peripheral(peripheral, didWriteValueFor: chars[1], error: nil)
        precondition(firstAnswers == 0 && secondAnswers == 1, "colliding write must survive cancellation")
        radio.peripheral(peripheral, didUpdateValueFor: chars[0], error: nil)
        precondition(firstAnswers == 0 && secondAnswers == 1)
      }
      let retried = DispatchSemaphore(value: 0)
      radio.retryCancellationCleanup("same", borrower: first) { receipt = $0; retried.signal() }
      precondition(retried.wait(timeout: .now() + 5) == .success && receipt?["state"] as? String == "released")
      print("Claim GATT cancellation collision passed"); return
    }
    if CommandLine.arguments.contains("--claim-gatt-retirement") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let chars = ["2A37", "2A38"].map { Characteristic(type: CBUUID(string: $0), properties: [.read, .write], value: nil, permissions: []) }
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = chars; fixtureService = service
      var firstRead = [NSError?](), secondRead = [NSError?](), firstWrite = [NSError?]()
      let done = DispatchSemaphore(value: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.readCharacteristic(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, operationIdentifier: "same") { _, _, error in firstRead.append(error) }
      radio.readCharacteristic(borrower: second, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, operationIdentifier: "same") { _, _, error in secondRead.append(error) }
      radio.write(borrower: first, peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A38", characteristicOccurrence: 0, value: NSData(), withResponse: true, operationIdentifier: "write") { firstWrite.append($0) }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "disconnect") { error in precondition(error == nil); done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(firstRead.count == 1 && firstRead[0] != nil && firstWrite.count == 1 && firstWrite[0] != nil)
        precondition(secondRead.isEmpty)
        radio.peripheral(peripheral, didUpdateValueFor: chars[0], error: nil)
        precondition(secondRead.isEmpty, "old callback must not complete the next borrower")
        radio.peripheral(peripheral, didUpdateValueFor: chars[0], error: nil)
        radio.peripheral(peripheral, didWriteValueFor: chars[1], error: nil)
        precondition(firstRead.count == 1 && firstWrite.count == 1 && secondRead.count == 1 && secondRead[0] == nil)
      }
      print("Claim GATT retirement passed"); return
    }
    if CommandLine.arguments.contains("--claim-discovery-receipt-collision") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral,
            let other = class_createInstance(OtherPeripheral.self, 0) as? OtherPeripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral); _ = Unmanaged.passRetained(other)
      let peer = peerID.uuidString, otherPeer = otherPeerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var firstReceipt: NSDictionary?, secondReceipt: NSDictionary?
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.peripheralByIdentifier[otherPeer] = other
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        radio.connectionClaims[otherPeer] = [ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.discover(borrower: first, peerIdentifier: peer, operationIdentifier: "same") { _, _ in fatalError("cancelled") }
      radio.discover(borrower: second, peerIdentifier: otherPeer, operationIdentifier: "same") { _, _ in fatalError("cancelled") }
      radio.cancelOperation("same", borrower: first) { firstReceipt = $0; done.signal() }
      radio.cancelOperation("same", borrower: second) { secondReceipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync { radio.peripheral(peripheral, didDiscoverServices: nil) }
      radio.retryCancellationCleanup("same", borrower: first) { firstReceipt = $0; done.signal() }
      radio.retryCancellationCleanup("same", borrower: second) { secondReceipt = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && done.wait(timeout: .now() + 5) == .success)
      precondition(firstReceipt?["state"] as? String == "released" && secondReceipt?["state"] as? String == "retryable", "each cleanup receipt must track only its own callback")
      radio.queue.sync { radio.peripheral(other, didDiscoverServices: nil) }
      print("Claim discovery receipt collision passed"); return
    }
    if CommandLine.arguments.contains("--claim-discovery-cancel-collision") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral,
            let other = class_createInstance(OtherPeripheral.self, 0) as? OtherPeripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral); _ = Unmanaged.passRetained(other)
      let peer = peerID.uuidString, otherPeer = otherPeerID.uuidString
      var firstAnswers = 0, secondAnswers = 0
      let cancelled = DispatchSemaphore(value: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.peripheralByIdentifier[otherPeer] = other
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        radio.connectionClaims[otherPeer] = [ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.discover(borrower: first, peerIdentifier: peer, operationIdentifier: "same") { _, _ in firstAnswers += 1 }
      radio.discover(borrower: second, peerIdentifier: otherPeer, operationIdentifier: "same") { _, error in
        precondition(error == nil); secondAnswers += 1
      }
      radio.cancelOperation("same", borrower: first) { _ in cancelled.signal() }
      precondition(cancelled.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        radio.peripheral(peripheral, didDiscoverServices: nil)
        radio.peripheral(other, didDiscoverServices: nil)
        precondition(firstAnswers == 0 && secondAnswers == 1, "cancellation must not match another borrower")
      }
      print("Claim discovery cancellation collision passed")
      return
    }
    if CommandLine.arguments.contains("--claim-discovery-retirement") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral,
            let other = class_createInstance(OtherPeripheral.self, 0) as? OtherPeripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral); _ = Unmanaged.passRetained(other)
      let peer = peerID.uuidString, otherPeer = otherPeerID.uuidString
      var firstAnswers = [NSError?](), secondAnswers = [NSError?]()
      let done = DispatchSemaphore(value: 0)
      radio.queue.sync {
        radio.peripheralByIdentifier[peer] = peripheral; radio.peripheralByIdentifier[otherPeer] = other
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        radio.connectionClaims[otherPeer] = [ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
      }
      radio.discover(borrower: first, peerIdentifier: peer, operationIdentifier: "same") { _, error in firstAnswers.append(error) }
      radio.discover(borrower: second, peerIdentifier: otherPeer, operationIdentifier: "same") { _, error in secondAnswers.append(error) }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "disconnect") { error in
        precondition(error == nil); done.signal()
      }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(firstAnswers.count == 1 && firstAnswers[0] != nil, "retired discovery must fail once")
        precondition(secondAnswers.isEmpty, "the other borrower's operation must survive")
        radio.peripheral(peripheral, didDiscoverServices: nil)
        radio.peripheral(other, didDiscoverServices: nil)
        precondition(firstAnswers.count == 1, "late callback must not complete a retired claim")
        precondition(secondAnswers.count == 1 && secondAnswers[0] == nil, "surviving discovery must complete")
      }
      print("Claim discovery retirement passed")
      return
    }
    if CommandLine.arguments.contains("--claim-readiness-fanout") {
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      radio.queue.sync {
        radio.delegate = first; radio.peripheralByIdentifier[peer] = peripheral
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        radio.connectionEpochs[peer] = UUID()
        radio.peripheralIsReady(toSendWriteWithoutResponse: peripheral)
        precondition(first.readiness == [peer] && second.readiness == [peer], "readiness reaches both owners")
        let gate = radio.orderedGraphGate(peer: peer)
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "fixture", snapshot: .init(owner: "fixture", attachment: 1, revision: 1), id: 1)
        precondition(gate.acquire(key, valid: true) == .acquired)
        radio.peripheralIsReady(toSendWriteWithoutResponse: peripheral)
        radio.connectionClaims[peer]?.removeValue(forKey: ObjectIdentifier(first))
        radio.connectionClaims[peer]?[ObjectIdentifier(first)] = .init(owner: first, route: first, pending: nil)
        precondition(gate.release(key) == .released)
        precondition(first.readiness == [peer] && second.readiness == [peer, peer], "deferred readiness must not reach a replacement claim")
        radio.connectionClaims[peer]?.removeValue(forKey: ObjectIdentifier(first))
        radio.peripheralIsReady(toSendWriteWithoutResponse: peripheral)
        precondition(first.readiness == [peer] && second.readiness == [peer, peer, peer], "departed delegate must not receive readiness")
        radio.releasingConnectionClaims[peer] = [ObjectIdentifier(second)]
        radio.peripheralIsReady(toSendWriteWithoutResponse: peripheral)
        precondition(second.readiness.count == 4, "a claim remains owned while cleanup can still fail")
        radio.releasingConnectionClaims.removeValue(forKey: peer)
        let next = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "fixture", snapshot: .init(owner: "fixture", attachment: 1, revision: 1), id: 2)
        precondition(gate.acquire(next, valid: true) == .acquired)
        radio.peripheralIsReady(toSendWriteWithoutResponse: peripheral)
        radio.connectionEpochs[peer] = UUID()
        precondition(gate.release(next) == .released && second.readiness.count == 4, "old connection readiness cannot reach a replacement generation")
      }
      print("Claim readiness fanout passed")
      return
    }
    if CommandLine.arguments.contains("--borrower-retry-nonconnection") {
      let done = DispatchSemaphore(value: 0)
      var result: NSDictionary?
      radio.queue.sync {
        var cleanup = PendingCancellationCleanup()
        cleanup.discoveryPeers.insert("peer")
        let claim = OwnedCoreBluetoothProtocolRadio.ConnectionClaim(owner: first, route: first, pending: nil)
        radio.connectionClaims["peer"] = [ObjectIdentifier(first): claim]
        radio.scopedGattOperations[ObjectIdentifier(first)] = ["discovery": .init(owner: first, claim: claim.identity, peer: "peer", physical: "physical-discovery")]
        radio.pendingDiscovery["peer"] = PendingDiscovery(operationIdentifier: "physical-discovery", completion: { _, _ in }, owner: first, claimIdentity: claim.identity)
        radio.pendingCancellationCleanup["physical-discovery"] = cleanup
      }
      radio.retryCancellationCleanup("discovery", borrower: first) { result = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && result?["state"] as? String == "retryable", "pending discovery cleanup cannot report released")
      print("Borrower non-connection cleanup retry passed")
      return
    }
    if CommandLine.arguments.contains("--claim-partial-notification-cleanup") || CommandLine.arguments.contains("--claim-partial-route-replaced") || CommandLine.arguments.contains("--claim-partial-other-borrower") {
      let replaceRoute = CommandLine.arguments.contains("--claim-partial-route-replaced")
      let otherBorrower = CommandLine.arguments.contains("--claim-partial-other-borrower")
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristics = ["2A37", "2A38"].map {
        Characteristic(type: CBUUID(string: $0), properties: [.notify], value: nil, permissions: [])
      }
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = characteristics; fixtureService = service
      let identifiers = ["one", "two"]
      let done = DispatchSemaphore(value: 0)
      var answer: NSError?
      var groups: [OwnedOrderedNotificationIngress] = []
      var otherGroups: [OwnedOrderedNotificationIngress] = []
      radio.queue.sync {
        radio.central = central; radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        if otherBorrower { radio.connectionClaims[peer]?[ObjectIdentifier(second)] = .init(owner: second, route: second, pending: nil) }
        let source = OwnedOrderedNotificationSource(queue: radio.queue, generation: "partial-cleanup")
        radio.orderedSources[peer] = source
        for (index, characteristic) in characteristics.enumerated() {
          let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
            characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(characteristic.uuid.uuidString), characteristicOccurrence: 0)
          precondition(radio.installNotificationRoute(identifiers[index], borrower: first))
          radio.subscriptions[address] = identifiers[index]; notifying[ObjectIdentifier(characteristic)] = true
          let route = radio.orderedRoute(for: address)
          let group = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation,
            members: [route], capacity: 4, byteCapacity: 32)
          precondition(source.add(group))
          let claim = source.makeClaim(route: route)!
          try! group.register(claim)
          radio.notificationRoutes[identifiers[index]]?.orderedClaim = claim
          if otherBorrower {
            let otherGroup = try! OwnedOrderedNotificationIngress(queue: radio.queue, generation: source.generation,
              members: [route], capacity: 4, byteCapacity: 32)
            precondition(source.add(otherGroup))
            try! otherGroup.register(source.makeClaim(route: route)!)
            otherGroups.append(otherGroup)
          }
          groups.append(group)
        }
      }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "release") { answer = $0; done.signal() }
      func waitForWrites(_ count: Int) {
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline && radio.queue.sync(execute: { physicalWrites.count }) < count { Thread.sleep(forTimeInterval: 0.01) }
        precondition(radio.queue.sync { physicalWrites.count } == count)
      }
      waitForWrites(1)
      let retired = radio.queue.sync { physicalWrites[0].0 }
      let retiredIndex = characteristics.firstIndex { ObjectIdentifier($0) == retired }!
      radio.queue.sync {
        notifying[retired] = false
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristics[retiredIndex], error: nil)
        precondition(first.failures.isEmpty, "a successful release must not emit a premature loss")
      }
      waitForWrites(2)
      let failedIndex = 1 - retiredIndex
      radio.queue.sync {
        if replaceRoute {
          radio.removeNotificationRoute(identifiers[retiredIndex], borrower: first)
          precondition(radio.installNotificationRoute(identifiers[retiredIndex], borrower: second))
        }
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristics[failedIndex], error: NSError(domain: "fixture", code: 1))
      }
      precondition(done.wait(timeout: .now() + 5) == .success && answer != nil)
      radio.queue.sync {
        precondition(radio.connectionClaims[peer]?[ObjectIdentifier(first)] != nil, "failure retains the connection claim")
        if replaceRoute {
          precondition(second.failures.isEmpty && radio.notificationRoutes[identifiers[retiredIndex]]?.borrower === second, "cleanup must not fail or remove a replacement route")
          precondition(first.failures == [identifiers[retiredIndex]] && first.failureErrors.first?.code == 1039, "departed ordinary owner must receive its loss")
          guard case let .terminal(.sourceFailed(failure, _, _, _), _, _) = groups[retiredIndex].drain().first else {
            fatalError("replacement route must not erase the retired ordered claimant's failure")
          }
          precondition(failure.domain == "fixture" && failure.code == 1, "ordered failure must retain the actual disable error")
        } else {
          precondition(first.failures == [identifiers[retiredIndex]], "the removed subscription must report terminal loss")
          precondition(first.failureErrors[0].code == 1039 && (first.failureErrors[0].userInfo[NSUnderlyingErrorKey] as? NSError)?.domain == "fixture", "loss must identify the retired source and preserve the disable failure")
          precondition(radio.notificationRoutes[identifiers[retiredIndex]] == nil && radio.notificationRoutes[identifiers[failedIndex]] != nil)
          let events = groups[retiredIndex].drain()
          if otherBorrower {
            guard case let .terminal(.sourceFailed(failure, _, _, _), _, _) = events.first else {
              fatalError("retired claimant must receive its ordered failure")
            }
            precondition(failure.domain == "fixture" && failure.code == 1)
            precondition(otherGroups[retiredIndex].drain().isEmpty, "another claimant on the same route must remain open")
          } else {
            guard case let .terminal(.sourceFailed(failure, _, _, _), _, _) = events.first else {
              fatalError("retired ordered source must report partial cleanup failure")
            }
            precondition(failure.domain == "fixture" && failure.code == 1, "ordered failure must retain the actual disable error")
          }
          precondition(groups[failedIndex].drain().isEmpty, "unaffected route must remain open")
        }
      }
      if replaceRoute { print("Replacement notification route fenced"); return }
      if otherBorrower { print("Other borrower ordered source preserved"); return }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "retry") { answer = $0; done.signal() }
      waitForWrites(3)
      radio.queue.sync {
        notifying[ObjectIdentifier(characteristics[failedIndex])] = false
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristics[failedIndex], error: nil)
        precondition(cancelledConnections == 1)
        connectionState = .disconnected
        radio.centralManager(central, didDisconnectPeripheral: peripheral, error: nil)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && answer == nil)
      radio.queue.sync { precondition(first.failures == [identifiers[retiredIndex]], "retry must not duplicate the loss") }
      print("Partial notification cleanup reported")
      return
    }
    if CommandLine.arguments.contains("--borrower-pending-failure-fence") {
      precondition(radio.attachDelegateIfAvailable(first))
      radio.queue.sync { first.pendingFailureRetention = true }
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure?.code == 1014)
      precondition(!radio.attachDelegateIfAvailable(second), "pending captured failure must prevent a replacement borrower")
      radio.queue.sync { first.pendingFailureRetention = false }
      radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      precondition(radio.attachDelegateIfAvailable(second), "borrower can be replaced after its failure is delivered")
      print("Pending failure fenced borrower release"); return
    }
    if CommandLine.arguments.contains("--claim-notification-retention-refused") {
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
      radio.queue.sync {
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        precondition(radio.installNotificationRoute("ordinary", borrower: first))
        radio.subscriptions[address] = "ordinary"; first.refuseFailureCapture = true
        var answer: NSError?
        radio.retireConnectionNotifications(owner: first, peer: peer) { answer = $0 }
        precondition(answer?.code == 1014 && radio.subscriptions[address] == "ordinary" && radio.notificationRoutes["ordinary"] != nil,
          "cleanup must retain the owner when its failure receipt cannot be reserved")
        precondition(physicalWrites.isEmpty)
      }
      print("Notification retention refusal retained owner"); return
    }
    if CommandLine.arguments.contains("--notification-stale-before-disconnect-fence") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      radio.queue.sync {
        radio.central = central; radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil)]
        precondition(radio.installNotificationRoute("ordinary", borrower: first))
        radio.subscriptions[address] = "ordinary"
        radio.unfencedConnections.insert(peer)
        connectionState = .disconnected
      }
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.unsubscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0, characteristicUUID: "2A37", characteristicOccurrence: 0,
        subscriptionIdentifier: "ordinary", operationIdentifier: "stop") { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure?.code == 1017)
      radio.queue.sync {
        precondition(radio.unfencedConnections.contains(peer) && radio.connectionClaims[peer] != nil && radio.subscriptions[address] == "ordinary",
          "a stale path must precede the authoritative disconnect fence")
        radio.centralManager(central, didDisconnectPeripheral: peripheral, error: nil)
        precondition(!radio.unfencedConnections.contains(peer) && radio.connectionClaims[peer] == nil && first.disconnects == [peer])
      }
      print("Stale path preceded disconnect fence"); return
    }
    if CommandLine.arguments.contains("--claim-unresolved-cleanup-fence") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      radio.queue.sync {
        radio.central = central; radio.peripheralByIdentifier[peer] = peripheral
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        var cleanup = PendingCancellationCleanup()
        cleanup.notificationDesiredStates[address] = false
        radio.pendingCancellationCleanup["unresolved"] = cleanup
        radio.disconnectOnQueue(owner: first, peerIdentifier: peer, operationIdentifier: "release") { failure in
          precondition(failure != nil, "unresolved peer notification cleanup cannot be reported as released")
        }
        precondition(radio.connectionClaims[peer]?[ObjectIdentifier(first)] != nil && cancelledConnections == 0)
      }
      print("Unresolved notification cleanup fenced")
      return
    }
    if CommandLine.arguments.contains("--claim-disconnect-physical-cleanup") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let characteristic = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      service.characteristics = [characteristic]; fixtureService = service
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
      let done = DispatchSemaphore(value: 0)
      var answer: NSError?
      radio.queue.sync {
        radio.central = central; radio.peripheralByIdentifier[peer] = peripheral; radio.servicesByPeer[peer] = [service]
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.installNotificationRoute("first-only", borrower: first))
        radio.subscriptions[address] = "first-only"; notifying[ObjectIdentifier(characteristic)] = true
      }
      func startRelease() {
        radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "release") { answer = $0; done.signal() }
      }
      func waitForWrites(_ count: Int) {
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline && radio.queue.sync(execute: { physicalWrites.count }) < count { Thread.sleep(forTimeInterval: 0.01) }
        precondition(radio.queue.sync { physicalWrites.count } == count, "cleanup must dispatch a physical disable")
      }
      startRelease(); waitForWrites(1)
      precondition(done.wait(timeout: .now() + 0.05) == .timedOut, "disconnect cannot succeed before CCCD confirmation")
      var cancellation: NSDictionary?
      radio.cancelOperation("release", borrower: first) { result in cancellation = result; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && cancellation?["state"] as? String == "retryable",
        "cancellation must retain the in-flight notification cleanup")
      radio.retryCancellationCleanup("release", borrower: first) { result in cancellation = result; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && cancellation?["state"] as? String == "retryable",
        "cleanup retry must remain pending until the physical callback")
      radio.queue.sync {
        precondition(radio.connectionClaims[peer]?[ObjectIdentifier(first)] != nil)
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: NSError(domain: "fixture", code: 1))
      }
      precondition(done.wait(timeout: .now() + 5) == .success && answer != nil, "failed disable must leave the claim retryable")
      startRelease(); waitForWrites(2)
      radio.queue.sync {
        notifying[ObjectIdentifier(characteristic)] = false
        radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: nil)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && answer == nil)
      radio.queue.sync {
        precondition(radio.connectionClaims[peer]?[ObjectIdentifier(first)] == nil && radio.connectionClaims[peer]?[ObjectIdentifier(second)] != nil)
        precondition(cancelledConnections == 0 && radio.notificationRoutes["first-only"] == nil)
      }
      print("Claim physical notification cleanup passed")
      return
    }
    if CommandLine.arguments.contains("--claim-shared-discovery") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var snapshots: [NSDictionary] = []
      radio.queue.sync {
        radio.central = central; radio.delegate = first; radio.peripheralByIdentifier[peer] = peripheral
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        precondition(radio.orderedSnapshotIdentities.ensureAttachment(peer))
        _ = radio.orderedSnapshotIdentities.publish(peer)
        radio.servicesByPeer[peer] = []
      }
      for operation in ["A", "B"] {
        radio.discover(peerIdentifier: peer, operationIdentifier: operation) { snapshot, error in
          precondition(error == nil && snapshot != nil); snapshots.append(snapshot!); done.signal()
        }
        precondition(done.wait(timeout: .now() + 5) == .success)
      }
      precondition(snapshots[0]["nativeSnapshotRevision"] as? NSNumber == snapshots[1]["nativeSnapshotRevision"] as? NSNumber,
        "a second discovery must share the live graph identity")
      radio.queue.sync {
        let before = radio.orderedSnapshotIdentities.current(peer)
        radio.peripheral(peripheral, didModifyServices: [])
        precondition(before != nil && radio.orderedSnapshotIdentities.current(peer) == nil)
        precondition(first.serviceChanges == [peer] && second.serviceChanges == [peer])
      }
      radio.discover(peerIdentifier: peer, operationIdentifier: "rediscover") { snapshot, error in
        precondition(error == nil && snapshot != nil); snapshots.append(snapshot!); done.signal()
      }
      radio.queue.sync {
        precondition(discoveryStarts == 1, "service change requires a fresh native discovery")
        radio.peripheral(peripheral, didDiscoverServices: nil)
      }
      precondition(done.wait(timeout: .now() + 5) == .success)
      precondition(snapshots[0]["nativeSnapshotRevision"] as? NSNumber != snapshots[2]["nativeSnapshotRevision"] as? NSNumber,
        "rediscovery after service change publishes a new graph identity")
      print("Shared graph discovery identity passed")
      return
    }
    if CommandLine.arguments.contains("--claim-state-and-service-fanout") || CommandLine.arguments.contains("--claim-disconnect-routes") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let address = CharacteristicAddress(peerIdentifier: peer, serviceUUID: "service", serviceOccurrence: 0,
        characteristicUUID: "shared", characteristicOccurrence: 0)
      let characteristic = CBMutableCharacteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
      radio.queue.sync {
        radio.central = central; radio.delegate = first; radio.peripheralByIdentifier[peer] = peripheral
        radio.connectionClaims[peer] = [ObjectIdentifier(first): .init(owner: first, route: first, pending: nil),
          ObjectIdentifier(second): .init(owner: second, route: second, pending: nil)]
        if CommandLine.arguments.contains("--claim-state-and-service-fanout") {
          radio.peripheral(peripheral, didModifyServices: [])
          precondition(first.serviceChanges == [peer] && second.serviceChanges == [peer], "service change reaches both connection owners")
          radio.routeCentralState(.poweredOff, snapshot: [:])
          precondition(first.states == 1 && second.states == 1, "powered off reaches connection-only owner once")
          radio.routeCentralState(.resetting, snapshot: [:])
          precondition(first.states == 2 && second.states == 2, "reset reaches owners even when their claims are invalidated")
        } else {
          precondition(radio.installNotificationRoute("first", borrower: first))
          precondition(radio.installNotificationRoute("second", borrower: second))
          radio.subscriptions[address] = "first"; radio.sharedSubscriptions[address] = ["second"]
          radio.disconnectOnQueue(owner: first, peerIdentifier: peer, operationIdentifier: "release") { error in precondition(error == nil) }
          radio.handleCharacteristicValueUpdate(address: address, peripheral: peripheral, characteristic: characteristic,
            error: nil, snapshotValue: Data([1]), snapshotIsNotifying: true, hasSnapshot: true)
          precondition(first.values.isEmpty && second.values == ["second"], "disconnect retires only the departing route")
          precondition(radio.subscriptions[address] == "second" && cancelledConnections == 0, "surviving CCCD and connection remain owned")
        }
      }
      if CommandLine.arguments.contains("--claim-disconnect-routes") {
        let done = DispatchSemaphore(value: 0)
        var answer: NSError?
        radio.queue.sync { precondition(radio.installNotificationRoute("first-again", borrower: first)) }
        radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
          characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "first-again",
          operationIdentifier: "after-disconnect") { answer = $0; done.signal() }
        precondition(done.wait(timeout: .now() + 5) == .success && answer?.code == 1007,
          "departed owner cannot acquire the surviving owner's graph or CCCD")
      }
      print("Claim ownership regression passed")
      return
    }
    // Red until connection calls carry borrower identity. This flag bypasses
    // the public attachment guard solely to expose the production boundary.
    if CommandLine.arguments.contains("--connection-ownership-red") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync {
        radio.central = central
        radio.peripheralByIdentifier[peer] = peripheral
        precondition(radio.borrowerRelease.attach(first))
        precondition(radio.borrowerRelease.attach(second))
      }
      for borrower in [first, second] {
        radio.connect(borrower: borrower, peerIdentifier: peer, operationIdentifier: "same") { failure = $0; done.signal() }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      }
      radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "release-A") { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      radio.queue.sync {
        precondition(cancelledConnections == 0, "one borrower's disconnect must retain another borrower's physical claim")
      }
      radio.disconnect(borrower: second, peerIdentifier: peer, operationIdentifier: "release-B") { failure = $0; done.signal() }
      radio.queue.sync {
        precondition(cancelledConnections == 1, "the last claim must disconnect the physical link")
        connectionState = .disconnected
        radio.centralManager(central, didDisconnectPeripheral: peripheral, error: nil)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      print("Connection borrower ownership passed")
      return
    }
    if CommandLine.arguments.contains("--connection-cancel-collision") || CommandLine.arguments.contains("--connection-pending-release") {
      let releasing = CommandLine.arguments.contains("--connection-pending-release")
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var firstCalls = 0
      var secondCalls = 0
      var secondError: NSError?
      connectionState = .disconnected
      radio.queue.sync { radio.central = central; radio.peripheralByIdentifier[peer] = peripheral }
      radio.connect(borrower: first, peerIdentifier: peer, operationIdentifier: "same") { _ in firstCalls += 1 }
      radio.connect(borrower: second, peerIdentifier: peer, operationIdentifier: "same") { error in secondCalls += 1; secondError = error; done.signal() }
      radio.queue.sync { precondition(startedConnections == 1, "concurrent claims share one physical connect") }
      if releasing {
        radio.disconnect(borrower: first, peerIdentifier: peer, operationIdentifier: "release") { error in
          precondition(error == nil); done.signal()
        }
      } else {
        radio.cancelOperation("same", borrower: first) { _ in done.signal() }
      }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(cancelledConnections == 0 && firstCalls == (releasing ? 1 : 0) && secondCalls == 0,
          "retiring A must not cancel B or the shared link")
        connectionState = .connected
        radio.centralManager(central, didConnect: peripheral)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && secondCalls == 1 && secondError == nil)
      radio.queue.sync {
        connectionState = .disconnected
        radio.centralManager(central, didDisconnectPeripheral: peripheral, error: NSError(domain: "fixture", code: 7))
        precondition(first.disconnects.isEmpty && second.disconnects == [peer], "disconnect reaches only the surviving owner")
      }
      print("Connection cancellation ownership passed")
      return
    }
    if CommandLine.arguments.contains("--connection-cross-peer-collision") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let a = class_createInstance(Peripheral.self, 0) as? Peripheral,
            let b = class_createInstance(OtherPeripheral.self, 0) as? OtherPeripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(a); _ = Unmanaged.passRetained(b)
      let done = DispatchSemaphore(value: 0)
      var secondCalls = 0
      connectionState = .disconnected; otherConnectionState = .disconnected
      radio.queue.sync {
        radio.central = central; radio.peripheralByIdentifier[peerID.uuidString] = a
        radio.peripheralByIdentifier[otherPeerID.uuidString] = b
      }
      radio.connect(borrower: first, peerIdentifier: peerID.uuidString, operationIdentifier: "same") { _ in fatalError("cancelled borrower completed") }
      radio.connect(borrower: second, peerIdentifier: otherPeerID.uuidString, operationIdentifier: "same") { error in
        precondition(error == nil); secondCalls += 1; done.signal()
      }
      radio.queue.sync { connectionState = .connecting; otherConnectionState = .connecting }
      radio.cancelOperation("same", borrower: first) { _ in done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success)
      radio.queue.sync {
        precondition(!cancelledPeers.isEmpty && cancelledPeers.allSatisfy { $0 == peerID }, "cancellation must not touch the other peer")
        precondition(radio.pendingConnect[otherPeerID.uuidString] != nil)
        otherConnectionState = .connected
        radio.centralManager(central, didConnect: b)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && secondCalls == 1)
      print("Cross-peer operation identity passed")
      return
    }
    if CommandLine.arguments.contains("--connection-failure-retry") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var firstErrors: [Int] = []
      var secondErrors: [Int] = []
      connectionState = .disconnected
      radio.queue.sync { radio.central = central; radio.peripheralByIdentifier[peer] = peripheral }
      radio.connect(borrower: first, peerIdentifier: peer, operationIdentifier: "same") { error in firstErrors.append(error?.code ?? 0); done.signal() }
      radio.connect(borrower: second, peerIdentifier: peer, operationIdentifier: "same") { error in secondErrors.append(error?.code ?? 0); done.signal() }
      radio.queue.sync {
        precondition(startedConnections == 1)
        radio.centralManager(central, didFailToConnect: peripheral, error: NSError(domain: "fixture", code: 33))
      }
      precondition(done.wait(timeout: .now() + 5) == .success && done.wait(timeout: .now() + 5) == .success)
      precondition(firstErrors == [33] && secondErrors == [33], "failure must reach both exact pending owners")
      radio.connect(borrower: first, peerIdentifier: peer, operationIdentifier: "retry") { error in firstErrors.append(error?.code ?? 0); done.signal() }
      radio.queue.sync { precondition(startedConnections == 2); connectionState = .connecting }
      var cleanupState = ""
      radio.cancelOperation("retry", borrower: first) { result in cleanupState = result["state"] as? String ?? ""; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && cleanupState == "retryable")
      radio.retryCancellationCleanup("retry", borrower: first) { result in cleanupState = result["state"] as? String ?? ""; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && cleanupState == "retryable", "retry must retain the exact physical cleanup")
      var refusal: NSError?
      radio.connect(borrower: second, peerIdentifier: peer, operationIdentifier: "blocked") { refusal = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && refusal != nil, "retry must wait for terminal fence")
      radio.queue.sync {
        precondition(cancelledConnections >= 1)
        connectionState = .disconnected
        radio.centralManager(central, didFailToConnect: peripheral, error: NSError(domain: "fixture", code: 34))
      }
      radio.retryCancellationCleanup("retry", borrower: first) { result in cleanupState = result["state"] as? String ?? ""; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && cleanupState == "released")
      radio.connect(borrower: second, peerIdentifier: peer, operationIdentifier: "retry-after-fence") { error in secondErrors.append(error?.code ?? 0); done.signal() }
      radio.queue.sync {
        precondition(startedConnections == 3, "terminal callback must permit a fresh generation")
        connectionState = .connected
        radio.centralManager(central, didConnect: peripheral)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && secondErrors == [33, 0])
      precondition(firstErrors == [33], "cancelled first generation must not complete after retry")
      print("Connection failure and retry passed")
      return
    }
    if CommandLine.arguments.contains("--connection-stale-callback") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      var firstCalls = 0
      var secondCalls = 0
      connectionState = .disconnected
      radio.queue.sync {
        radio.central = central; radio.peripheralByIdentifier[peer] = peripheral
        radio.connectOnQueue(owner: first, route: first, peerIdentifier: peer, operationIdentifier: "old") { error in
          precondition(error != nil); firstCalls += 1
        }
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "old",
          snapshot: .init(owner: "fixture", attachment: 1, revision: 1), id: 1)
        let gate = radio.orderedGraphGate(peer: peer)
        precondition(gate.acquire(key, valid: true) == .acquired)
        connectionState = .connected
        radio.centralManager(central, didConnect: peripheral)
        radio.completeObservedDisconnect(peer, error: NSError(domain: "fixture", code: 9))
        precondition(firstCalls == 1)
        connectionState = .disconnected
        radio.connectOnQueue(owner: second, route: second, peerIdentifier: peer, operationIdentifier: "new") { error in
          precondition(error == nil); secondCalls += 1
        }
        precondition(gate.release(key) == .released)
        precondition(secondCalls == 0, "deferred callback from an old epoch must not complete the new owner")
        connectionState = .connected
        radio.centralManager(central, didConnect: peripheral)
        precondition(secondCalls == 1)
      }
      print("Connection stale callback fenced")
      return
    }
    if CommandLine.arguments.contains("--connection-terminal-fence") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      radio.queue.sync {
        radio.central = central; radio.peripheralByIdentifier[peer] = peripheral
        radio.connectOnQueue(owner: first, route: first, peerIdentifier: peer, operationIdentifier: "old") { precondition($0 == nil) }
        let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "old",
          snapshot: .init(owner: "fixture", attachment: 1, revision: 1), id: 2)
        let gate = radio.orderedGraphGate(peer: peer)
        precondition(gate.acquire(key, valid: true) == .acquired)
        connectionState = .disconnected
        radio.centralManager(central, didDisconnectPeripheral: peripheral, error: nil)
        var rejected = false
        radio.connectOnQueue(owner: second, route: second, peerIdentifier: peer, operationIdentifier: "new") { rejected = $0 != nil }
        precondition(rejected, "a queued terminal callback must fence a new borrower")
        precondition(gate.release(key) == .released)
        precondition(first.disconnects == [peer] && second.disconnects.isEmpty)
        var completed = false
        radio.connectOnQueue(owner: second, route: second, peerIdentifier: peer, operationIdentifier: "retry") { error in
          precondition(error == nil); completed = true
        }
        connectionState = .connected
        radio.centralManager(central, didConnect: peripheral)
        precondition(completed)
      }
      print("Connection terminal callback fence passed")
      return
    }
    if CommandLine.arguments.contains("--connection-final-release") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let peer = peerID.uuidString
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync { radio.central = central; radio.peripheralByIdentifier[peer] = peripheral }
      precondition(radio.attachDelegateIfAvailable(first))
      radio.connect(borrower: first, peerIdentifier: peer, operationIdentifier: "connect") { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
      radio.queue.sync {}
      radio.queue.sync {}
      precondition(!radio.attachDelegateIfAvailable(second), "attachment must remain fenced while cleanup waits")
      radio.queue.sync {
        precondition(cancelledConnections == 1, "final release must disconnect")
        connectionState = .disconnected
        radio.centralManager(central, didDisconnectPeripheral: peripheral, error: nil)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      precondition(radio.attachDelegateIfAvailable(second), "a new borrower can attach after cleanup")
      print("Connection final release passed")
      return
    }
    if CommandLine.arguments.contains("--borrower-release-stale-retry") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync { radio.central = central }
      precondition(radio.attachDelegateIfAvailable(first))
      radio.startScan(borrower: first, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") {
        failure = $0; done.signal()
      }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      radio.queue.sync {
        first.refuseLoss = true
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataManufacturerDataKey: Data(repeating: 1, count: OwnedCoreBluetoothProtocolRadio.scanCallbackByteLimit + 1)], rssi: -40)
      }
      radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure?.code == 1014)
      radio.queue.sync { first.refuseLoss = false }
      radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      precondition(radio.attachDelegateIfAvailable(first))
      Thread.sleep(forTimeInterval: 1.5)
      radio.queue.sync {
        precondition(radio.borrowerRelease.admits(first), "a retry from a retired attachment must not release its replacement")
      }
      print("Borrower stale release retry fenced")
      return
    }
    if CommandLine.arguments.contains("--borrower-release-retry-isolation") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync {
        radio.central = central
        precondition(radio.borrowerRelease.attach(first))
        precondition(radio.borrowerRelease.attach(second))
      }
      for borrower in [first, second] {
        radio.startScan(borrower: borrower, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") {
          failure = $0; done.signal()
        }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      }
      radio.queue.sync {
        first.refuseLoss = true; second.refuseLoss = true
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataManufacturerDataKey: Data(repeating: 1, count: OwnedCoreBluetoothProtocolRadio.scanCallbackByteLimit + 1)], rssi: -40)
      }
      for borrower in [first, second] {
        radio.releaseBorrowerIfCurrent(borrower) { failure = $0; done.signal() }
        precondition(done.wait(timeout: .now() + 5) == .success && failure?.code == 1014)
      }
      radio.queue.sync { first.refuseLoss = false; second.refuseLoss = false }
      let deadline = Date().addingTimeInterval(3)
      while Date() < deadline && radio.queue.sync(execute: { !radio.scanLeases.isEmpty }) {
        Thread.sleep(forTimeInterval: 0.02)
      }
      radio.queue.sync {
        precondition(radio.scanLeases.isEmpty, "each borrower's failed release must receive its own automatic retry")
        precondition(first.lostAdvertisements == 1 && second.lostAdvertisements == 1)
      }
      print("Borrower release retry isolation passed")
      return
    }
    if CommandLine.arguments.contains("--scan-loss-isolation") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync { radio.central = central }
      for borrower in [first, second] {
        radio.startScan(borrower: borrower, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") {
          failure = $0; done.signal()
        }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      }
      radio.queue.sync {
        first.rejectAdvertisements = true; first.refuseLoss = true
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.drainScanCallbacks()
        precondition(radio.pendingScanLosses[ObjectIdentifier(first)] != nil && second.advertisements == 1)
        scanState = .poweredOff; radio.scanAdapterStateChanged(.poweredOff)
        scanState = .poweredOn; radio.scanAdapterStateChanged(.poweredOn)
        precondition(radio.physicalScanActive, "A's old loss must not block B's recovered physical scan")
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.drainScanCallbacks()
        precondition(second.advertisements == 2 && first.advertisements == 0)
        first.rejectAdvertisements = false; first.refuseLoss = false
        radio.drainScanCallbacks()
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.drainScanCallbacks()
        precondition(first.lostAdvertisements == 1 && first.advertisements == 1 && second.advertisements == 3)
      }
      print("Scan reset loss isolation passed")
      return
    }
    if CommandLine.arguments.contains("--scan-release-retry") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync { radio.central = central }
      precondition(radio.attachDelegateIfAvailable(first))
      for borrower in [first, second] {
        radio.startScan(borrower: borrower, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") {
          failure = $0; done.signal()
        }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      }
      radio.queue.sync {
        first.refuseLoss = true
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataManufacturerDataKey: Data(repeating: 1, count: OwnedCoreBluetoothProtocolRadio.scanCallbackByteLimit + 1)], rssi: -40)
      }
      radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure?.code == 1014)
      radio.queue.sync {
        precondition(radio.scanLeases[ObjectIdentifier(first)] != nil && radio.pendingScanLosses[ObjectIdentifier(first)] != nil)
        precondition(radio.scanLeases[ObjectIdentifier(second)] != nil && scanStops == 0)
        first.refuseLoss = false
      }
      radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      radio.queue.sync {
        precondition(first.lostAdvertisements == 1 && radio.pendingScanLosses[ObjectIdentifier(first)] == nil)
        precondition(radio.scanLeases[ObjectIdentifier(first)] == nil && radio.scanLeases[ObjectIdentifier(second)] != nil)
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.drainScanCallbacks()
        precondition(first.advertisements == 0 && second.advertisements == 1 && scanStops == 0)
      }
      print("Scan borrower release and loss retry passed")
      return
    }
    if CommandLine.arguments.contains("--scan-filtering") || CommandLine.arguments.contains("--scan-duplicates") || CommandLine.arguments.contains("--scan-release-owner") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync { radio.central = central }
      precondition(radio.attachDelegateIfAvailable(first))
      for (borrower, service, duplicates) in [(first, "180D", true), (second, "180F", false)] {
        radio.startScan(borrower: borrower, serviceUUIDs: [service], allowDuplicates: duplicates, operationIdentifier: "scan") {
          failure = $0; done.signal()
        }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      }
      if CommandLine.arguments.contains("--scan-release-owner") {
        radio.releaseBorrowerIfCurrent(first) { failure = $0; done.signal() }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
        radio.queue.sync {
          precondition(radio.scanLeases[ObjectIdentifier(second)] != nil, "releasing A must retain B's scan claim")
          precondition(scanStops == 0, "releasing A must not stop B's physical scan")
        }
        print("Scan release owner passed")
        return
      }
      if CommandLine.arguments.contains("--scan-duplicates") {
        radio.queue.sync {
          radio.centralManager(central, didDiscover: peripheral,
            advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180D")]], rssi: -40)
          radio.drainScanCallbacks()
          let beforeMatch = second.advertisements
          radio.centralManager(central, didDiscover: peripheral,
            advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180F")]], rssi: -40)
          radio.drainScanCallbacks()
          precondition(second.advertisements == beforeMatch + 1, "indeterminate evidence must not consume B's first match")
          radio.centralManager(central, didDiscover: peripheral,
            advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180F")]], rssi: -40)
          radio.drainScanCallbacks()
          precondition(second.advertisements == beforeMatch + 1, "a duplicate-disabled borrower must receive one matching peer")
        }
        print("Scan duplicate policy passed")
        return
      }
      radio.queue.sync {
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180D")]], rssi: -40)
          radio.drainScanCallbacks()
        precondition(first.advertisements == 1, "A must receive its visible match")
        precondition(second.records.allSatisfy { ($0["scanMatch"] as? String) == "indeterminate" },
          "a callback without B's service cannot be represented as B's proven match")
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180F")]], rssi: -40)
        radio.drainScanCallbacks()
        let afterFirstMatch = second.advertisements
        precondition(afterFirstMatch > 0, "B must receive its visible match")
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180F")]], rssi: -40)
        radio.drainScanCallbacks()
        precondition(second.advertisements == afterFirstMatch, "B receives only its first matching peer")
      }
      print("Scan per-owner filtering passed")
      return
    }
    if CommandLine.arguments.contains("--scan-reset-fanout") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync { radio.central = central; radio.delegate = first }
      for borrower in [first, second] {
        radio.startScan(borrower: borrower, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") {
          failure = $0; done.signal()
        }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      }
      radio.queue.sync {
        scanState = .resetting
        // Whichever callback arrives first removes the other claim. A state
        // captured for both owners must still reach both exactly once.
        first.onState = { radio.stopScanOnQueue(owner: second) { _ in } }
        second.onState = { radio.stopScanOnQueue(owner: first) { _ in } }
        radio.centralManagerDidUpdateState(central)
        precondition(first.states == 1 && second.states == 1, "adapter reset must reach each captured scan owner once despite reentrant removal")
      }
      print("Scan adapter reset fanout passed")
      return
    }
    if CommandLine.arguments.contains("--scan-teardown-race") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      let key = OwnedOrderedGraphPermitGate.Key(peer: "held", connection: "c",
        snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
      radio.queue.sync {
        radio.central = central; radio.delegate = first
        radio.startScanOnQueue(owner: first, route: first, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") { failure = $0 }
        precondition(failure == nil)
        precondition(radio.orderedGraphGate(peer: "held").acquire(key, valid: true) == .acquired)
      }
      radio.destroy { failure = $0; done.signal() }
      radio.queue.sync {
        first.refuseLoss = true
        let big = Data(repeating: 1, count: OwnedCoreBluetoothProtocolRadio.scanCallbackByteLimit + 1)
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataManufacturerDataKey: big], rssi: -40)
        precondition(radio.orderedGraphGate(peer: "held").release(key) == .released)
      }
      precondition(done.wait(timeout: .now() + 5) == .success && failure != nil,
        "loss arriving during a held teardown barrier must block retirement")
      radio.queue.sync {
        precondition(!radio.destroyed && radio.pendingScanLosses.count == 1)
        first.refuseLoss = false
        radio.drainScanCallbacks()
        precondition(first.lostAdvertisements == 1)
        radio.stopScanOnQueue(owner: first) { failure = $0 }
        precondition(failure == nil)
      }
      radio.destroy { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      print("Scan loss during teardown barrier passed")
      return
    }
    if CommandLine.arguments.contains("--scan-loss-lifecycle") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync { radio.central = central; radio.delegate = first }
      radio.startScan(serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      var generation = UUID()
      let big = Data(repeating: 1, count: OwnedCoreBluetoothProtocolRadio.scanCallbackByteLimit + 1)
      radio.queue.sync {
        generation = radio.scanLeases[ObjectIdentifier(first)]!.generation
        first.refuseLoss = true
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataManufacturerDataKey: big], rssi: -40)
        radio.drainScanCallbacks()
        precondition(radio.pendingScanCallbacks.isEmpty && radio.pendingScanCallbackBytes == 0)
        precondition(radio.pendingScanLosses[ObjectIdentifier(first)]?.exactCount == 1, "byte rejection is counted exactly")
        precondition(first.refusedLossAttempts > 0)
        scanState = .poweredOff
        radio.scanAdapterStateChanged(.poweredOff)
        scanState = .poweredOn
        radio.scanAdapterStateChanged(.poweredOn)
        precondition(!radio.physicalScanActive, "new generation waits for old loss acknowledgement")
        radio.stopScanOnQueue(owner: first) { failure = $0 }
        precondition(failure == nil)
        radio.startScanOnQueue(owner: first, route: first, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") { failure = $0 }
        precondition(failure != nil, "restart cannot overwrite unacknowledged loss")
      }
      radio.destroy { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure != nil, "teardown must retain refused control")
      radio.queue.sync {
        precondition(!radio.destroyed)
        first.refuseLoss = false
        radio.drainScanCallbacks()
        precondition(first.lossGenerations == [generation] && first.lostAdvertisements == 1)
        radio.startScanOnQueue(owner: first, route: first, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") { failure = $0 }
        precondition(failure == nil && radio.scanLeases[ObjectIdentifier(first)]!.generation != generation)
        first.rejectAdvertisements = true
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.drainScanCallbacks()
        precondition(first.lostAdvertisements == 3, "real data-ingress refusals are counted")
        precondition(first.lossGenerations.last == radio.scanLeases[ObjectIdentifier(first)]!.generation)
        first.rejectAdvertisements = false
        first.onLoss = {
          first.onLoss = nil
          radio.centralManager(central, didDiscover: peripheral,
            advertisementData: [CBAdvertisementDataManufacturerDataKey: big], rssi: -40)
        }
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataManufacturerDataKey: big], rssi: -40)
        radio.drainScanCallbacks()
        precondition(first.lostAdvertisements == 4 && radio.pendingScanLosses[ObjectIdentifier(first)]?.exactCount == 1,
          "a reentrant rejection must survive acknowledgement of the earlier loss")
        radio.drainScanCallbacks()
        precondition(first.lostAdvertisements == 5 && radio.pendingScanLosses.isEmpty)
        radio.stopScanOnQueue(owner: first) { failure = $0 }
        precondition(failure == nil)
        radio.startScanOnQueue(owner: first, route: first, serviceUUIDs: [], allowDuplicates: false, operationIdentifier: "scan") { failure = $0 }
        precondition(failure == nil)
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataManufacturerDataKey: big], rssi: -40)
        radio.drainScanCallbacks()
        precondition(first.unknownLosses == 1, "duplicate policy reports unknown rather than an invented count")
        first.rejectAdvertisements = true
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.drainScanCallbacks()
        let deliveredBeforeRetry = first.advertisements
        first.rejectAdvertisements = false
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.drainScanCallbacks()
        precondition(first.advertisements == deliveredBeforeRetry + 1,
          "a rejected first observation must not consume the duplicate-suppression slot")
        radio.stopScanOnQueue(owner: first) { failure = $0 }
        precondition(failure == nil)
        let owners = (0..<OwnedCoreBluetoothProtocolRadio.scanOwnerLimit).map { _ in Borrower() }
        for owner in owners {
          radio.startScanOnQueue(owner: owner, route: owner, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "bounded") { failure = $0 }
          precondition(failure == nil)
        }
        let excess = Borrower()
        radio.startScanOnQueue(owner: excess, route: excess, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "excess") { failure = $0 }
        precondition(failure != nil, "every admitted scan reserves a bounded loss-control slot")
        for owner in owners { radio.stopScanOnQueue(owner: owner) { failure = $0 }; precondition(failure == nil) }
        radio.peripheralByIdentifier.removeAll()
      }
      radio.destroy { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil, "acknowledged loss permits teardown")
      print("Scan loss lifecycle passed")
      return
    }
    if CommandLine.arguments.contains("--scan-saturation") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral,
            let otherPeripheral = class_createInstance(OtherPeripheral.self, 0) as? OtherPeripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral); _ = Unmanaged.passRetained(otherPeripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync {
        radio.central = central; radio.delegate = first
        radio.peripheralByIdentifier[peerID.uuidString] = peripheral
        radio.peripheralByIdentifier[otherPeerID.uuidString] = otherPeripheral
        radio.unfencedConnections.formUnion([peerID.uuidString, otherPeerID.uuidString])
      }
      radio.startScan(serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      radio.startScan(borrower: second, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "second") { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      radio.queue.sync {
        let key = OwnedOrderedGraphPermitGate.Key(peer: peerID.uuidString, connection: "active",
          snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
        let gate = radio.orderedGraphGate(peer: peerID.uuidString)
        let otherKey = OwnedOrderedGraphPermitGate.Key(peer: otherPeerID.uuidString, connection: "other-active",
          snapshot: .init(owner: "other", attachment: 1, revision: 1), id: 1)
        let otherGate = radio.orderedGraphGate(peer: otherPeerID.uuidString)
        precondition(gate.acquire(key, valid: true) == .acquired)
        precondition(otherGate.acquire(otherKey, valid: true) == .acquired)
        for _ in 0..<1026 {
          radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        }
        precondition(gate.commit(key) == .committed, "scan traffic must not invalidate a connection graph permit")
        precondition(otherGate.commit(otherKey) == .committed, "scan traffic must not invalidate another peer's permit")
        first.refuseLoss = true
        radio.drainScanCallbacks()
        precondition(first.refusedLossAttempts > 0 && first.lostAdvertisements == 0, "refused loss remains retained")
        precondition(second.advertisements + second.lostAdvertisements == 1026 && second.lostAdvertisements == 2,
          "loss is independently attributed and acknowledged by the second scan owner")
        precondition(second.lossGenerations == [radio.scanLeases[ObjectIdentifier(second)]!.generation])
        precondition(gate.release(key) == .released)
        precondition(otherGate.release(otherKey) == .released)
        first.refuseLoss = false
        radio.drainScanCallbacks()
        precondition(first.advertisements + first.lostAdvertisements == 1026,
          "each observed advertisement must be delivered or counted as source loss")
        precondition(cancelledConnections == 0, "scan traffic must not disconnect an active connection")
      }
      radio.stopScan(operationIdentifier: "stop") { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil, "scan cleanup must complete")
      radio.stopScan(borrower: second) { failure = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && failure == nil, "second scan cleanup must complete")
      print("Scan saturation preserved connection and observations")
      return
    }
    if CommandLine.arguments.contains("--scan-leases") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral,
            let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central); _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var answer: NSError?
      radio.queue.sync { radio.central = central }
      func start(_ borrower: Borrower, _ service: String, _ operation: String, duplicates: Bool = true) -> NSError? {
        radio.startScan(borrower: borrower, serviceUUIDs: [service], allowDuplicates: duplicates, operationIdentifier: operation) {
          answer = $0; done.signal()
        }
        precondition(done.wait(timeout: .now() + 5) == .success)
        return answer
      }
      func stop(_ borrower: Borrower) -> NSError? {
        radio.stopScan(borrower: borrower) { answer = $0; done.signal() }
        precondition(done.wait(timeout: .now() + 5) == .success)
        return answer
      }
      scanState = .poweredOff
      precondition(start(first, "180D", "refused") != nil, "powered-off registration must report refusal")
      scanState = .poweredOn
      precondition(start(first, "180D", "first") == nil)
      precondition(start(second, "180F", "first", duplicates: false) == nil, "independent borrower must acquire a scan")
      radio.queue.sync {
        precondition(scanStarts.count == 2 && scanStops == 0)
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180D")]], rssi: -40)
        radio.drainScanCallbacks()
        precondition(first.advertisements == 1, "A receives visible matching evidence")
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180F")]], rssi: -41)
        radio.drainScanCallbacks()
        precondition(second.advertisements >= 1, "B receives visible matching evidence")
      }
      radio.updateScan(borrower: second, serviceUUIDs: ["1810"], allowDuplicates: false) { answer = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && answer == nil)
      radio.queue.sync { precondition(scanStarts.last! == ["180D", "1810"] || scanStarts.last! == ["0000180D-0000-1000-8000-00805F9B34FB", "00001810-0000-1000-8000-00805F9B34FB"]) }
      radio.updateScan(borrower: second, serviceUUIDs: ["180F"], allowDuplicates: false) { answer = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && answer == nil)
      let beforeFirst = first.advertisements
      let beforeSecond = second.advertisements
      let key = OwnedOrderedGraphPermitGate.Key(peer: peerID.uuidString, connection: "c",
        snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 2)
      radio.queue.sync {
        precondition(radio.orderedGraphGate(peer: peerID.uuidString).acquire(key, valid: true) == .acquired)
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180D"), CBUUID(string: "180F")]], rssi: -40)
        radio.stopScanOnQueue(owner: first) { answer = $0 }
        precondition(answer == nil)
        radio.startScanOnQueue(owner: first, route: first, serviceUUIDs: ["180D"], allowDuplicates: true, operationIdentifier: "first") { answer = $0 }
        precondition(answer == nil)
        radio.drainScanCallbacks()
        precondition(radio.orderedGraphGate(peer: peerID.uuidString).release(key) == .released)
        precondition(first.advertisements == beforeFirst + 1 && second.advertisements == beforeSecond + 1, "stop must drain each owner's already observed callback")
        radio.cancelScan(borrower: first, operationIdentifier: "first")
        precondition(scanStops == 0)
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180F")]], rssi: -40)
        radio.drainScanCallbacks()
        precondition(second.advertisements == beforeSecond + 1, "duplicate-disabled scan suppresses a repeated peer")
      }
      precondition(start(first, "invalid", "bad") != nil)
      radio.updateScan(borrower: second, serviceUUIDs: ["invalid"], allowDuplicates: true) { answer = $0; done.signal() }
      precondition(done.wait(timeout: .now() + 5) == .success && answer != nil)
      radio.queue.sync {
        let priorStarts = scanStarts.count
        let recoveryKey = OwnedOrderedGraphPermitGate.Key(peer: peerID.uuidString, connection: "recovery",
          snapshot: .init(owner: "o", attachment: 2, revision: 1), id: 3)
        let gate = radio.orderedGraphGate(peer: peerID.uuidString)
        precondition(gate.acquire(recoveryKey, valid: true) == .acquired)
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180F")]], rssi: -40)
        scanState = .poweredOff
        radio.scanAdapterStateChanged(.poweredOff)
        scanState = .poweredOn
        radio.scanAdapterStateChanged(.poweredOn)
        precondition(scanStarts.count == priorStarts + 1, "live claims must be restored after adapter recovery")
        precondition(gate.release(recoveryKey) == .released)
        radio.drainScanCallbacks()
        precondition(second.advertisements == beforeSecond + 1, "the pre-reset duplicate remains suppressed")
        radio.centralManager(central, didDiscover: peripheral,
          advertisementData: [CBAdvertisementDataServiceUUIDsKey: [CBUUID(string: "180F")]], rssi: -40)
        radio.drainScanCallbacks()
        precondition(second.advertisements == beforeSecond + 2)
      }
      precondition(stop(second) == nil)
      radio.queue.sync {
        precondition(scanStops == 1, "only the last claim stops the physical scan")
        restoredScanActive = true
        radio.clearAllScanLeases()
        precondition(scanStops == 2, "teardown must also stop a restored unclaimed scan")
        restoredScanActive = false
      }
      print("Scan borrower leases passed")
      if CommandLine.arguments.contains("--scan-leases") { return }
      first.advertisements = 0
      scanStarts.removeAll(); scanStops = 0
    }
    if CommandLine.arguments.contains("--scan-fence") {
      guard let central = class_createInstance(ScanCentral.self, 0) as? ScanCentral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(central)
      guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
      _ = Unmanaged.passRetained(peripheral)
      let done = DispatchSemaphore(value: 0)
      var failure: NSError?
      radio.queue.sync { radio.central = central; radio.delegate = first }
      func start() {
        radio.startScan(serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") {
          failure = $0; done.signal()
        }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      }
      func stop() {
        radio.stopScan(operationIdentifier: "stop") { failure = $0; done.signal() }
        precondition(done.wait(timeout: .now() + 5) == .success && failure == nil)
      }
      start()
      let key = OwnedOrderedGraphPermitGate.Key(peer: peerID.uuidString, connection: "c",
        snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
      radio.queue.sync {
        let gate = radio.orderedGraphGate(peer: peerID.uuidString)
        precondition(gate.acquire(key, valid: true) == .acquired)
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.stopScanOnQueue(owner: first) { failure = $0 }
        precondition(failure == nil && first.advertisements == 1, "stop drains the observed old-generation callback")
        radio.startScanOnQueue(owner: first, route: first, serviceUUIDs: [], allowDuplicates: true, operationIdentifier: "scan") { failure = $0 }
        precondition(failure == nil)
        radio.drainScanCallbacks()
        precondition(radio.orderedGraphGate(peer: peerID.uuidString).release(key) == .released)
        precondition(first.advertisements == 1, "an old callback must not cross scan generations")
        radio.centralManager(central, didDiscover: peripheral, advertisementData: [:], rssi: -40)
        radio.drainScanCallbacks()
        precondition(first.advertisements == 2, "the new scan must still receive its own advertisement")
      }
      stop()
      print("Scan generation fence passed")
      return
    }
    if CommandLine.arguments.contains("--routes") {
      radio.queue.sync {
        let address = CharacteristicAddress(peerIdentifier: "peer", serviceUUID: "service", serviceOccurrence: 0,
          characteristicUUID: "shared", characteristicOccurrence: 0)
        let peripheral = NSObject()
        let characteristic = CBMutableCharacteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
        precondition(radio.installNotificationRoute("first", borrower: first))
        precondition(radio.installNotificationRoute("second", borrower: second))
        precondition(!radio.installNotificationRoute("second", borrower: first))
        radio.subscriptions[address] = "first"
        radio.sharedSubscriptions[address] = ["second"]
        radio.handleCharacteristicValueUpdate(address: address, peripheral: peripheral, characteristic: characteristic,
          error: nil, snapshotValue: Data([1]), snapshotIsNotifying: true, hasSnapshot: true)
        radio.reportNotificationFailure(address: address, error: NSError(domain: "fixture", code: 1))
        precondition(first.values == ["first"] && second.values == ["second"])
        precondition(first.failures == ["first"] && second.failures == ["second"])
        radio.removeNotificationRoute("second", borrower: first)
        radio.handleCharacteristicValueUpdate(address: address, peripheral: peripheral, characteristic: characteristic,
          error: nil, snapshotValue: Data([2]), snapshotIsNotifying: true, hasSnapshot: true)
        precondition(second.values == ["second", "second"], "another borrower cannot retire this route")
      }
      print("Borrower notification routes passed")
      return
    }
    precondition(radio.attachDelegateIfAvailable(first), "first borrower must attach")
    precondition(radio.attachDelegateIfAvailable(second), "a cached radio must admit a second independent borrower")
    guard let peripheral = class_createInstance(Peripheral.self, 0) as? Peripheral else { fatalError("fixture") }
    _ = Unmanaged.passRetained(peripheral)
    let shared = Characteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [])
    let independent = Characteristic(type: CBUUID(string: "2A38"), properties: [.notify], value: nil, permissions: [])
    let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
    service.characteristics = [shared, independent]
    fixtureService = service
    let peer = peerID.uuidString
    let sharedAddress = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
      characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
    let independentAddress = CharacteristicAddress(peerIdentifier: peer, serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
      characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A38"), characteristicOccurrence: 0)
    radio.queue.sync {
      radio.peripheralByIdentifier[peer] = peripheral
      radio.servicesByPeer[peer] = [service]
      radio.restoredPeerIdentifiers = [peer]
      precondition(radio.installNotificationRoute("first-shared", borrower: first))
      precondition(radio.installNotificationRoute("first-independent", borrower: first))
      precondition(radio.installNotificationRoute("second-shared", borrower: second))
    }
    let done = DispatchSemaphore(value: 0)
    var answer: NSError?
    func subscribe(_ id: String, _ characteristic: Characteristic, _ uuid: String, physical: Bool) {
      radio.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: uuid, characteristicOccurrence: 0, subscriptionIdentifier: id,
        operationIdentifier: "enable-\(id)") { error in answer = error; done.signal() }
      radio.queue.sync {
        if physical {
          notifying[ObjectIdentifier(characteristic)] = true
          radio.peripheral(peripheral, didUpdateNotificationStateFor: characteristic, error: nil)
        }
      }
      precondition(done.wait(timeout: .now() + 5) == .success && answer == nil)
    }
    subscribe("first-shared", shared, "2A37", physical: true)
    subscribe("second-shared", shared, "2A37", physical: false)
    subscribe("first-independent", independent, "2A38", physical: true)
    func value(_ address: CharacteristicAddress, _ characteristic: Characteristic) {
      radio.handleCharacteristicValueUpdate(address: address, peripheral: peripheral, characteristic: characteristic,
        error: nil, snapshotValue: Data([1]), snapshotIsNotifying: true, hasSnapshot: true)
    }
    radio.queue.sync { value(sharedAddress, shared); value(independentAddress, independent) }
    precondition(Set(first.values) == ["first-shared", "first-independent"] && second.values == ["second-shared"])
    physicalWrites.removeAll()
    func awaitWrites(_ count: Int) {
      let deadline = Date().addingTimeInterval(5)
      while Date() < deadline && radio.queue.sync(execute: { physicalWrites.count }) < count {
        Thread.sleep(forTimeInterval: 0.01)
      }
      precondition(radio.queue.sync { physicalWrites.count } >= count, "physical cleanup was not dispatched")
    }
    radio.releaseBorrowerIfCurrent(first) { error in answer = error; done.signal() }
    awaitWrites(1)
    radio.queue.sync {
      precondition(physicalWrites.map { $0.0 } == [ObjectIdentifier(independent)] && physicalWrites.allSatisfy { !$0.1 },
        "retiring the first borrower must leave the shared CCCD enabled")
      radio.peripheral(peripheral, didUpdateNotificationStateFor: independent, error: NSError(domain: "fixture", code: 1))
    }
    precondition(done.wait(timeout: .now() + 5) == .success && answer != nil, "failed physical cleanup must be retryable")
    radio.queue.sync {
      value(sharedAddress, shared)
      precondition(second.values == ["second-shared", "second-shared"])
    }
    radio.releaseBorrowerIfCurrent(first) { error in answer = error; done.signal() }
    awaitWrites(2)
    radio.queue.sync {
      notifying[ObjectIdentifier(independent)] = false
      radio.peripheral(peripheral, didUpdateNotificationStateFor: independent, error: nil)
    }
    precondition(done.wait(timeout: .now() + 5) == .success && answer == nil)
    radio.queue.sync {
      // A late callback belonging to the retired borrower cannot affect the live shared route.
      radio.peripheral(peripheral, didUpdateNotificationStateFor: independent, error: nil)
      value(sharedAddress, shared)
      precondition(second.values == ["second-shared", "second-shared", "second-shared"])
    }
    radio.releaseBorrowerIfCurrent(second) { error in answer = error; done.signal() }
    awaitWrites(3)
    radio.queue.sync {
      notifying[ObjectIdentifier(shared)] = false
      radio.peripheral(peripheral, didUpdateNotificationStateFor: shared, error: nil)
    }
    precondition(done.wait(timeout: .now() + 5) == .success && answer == nil)
    let expected = [ObjectIdentifier(independent), ObjectIdentifier(independent), ObjectIdentifier(shared)]
    precondition(physicalWrites.map { $0.0 } == expected && physicalWrites.allSatisfy { !$0.1 },
      "each final physical claim must be disabled exactly once per attempted cleanup")
  }
}
