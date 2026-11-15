// ios/__tests__/AppleOrderedRadioCallbackHarness.swift
import Foundation
import CoreBluetooth
import ObjectiveC.runtime

// CoreBluetooth has no public peripheral constructor. Allocate inert subclasses
// with overridden accessors so real delegate entrypoints can be driven without a radio.
private let fixtureIdentifier = UUID(uuidString: "00000000-0000-0000-0000-000000000522")!
private var fixtureServices: [CBService] = []
private var fixtureService: CBService?
private var fixtureNotifying = false
private var fixtureWrites: [Bool] = []
private final class FixturePeripheral: CBPeripheral {
  override var identifier: UUID { fixtureIdentifier }
  override var state: CBPeripheralState { .connected }
  override var services: [CBService]? { fixtureServices }
  override var delegate: CBPeripheralDelegate? { get { nil } set {} }
  override func discoverServices(_ serviceUUIDs: [CBUUID]?) {}
  override func setNotifyValue(_ enabled: Bool, for characteristic: CBCharacteristic) { fixtureWrites.append(enabled) }
}
private final class FixtureCharacteristic: CBMutableCharacteristic {
  override var isNotifying: Bool { fixtureNotifying }
  override var service: CBService? { fixtureService }
}
private final class FixtureCentral: CBCentralManager {
  override func cancelPeripheralConnection(_ peripheral: CBPeripheral) {}
}

private final class OverflowRadioDelegate: NSObject, OwnedCoreBluetoothProtocolRadioDelegate {
  var disconnects: [String] = []
  var states: [NSDictionary] = []
  var notifications: [(String, Data)] = []
  var notificationFailures: [(String, NSError)] = []
  var readiness: [String] = []
  func protocolRadioDidUpdateAdapterState(_ snapshot: NSDictionary) { states.append(snapshot) }
  func protocolRadioDidReceiveAdvertisement(_ advertisement: NSDictionary) {}
  func protocolRadioDidDisconnectPeer(_ peerIdentifier: String, error: NSError?) { disconnects.append(peerIdentifier) }
  func protocolRadioDidModifyServices(_ peerIdentifier: String) {}
  func protocolRadioDidReceiveNotification(_ subscriptionIdentifier: String, value: NSData) {
    notifications.append((subscriptionIdentifier, value as Data))
  }
  @objc func protocolRadioDidFailNotification(_ subscriptionIdentifier: String, error: NSError) {
    notificationFailures.append((subscriptionIdentifier, error))
  }
  @objc func protocolRadioDidBecomeReadyToSendWriteWithoutResponse(_ peerIdentifier: String) { readiness.append(peerIdentifier) }
}


