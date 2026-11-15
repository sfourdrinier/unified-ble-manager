// ios/Owned/OwnedCoreBluetoothProtocolRadioCancellation.swift

import CoreBluetooth
import Foundation

extension OwnedCoreBluetoothProtocolRadio {
  @objc public func cancelOperation(
    _ operationIdentifier: String,
    completion: @escaping (NSDictionary) -> Void
  ) {
    queue.async {
      if let result = self.cancelConnectionOperation(operationIdentifier, owner: self.delegate ?? self) {
        completion(result); return
      }
      self.cancelPendingOperation(operationIdentifier)
      completion(self.retryCancellationCleanupOnQueue(operationIdentifier))
    }
  }

  func cancelOperation(_ operationIdentifier: String, borrower: OwnedCoreBluetoothProtocolRadioDelegate,
                       completion: @escaping (NSDictionary) -> Void) {
    queue.async {
      if let result = self.cancelConnectionOperation(operationIdentifier, owner: borrower) {
        completion(result); return
      }
      if let entry = self.scopedGattOperations[ObjectIdentifier(borrower)]?[operationIdentifier], entry.owner === borrower {
        self.cancelPendingOperation(entry.physical, scanOwner: borrower, scopedGattPhysical: true)
        let result = self.retryCancellationCleanupOnQueue(entry.physical)
        if self.pendingCancellationCleanup[entry.physical] == nil {
          self.forgetScopedGatt(owner: borrower, operation: operationIdentifier, physical: entry.physical)
        }
        completion(result); return
      }
      let key = ObjectIdentifier(borrower)
      if self.fallbackCancellationKeys[key]?[operationIdentifier] != nil {
        completion(self.retryFallbackCancellation(owner: borrower, operation: operationIdentifier)); return
      }
      let physical = "fallback-cleanup-\(UUID().uuidString)"
      self.fallbackCancellationKeys[key, default: [:]][operationIdentifier] =
        ConnectionCancellation(owner: borrower, physicalOperation: physical)
      self.cancelPendingOperation(operationIdentifier, scanOwner: borrower, cleanupIdentifier: physical)
      completion(self.retryFallbackCancellation(owner: borrower, operation: operationIdentifier))
    }
  }

  func hasActiveGattClaim(owner: AnyObject, peer: String) -> Bool {
    let key = ObjectIdentifier(owner)
    guard let claim = connectionClaims[peer]?[key], claim.owner === owner, claim.pending == nil,
          releasingConnectionClaims[peer]?.contains(key) != true else { return false }
    return !(owner is OwnedCoreBluetoothFencedBorrower) || borrowerRelease.admits(owner)
  }

  func connectionClaimIsCurrent(owner: AnyObject, peer: String, identity: UUID) -> Bool {
    hasActiveGattClaim(owner: owner, peer: peer) &&
      connectionClaims[peer]?[ObjectIdentifier(owner)]?.identity == identity
  }

  func registerScopedGatt(owner: AnyObject, peer: String, operation: String) -> String? {
    let key = ObjectIdentifier(owner)
    pruneFallbackCancellationKeys()
    pruneConnectionCancellationKeys()
    guard hasActiveGattClaim(owner: owner, peer: peer), let claim = connectionClaims[peer]?[key],
          scopedGattOperations[key]?[operation] == nil,
          fallbackCancellationKeys[key]?[operation] == nil,
          connectionCancellationKeys[key]?[operation] == nil else { return nil }
    let physical = "gatt-physical-\(UUID().uuidString)"
    scopedGattOperations[key, default: [:]][operation] = ScopedGattOperation(owner: owner, claim: claim.identity, peer: peer, physical: physical)
    return physical
  }

  func scopedGattClaimIsCurrent(owner: AnyObject, peer: String, operation: String, physical: String) -> Bool {
    let key = ObjectIdentifier(owner)
    guard let entry = scopedGattOperations[key]?[operation], entry.owner === owner, entry.physical == physical,
          entry.peer == peer, let claim = connectionClaims[peer]?[key] else { return false }
    return claim.owner === owner && claim.identity == entry.claim
  }

  func forgetScopedGatt(owner: AnyObject, operation: String, physical: String) {
    let key = ObjectIdentifier(owner)
    guard let entry = scopedGattOperations[key]?[operation], entry.owner === owner, entry.physical == physical else { return }
    scopedGattOperations[key]?.removeValue(forKey: operation)
    if scopedGattOperations[key]?.isEmpty == true { scopedGattOperations.removeValue(forKey: key) }
  }

  func retireScopedGatt(owner: AnyObject, claim: UUID, peer: String) {
    let entries = scopedGattOperations[ObjectIdentifier(owner)]?.filter { $0.value.owner === owner && $0.value.claim == claim && $0.value.peer == peer } ?? [:]
    let failure = error(code: 1020, message: "Connection claim released during GATT operation")
    for (_, entry) in entries {
      for address in Array(pendingRead.keys) {
        guard let lane = pendingRead[address] else { continue }
        let selected = lane.waiting.filter { $0.operationIdentifier == entry.physical }
        guard !selected.isEmpty else { continue }
        if lane.inFlight?.operationIdentifier == entry.physical, readObjects[address] != nil {
          if subscriptions[address] != nil || pendingNotify[address] != nil || resolve(address)?.characteristic.isNotifying == true || hasPendingCancellationCleanup(for: address) {
            readObjects[address]?.unattributable = true
          }
          readObjects[address]?.cancelledOperation = entry.physical
        }
        pendingRead[address]?.cancel { $0.operationIdentifier == entry.physical }
        for waiter in selected { waiter.completion(nil, .readResponse, failure) }
        quarantineCancelledRead(address)
        if pendingRead[address]?.isIdle == true { pendingRead.removeValue(forKey: address); readObjects.removeValue(forKey: address) }
      }
      pendingWrite.retire(operation: entry.physical, failure: failure)
      pendingRssi.retire(operation: entry.physical, failure: (nil, failure))
      descriptorOperations.pendingReads.retire(operation: entry.physical, failure: (nil, failure))
      descriptorOperations.pendingWrites.retire(operation: entry.physical, failure: failure)
    }
  }

  /// Returns nil only when this borrower has no matching connection operation.
  func cancelConnectionOperation(_ operation: String, owner: AnyObject) -> NSDictionary? {
    let key = ObjectIdentifier(owner)
    if let peer = releasingConnectionOperations.first(where: { $0.value[key] == operation })?.key {
      return ["state": "retryable", "failures": [["resource": "notification", "peerIdentifier": peer,
        "code": "cleanup.awaiting-notify-callback"]]] as NSDictionary
    }
    for (peer, claims) in connectionClaims {
      guard let claim = claims[key], claim.owner === owner,
            claim.pending?.operationIdentifier == operation else { continue }
      connectionClaims[peer]?.removeValue(forKey: key)
      if connectionClaims[peer]?.isEmpty == false { return ["state": "released", "failures": []] as NSDictionary }
      connectionClaims.removeValue(forKey: peer)
      guard let physical = pendingConnect[peer] else { return ["state": "released", "failures": []] as NSDictionary }
      cancelPendingOperation(physical.operationIdentifier)
      return recordConnectionCancellation(owner: owner, operation: operation, physical: physical.operationIdentifier)
    }
    for (peer, pendingOwner) in disconnectOwners where pendingOwner.owner === owner && pendingOwner.operation == operation {
      disconnectOwners.removeValue(forKey: peer)
      guard let physical = pendingDisconnect[peer] else { return ["state": "released", "failures": []] as NSDictionary }
      cancelPendingOperation(physical.operationIdentifier)
      return recordConnectionCancellation(owner: owner, operation: operation, physical: physical.operationIdentifier)
    }
    if let recorded = connectionCancellationKeys[key]?[operation], recorded.owner === owner {
      return retryConnectionCancellation(owner: owner, operation: operation)
    }
    return nil
  }

  func recordConnectionCancellation(owner: AnyObject, operation: String, physical: String) -> NSDictionary {
    let result = retryCancellationCleanupOnQueue(physical)
    if pendingCancellationCleanup[physical] != nil {
      connectionCancellationKeys[ObjectIdentifier(owner), default: [:]][operation] =
        ConnectionCancellation(owner: owner, physicalOperation: physical)
    }
    return result
  }

  func retryConnectionCancellation(owner: AnyObject, operation: String) -> NSDictionary {
    let key = ObjectIdentifier(owner)
    if let peer = releasingConnectionOperations.first(where: { $0.value[key] == operation })?.key ??
      disconnectOwners.first(where: { $0.value.owner === owner && $0.value.operation == operation })?.key {
      return ["state": "retryable", "failures": [["resource": "connection", "peerIdentifier": peer,
        "code": "cleanup.awaiting-callback"]]] as NSDictionary
    }
    guard let recorded = connectionCancellationKeys[key]?[operation], recorded.owner === owner else {
      return ["state": "released", "failures": []] as NSDictionary
    }
    let result = retryCancellationCleanupOnQueue(recorded.physicalOperation)
    if pendingCancellationCleanup[recorded.physicalOperation] == nil {
      connectionCancellationKeys[key]?.removeValue(forKey: operation)
      if connectionCancellationKeys[key]?.isEmpty == true { connectionCancellationKeys.removeValue(forKey: key) }
    }
    return result
  }

  func pruneConnectionCancellationKeys() {
    for (key, entries) in connectionCancellationKeys {
      let remaining = entries.filter { pendingCancellationCleanup[$0.value.physicalOperation] != nil }
      connectionCancellationKeys[key] = remaining.isEmpty ? nil : remaining
    }
  }