@main enum AppleOrderedRadioCallbackHarness {
  static func main() throws {
    // A callback reports an object, not a request ID. Cancelled slots must
    // drain first; a graph replacement may use a different object at the
    // same address without inheriting an old callback.
    let oldPeripheral = NSObject(), newPeripheral = NSObject()
    let oldAttribute = NSObject(), newAttribute = NSObject()
    let ledger = OwnedCoreBluetoothCallbackLedger<String, Int>()
    var answers: [Int] = []
    precondition(ledger.admit("path", peer: "peer", peripheral: oldPeripheral, object: oldAttribute,
      operation: "A", completion: { answers.append($0) }))
    precondition(ledger.cancel("A"))
    precondition(!ledger.admit("path", peer: "peer", peripheral: oldPeripheral, object: oldAttribute,
      operation: "B", completion: { answers.append($0) }))
    precondition(ledger.hasOutstanding("A"))
    precondition(ledger.complete(peripheral: oldPeripheral, object: oldAttribute, value: 1) == "A")
    precondition(answers.isEmpty && !ledger.hasOutstanding("A"))
    precondition(ledger.admit("path", peer: "peer", peripheral: oldPeripheral, object: oldAttribute,
      operation: "B", completion: { answers.append($0) }))
    precondition(ledger.complete(peripheral: newPeripheral, object: newAttribute, value: 2) == nil)
    precondition(answers.isEmpty)
    precondition(ledger.complete(peripheral: oldPeripheral, object: oldAttribute, value: 3) == "B")
    precondition(answers == [3])

    precondition(ledger.admit("path", peer: "peer", peripheral: oldPeripheral, object: oldAttribute,
      operation: "marker", settleOnCancel: true, completion: { answers.append($0) }))
    precondition(ledger.cancel("marker") && ledger.hasOutstanding("marker"))
    precondition(ledger.complete(peripheral: oldPeripheral, object: oldAttribute, value: 4) == "marker")
    precondition(answers == [3, 4], "marker settlement must survive caller cancellation")

    precondition(ledger.admit("path", peer: "peer", peripheral: oldPeripheral, object: oldAttribute,
      operation: "old-generation", completion: { answers.append($0) }))
    ledger.retire(peer: "peer", failure: -1)
    precondition(answers == [3, 4, -1])
    precondition(!ledger.admit("path", peer: "peer", peripheral: oldPeripheral, object: oldAttribute,
      operation: "same-object", completion: { answers.append($0) }))
    precondition(ledger.admit("path", peer: "peer", peripheral: newPeripheral, object: newAttribute,
      operation: "new-generation", completion: { answers.append($0) }))
    precondition(ledger.complete(peripheral: oldPeripheral, object: oldAttribute, value: 5) == "old-generation")
    precondition(answers == [3, 4, -1])
    precondition(ledger.complete(peripheral: newPeripheral, object: newAttribute, value: 6) == "new-generation")
    precondition(answers == [3, 4, -1, 6])
    let bounded = OwnedCoreBluetoothCallbackLedger<Int, Int>()
    for index in 0..<256 {
      let peripheral = NSObject(), object = NSObject()
      precondition(bounded.admit(index, peer: "bounded", peripheral: peripheral, object: object,
        operation: "debt-\(index)", completion: { _ in }))
      bounded.retire(peer: "bounded", failure: -1)
    }
    precondition(!bounded.admit(256, peer: "bounded", peripheral: NSObject(), object: NSObject(),
      operation: "overflow", completion: { _ in }), "graph churn must have a bounded callback debt")
    bounded.clearRetired(peer: "bounded")
    precondition(bounded.admit(256, peer: "bounded", peripheral: NSObject(), object: NSObject(),
      operation: "after-fence", completion: { _ in }))
    let readOwner = OwnedCoreBluetoothReadObjectFence(peripheral: oldPeripheral, object: oldAttribute,
      cancelledOperation: "cancelled-read")
    precondition(readOwner.matches(peripheral: oldPeripheral, object: oldAttribute))
    precondition(!readOwner.matches(peripheral: newPeripheral, object: newAttribute))
    let ambiguousRead = OwnedCoreBluetoothReadObjectFence(peripheral: oldPeripheral, object: oldAttribute,
      cancelledOperation: "ambiguous-read", unattributable: true)
    precondition(ambiguousRead.unattributable)

    let descriptorReads = OwnedCoreBluetoothCallbackLedger<String, (NSData?, NSError?)>()
    let descriptorWrites = OwnedCoreBluetoothCallbackLedger<String, NSError?>()
    let rssiReads = OwnedCoreBluetoothCallbackLedger<String, (NSNumber?, NSError?)>()
    var strayCompletions = 0
    precondition(descriptorReads.admit("descriptor", peer: "peer", peripheral: oldPeripheral,
      object: oldAttribute, operation: "descriptor-read", completion: { _ in strayCompletions += 1 }))
    precondition(descriptorWrites.admit("descriptor", peer: "peer", peripheral: oldPeripheral,
      object: oldAttribute, operation: "descriptor-write", completion: { _ in strayCompletions += 1 }))
    precondition(rssiReads.admit("peer", peer: "peer", peripheral: oldPeripheral,
      object: oldPeripheral, operation: "rssi", completion: { _ in strayCompletions += 1 }))
    precondition(descriptorReads.cancel("descriptor-read") && descriptorWrites.cancel("descriptor-write") && rssiReads.cancel("rssi"))
    descriptorReads.retire(peer: "peer", failure: (nil, NSError(domain: "test", code: 1)))
    descriptorWrites.retire(peer: "peer", failure: NSError(domain: "test", code: 1))
    rssiReads.retire(peer: "peer", failure: (nil, NSError(domain: "test", code: 1)))
    precondition(strayCompletions == 0)
    precondition(descriptorReads.admit("descriptor", peer: "peer", peripheral: newPeripheral,
      object: newAttribute, operation: "new-read", completion: { _ in strayCompletions += 1 }))
    precondition(descriptorWrites.admit("descriptor", peer: "peer", peripheral: newPeripheral,
      object: newAttribute, operation: "new-write", completion: { _ in strayCompletions += 1 }))
    precondition(rssiReads.admit("peer", peer: "peer", peripheral: newPeripheral,
      object: newPeripheral, operation: "new-rssi", completion: { _ in strayCompletions += 1 }))
    precondition(descriptorReads.complete(peripheral: oldPeripheral, object: oldAttribute, value: (nil, nil)) == "descriptor-read")
    precondition(descriptorWrites.complete(peripheral: oldPeripheral, object: oldAttribute, value: nil) == "descriptor-write")
    precondition(rssiReads.complete(peripheral: oldPeripheral, object: oldPeripheral, value: (nil, nil)) == "rssi")
    precondition(strayCompletions == 0)
    precondition(descriptorReads.complete(peripheral: newPeripheral, object: newAttribute, value: (nil, nil)) == "new-read")
    precondition(descriptorWrites.complete(peripheral: newPeripheral, object: newAttribute, value: nil) == "new-write")
    precondition(rssiReads.complete(peripheral: newPeripheral, object: newPeripheral, value: (nil, nil)) == "new-rssi")
    precondition(strayCompletions == 3)

    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    try radio.queue.sync {
      // Synthetic platform objects exercise the production callback handler;
      // no central manager or radio is allocated.
      let callbackPeripheral = NSObject()
      let callbackCharacteristic = CBMutableCharacteristic(type: CBUUID(string: "2A37"),
        properties: [.read, .notify], value: nil, permissions: [.readable])
      let raceAddress = CharacteristicAddress(peerIdentifier: "read-race", serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      // An error can be a notification failure while read A is still owed.
      let errorAddress = CharacteristicAddress(peerIdentifier: "error-race", serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      let errorRoute = radio.orderedRoute(for: errorAddress)
      let errorSource = OwnedOrderedNotificationSource(queue: radio.queue, generation: "error-race")
      let errorGroup = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "error-race",
        members: [errorRoute], capacity: 8, byteCapacity: 32)
      precondition(errorSource.add(errorGroup))
      guard let errorClaim = errorSource.makeClaim(route: errorRoute) else { fatalError("error claim") }
      try errorGroup.register(errorClaim)
      radio.orderedSources["error-race"] = errorSource
      radio.subscriptions[errorAddress] = "error-owner"
      radio.sharedSubscriptions[errorAddress] = ["shared-owner"]
      let failureObserver = OverflowRadioDelegate()
      radio.delegate = failureObserver
      var firstError: NSError?
      var secondError: NSError?
      var secondSucceeded = false
      var errorLane = OwnedCoreBluetoothReadLane<PendingCharacteristicRead>()
      _ = errorLane.admit(PendingCharacteristicRead(operationIdentifier: "error-A", completion: { _, _, error in firstError = error }))
      _ = errorLane.admit(PendingCharacteristicRead(operationIdentifier: "error-B", completion: { _, _, error in
        secondError = error; secondSucceeded = error == nil
      }))
      radio.pendingRead[errorAddress] = errorLane
      radio.readObjects[errorAddress] = OwnedCoreBluetoothReadObjectFence(peripheral: callbackPeripheral,
        object: callbackCharacteristic, cancelledOperation: nil, unattributable: true)
      let callbackFailure = NSError(domain: "CBFixture", code: 51)
      radio.handleCharacteristicValueUpdate(address: errorAddress, peripheral: callbackPeripheral,
        characteristic: callbackCharacteristic, error: callbackFailure, snapshotIsNotifying: true, hasSnapshot: true)
      precondition(Set(failureObserver.notificationFailures.map { $0.0 }) == ["error-owner", "shared-owner"] &&
        failureObserver.notificationFailures.allSatisfy { $0.1 === callbackFailure },
        "all ordinary owners must receive the native notification failure")
      precondition(firstError === callbackFailure && secondError != nil && !secondSucceeded)
      precondition(radio.pendingRead[errorAddress]?.updateOwed == true, "an ambiguous error cannot discharge A")
      guard case .terminal(.sourceFailed(let failure, member: _, sourceMember: _, epoch: _), .uncertain, nil) = errorGroup.drain().first else {
        fatalError("ambiguous notification failure must reach the ordered subscriber")
      }
      precondition(failure.domain == "CBFixture" && failure.code == 51)
      radio.handleCharacteristicValueUpdate(address: errorAddress, peripheral: callbackPeripheral,
        characteristic: callbackCharacteristic, error: nil, snapshotValue: Data([9]), snapshotIsNotifying: true, hasSnapshot: true)
      precondition(!secondSucceeded && radio.pendingRead[errorAddress]?.updateOwed == true,
        "A's later response must not complete B or release the debt")
      radio.completeObservedDisconnect("error-race", error: nil)

      // A failure held by the graph gate belongs to the claim visible when it
      // arrived, not a replacement registered before the gate is released.
      let deferredAddress = CharacteristicAddress(peerIdentifier: "deferred-failure", serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      let deferredRoute = radio.orderedRoute(for: deferredAddress)
      let deferredSource = OwnedOrderedNotificationSource(queue: radio.queue, generation: "deferred")
      let deferredGroup = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "deferred",
        members: [deferredRoute], capacity: 8, byteCapacity: 32)
      precondition(deferredSource.add(deferredGroup))
      try deferredGroup.register(deferredSource.makeClaim(route: deferredRoute)!)
      radio.orderedSources[deferredAddress.peerIdentifier] = deferredSource
      radio.subscriptions[deferredAddress] = "old"
      let deferredGate = radio.orderedGraphGate(peer: deferredAddress.peerIdentifier)
      let deferredKey = OwnedOrderedGraphPermitGate.Key(peer: deferredAddress.peerIdentifier, connection: "c",
        snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
      precondition(deferredGate.acquire(deferredKey, valid: true) == .acquired)
      radio.reportOrderedCharacteristicFailure(address: deferredAddress, error: NSError(domain: "old", code: 1))
      try deferredGroup.retire(deferredRoute)
      try deferredGroup.register(deferredSource.makeClaim(route: deferredRoute)!)
      precondition(deferredGate.release(deferredKey) == .released)
      precondition(!deferredGroup.drain().contains { if case .terminal = $0 { return true }; return false },
        "a deferred failure must not terminate a replacement claim")

      // Cancellation must retain the disconnect callback fence even when the
      // platform object already looks disconnected or has become unavailable.
      var cancelledDisconnectCalled = false
      radio.unfencedConnections.insert("fence-peer")
      precondition(!radio.connectionGenerationAvailable("fence-peer", isConnected: false),
        "a disconnected-looking peripheral cannot start a new generation before its terminal callback")
      radio.pendingDisconnect["fence-peer"] = PendingVoid(operationIdentifier: "disconnect-cancel", completion: { _ in
        cancelledDisconnectCalled = true
      })
      radio.cancelPendingOperation("disconnect-cancel")
      precondition(radio.pendingDisconnect["fence-peer"] != nil,
        "cancelling a disconnect must keep its generation fenced until the terminal callback")
      precondition(!radio.pendingCancellationCleanupRetryScheduled.contains("disconnect-cancel"),
        "an awaited terminal callback must not start an unbounded retry timer")
      precondition(radio.retryCancellationCleanupOnQueue("disconnect-cancel")["state"] as? String == "retryable")
      precondition(!radio.connectionGenerationAvailable("fence-peer", isConnected: false))
      radio.completeObservedDisconnect("fence-peer", error: nil)
      precondition(!cancelledDisconnectCalled && radio.pendingDisconnect["fence-peer"] == nil)
      precondition(radio.connectionGenerationAvailable("fence-peer", isConnected: false),
        "the old callback releases the fence before a new connect can be admitted")


      var firstRead: OwnedCoreBluetoothReadProvenance?
      var secondReadCompleted = false
      var queuedReadFailed = false
      let callbackObserver = OverflowRadioDelegate()
      radio.delegate = callbackObserver
      let callbackRoute = radio.orderedRoute(for: raceAddress)
      let callbackSource = OwnedOrderedNotificationSource(queue: radio.queue, generation: "read-race")
      let callbackGroup = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "read-race",
        members: [callbackRoute], capacity: 8, byteCapacity: 32)
      precondition(callbackSource.add(callbackGroup))
      guard let callbackClaim = callbackSource.makeClaim(route: callbackRoute) else { fatalError("callback claim") }
      try callbackGroup.register(callbackClaim)
      radio.orderedSources["read-race"] = callbackSource
      var raceLane = OwnedCoreBluetoothReadLane<PendingCharacteristicRead>()
      _ = raceLane.admit(PendingCharacteristicRead(operationIdentifier: "read-A", completion: { _, provenance, error in
        precondition(error == nil); firstRead = provenance
      }))
      precondition(!raceLane.admit(PendingCharacteristicRead(operationIdentifier: "queued-read", completion: { _, provenance, error in
        queuedReadFailed = error != nil && provenance == .readOrNotification
      })))
      radio.pendingRead[raceAddress] = raceLane
      radio.readObjects[raceAddress] = OwnedCoreBluetoothReadObjectFence(peripheral: callbackPeripheral,
        object: callbackCharacteristic, cancelledOperation: nil, unattributable: true)
      radio.subscriptions[raceAddress] = "read-race-subscriber"
      radio.handleCharacteristicValueUpdate(address: raceAddress, peripheral: callbackPeripheral,
        characteristic: callbackCharacteristic, error: nil, snapshotValue: Data([1]), snapshotIsNotifying: true, hasSnapshot: true)
      precondition(firstRead == .readOrNotification)
      precondition(queuedReadFailed, "queued reads cannot be issued behind an ambiguous completion")
      precondition(callbackObserver.notifications.count == 1 && callbackObserver.notifications[0].1 == Data([1]))
      guard case .value(_, _, _, let firstValue, .readOrNotification) = callbackGroup.drain().first else {
        fatalError("ambiguous value did not reach ordered ingress")
      }
      precondition(firstValue == Data([1]))
      precondition(radio.pendingRead[raceAddress]?.updateOwed == true,
        "an ambiguous completion must keep the physical read outstanding")
      radio.handleCharacteristicValueUpdate(address: raceAddress, peripheral: callbackPeripheral,
        characteristic: callbackCharacteristic, error: NSError(domain: "after-read", code: 52),
        snapshotIsNotifying: true, hasSnapshot: true)
      guard case .terminal(.sourceFailed(let laterFailure, member: _, sourceMember: _, epoch: _), .uncertain, nil) = callbackGroup.drain().first else {
        fatalError("notification failure after ambiguous logical completion must be reported")
      }
      precondition(laterFailure.domain == "after-read" && laterFailure.code == 52)
      precondition(radio.pendingRead[raceAddress]?.updateOwed == true)
      radio.subscriptions.removeValue(forKey: raceAddress)
      _ = radio.pendingRead[raceAddress]?.admit(PendingCharacteristicRead(operationIdentifier: "read-B", completion: { _, _, error in
        if error == nil { secondReadCompleted = true }
      }))
      radio.handleCharacteristicValueUpdate(address: raceAddress, peripheral: callbackPeripheral,
        characteristic: callbackCharacteristic, error: nil, snapshotValue: Data([2]), snapshotIsNotifying: false, hasSnapshot: true)
      precondition(!secondReadCompleted, "A's possible reply must not settle B after notifications stop")
      radio.completeObservedDisconnect("read-race", error: nil)
      precondition(!radio.consumeRetiredReadCallback(peripheral: callbackPeripheral, characteristic: callbackCharacteristic))
      let replacementSource = OwnedOrderedNotificationSource(queue: radio.queue, generation: "reconnected")
      let replacementGroup = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "reconnected",
        members: [callbackRoute], capacity: 8, byteCapacity: 32)
      precondition(replacementSource.add(replacementGroup))
      guard let replacementClaim = replacementSource.makeClaim(route: callbackRoute) else { fatalError("replacement claim") }
      try replacementGroup.register(replacementClaim)
      radio.orderedSources["read-race"] = replacementSource
      radio.subscriptions[raceAddress] = "replacement-subscriber"
      radio.handleCharacteristicValueUpdate(address: raceAddress, peripheral: callbackPeripheral,
        characteristic: callbackCharacteristic, error: nil, snapshotValue: Data([3]), snapshotIsNotifying: true, hasSnapshot: true)
      precondition(callbackObserver.notifications.last?.0 == "replacement-subscriber" &&
        callbackObserver.notifications.last?.1 == Data([3]), "reused objects deliver to the replacement subscription")
      guard case .value(_, _, _, let replacementValue, .notification) = replacementGroup.drain().first else {
        fatalError("reused objects did not deliver to ordered ingress")
      }
      precondition(replacementValue == Data([3]))
      radio.notificationStateWriter = { _, _ in true }
      radio.pendingNotify[raceAddress] = PendingNotify(operationIdentifier: "cancel-stop",
        subscriptionIdentifier: "replacement-subscriber", enabled: false, completion: { _ in
          preconditionFailure("cancelled stop must not complete")
        })
      radio.cancelPendingOperation("cancel-stop")
      radio.handleCharacteristicValueUpdate(address: raceAddress, peripheral: callbackPeripheral,
        characteristic: callbackCharacteristic, error: nil, snapshotValue: Data([4]), snapshotIsNotifying: true, hasSnapshot: true)
      precondition(callbackObserver.notifications.last?.1 == Data([4]), "owned value before disable must be delivered")
      radio.handleNotificationStateUpdate(address: raceAddress, isNotifying: false, error: nil)
      precondition(callbackObserver.notificationFailures.contains { $0.0 == "replacement-subscriber" && $0.1.code == 1039 },
        "ordinary ownership must receive the uncertain continuity failure")
      let interrupted = replacementGroup.drain()
      precondition(interrupted.contains { if case .terminal(.sourceFailed, .uncertain, nil) = $0 { return true }; return false },
        "the temporary physical disable must report uncertain continuity")
      radio.handleCharacteristicValueUpdate(address: raceAddress, peripheral: callbackPeripheral,
        characteristic: callbackCharacteristic, error: nil, snapshotValue: Data([5]), snapshotIsNotifying: true, hasSnapshot: true)
      precondition(callbackObserver.notifications.last?.1 == Data([5]), "owned value during restoration must be delivered")
      radio.handleNotificationStateUpdate(address: raceAddress, isNotifying: true, error: nil)
      radio.notificationStateWriter = nil
      radio.subscriptions.removeValue(forKey: raceAddress)
      let a = CharacteristicAddress(peerIdentifier: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
      let b = CharacteristicAddress(peerIdentifier: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 1)
      let cancelledWrite = CharacteristicAddress(peerIdentifier: "write-peer", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
      var cancelledWriteDelivered = false
      let writePeripheral = NSObject(), writeCharacteristic = NSObject()
      precondition(radio.pendingWrite.admit(cancelledWrite, peer: "write-peer", peripheral: writePeripheral,
        object: writeCharacteristic, operation: "write-cancel", completion: { _ in cancelledWriteDelivered = true }))
      radio.cancelPendingOperation("write-cancel")
      precondition(radio.pendingWrite.hasOutstanding("write-cancel"), "a cancelled write must retain its CoreBluetooth callback slot")
      precondition(radio.retryCancellationCleanupOnQueue("write-cancel")["state"] as? String == "retryable")
      precondition(radio.pendingWrite.complete(peripheral: writePeripheral, object: writeCharacteristic, value: nil) == "write-cancel")
      radio.refreshCallbackCancellationCleanup("write-cancel")
      precondition(!cancelledWriteDelivered, "a cancelled caller must not be completed")
      precondition(radio.retryCancellationCleanupOnQueue("write-cancel")["state"] as? String == "released")
      let oldGraph = NSObject(), oldWrite = NSObject(), nextGraph = NSObject(), nextWrite = NSObject()
      var markerSettlements: [Int] = []
      precondition(radio.pendingWrite.admit(cancelledWrite, peer: "write-peer", peripheral: oldGraph,
        object: oldWrite, operation: "marker-cancel", settleOnCancel: true,
        completion: { markerSettlements.append($0?.code ?? 0) }))
      radio.cancelPendingOperation("marker-cancel")
      precondition(radio.retryCancellationCleanupOnQueue("marker-cancel")["state"] as? String == "retryable")
      radio.failPendingGATT(for: "write-peer", error: NSError(domain: "graph", code: 71))
      precondition(markerSettlements == [71], "graph loss must settle an admitted marker with failure once")
      precondition(radio.retryCancellationCleanupOnQueue("marker-cancel")["state"] as? String == "retryable")
      precondition(!radio.pendingWrite.canAdmit(cancelledWrite, peripheral: oldGraph, object: oldWrite))
      var nextWriteSettled = false
      precondition(radio.pendingWrite.admit(cancelledWrite, peer: "write-peer", peripheral: nextGraph,
        object: nextWrite, operation: "next-write", completion: { _ in nextWriteSettled = true }))
      precondition(radio.pendingWrite.complete(peripheral: oldGraph, object: oldWrite, value: nil) == "marker-cancel")
      radio.refreshCallbackCancellationCleanup("marker-cancel")
      precondition(!nextWriteSettled && markerSettlements == [71])
      precondition(radio.retryCancellationCleanupOnQueue("marker-cancel")["state"] as? String == "released")
      precondition(radio.pendingWrite.complete(peripheral: nextGraph, object: nextWrite, value: nil) == "next-write")
      precondition(nextWriteSettled)

      var cancelledDescriptorCompletion = false
      let descriptorAddress = OwnedCoreBluetoothDescriptorAddress(characteristic: cancelledWrite,
        descriptorUUID: "d", descriptorOccurrence: 0)
      precondition(radio.descriptorOperations.pendingReads.admit(descriptorAddress, peer: "write-peer",
        peripheral: oldGraph, object: oldWrite, operation: "descriptor-cancel",
        completion: { _ in cancelledDescriptorCompletion = true }))
      radio.cancelPendingOperation("descriptor-cancel")
      radio.failPendingGATT(for: "write-peer", error: NSError(domain: "graph", code: 72))
      precondition(radio.retryCancellationCleanupOnQueue("descriptor-cancel")["state"] as? String == "retryable")
      precondition(radio.descriptorOperations.pendingReads.complete(peripheral: oldGraph,
        object: oldWrite, value: (nil, nil)) == "descriptor-cancel")
      radio.refreshCallbackCancellationCleanup("descriptor-cancel")
      precondition(!cancelledDescriptorCompletion &&
        radio.retryCancellationCleanupOnQueue("descriptor-cancel")["state"] as? String == "released")

      var cancelledReadCompleted = false
      var oldReadLane = OwnedCoreBluetoothReadLane<PendingCharacteristicRead>()
      precondition(oldReadLane.admit(PendingCharacteristicRead(operationIdentifier: "read-cancel",
        completion: { _, _, _ in cancelledReadCompleted = true })))
      radio.pendingRead[cancelledWrite] = oldReadLane
      radio.readObjects[cancelledWrite] = OwnedCoreBluetoothReadObjectFence(
        peripheral: oldGraph, object: oldWrite, cancelledOperation: nil)
      radio.cancelPendingOperation("read-cancel")
      precondition(radio.retryCancellationCleanupOnQueue("read-cancel")["state"] as? String == "retryable")
      radio.failPendingGATT(for: "write-peer", error: NSError(domain: "graph", code: 73))
      precondition(!cancelledReadCompleted && radio.retiredReads.count == 1)
      precondition(!radio.consumeRetiredReadCallback(peripheral: nextGraph, characteristic: nextWrite))
      precondition(radio.consumeRetiredReadCallback(peripheral: oldGraph, characteristic: oldWrite))
      precondition(radio.retryCancellationCleanupOnQueue("read-cancel")["state"] as? String == "released")
      var ambiguousLane = OwnedCoreBluetoothReadLane<PendingCharacteristicRead>()
      precondition(ambiguousLane.admit(PendingCharacteristicRead(operationIdentifier: "ambiguous-read",
        completion: { _, _, _ in fatalError("cancelled ambiguous read completed") })))
      var strandedReadFailed = false
      precondition(!ambiguousLane.admit(PendingCharacteristicRead(operationIdentifier: "stranded-read",
        completion: { _, _, failure in strandedReadFailed = failure != nil })))
      radio.pendingRead[cancelledWrite] = ambiguousLane
      radio.readObjects[cancelledWrite] = ambiguousRead
      radio.cancelPendingOperation("ambiguous-read")
      precondition(strandedReadFailed, "a queued read cannot wait behind an unattributable cancelled response")
      radio.failPendingGATT(for: "write-peer", error: NSError(domain: "graph", code: 74))
      precondition(radio.consumeRetiredReadCallback(peripheral: oldPeripheral, characteristic: oldAttribute))
      precondition(!radio.readCallbackObjectIsAvailable(peripheral: oldPeripheral, characteristic: oldAttribute),
        "a replacement subscription must not claim an object with an unresolved old read")
      precondition(radio.readCallbackObjectIsAvailable(peripheral: newPeripheral, characteristic: newAttribute))
      precondition(radio.retryCancellationCleanupOnQueue("ambiguous-read")["state"] as? String == "retryable",
        "a notification cannot prove that a cancelled read response was consumed")
      var terminalMarkerResults: [Int] = []
      precondition(radio.pendingWrite.admit(cancelledWrite, peer: "write-peer", peripheral: oldGraph,
        object: oldWrite, operation: "terminal-marker", settleOnCancel: true,
        completion: { terminalMarkerResults.append($0?.code ?? 0) }))
      radio.cancelPendingOperation("terminal-marker")
      let rssiObject = NSObject()
      precondition(radio.pendingRssi.admit("write-peer", peer: "write-peer", peripheral: rssiObject,
        object: rssiObject, operation: "terminal-rssi", completion: { _ in }))
      radio.cancelPendingOperation("terminal-rssi")
      precondition(radio.descriptorOperations.pendingWrites.admit(descriptorAddress, peer: "write-peer",
        peripheral: oldGraph, object: oldWrite, operation: "terminal-descriptor", completion: { _ in }))
      radio.cancelPendingOperation("terminal-descriptor")
      let otherAddress = CharacteristicAddress(peerIdentifier: "other-peer", serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      let otherObject = NSObject(), otherAttribute = NSObject()
      precondition(radio.pendingWrite.admit(otherAddress, peer: "other-peer", peripheral: otherObject,
        object: otherAttribute, operation: "other-debt", completion: { _ in }))
      radio.failPendingGATT(for: "other-peer", error: NSError(domain: "graph", code: 1))
      // didDisconnect is a terminal delegate fence, unlike graph invalidation.
      // No callbacks need to be fabricated for the old objects to be reusable.
      radio.completeObservedDisconnect("write-peer", error: nil)
      precondition(!radio.consumeRetiredReadCallback(peripheral: oldPeripheral, characteristic: oldAttribute),
        "terminal disconnect must release the ambiguous read fence")
      precondition(radio.readCallbackObjectIsAvailable(peripheral: oldPeripheral, characteristic: oldAttribute))
      precondition(radio.retryCancellationCleanupOnQueue("ambiguous-read")["state"] as? String == "released")
      precondition(radio.pendingWrite.canAdmit(cancelledWrite, peripheral: oldGraph, object: oldWrite),
        "terminal disconnect must release omitted write callbacks")
      precondition(terminalMarkerResults == [1020], "terminal loss settles a cancelled marker exactly once")
      precondition(radio.retryCancellationCleanupOnQueue("terminal-marker")["state"] as? String == "released")
      precondition(radio.pendingRssi.canAdmit("write-peer", peripheral: rssiObject, object: rssiObject))
      precondition(radio.retryCancellationCleanupOnQueue("terminal-rssi")["state"] as? String == "released")
      precondition(radio.descriptorOperations.pendingWrites.canAdmit(descriptorAddress, peripheral: oldGraph, object: oldWrite))
      precondition(radio.retryCancellationCleanupOnQueue("terminal-descriptor")["state"] as? String == "released")
      precondition(!radio.pendingWrite.canAdmit(otherAddress, peripheral: otherObject, object: otherAttribute),
        "one peer's disconnect must not release another peer's callback debt")
      radio.completeObservedDisconnect("other-peer", error: nil)
      for iteration in 0..<3 {
        let object = iteration == 1 ? NSObject() : oldWrite
        precondition(radio.pendingWrite.admit(cancelledWrite, peer: "write-peer", peripheral: oldGraph,
          object: object, operation: "reconnect-\(iteration)", completion: { _ in }))
        radio.completeObservedDisconnect("write-peer", error: nil)
        precondition(radio.pendingWrite.canAdmit(cancelledWrite, peripheral: oldGraph, object: object))
      }
      let oldObject = NSObject(), replacementObject = NSObject(), fakePeripheral = NSObject()
      let retired = RetiredNotificationOwner(generation: "old", address: a, peripheral: fakePeripheral,
        characteristic: oldObject, subscriptions: ["old-owner"], phase: .awaitingOriginal)
      let owners = [ObjectIdentifier(oldObject): retired]
      precondition(owners[ObjectIdentifier(oldObject)]?.generation == "old")
      precondition(owners[ObjectIdentifier(replacementObject)] == nil)
      let retiredObjects: Set<ObjectIdentifier> = [ObjectIdentifier(oldObject)]
      precondition(!OwnedCoreBluetoothProtocolRadio.notificationObjectIsAvailable(oldObject, retired: retiredObjects))
      precondition(OwnedCoreBluetoothProtocolRadio.notificationObjectIsAvailable(replacementObject, retired: retiredObjects))
      precondition(retired.subscriptions.contains("old-owner") && !retired.subscriptions.contains("new-owner"))
      precondition(retired.callback(isNotifying: true, failed: false) == .disable)
      retired.phase = .awaitingDisable
      precondition(retired.callback(isNotifying: true, failed: true) == .retry)
      retired.phase = .awaitingDisable
      precondition(retired.callback(isNotifying: false, failed: false) == .released)
      retired.phase = .invalidated
      precondition(retired.callback(isNotifying: false, failed: false) == .none)
      var oldDebt = PendingCancellationCleanup()
      oldDebt.notificationDesiredStates[a] = false
      radio.pendingCancellationCleanup["old-debt"] = oldDebt
      radio.transferNotificationCleanup(address: a, toRetiredObject: ObjectIdentifier(oldObject))
      var newDebt = PendingCancellationCleanup()
      newDebt.notificationDesiredStates[a] = false
      radio.pendingCancellationCleanup["new-debt"] = newDebt
      radio.transferNotificationCleanup(address: a, toRetiredObject: ObjectIdentifier(replacementObject))
      radio.clearRetiredNotificationCleanup(ObjectIdentifier(oldObject))
      precondition(radio.pendingCancellationCleanup["old-debt"] == nil && radio.pendingCancellationCleanup["new-debt"]?.retiredNotificationObjects == [ObjectIdentifier(replacementObject)])
      radio.clearRetiredNotificationCleanup(ObjectIdentifier(replacementObject))
      precondition(radio.pendingCancellationCleanup["new-debt"] == nil)
      radio.subscriptions[a] = "first"
      var completions = 0
      func completion(_ error: NSError?) { precondition(error == nil); completions += 1 }
      precondition(radio.updateActiveSharedSubscription(address: a, enabled: true, subscriptionIdentifier: "second", completion: completion))
      precondition(radio.subscriptions[a] == "first" && radio.sharedSubscriptions[a] == ["second"])
      precondition(radio.updateActiveSharedSubscription(address: a, enabled: false, subscriptionIdentifier: "first", completion: completion))
      precondition(radio.subscriptions[a] == "second" && radio.sharedSubscriptions[a] == nil && completions == 2)
      precondition(!radio.updateActiveSharedSubscription(address: a, enabled: false, subscriptionIdentifier: "second", completion: completion))
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "start-1", subscriptionIdentifier: "one", enabled: true, completion: completion)
      radio.pendingNotifyJoins[b] = ["two": PendingNotify(operationIdentifier: "start-2", subscriptionIdentifier: "two", enabled: true, completion: completion)]
      radio.cancelPendingOperation("start-1")
      precondition(radio.pendingNotify[b]?.subscriptionIdentifier == "two" && radio.pendingNotifyJoins[b] == nil)
      radio.handleNotificationStateUpdate(address: b, isNotifying: true, error: nil)
      precondition(radio.subscriptions[b] == "two" && completions == 3)
      radio.subscriptions.removeValue(forKey: a)
      radio.pendingNotify[a] = PendingNotify(operationIdentifier: "start-3", subscriptionIdentifier: "three", enabled: true, completion: completion)
      radio.pendingNotifyJoins[a] = ["four": PendingNotify(operationIdentifier: "start-4", subscriptionIdentifier: "four", enabled: true, completion: completion)]
      radio.handleNotificationStateUpdate(address: a, isNotifying: true, error: nil)
      precondition(radio.subscriptions[a] == "three" && radio.sharedSubscriptions[a] == ["four"] && completions == 5)
      var writes: [Bool] = []
      radio.notificationStateWriter = { _, enabled in writes.append(enabled); return true }
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "stop", subscriptionIdentifier: "two", enabled: false, completion: completion)
      precondition(radio.queueNotificationJoin(address: b, subscriptionIdentifier: "next", operationIdentifier: "start-next", completion: completion))
      precondition(writes.isEmpty && completions == 5)
      radio.handleNotificationStateUpdate(address: b, isNotifying: false, error: nil)
      precondition(writes == [true] && completions == 6 && radio.pendingNotify[b]?.subscriptionIdentifier == "next")
      radio.handleNotificationStateUpdate(address: b, isNotifying: true, error: nil)
      precondition(radio.subscriptions[b] == "next" && completions == 7)
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "cancel-stop", subscriptionIdentifier: "next", enabled: false, completion: { _ in fatalError("cancelled stop completed") })
      precondition(radio.queueNotificationJoin(address: b, subscriptionIdentifier: "after-cancel", operationIdentifier: "join-after-cancel", completion: completion))
      radio.cancelPendingOperation("cancel-stop")
      precondition(radio.queueNotificationJoin(address: b, subscriptionIdentifier: "late", operationIdentifier: "late-start", completion: completion))
      radio.handleNotificationStateUpdate(address: b, isNotifying: false, error: nil)
      precondition(writes == [true, true] && completions == 7)
      radio.handleNotificationStateUpdate(address: b, isNotifying: true, error: nil)
      precondition(radio.subscriptions[b] == "next" && radio.sharedSubscriptions[b] == ["after-cancel", "late"] && completions == 9)
      radio.sharedSubscriptions.removeValue(forKey: b)
      var stopFailed = false
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "failed-stop", subscriptionIdentifier: "next", enabled: false, completion: { stopFailed = $0 != nil })
      precondition(radio.queueNotificationJoin(address: b, subscriptionIdentifier: "after-failure", operationIdentifier: "join-failure", completion: completion))
      radio.handleNotificationStateUpdate(address: b, isNotifying: true, error: NSError(domain: "test", code: 1))
      precondition(stopFailed && radio.subscriptions[b] == "next" && radio.sharedSubscriptions[b] == ["after-failure"] && completions == 10)
      radio.sharedSubscriptions.removeValue(forKey: b)
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "failed-off", subscriptionIdentifier: "next", enabled: false, completion: { precondition($0 != nil) })
      let failedOffCleanup = radio.pendingNotify[b]!.cleanupIdentifier
      precondition(radio.queueNotificationJoin(address: b, subscriptionIdentifier: "restored", operationIdentifier: "restore-join", completion: completion))
      radio.handleNotificationStateUpdate(address: b, isNotifying: false, error: NSError(domain: "test", code: 2))
      precondition(radio.pendingCancellationCleanup[failedOffCleanup] != nil && writes == [true, true, true])
      radio.handleNotificationStateUpdate(address: b, isNotifying: true, error: nil)
      precondition(radio.pendingCancellationCleanup[failedOffCleanup] == nil && radio.subscriptions[b] == "next" && radio.sharedSubscriptions[b] == ["restored"] && completions == 11)
      radio.sharedSubscriptions.removeValue(forKey: b)
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "stop-before-cancel-join", subscriptionIdentifier: "next", enabled: false, completion: completion)
      precondition(radio.queueNotificationJoin(address: b, subscriptionIdentifier: "withdrawn", operationIdentifier: "cancel-join", completion: { _ in fatalError("cancelled join completed") }))
      radio.cancelPendingOperation("cancel-join")
      radio.handleNotificationStateUpdate(address: b, isNotifying: false, error: nil)
      precondition(radio.pendingNotify[b] == nil && radio.subscriptions[b] == nil && writes == [true, true, true] && completions == 12)
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "failed-start", subscriptionIdentifier: "failed", enabled: true, completion: { precondition($0 != nil) })
      radio.handleNotificationStateUpdate(address: b, isNotifying: true, error: NSError(domain: "test", code: 3))
      precondition(writes == [true, true, true, false] && radio.hasPendingCancellationCleanup(for: b))
      precondition(!radio.notificationTransitionAvailable(for: b))
      radio.handleNotificationStateUpdate(address: b, isNotifying: false, error: nil)
      precondition(!radio.hasPendingCancellationCleanup(for: b))
      var teardownFailures = 0
      radio.subscriptions[b] = "owner"
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "disconnect-stop", subscriptionIdentifier: "owner", enabled: false, completion: { _ in fatalError("cancelled stop") })
      precondition(radio.queueNotificationJoin(address: b, subscriptionIdentifier: "disconnect-join", operationIdentifier: "disconnect-join-op", completion: { precondition($0 != nil); teardownFailures += 1 }))
      radio.cancelPendingOperation("disconnect-stop")
      radio.failPendingGATT(for: "p", error: nil)
      precondition(teardownFailures == 1 && radio.pendingNotifyJoins[b] == nil)
      radio.clearCancellationCleanup(forPeerIdentifier: "p")
      radio.subscriptions[b] = "owner"
      radio.pendingNotify[b] = PendingNotify(operationIdentifier: "service-stop", subscriptionIdentifier: "owner", enabled: false, completion: { _ in fatalError("cancelled stop") })
      precondition(radio.queueNotificationJoin(address: b, subscriptionIdentifier: "service-join", operationIdentifier: "service-join-op", completion: { precondition($0 != nil); teardownFailures += 1 }))
      radio.cancelPendingOperation("service-stop")
      radio.clearNotificationOwnership(forPeerIdentifier: "p")
      precondition(teardownFailures == 2 && radio.pendingNotifyJoins[b] == nil)
      radio.clearCancellationCleanup(forPeerIdentifier: "p")
      let source = OwnedOrderedNotificationSource(queue: radio.queue, generation: "g")
      let group = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "g", members: [radio.orderedRoute(for: a), radio.orderedRoute(for: b)], capacity: 4, byteCapacity: 10)
      precondition(source.add(group))
      radio.orderedSources["p"] = source
      guard let ac = source.makeClaim(route: radio.orderedRoute(for: a)), let bc = source.makeClaim(route: radio.orderedRoute(for: b)) else { fatalError("claim") }
      try group.register(ac)
      try group.register(bc)
      radio.admitOrderedCharacteristicValue(address: a, value: Data([1]))
      var lane = OwnedCoreBluetoothReadLane<PendingCharacteristicRead>()
      _ = lane.admit(PendingCharacteristicRead(operationIdentifier: "read", completion: { _, _, _ in }))
      radio.pendingRead[b] = lane
      radio.admitOrderedCharacteristicValue(address: b, value: Data([2]))
      radio.invalidateOrderedSource(for: "p", cause: .serviceChanged)
      radio.admitOrderedCharacteristicValue(address: a, value: Data([3]))
      let events = group.drain()
      precondition(events.count == 3)
      guard case .value(let first, _, let ordinalA, let bytesA, .notification) = events[0],
            case .value(let second, _, let ordinalB, let bytesB, .readOrNotification) = events[1],
            case .terminal(.serviceChanged, .uncertain, nil) = events[2] else { fatalError("events") }
      precondition(first == radio.orderedRoute(for: a) && second == radio.orderedRoute(for: b))
      precondition(ordinalA < ordinalB && bytesA == Data([1]) && bytesB == Data([2]))

      let failureSource = OwnedOrderedNotificationSource(queue: radio.queue, generation: "failure")
      let groupA = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "failure", members: [radio.orderedRoute(for: a)], capacity: 4, byteCapacity: 10)
      let groupB = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "failure", members: [radio.orderedRoute(for: b)], capacity: 4, byteCapacity: 10)
      precondition(failureSource.add(groupA) && failureSource.add(groupB))
      guard let failureA = failureSource.makeClaim(route: radio.orderedRoute(for: a)),
            let failureB = failureSource.makeClaim(route: radio.orderedRoute(for: b)) else { fatalError("failure claim") }
      try groupA.register(failureA); try groupB.register(failureB)
      radio.orderedSources["p"] = failureSource
      radio.subscriptions[a] = "a-owner"; radio.subscriptions[b] = "b-owner"
      let nativeFailure = NSError(domain: "CBFixture", code: 42)
      radio.reportOrderedCharacteristicFailure(address: b, error: nativeFailure)
      precondition(groupB.drain() == [.terminal(.sourceFailed(.init(error: nativeFailure), member: radio.orderedRoute(for: b), sourceMember: failureB.token, epoch: 1), platformTail: .uncertain, rejected: nil)])
      precondition(groupA.drain().isEmpty, "failure on one route must not end an unrelated group")
      radio.reportOrderedCharacteristicFailure(address: a, error: nativeFailure)
      precondition(groupA.drain() == [.terminal(.sourceFailed(.init(error: nativeFailure), member: radio.orderedRoute(for: a), sourceMember: failureA.token, epoch: 1), platformTail: .uncertain, rejected: nil)])
      radio.pendingRead.removeValue(forKey: b)

      let heldAddress = CharacteristicAddress(peerIdentifier: "held", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
      let heldRoute = radio.orderedRoute(for: heldAddress)
      let heldSource = OwnedOrderedNotificationSource(queue: radio.queue, generation: "held")
      let heldGroup = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "held", members: [heldRoute], capacity: 4, byteCapacity: 10)
      precondition(heldSource.add(heldGroup))
      guard let heldClaim = heldSource.makeClaim(route: heldRoute) else { fatalError("held claim") }
      try heldGroup.register(heldClaim)
      radio.orderedSources["held"] = heldSource
      radio.orderedPermitSources["held"] = heldSource
      let heldGate = radio.orderedGraphGate(peer: "held")
      let heldKey = OwnedOrderedGraphPermitGate.Key(peer: "held", connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
      precondition(heldGate.acquire(heldKey, valid: true) == .acquired)
      radio.admitOrderedCharacteristicValue(address: heldAddress, value: Data([7]))
      radio.invalidateOrderedSource(for: "held", cause: .serviceChanged)
      radio.admitOrderedCharacteristicValue(address: heldAddress, value: Data([8]))
      precondition(heldGroup.drain().isEmpty && heldGate.commit(heldKey) == .invalidated)
      precondition(heldGate.release(heldKey) == .released)
      let heldEvents = heldGroup.drain()
      guard heldEvents.count == 2,
            case .value(_, _, _, let captured, .notification) = heldEvents[0],
            case .terminal(.serviceChanged, .uncertain, nil) = heldEvents[1] else { fatalError("held source ordering") }
      precondition(captured == Data([7]))

      let resetP = radio.orderedGraphGate(peer: "reset-p")
      let resetQ = radio.orderedGraphGate(peer: "reset-q")
      let snapshot = OwnedGattSnapshotIdentity(owner: "reset", attachment: 1, revision: 1)
      let resetPK = OwnedOrderedGraphPermitGate.Key(peer: "reset-p", connection: "c", snapshot: snapshot, id: 10)
      let resetQK = OwnedOrderedGraphPermitGate.Key(peer: "reset-q", connection: "c", snapshot: snapshot, id: 11)
      precondition(resetP.acquire(resetPK, valid: true) == .acquired && resetQ.acquire(resetQK, valid: true) == .acquired)
      var resetEvents: [String] = []
      let readAddress = CharacteristicAddress(peerIdentifier: "reset-p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
      var readCompletions = 0
      var resetRead = OwnedCoreBluetoothReadLane<PendingCharacteristicRead>()
      _ = resetRead.admit(PendingCharacteristicRead(operationIdentifier: "held-read", completion: { _, _, error in
        precondition(error != nil); readCompletions += 1
      }))
      radio.pendingRead[readAddress] = resetRead
      radio.performOrderedGlobalBarrier(peerOperation: { peer in
        if peer == "reset-p" || peer == "reset-q" { resetEvents.append("\(peer)-reset") }
        radio.resetOrderedPeer(peer, cause: .adapterLost)
      }) { resetEvents.append("global-reset") }
      let callbackBytes = Data([9])
      radio.deliverCharacteristicSnapshot(peer: "reset-p", byteCount: callbackBytes.count) {
        if var lane = radio.pendingRead[readAddress] {
          let (pending, _) = lane.answer()
          radio.pendingRead[readAddress] = lane.isIdle ? nil : lane
          pending?.completion(callbackBytes as NSData, .readResponse, nil)
        }
        resetEvents.append("p-value")
      }
      resetQ.enqueue { resetEvents.append("q-value") }
      precondition(resetEvents.isEmpty && resetP.commit(resetPK) == .invalidated && resetQ.commit(resetQK) == .invalidated)
      precondition(resetP.release(resetPK) == .released && resetEvents == ["reset-p-reset", "p-value"] && readCompletions == 1)
      precondition(resetQ.release(resetQK) == .released)
      precondition(resetEvents == ["reset-p-reset", "p-value", "reset-q-reset", "global-reset", "q-value"] && readCompletions == 1)

      let multiP = radio.orderedGraphGate(peer: "multi-p")
      let multiQ = radio.orderedGraphGate(peer: "multi-q")
      let multiPK = OwnedOrderedGraphPermitGate.Key(peer: "multi-p", connection: "c", snapshot: snapshot, id: 12)
      let multiQK = OwnedOrderedGraphPermitGate.Key(peer: "multi-q", connection: "c", snapshot: snapshot, id: 13)
      precondition(multiP.acquire(multiPK, valid: true) == .acquired && multiQ.acquire(multiQK, valid: true) == .acquired)
      precondition(multiP.commit(multiPK) == .committed && multiQ.commit(multiQK) == .committed)
      var multiEvents: [String] = []
      radio.performOrderedGlobalBarrier(peerOperation: { if $0.hasPrefix("multi-") { multiEvents.append("\($0)-reset1") } }) { multiEvents.append("global1") }
      multiP.enqueue { multiEvents.append("p-value1") }; multiQ.enqueue { multiEvents.append("q-value1") }
      radio.performOrderedGlobalBarrier(peerOperation: { if $0.hasPrefix("multi-") { multiEvents.append("\($0)-reset2") } }) { multiEvents.append("global2") }
      multiP.enqueue { multiEvents.append("p-value2") }; multiQ.enqueue { multiEvents.append("q-value2") }
      precondition(multiP.release(multiPK) == .released)
      precondition(multiEvents == ["multi-p-reset1", "p-value1", "multi-p-reset2", "p-value2"])
      precondition(multiQ.release(multiQK) == .released)
      precondition(multiEvents == ["multi-p-reset1", "p-value1", "multi-p-reset2", "p-value2", "multi-q-reset1", "global1", "q-value1", "multi-q-reset2", "global2", "q-value2"])

      let overflowAddress = CharacteristicAddress(peerIdentifier: "overflow", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
      let overflowRoute = radio.orderedRoute(for: overflowAddress)
      let overflowSource = OwnedOrderedNotificationSource(queue: radio.queue, generation: "overflow")
      let overflowGroup = try OwnedOrderedNotificationIngress(queue: radio.queue, generation: "overflow", members: [overflowRoute], capacity: 2048, byteCapacity: 2048)
      precondition(overflowSource.add(overflowGroup))
      guard let overflowClaim = overflowSource.makeClaim(route: overflowRoute) else { fatalError("overflow claim") }
      try overflowGroup.register(overflowClaim)
      radio.orderedSources["overflow"] = overflowSource
      radio.orderedPermitSources["overflow"] = overflowSource
      let overflowGate = radio.orderedGraphGate(peer: "overflow")
      let overflowKey = OwnedOrderedGraphPermitGate.Key(peer: "overflow", connection: "c", snapshot: snapshot, id: 14)
      precondition(overflowGate.acquire(overflowKey, valid: true) == .acquired)
      for _ in 0...1024 { radio.admitOrderedCharacteristicValue(address: overflowAddress, value: Data([1])) }
      precondition(overflowGate.commit(overflowKey) == .invalidated && overflowGroup.drain().isEmpty)
      precondition(overflowGate.release(overflowKey) == .released)
      let overflowEvents = overflowGroup.drain()
      // The graph accepted 1,024 values. The 1,025th value was observed by
      // the source before graph refusal, so it remains an explicit loss record
      // rather than being silently discarded before the overflow terminal.
      precondition(overflowEvents.count == 1026)
      guard case .loss(member: overflowRoute, count: 1) = overflowEvents[1024] else {
        fatalError("missing rejected ordered value loss")
      }
      guard case .terminal(.overflow, .uncertain, nil) = overflowEvents.last else { fatalError("missing ordered overflow terminal") }
      precondition(overflowGate.forget(overflowKey))
      radio.cleanupOrderedGraphGateIfIdle("overflow")
      precondition(radio.orderedGraphPermitGates["overflow"] == nil && radio.orderedPermitSources["overflow"] == nil)

      let failstopGate = radio.orderedGraphGate(peer: "failstop")
      let failstopKey = OwnedOrderedGraphPermitGate.Key(peer: "failstop", connection: "c", snapshot: snapshot, id: 15)
      precondition(failstopGate.acquire(failstopKey, valid: true) == .acquired)
      var preparationCompletions = 0
      radio.radioPreparation.start("held-preparation", snapshot: ["power": "unknown"], waitForInitialState: true) { _, error in
        precondition(error != nil); preparationCompletions += 1
      }
      for _ in 0..<64 { precondition(failstopGate.enqueueBarrier { resume in resume() }) }
      precondition(!failstopGate.enqueueBarrier { resume in resume() })
      precondition(preparationCompletions == 0)
      precondition(failstopGate.release(failstopKey) == .released && preparationCompletions == 1)
      radio.radioPreparation.update(["power": "poweredOn"])
      precondition(preparationCompletions == 1)

      let observer = OverflowRadioDelegate()
      radio.delegate = observer
      let lossPeer = "lost-disconnect"
      let lossGate = radio.orderedGraphGate(peer: lossPeer)
      let lossKey = OwnedOrderedGraphPermitGate.Key(peer: lossPeer, connection: "c", snapshot: snapshot, id: 16)
      precondition(lossGate.acquire(lossKey, valid: true) == .acquired)
      var connectFailures = 0, discoveryFailures = 0, rssiFailures = 0
      radio.pendingConnect[lossPeer] = PendingVoid(operationIdentifier: "lost-connect", completion: { error in
        precondition(error != nil); connectFailures += 1
      })
      let oldService = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      var stranded = PendingDiscovery(operationIdentifier: "lost-discovery", completion: { _, error in
        precondition(error != nil); discoveryFailures += 1
      })
      stranded.awaitingServices = false
      stranded.includeCallbacks.insert(ObjectIdentifier(oldService))
      radio.pendingDiscovery[lossPeer] = stranded
      let rssiPeripheral = NSObject()
      precondition(radio.pendingRssi.admit(lossPeer, peer: lossPeer, peripheral: rssiPeripheral,
        object: rssiPeripheral, operation: "lost-rssi", completion: { result in
        precondition(result.1 != nil); rssiFailures += 1
      }))
      for _ in 0..<1024 { lossGate.enqueue {} }
      let lost = lossGate.invalidateAndEnqueue { fatalError("overflowed disconnect ran") }
      precondition(!lost)
      radio.orderedLostDisconnects[lossPeer] = .init(error: nil)
      precondition(lossGate.release(lossKey) == .released)
      precondition(connectFailures == 1 && discoveryFailures == 1 && rssiFailures == 1)
      precondition(radio.pendingDiscovery[lossPeer] == nil && observer.disconnects == [lossPeer])
      let replacementService = CBMutableService(type: CBUUID(string: "180D"), primary: true)
      var replacement = PendingDiscovery(operationIdentifier: "replacement", completion: { _, _ in })
      replacement.awaitingServices = false
      replacement.includeCallbacks.insert(ObjectIdentifier(replacementService))
      precondition(!replacement.consumeIncludes(oldService) && replacement.consumeIncludes(replacementService))

      let disconnectPeer = "lost-explicit-disconnect"
      let disconnectGate = radio.orderedGraphGate(peer: disconnectPeer)
      let disconnectKey = OwnedOrderedGraphPermitGate.Key(peer: disconnectPeer, connection: "c", snapshot: snapshot, id: 17)
      precondition(disconnectGate.acquire(disconnectKey, valid: true) == .acquired)
      var explicitCompletions = 0
      radio.pendingDisconnect[disconnectPeer] = PendingVoid(operationIdentifier: "lost-disconnect", completion: { error in
        precondition(error == nil); explicitCompletions += 1
      })
      for _ in 0..<1024 { disconnectGate.enqueue {} }
      precondition(!disconnectGate.invalidateAndEnqueue { fatalError("overflowed explicit disconnect ran") })
      radio.orderedLostDisconnects[disconnectPeer] = .init(error: nil)
      precondition(disconnectGate.release(disconnectKey) == .released && explicitCompletions == 1)
      precondition(observer.disconnects == [lossPeer])

      let failedPeer = "lost-connect-callback"
      let failedGate = radio.orderedGraphGate(peer: failedPeer)
      let failedKey = OwnedOrderedGraphPermitGate.Key(peer: failedPeer, connection: "c", snapshot: snapshot, id: 19)
      precondition(failedGate.acquire(failedKey, valid: true) == .acquired)
      var failedConnect = 0, failedDisconnect = 0
      radio.pendingConnect[failedPeer] = PendingVoid(operationIdentifier: "connect", completion: { error in
        precondition(error != nil); failedConnect += 1
      })
      radio.pendingDisconnect[failedPeer] = PendingVoid(operationIdentifier: "disconnect", completion: { error in
        precondition(error != nil); failedDisconnect += 1
      })
      for _ in 0..<1024 { failedGate.enqueue {} }
      precondition(!failedGate.enqueue { fatalError("overflowed connection callback ran") })
      precondition(failedGate.release(failedKey) == .released)
      precondition(failedConnect == 1 && failedDisconnect == 1 && observer.disconnects == [lossPeer])

      let statePeer = "lost-central-state"
      let stateGate = radio.orderedGraphGate(peer: statePeer)
      let stateKey = OwnedOrderedGraphPermitGate.Key(peer: statePeer, connection: "c", snapshot: snapshot, id: 18)
      precondition(stateGate.acquire(stateKey, valid: true) == .acquired)
      let stateOther = radio.orderedGraphGate(peer: "other-central-state")
      let stateOtherKey = OwnedOrderedGraphPermitGate.Key(peer: "other-central-state", connection: "c", snapshot: snapshot, id: 20)
      precondition(stateOther.acquire(stateOtherKey, valid: true) == .acquired)
      for _ in 0..<64 { precondition(stateGate.enqueueBarrier { resume in resume() }) }
      var statePreparation = 0
      radio.radioPreparation.start("state-waiter", snapshot: ["power": "unknown"], waitForInitialState: true) { _, error in
        precondition(error != nil); statePreparation += 1
      }
      let stateSnapshot: NSDictionary = ["power": "poweredOff"]
      radio.routeCentralState(.poweredOff, snapshot: stateSnapshot)
      precondition(observer.states.isEmpty && statePreparation == 0)
      precondition(stateGate.release(stateKey) == .released)
      precondition(observer.states.isEmpty)
      precondition(stateOther.release(stateOtherKey) == .released)
      precondition(observer.states.count == 1 && observer.states[0] == stateSnapshot && statePreparation == 1)
      precondition(radio.pendingOverflowCentralState == nil)
      radio.radioPreparation.update(["power": "poweredOn"])
      precondition(statePreparation == 1)

      let revisionPeer = "central-state-order"
      let revisionGate = radio.orderedGraphGate(peer: revisionPeer)
      let revisionKey = OwnedOrderedGraphPermitGate.Key(peer: revisionPeer, connection: "c", snapshot: snapshot, id: 21)
      precondition(revisionGate.acquire(revisionKey, valid: true) == .acquired)
      let stateCountBeforeRevision = observer.states.count
      radio.routeCentralState(.poweredOn, snapshot: ["power": "older"])
      for _ in 0..<63 { precondition(revisionGate.enqueueBarrier { resume in resume() }) }
      radio.routeCentralState(.poweredOff, snapshot: ["power": "overflowed"])
      radio.routeCentralState(.resetting, snapshot: ["power": "newest"])
      precondition(observer.states.count == stateCountBeforeRevision)
      precondition(revisionGate.release(revisionKey) == .released)
      precondition(observer.states.count == stateCountBeforeRevision + 1)
      precondition(observer.states.last?["power"] as? String == "newest")
      precondition(radio.pendingOverflowCentralState == nil)

      let singleRevisionGate = radio.orderedGraphGate(peer: "central-state-single")
      let singleRevisionKey = OwnedOrderedGraphPermitGate.Key(peer: "central-state-single", connection: "c", snapshot: snapshot, id: 22)
      precondition(singleRevisionGate.acquire(singleRevisionKey, valid: true) == .acquired)
      let beforeSingleRevision = observer.states.count
      radio.routeCentralState(.poweredOn, snapshot: ["power": "accepted-A"])
      for _ in 0..<63 { precondition(singleRevisionGate.enqueueBarrier { resume in resume() }) }
      radio.routeCentralState(.poweredOff, snapshot: ["power": "overflow-B"])
      precondition(singleRevisionGate.release(singleRevisionKey) == .released)
      precondition(observer.states.count == beforeSingleRevision + 1 && observer.states.last?["power"] as? String == "overflow-B")

      let recoveryPeer = "adapter-reset-recovery"
      radio.orderedOverflowDisconnectPending.insert(recoveryPeer)
      var recoveredConnect = 0, recoveredDisconnect = 0, recoveredDiscovery = 0
      radio.pendingConnect[recoveryPeer] = PendingVoid(operationIdentifier: "recovery-connect", completion: { error in
        precondition(error != nil); recoveredConnect += 1
      })
      radio.pendingDisconnect[recoveryPeer] = PendingVoid(operationIdentifier: "recovery-disconnect", completion: { error in
        precondition(error != nil); recoveredDisconnect += 1
      })
      radio.pendingDiscovery[recoveryPeer] = PendingDiscovery(operationIdentifier: "recovery-discovery", completion: { _, error in
        precondition(error != nil); recoveredDiscovery += 1
      })
      radio.routeCentralState(.poweredOff, snapshot: ["power": "poweredOff"])
      precondition(radio.orderedOverflowDisconnectPending.contains(recoveryPeer))
      radio.routeCentralState(.resetting, snapshot: ["power": "resetting"])
      precondition(!radio.orderedOverflowDisconnectPending.contains(recoveryPeer))
      precondition(radio.pendingDiscovery[recoveryPeer] == nil && recoveredConnect == 1 && recoveredDisconnect == 1 && recoveredDiscovery == 1)

      let adapterPeer = "adapter-terminal-debt"
      let adapterAddress = CharacteristicAddress(peerIdentifier: adapterPeer, serviceUUID: "s", serviceOccurrence: 0,
        characteristicUUID: "c", characteristicOccurrence: 0)
      let adapterObject = NSObject(), adapterAttribute = NSObject()
      _ = radio.orderedGraphGate(peer: adapterPeer)
      precondition(radio.pendingWrite.admit(adapterAddress, peer: adapterPeer, peripheral: adapterObject,
        object: adapterAttribute, operation: "adapter-write", completion: { _ in }))
      radio.cancelPendingOperation("adapter-write")
      radio.failPendingGATT(for: adapterPeer, error: NSError(domain: "graph", code: 1))
      radio.routeCentralState(.poweredOff, snapshot: ["power": "poweredOff"])
      precondition(!radio.pendingWrite.canAdmit(adapterAddress, peripheral: adapterObject, object: adapterAttribute),
        "powered off alone is not the invalidation fence")
      radio.routeCentralState(.resetting, snapshot: ["power": "resetting"])
      precondition(radio.pendingWrite.canAdmit(adapterAddress, peripheral: adapterObject, object: adapterAttribute))
      precondition(radio.retryCancellationCleanupOnQueue("adapter-write")["state"] as? String == "released")

      let discoveryPeer = "invalidated-discovery"
      _ = radio.orderedGraphGate(peer: discoveryPeer)
      radio.pendingDiscovery[discoveryPeer] = PendingDiscovery(operationIdentifier: "cancel-discovery", completion: { _, _ in
        preconditionFailure("cancelled discovery must not complete")
      })
      radio.cancelPendingOperation("cancel-discovery")
      precondition(radio.retryCancellationCleanupOnQueue("cancel-discovery")["state"] as? String == "retryable")
      radio.routeCentralState(.resetting, snapshot: ["power": "resetting"])
      precondition(radio.pendingDiscovery[discoveryPeer] == nil &&
        radio.retryCancellationCleanupOnQueue("cancel-discovery")["state"] as? String == "released",
        "adapter invalidation must retire discovery callback reservations")
      for kind in 0..<3 {
        for cancelled in [false, true] {
          let peer = "discovery-\(kind)-\(cancelled)"
          let operation = "operation-\(peer)"
          let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
          let characteristic = CBMutableCharacteristic(type: CBUUID(string: "2A37"), properties: [.read], value: nil, permissions: [.readable])
          var failures = 0
          var pending = PendingDiscovery(operationIdentifier: operation, completion: { _, error in
            precondition(error != nil); failures += 1
          })
          pending.awaitingServices = false
          if kind == 0 { pending.characteristicCallbacks.insert(ObjectIdentifier(service)) }
          if kind == 1 { pending.includeCallbacks.insert(ObjectIdentifier(service)) }
          if kind == 2 { pending.descriptorCallbacks[ObjectIdentifier(characteristic)] = ObjectIdentifier(service) }
          radio.pendingDiscovery[peer] = pending
          if cancelled { radio.cancelPendingOperation(operation) }
          radio.routeCentralState(.resetting, snapshot: ["power": "resetting"])
          precondition(radio.pendingDiscovery[peer] == nil && failures == (cancelled ? 0 : 1))
          if cancelled { precondition(radio.retryCancellationCleanupOnQueue(operation)["state"] as? String == "released") }
        }
      }
    }
    try testDelegateEntryPoints()
    print("Ordered radio callback seam: deterministic tests passed")
  }

  static func testDelegateEntryPoints() throws {
    guard let old = class_createInstance(FixturePeripheral.self, 0) as? FixturePeripheral,
          let replacement = class_createInstance(FixturePeripheral.self, 0) as? FixturePeripheral,
          let central = class_createInstance(FixtureCentral.self, 0) as? FixtureCentral else { fatalError("fixture allocation") }
    // CoreBluetooth's dealloc assumes private initialization registered KVO.
    // Keep these inert objects alive for the lifetime of this test process.
    _ = Unmanaged.passRetained(old)
    _ = Unmanaged.passRetained(replacement)
    _ = Unmanaged.passRetained(central)
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let observer = OverflowRadioDelegate()
    let peer = fixtureIdentifier.uuidString
    let characteristic = FixtureCharacteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [.readable])
    let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
    service.characteristics = [characteristic]
    fixtureService = service
    fixtureServices = [service]
    let address = CharacteristicAddress(peerIdentifier: peer,
      serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
      characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
    radio.queue.sync {
      radio.delegate = observer
      radio.peripheralByIdentifier[peer] = replacement
      radio.servicesByPeer[peer] = [service]
      var connected = false
      radio.pendingConnect[peer] = PendingVoid(operationIdentifier: "new-connect", completion: { _ in connected = true })
      radio.centralManager(central, didConnect: old)
      precondition(!connected && radio.peripheralByIdentifier[peer] === replacement)
      var discovered = false
      radio.pendingDiscovery[peer] = PendingDiscovery(operationIdentifier: "new-discovery", completion: { _, _ in discovered = true })
      radio.peripheral(old, didDiscoverServices: nil)
      precondition(!discovered && radio.pendingDiscovery[peer]?.awaitingServices == true)
      radio.peripheralIsReady(toSendWriteWithoutResponse: old)
      precondition(observer.readiness.isEmpty)
      let gate = radio.orderedGraphGate(peer: peer)
      let key = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "fixture", snapshot: .init(owner: "fixture", attachment: 1, revision: 1), id: 1)
      precondition(gate.acquire(key, valid: true) == .acquired)
      radio.peripheralIsReady(toSendWriteWithoutResponse: replacement)
      radio.peripheralByIdentifier[peer] = old
      precondition(gate.release(key) == .released && observer.readiness.isEmpty)
      let connectKey = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "fixture", snapshot: .init(owner: "fixture", attachment: 1, revision: 1), id: 2)
      radio.peripheralByIdentifier[peer] = replacement
      precondition(gate.acquire(connectKey, valid: true) == .acquired)
      radio.centralManager(central, didConnect: replacement)
      radio.peripheralByIdentifier[peer] = old
      precondition(gate.release(connectKey) == .released && !connected)
      let discoveryKey = OwnedOrderedGraphPermitGate.Key(peer: peer, connection: "fixture", snapshot: .init(owner: "fixture", attachment: 1, revision: 1), id: 3)
      radio.peripheralByIdentifier[peer] = replacement
      precondition(gate.acquire(discoveryKey, valid: true) == .acquired)
      radio.peripheral(replacement, didDiscoverServices: nil)
      radio.peripheralByIdentifier[peer] = old
      precondition(gate.release(discoveryKey) == .released && radio.pendingDiscovery[peer]?.awaitingServices == true)
      radio.peripheralByIdentifier[peer] = replacement
      radio.subscriptions[address] = "delegate-owner"
      radio.sharedSubscriptions[address] = ["delegate-shared"]
      let failure = NSError(domain: "CBDelegateFixture", code: 77)
      radio.peripheral(replacement, didUpdateValueFor: characteristic, error: failure)
      precondition(Set(observer.notificationFailures.map { $0.0 }) == ["delegate-owner", "delegate-shared"])
      radio.peripheral(replacement, didUpdateNotificationStateFor: characteristic, error: failure)
      precondition(observer.notificationFailures.count == 4)
      radio.peripheral(replacement, didUpdateNotificationStateFor: characteristic, error: nil)
      precondition(observer.notificationFailures.count == 6 && observer.notificationFailures.last?.1.code == 1039)
      var readFailed = false
      var lane = OwnedCoreBluetoothReadLane<PendingCharacteristicRead>()
      _ = lane.admit(PendingCharacteristicRead(operationIdentifier: "delegate-read", completion: { _, _, error in readFailed = error != nil }))
      radio.pendingRead[address] = lane
      radio.readObjects[address] = OwnedCoreBluetoothReadObjectFence(peripheral: replacement, object: characteristic,
        cancelledOperation: nil, unattributable: true)
      radio.peripheral(replacement, didUpdateValueFor: characteristic, error: failure)
      precondition(readFailed && observer.notificationFailures.count == 8)
      radio.pendingRead.removeValue(forKey: address)
      radio.readObjects.removeValue(forKey: address)
      radio.pendingNotify[address] = PendingNotify(operationIdentifier: "delegate-stop", subscriptionIdentifier: "delegate-owner",
        enabled: false, completion: { _ in preconditionFailure("cancelled stop completed") })
      radio.cancelPendingOperation("delegate-stop")
      fixtureNotifying = false
      radio.peripheral(replacement, didUpdateNotificationStateFor: characteristic, error: nil)
      precondition(observer.notificationFailures.count == 10 && observer.notificationFailures.last?.1.code == 1039)
      fixtureNotifying = true
      radio.peripheral(replacement, didUpdateNotificationStateFor: characteristic, error: nil)
      precondition(radio.retryCancellationCleanupOnQueue("delegate-stop")["state"] as? String == "released")
      radio.restoredPeerIdentifiers = [peer]
    }
    fixtureWrites = []
    let released = DispatchSemaphore(value: 0)
    var releaseError: NSError?
    radio.releaseProtocolClient { releaseError = $0; released.signal() }
    precondition(released.wait(timeout: .now() + 5) == .success && releaseError != nil)
    radio.queue.sync {
      precondition(fixtureWrites == [false] && radio.retiredNotificationOwners.count == 1)
      fixtureNotifying = false
      radio.peripheral(replacement, didUpdateNotificationStateFor: characteristic, error: nil)
      precondition(radio.retiredNotificationOwners.isEmpty)
    }
    radio.releaseProtocolClient { releaseError = $0; released.signal() }
    precondition(released.wait(timeout: .now() + 5) == .success && releaseError == nil)

    for enabling in [true, false] {
      let transitioning = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
      var failedTransitions = 0
      transitioning.queue.sync {
        transitioning.peripheralByIdentifier[peer] = replacement
        transitioning.servicesByPeer[peer] = [service]
        transitioning.restoredPeerIdentifiers = [peer]
        if !enabling { transitioning.subscriptions[address] = "transition-owner" }
        transitioning.pendingNotify[address] = PendingNotify(operationIdentifier: "transition", subscriptionIdentifier: "transition-owner",
          enabled: enabling, completion: { error in if error != nil { failedTransitions += 1 } })
        transitioning.notificationTransitionObjects[address] = ObjectIdentifier(characteristic)
      }
      fixtureWrites = []
      transitioning.releaseProtocolClient { releaseError = $0; released.signal() }
      precondition(released.wait(timeout: .now() + 5) == .success && releaseError != nil)
      var replacementAnswers = 0
      var replacementError: NSError?
      transitioning.subscribe(peerIdentifier: peer, serviceUUID: "180D", serviceOccurrence: 0,
        characteristicUUID: "2A37", characteristicOccurrence: 0, subscriptionIdentifier: "replacement-owner",
        operationIdentifier: "replacement-operation", completion: { error in
          replacementAnswers += 1; replacementError = error; released.signal()
        })
      precondition(released.wait(timeout: .now() + 5) == .success && replacementError != nil && replacementAnswers == 1)
      transitioning.queue.sync {
        precondition(failedTransitions == 1 && fixtureWrites.isEmpty)
        precondition(!transitioning.notificationObjectIsAvailable(characteristic), "old callback must fence a replacement")
        fixtureNotifying = enabling
        transitioning.peripheral(replacement, didUpdateNotificationStateFor: characteristic, error: nil)
        precondition(fixtureWrites == [false] && !transitioning.notificationObjectIsAvailable(characteristic) && replacementAnswers == 1)
        fixtureNotifying = false
        transitioning.peripheral(replacement, didUpdateNotificationStateFor: characteristic, error: nil)
        precondition(transitioning.notificationObjectIsAvailable(characteristic))
      }
      transitioning.releaseProtocolClient { releaseError = $0; released.signal() }
      precondition(released.wait(timeout: .now() + 5) == .success && releaseError == nil && failedTransitions == 1)
    }
    let invalidation = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    invalidation.queue.sync {
      var originalFailures = 0, replacementFailures = 0
      invalidation.peripheralByIdentifier[peer] = old
      invalidation.servicesByPeer[peer] = [service]
      invalidation.pendingDiscovery[peer] = PendingDiscovery(operationIdentifier: "old-discovery", completion: { _, error in
        if error != nil { originalFailures += 1 }
      })
      invalidation.routeCentralState(.resetting, snapshot: ["power": "resetting"])
      precondition(originalFailures == 1 && invalidation.peripheralByIdentifier[peer] == nil && invalidation.servicesByPeer[peer] == nil)
      invalidation.peripheralByIdentifier[peer] = replacement
      invalidation.pendingDiscovery[peer] = PendingDiscovery(operationIdentifier: "reacquired-discovery", completion: { _, error in
        if error != nil { replacementFailures += 1 }
      })
      let failure = NSError(domain: "discovery", code: 1)
      invalidation.peripheral(old, didDiscoverServices: failure)
      precondition(replacementFailures == 0 && invalidation.pendingDiscovery[peer]?.awaitingServices == true)
      invalidation.peripheral(replacement, didDiscoverServices: failure)
      precondition(replacementFailures == 1 && invalidation.pendingDiscovery[peer] == nil)
    }
  }
}