  func pruneFallbackCancellationKeys() {
    for (key, entries) in fallbackCancellationKeys {
      let remaining = entries.filter { pendingCancellationCleanup[$0.value.physicalOperation] != nil }
      fallbackCancellationKeys[key] = remaining.isEmpty ? nil : remaining
    }
  }

  func retryFallbackCancellation(owner: AnyObject, operation: String) -> NSDictionary {
    let key = ObjectIdentifier(owner)
    guard let entry = fallbackCancellationKeys[key]?[operation], entry.owner === owner else {
      return ["state": "released", "failures": []] as NSDictionary
    }
    let result = retryCancellationCleanupOnQueue(entry.physicalOperation)
    if pendingCancellationCleanup[entry.physicalOperation] == nil {
      fallbackCancellationKeys[key]?.removeValue(forKey: operation)
      if fallbackCancellationKeys[key]?.isEmpty == true { fallbackCancellationKeys.removeValue(forKey: key) }
    }
    return result
  }

  @objc public func retryCancellationCleanup(
    _ operationIdentifier: String,
    completion: @escaping (NSDictionary) -> Void
  ) {
    queue.async {
      let owner: AnyObject = self.delegate ?? self
      let key = ObjectIdentifier(owner)
      if self.connectionCancellationKeys[key]?[operationIdentifier] != nil ||
         self.releasingConnectionOperations.values.contains(where: { $0[key] == operationIdentifier }) ||
         self.disconnectOwners.values.contains(where: { $0.owner === owner && $0.operation == operationIdentifier }) {
        completion(self.retryConnectionCancellation(owner: owner, operation: operationIdentifier)); return
      }
      completion(self.retryCancellationCleanupOnQueue(operationIdentifier))
    }
  }

  func retryCancellationCleanup(_ operationIdentifier: String, borrower: OwnedCoreBluetoothProtocolRadioDelegate,
                                completion: @escaping (NSDictionary) -> Void) {
    queue.async {
      let key = ObjectIdentifier(borrower)
      if self.connectionCancellationKeys[key]?[operationIdentifier] != nil ||
         self.releasingConnectionOperations.values.contains(where: { $0[key] == operationIdentifier }) ||
         self.disconnectOwners.values.contains(where: { $0.owner === borrower && $0.operation == operationIdentifier }) {
        completion(self.retryConnectionCancellation(owner: borrower, operation: operationIdentifier)); return
      }
      if let entry = self.scopedGattOperations[key]?[operationIdentifier], entry.owner === borrower {
        let result = self.retryCancellationCleanupOnQueue(entry.physical)
        if self.pendingCancellationCleanup[entry.physical] == nil {
          self.forgetScopedGatt(owner: borrower, operation: operationIdentifier, physical: entry.physical)
        }
        completion(result); return
      }
      completion(self.retryFallbackCancellation(owner: borrower, operation: operationIdentifier))
    }
  }

  func cancelPendingOperation(_ operationIdentifier: String, scanOwner: AnyObject? = nil, scopedGattPhysical: Bool = false,
                              cleanupIdentifier: String? = nil) {
    radioPreparation.cancel(operationIdentifier, owner: scanOwner, error: error(code: 1020, message: "The radio operation was cancelled"))
    let cleanupKey = cleanupIdentifier ?? operationIdentifier
    var cleanup = pendingCancellationCleanup[cleanupKey] ?? PendingCancellationCleanup()
    cancelScan(owner: scanOwner ?? delegate ?? self, operationIdentifier: operationIdentifier)
    // Borrower connection cancellations resolve their physical operation through
    // cancelConnectionOperation. An unscoped fallback must never match another
    // connection merely because its physical identifier has the same text.
    for (peerIdentifier, pending) in pendingConnect where scanOwner == nil && pending.operationIdentifier == operationIdentifier {
      pendingConnect.removeValue(forKey: peerIdentifier)
      if unfencedConnections.contains(peerIdentifier) {
        cleanup.peerIdentifiers.insert(peerIdentifier)
        if let peripheral = peripheralByIdentifier[peerIdentifier], peripheral.state != .disconnected {
          central?.cancelPeripheralConnection(peripheral)
        }
      }
    }
    let cancelledDisconnects = pendingDisconnect.compactMap { peerIdentifier, pending in
      scanOwner == nil && pending.operationIdentifier == operationIdentifier ? peerIdentifier : nil
    }
    for peerIdentifier in cancelledDisconnects {
      // Cancel the logical waiter, not the physical disconnect callback slot.
      pendingDisconnect[peerIdentifier] = PendingVoid(operationIdentifier: operationIdentifier, completion: { _ in })
      cleanup.peerIdentifiers.insert(peerIdentifier)
    }
    for (peerIdentifier, var pending) in pendingDiscovery where pending.operationIdentifier == operationIdentifier &&
      (scanOwner == nil || pending.owner === scanOwner) {
      pending.cancelled = true
      pendingDiscovery[peerIdentifier] = pending
      cleanup.discoveryPeers.insert(peerIdentifier)
    }
    for address in pendingRead.keys where scanOwner == nil || scopedGattPhysical {
      if pendingRead[address]?.inFlight?.operationIdentifier == operationIdentifier,
         readObjects[address] != nil {
        if subscriptions[address] != nil || pendingNotify[address] != nil ||
           resolve(address)?.characteristic.isNotifying == true || hasPendingCancellationCleanup(for: address) {
          readObjects[address]?.unattributable = true
        }
        readObjects[address]?.cancelledOperation = operationIdentifier
        cleanup.callbackOutstanding = true
      }
      pendingRead[address]?.cancel { $0.operationIdentifier == operationIdentifier }
      quarantineCancelledRead(address)
      if pendingRead[address]?.isIdle == true {
        pendingRead.removeValue(forKey: address)
        readObjects.removeValue(forKey: address)
      }
    }
    let ownsGattOperation = scanOwner == nil || scopedGattPhysical
    let rssiCancelled = ownsGattOperation && pendingRssi.cancel(operationIdentifier)
    let writeCancelled = ownsGattOperation && pendingWrite.cancel(operationIdentifier)
    let descriptorCancelled = ownsGattOperation && descriptorOperations.cancel(operationIdentifier)
    if rssiCancelled || writeCancelled || descriptorCancelled { cleanup.callbackOutstanding = true }
    func matchesNotification(_ pending: PendingNotify, address: CharacteristicAddress) -> Bool {
      guard pending.operationIdentifier == operationIdentifier else { return false }
      guard let scanOwner else { return true }
      guard pending.borrower === scanOwner else { return false }
      if let expected = pending.claimIdentity,
         let current = connectionClaims[address.peerIdentifier]?[ObjectIdentifier(scanOwner)],
         current.identity != expected { return false }
      return true
    }
    for (address, joins) in pendingNotifyJoins {
      let remaining = joins.filter { !matchesNotification($0.value, address: address) }
      pendingNotifyJoins[address] = remaining.isEmpty ? nil : remaining
    }
    for (address, pending) in pendingNotify where matchesNotification(pending, address: address) {
      if pending.enabled, var joins = pendingNotifyJoins[address],
         let replacementKey = joins.keys.sorted().first, let replacement = joins.removeValue(forKey: replacementKey) {
        pendingNotify[address] = replacement
        pendingNotifyJoins[address] = joins.isEmpty ? nil : joins
        continue
      }
      pendingNotify.removeValue(forKey: address)
      // `subscriptions` remains installed while an unsubscribe is pending.  Its
      // cancellation therefore restores CCCD rather than disabling it.
      cleanup.notificationDesiredStates[address] = pending.enabled ? false : true
      cleanup.notificationAwaitingCallbacks.insert(address)
      // Wait for the original transition's callback before issuing compensation.
      // Otherwise callbacks cannot be attributed to the operation they settle.
    }
    if cleanup.hasWork {
      pendingCancellationCleanup[cleanupKey] = cleanup
      scheduleCancellationCleanupRetry(cleanupKey)
    }
  }

  func retryCancellationCleanupOnQueue(_ operationIdentifier: String) -> NSDictionary {
    defer { pruneFallbackCancellationKeys() }
    guard var cleanup = pendingCancellationCleanup[operationIdentifier] else {
      return ["state": "released", "failures": []] as NSDictionary
    }
    var failures = [NSDictionary]()
    if cleanup.callbackOutstanding {
      if hasCallbackObligation(operationIdentifier) {
        failures.append(["resource": "gatt-callback", "code": "cleanup.awaiting-callback"] as NSDictionary)
      } else {
        cleanup.callbackOutstanding = false
      }
    }
    for object in Array(cleanup.retiredNotificationObjects) {
      guard let owner = retiredNotificationOwners[object] else {
        cleanup.retiredNotificationObjects.remove(object)
        continue
      }
      retryRetiredNotification(owner)
      failures.append(["resource": "notification", "peerIdentifier": owner.address.peerIdentifier,
        "code": "cleanup.pending"] as NSDictionary)
    }
    for peerIdentifier in Array(cleanup.peerIdentifiers) {
      if let peripheral = peripheralByIdentifier[peerIdentifier], peripheral.state != .disconnected {
        central?.cancelPeripheralConnection(peripheral)
      }
      failures.append(["resource": "connection", "peerIdentifier": peerIdentifier, "code": "cleanup.awaiting-callback"] as NSDictionary)
    }
    for (address, desiredState) in Array(cleanup.notificationDesiredStates) {
      guard !invalidatedNotificationCleanup.contains(address) else {
        failures.append([
          "resource": "notification", "peerIdentifier": address.peerIdentifier,
          "code": "cleanup.path-unresolved"
        ] as NSDictionary)
        continue
      }
      guard let resolved = resolve(address) else {
        // Discovery can be refreshed while CoreBluetooth still owns the old
        // characteristic.  Retain this exact cleanup owner until disconnect
        // proves the peer is gone; otherwise a cancelled CCCD transition could
        // become permanently unreachable.
        failures.append([
          "resource": "notification",
          "peerIdentifier": address.peerIdentifier,
          "code": "cleanup.path-unresolved"
        ] as NSDictionary)
        continue
      }
      if cleanup.notificationAwaitingCallbacks.contains(address) {
        // `isNotifying` may still report the pre-cancellation value until the
        // underlying callback settles.  Preserve ownership and retry instead of
        // falsely releasing an operation that can still change CCCD afterwards.
        failures.append([
          "resource": "notification",
          "peerIdentifier": address.peerIdentifier,
          "code": "cleanup.awaiting-notify-callback"
        ] as NSDictionary)
        continue
      }
      if resolved.characteristic.isNotifying == desiredState {
        cleanup.notificationDesiredStates.removeValue(forKey: address)
      } else {
        _ = writeNotificationState(desiredState, for: address)
        cleanup.notificationAwaitingCallbacks.insert(address)
        failures.append([
          "resource": "notification",
          "peerIdentifier": address.peerIdentifier,
          "code": "cleanup.pending"
        ] as NSDictionary)
      }
    }
    for peerIdentifier in cleanup.discoveryPeers {
      if pendingDiscovery[peerIdentifier]?.operationIdentifier != operationIdentifier {
        cleanup.discoveryPeers.remove(peerIdentifier)
      } else {
        failures.append(["resource": "discovery", "peerIdentifier": peerIdentifier,
          "code": "cleanup.awaiting-discovery-callback"] as NSDictionary)
      }
    }
    if !cleanup.hasWork {
      pendingCancellationCleanup.removeValue(forKey: operationIdentifier)
      return ["state": "released", "failures": []] as NSDictionary
    }
    pendingCancellationCleanup[operationIdentifier] = cleanup
    scheduleCancellationCleanupRetry(operationIdentifier)
    return ["state": "retryable", "failures": failures] as NSDictionary
  }

  func hasCallbackObligation(_ operationIdentifier: String) -> Bool {
    pendingRssi.hasOutstanding(operationIdentifier) || pendingWrite.hasOutstanding(operationIdentifier) ||
      descriptorOperations.pendingReads.hasOutstanding(operationIdentifier) ||
      descriptorOperations.pendingWrites.hasOutstanding(operationIdentifier) ||
      readObjects.values.contains { $0.cancelledOperation == operationIdentifier } ||
      retiredReads.contains { $0.cancelledOperation == operationIdentifier }
  }

  func quarantineCancelledRead(_ address: CharacteristicAddress) {
    guard readObjects[address]?.cancelledOperation != nil,
          readObjects[address]?.unattributable == true,
          var lane = pendingRead[address] else { return }
    let stranded = lane.waiting
    lane.cancel { _ in true }
    pendingRead[address] = lane
    let failure = error(code: 1010, message: "A cancelled read response cannot be distinguished from a notification")
    for pending in stranded { pending.completion(nil, .readOrNotification, failure) }
  }

  func refreshCallbackCancellationCleanup(_ operationIdentifier: String) {
    guard !hasCallbackObligation(operationIdentifier),
          var cleanup = pendingCancellationCleanup[operationIdentifier], cleanup.callbackOutstanding else { return }
    cleanup.callbackOutstanding = false
    if cleanup.hasWork { pendingCancellationCleanup[operationIdentifier] = cleanup }
    else {
      pendingCancellationCleanup.removeValue(forKey: operationIdentifier)
      pendingCancellationCleanupRetryScheduled.remove(operationIdentifier)
    }
  }

  func clearDiscoveryCancellationCleanup(forPeerIdentifier peerIdentifier: String) {
    for (operationIdentifier, var cleanup) in pendingCancellationCleanup {
      cleanup.discoveryPeers.remove(peerIdentifier)
      if !cleanup.hasWork {
        pendingCancellationCleanup.removeValue(forKey: operationIdentifier)
        pendingCancellationCleanupRetryScheduled.remove(operationIdentifier)
      } else { pendingCancellationCleanup[operationIdentifier] = cleanup }
    }
    pruneFallbackCancellationKeys()
  }

  func clearCancellationCleanup(forPeerIdentifier peerIdentifier: String) {
    let retired = Set(retiredNotificationOwners.filter { $0.value.address.peerIdentifier == peerIdentifier }.map(\.key))
    retiredNotificationOwners = retiredNotificationOwners.filter { $0.value.address.peerIdentifier != peerIdentifier }
    invalidatedNotificationCleanup = invalidatedNotificationCleanup.filter { $0.peerIdentifier != peerIdentifier }
    for (operationIdentifier, var cleanup) in pendingCancellationCleanup {
      cleanup.peerIdentifiers.remove(peerIdentifier)
      cleanup.retiredNotificationObjects.subtract(retired)
      // A disconnection is the only terminal proof that CoreBluetooth no
      // longer owns this peer's CCCD state.  It releases both connection and
      // notification cleanup entries for that peer, including entries whose
      // discovery cache was refreshed before their callback arrived.
      cleanup.notificationDesiredStates = cleanup.notificationDesiredStates.filter {
        $0.key.peerIdentifier != peerIdentifier
      }
      cleanup.notificationAwaitingCallbacks = cleanup.notificationAwaitingCallbacks.filter {
        $0.peerIdentifier != peerIdentifier
      }
      if !cleanup.hasWork {
        pendingCancellationCleanup.removeValue(forKey: operationIdentifier)
        pendingCancellationCleanupRetryScheduled.remove(operationIdentifier)
      } else {
        pendingCancellationCleanup[operationIdentifier] = cleanup
      }
    }
    pruneConnectionCancellationKeys()
    pruneFallbackCancellationKeys()
  }

  func clearCancellationCleanup(forNotificationAddress address: CharacteristicAddress) {
    for (operationIdentifier, var cleanup) in pendingCancellationCleanup {
      cleanup.notificationDesiredStates.removeValue(forKey: address)
      cleanup.notificationAwaitingCallbacks.remove(address)
      if !cleanup.hasWork {
        pendingCancellationCleanup.removeValue(forKey: operationIdentifier)
        pendingCancellationCleanupRetryScheduled.remove(operationIdentifier)
      } else {
        pendingCancellationCleanup[operationIdentifier] = cleanup
      }
    }
    pruneFallbackCancellationKeys()
  }

  func clearRetiredNotificationCleanup(_ object: ObjectIdentifier) {
    for (operation, var cleanup) in pendingCancellationCleanup {
      cleanup.retiredNotificationObjects.remove(object)
      if !cleanup.hasWork {
        pendingCancellationCleanup.removeValue(forKey: operation)
        pendingCancellationCleanupRetryScheduled.remove(operation)
      } else { pendingCancellationCleanup[operation] = cleanup }
    }
    pruneFallbackCancellationKeys()
  }

  func transferNotificationCleanup(address: CharacteristicAddress, toRetiredObject object: ObjectIdentifier) {
    for (operation, var cleanup) in pendingCancellationCleanup {
      guard cleanup.notificationDesiredStates.removeValue(forKey: address) != nil else { continue }
      cleanup.notificationAwaitingCallbacks.remove(address)
      cleanup.retiredNotificationObjects.insert(object)
      pendingCancellationCleanup[operation] = cleanup
    }
  }

  func cancellationDesiredState(forNotificationAddress address: CharacteristicAddress) -> Bool? {
    for cleanup in pendingCancellationCleanup.values {
      if let desiredState = cleanup.notificationDesiredStates[address] {
        return desiredState
      }
    }
    return nil
  }

  func markCancellationNotificationCallbackReceived(for address: CharacteristicAddress) {
    for (operationIdentifier, var cleanup) in pendingCancellationCleanup {
      guard cleanup.notificationDesiredStates[address] != nil else { continue }
      cleanup.notificationAwaitingCallbacks.remove(address)
      pendingCancellationCleanup[operationIdentifier] = cleanup
    }
  }

  func resolvedNotifyStateNeedsReconciliation(
    _ address: CharacteristicAddress,
    desiredState: Bool
  ) {
    guard writeNotificationState(desiredState, for: address) else { return }
    for (operationIdentifier, var cleanup) in pendingCancellationCleanup {
      guard cleanup.notificationDesiredStates[address] != nil else { continue }
      cleanup.notificationAwaitingCallbacks.insert(address)
      pendingCancellationCleanup[operationIdentifier] = cleanup
      scheduleCancellationCleanupRetry(operationIdentifier)
    }
  }

  private func scheduleCancellationCleanupRetry(_ operationIdentifier: String) {
    // Callback-only and invalidated debts stay protocol-reachable without an
    // endless polling timer. Only schedule work that can actually be retried.
    guard let cleanup = pendingCancellationCleanup[operationIdentifier] else { return }
    let retryableNotification = cleanup.notificationDesiredStates.keys.contains { address in
      !cleanup.notificationAwaitingCallbacks.contains(address) &&
        !invalidatedNotificationCleanup.contains(address) && resolve(address) != nil
    }
    let retryableRetired = cleanup.retiredNotificationObjects.contains { object in
      guard let owner = retiredNotificationOwners[object] else { return false }
      if case .stalled = owner.phase { return true }
      return false
    }
    guard retryableNotification || retryableRetired,
          pendingCancellationCleanupRetryScheduled.insert(operationIdentifier).inserted else { return }
    queue.asyncAfter(deadline: .now() + .milliseconds(250)) { [weak self] in
      guard let self, !self.destroyed,
            self.pendingCancellationCleanup[operationIdentifier] != nil else {
        self?.pendingCancellationCleanupRetryScheduled.remove(operationIdentifier)
        return
      }
      self.pendingCancellationCleanupRetryScheduled.remove(operationIdentifier)
      _ = self.retryCancellationCleanupOnQueue(operationIdentifier)
    }
  }

  func disableActiveNotifications() {
    for address in subscriptions.keys {
      if let resolved = resolve(address) {
        resolved.peripheral.setNotifyValue(false, for: resolved.characteristic)
      }
    }
  }

  func completeAfterPendingDisconnects(
    preserving restoredIdentifiers: Set<String>,
    destroyRadio: Bool,
    completion: @escaping (NSError?) -> Void
  ) {
    final class DisconnectWaiter {
      var remaining: Int
      var finished = false
      let finish: (NSError?) -> Void
      init(remaining: Int, finish: @escaping (NSError?) -> Void) {
        self.remaining = remaining
        self.finish = finish
      }
      func decrement() {
        remaining -= 1
        if remaining <= 0 { complete(nil) }
      }
      func complete(_ error: NSError?) {
        guard !finished else { return }
        finished = true
        finish(error)
      }
    }

    let waited: [(identifier: String, peripheral: CBPeripheral)] = peripheralByIdentifier.compactMap { identifier, peripheral in
      guard !restoredIdentifiers.contains(identifier) else { return nil }
      if unfencedConnections.contains(identifier) || peripheral.state != .disconnected {
        return (identifier, peripheral)
      }
      peripheral.delegate = nil
      return nil
    }
    let waiter = DisconnectWaiter(remaining: waited.count) { [weak self] error in
      guard let self else {
        completion(error)
        return
      }
      if error != nil && !destroyRadio {
        completion(error)
        return
      }
      for (identifier, peripheral) in self.peripheralByIdentifier where !restoredIdentifiers.contains(identifier) {
        peripheral.delegate = nil
      }
      if destroyRadio {
        for peripheral in self.peripheralByIdentifier.values {
          peripheral.delegate = nil
        }
        self.peripheralByIdentifier.removeAll()
        self.servicesByPeer.removeAll()
        self.restoredPeerIdentifiers.removeAll()
        self.central?.delegate = nil
      } else {
        self.peripheralByIdentifier = self.peripheralByIdentifier.filter { restoredIdentifiers.contains($0.key) }
        self.servicesByPeer = self.servicesByPeer.filter { restoredIdentifiers.contains($0.key) }
      }
      completion(error)
    }
    if waited.isEmpty {
      waiter.complete(nil)
      return
    }
    queue.asyncAfter(deadline: .now() + .seconds(5)) { [weak self] in
      guard let self else { return }
      waiter.complete(self.error(code: 1026, message: "CoreBluetooth disconnect confirmation timed out"))
    }
    for entry in waited {
      let previous = pendingDisconnect[entry.identifier]
      pendingDisconnect[entry.identifier] = PendingVoid(operationIdentifier: "teardown-\(entry.identifier)") { error in
        previous?.completion(error)
        waiter.decrement()
      }
      if entry.peripheral.state != .disconnected { central?.cancelPeripheralConnection(entry.peripheral) }
    }
  }

  func hasPendingCancellationCleanup(for address: CharacteristicAddress) -> Bool {
    pendingCancellationCleanup.values.contains { cleanup in
      cleanup.notificationDesiredStates[address] != nil || cleanup.notificationAwaitingCallbacks.contains(address)
    }
  }

  /// One `didUpdateValueFor`: completes the oldest read of the characteristic
  /// delivers a value that may be a notification to the subscription. A
  /// potentially ambiguous completion keeps the physical read fenced.
  func handleCharacteristicValueUpdate(
    address: CharacteristicAddress,
    peripheral: AnyObject,
    characteristic: CBCharacteristic,
    error: Error?,
    snapshotValue: Data? = nil,
    snapshotIsNotifying: Bool? = nil,
    hasSnapshot: Bool = false
  ) {
    if consumeRetiredReadCallback(peripheral: peripheral, characteristic: characteristic) { return }
    let callbackValue = hasSnapshot ? snapshotValue : characteristic.value
    let isNotifying = snapshotIsNotifying ?? characteristic.isNotifying
    let pendingCleanup = hasPendingCancellationCleanup(for: address)
    if isNotifying || subscriptions[address] != nil || pendingNotify[address] != nil || pendingCleanup {
      readObjects[address]?.unattributable = true
    }
    quarantineCancelledRead(address)
    if let failure = error as NSError?,
       subscriptions[address] != nil || pendingNotify[address]?.enabled == true ||
         readObjects[address]?.unattributable == true {
      reportNotificationFailure(address: address, error: failure)
    }
    // Admit on the delegate queue before read completion or subscription fan-out.
    // CoreBluetooth does not distinguish a read response while a read is pending.
    if error == nil, let value = callbackValue {
      admitOrderedCharacteristicValue(address: address, value: value)
    }
    let currentProvenance = OwnedCoreBluetoothReadNotifyProvenance.readProvenance(
      isNotifying: isNotifying,
      hasInstalledSubscription: subscriptions[address] != nil,
      pendingNotifyChange: pendingNotify[address] != nil,
      pendingCancellationCleanup: pendingCleanup
    )
    let provenance: OwnedCoreBluetoothReadProvenance = readObjects[address]?.unattributable == true
      ? .readOrNotification : currentProvenance
    if var lane = pendingRead[address],
       readObjects[address]?.matches(peripheral: peripheral, object: characteristic) == true {
      if readObjects[address]?.ambiguousCompletion == true {
        // No subsequent value can identify which event answered the old read.
      } else if readObjects[address]?.unattributable == true &&
                  readObjects[address]?.cancelledOperation == nil {
        let completed = lane.answerAmbiguously()
        let stranded = lane.waiting
        lane.cancel { _ in true }
        pendingRead[address] = lane
        readObjects[address]?.ambiguousCompletion = true
        completed?.completion(error == nil ? callbackValue as NSData? : nil, .readOrNotification, error as NSError?)
        let failure = self.error(code: 1010, message: "A previous read response cannot be distinguished from notifications")
        for pending in stranded { pending.completion(nil, .readOrNotification, failure) }
      } else if !(readObjects[address]?.cancelledOperation != nil && readObjects[address]?.unattributable == true) {
        let cancelledOperation = readObjects[address]?.cancelledOperation
        let (completed, issueNext) = lane.answer()
        pendingRead[address] = lane.isIdle ? nil : lane
        if lane.isIdle { readObjects.removeValue(forKey: address) }
        if cancelledOperation != nil { readObjects[address]?.cancelledOperation = nil }
        completed?.completion(error == nil ? callbackValue as NSData? : nil, provenance, error as NSError?)
        if let cancelledOperation { refreshCallbackCancellationCleanup(cancelledOperation) }
        if issueNext { issueNextRead(address) }
      }
    }
    guard OwnedCoreBluetoothReadNotifyProvenance.deliversNotification(
      hasInstalledSubscription: subscriptions[address] != nil,
      pendingNotifyEnable: pendingNotify[address]?.enabled == true,
      pendingCancellationCleanup: pendingCleanup,
      hasError: error != nil,
      hasValue: callbackValue != nil
    ) else { return }
    let pendingSubscriptionIdentifier = pendingNotify[address].flatMap { pending in
      pending.enabled ? pending.subscriptionIdentifier : nil
    }
    guard let subscriptionIdentifier = subscriptions[address] ?? pendingSubscriptionIdentifier,
          let value = callbackValue else { return }
    notificationRoute(subscriptionIdentifier)?.protocolRadioDidReceiveNotification(subscriptionIdentifier, value: value as NSData)
    for owner in (sharedSubscriptions[address] ?? []).sorted() {
      notificationRoute(owner)?.protocolRadioDidReceiveNotification(owner, value: value as NSData)
    }
    for owner in (pendingNotifyJoins[address]?.keys.map { $0 } ?? []).sorted() {
      notificationRoute(owner)?.protocolRadioDidReceiveNotification(owner, value: value as NSData)
    }
  }

  /// Issues the next queued read of a characteristic; when the path no longer
  /// resolves, every read still queued on it fails as stale instead of waiting.
  private func issueNextRead(_ address: CharacteristicAddress) {
    if let resolved = resolve(address),
       readObjects[address]?.matches(peripheral: resolved.peripheral, object: resolved.characteristic) == true {
      resolved.peripheral.readValue(for: resolved.characteristic)
      return
    }
    let stale = error(code: 1010, message: "The generation-bound characteristic path is stale")
    for pending in pendingRead.removeValue(forKey: address)?.waiting ?? [] {
      pending.completion(nil, .readResponse, stale)
    }
    readObjects.removeValue(forKey: address)
  }

  func consumeRetiredReadCallback(peripheral: AnyObject, characteristic: AnyObject) -> Bool {
    guard let index = retiredReads.firstIndex(where: { $0.matches(peripheral: peripheral, object: characteristic) }) else {
      return false
    }
    if retiredReads[index].unattributable { return true }
    let owner = retiredReads.remove(at: index)
    if let operation = owner.cancelledOperation { refreshCallbackCancellationCleanup(operation) }
    return true
  }
}
