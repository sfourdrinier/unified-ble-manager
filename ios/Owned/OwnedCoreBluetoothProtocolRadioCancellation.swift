// ios/Owned/OwnedCoreBluetoothProtocolRadioCancellation.swift

import CoreBluetooth
import Foundation

extension OwnedCoreBluetoothProtocolRadio {
  @objc public func cancelOperation(
    _ operationIdentifier: String,
    completion: @escaping (NSDictionary) -> Void
  ) {
    queue.async {
      self.cancelPendingOperation(operationIdentifier)
      completion(self.retryCancellationCleanupOnQueue(operationIdentifier))
    }
  }

  @objc public func retryCancellationCleanup(
    _ operationIdentifier: String,
    completion: @escaping (NSDictionary) -> Void
  ) {
    queue.async {
      completion(self.retryCancellationCleanupOnQueue(operationIdentifier))
    }
  }

  private func cancelPendingOperation(_ operationIdentifier: String) {
    var cleanup = pendingCancellationCleanup[operationIdentifier] ?? PendingCancellationCleanup()
    if activeScanOperationIdentifier == operationIdentifier {
      central.stopScan()
      activeScanOperationIdentifier = nil
    }
    for (peerIdentifier, pending) in pendingConnect where pending.operationIdentifier == operationIdentifier {
      pendingConnect.removeValue(forKey: peerIdentifier)
      if let peripheral = peripheralByIdentifier[peerIdentifier], peripheral.state != .disconnected {
        cleanup.peerIdentifiers.insert(peerIdentifier)
        central.cancelPeripheralConnection(peripheral)
      }
    }
    let cancelledDisconnects = pendingDisconnect.compactMap { peerIdentifier, pending in
      pending.operationIdentifier == operationIdentifier ? peerIdentifier : nil
    }
    for peerIdentifier in cancelledDisconnects {
      pendingDisconnect.removeValue(forKey: peerIdentifier)
      if let peripheral = peripheralByIdentifier[peerIdentifier], peripheral.state != .disconnected {
        cleanup.peerIdentifiers.insert(peerIdentifier)
      }
    }
    pendingDiscovery = pendingDiscovery.filter { $0.value.operationIdentifier != operationIdentifier }
    pendingRead = pendingRead.filter { $0.value.operationIdentifier != operationIdentifier }
    pendingRssi = pendingRssi.filter { $0.value.operationIdentifier != operationIdentifier }
    pendingWrite = pendingWrite.filter { $0.value.operationIdentifier != operationIdentifier }
    descriptorOperations.cancel(operationIdentifier)
    for (address, pending) in pendingNotify where pending.operationIdentifier == operationIdentifier {
      pendingNotify.removeValue(forKey: address)
      // `subscriptions` remains installed while an unsubscribe is pending.  Its
      // cancellation therefore restores CCCD rather than disabling it.
      cleanup.notificationDesiredStates[address] = pending.enabled ? false : true
      cleanup.notificationAwaitingCallbacks.insert(address)
      if let resolved = resolve(address) {
        resolved.peripheral.setNotifyValue(pending.enabled ? false : true, for: resolved.characteristic)
      }
    }
    if !cleanup.peerIdentifiers.isEmpty || !cleanup.notificationDesiredStates.isEmpty {
      pendingCancellationCleanup[operationIdentifier] = cleanup
      scheduleCancellationCleanupRetry(operationIdentifier)
    }
  }

  private func retryCancellationCleanupOnQueue(_ operationIdentifier: String) -> NSDictionary {
    guard var cleanup = pendingCancellationCleanup[operationIdentifier] else {
      return ["state": "released", "failures": []] as NSDictionary
    }
    var failures = [NSDictionary]()
    for peerIdentifier in Array(cleanup.peerIdentifiers) {
      guard let peripheral = peripheralByIdentifier[peerIdentifier] else {
        cleanup.peerIdentifiers.remove(peerIdentifier)
        continue
      }
      if peripheral.state == .disconnected {
        cleanup.peerIdentifiers.remove(peerIdentifier)
      } else {
        central.cancelPeripheralConnection(peripheral)
        failures.append(["resource": "connection", "peerIdentifier": peerIdentifier, "code": "cleanup.pending"] as NSDictionary)
      }
    }
    for (address, desiredState) in Array(cleanup.notificationDesiredStates) {
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
        resolved.peripheral.setNotifyValue(desiredState, for: resolved.characteristic)
        cleanup.notificationAwaitingCallbacks.insert(address)
        failures.append([
          "resource": "notification",
          "peerIdentifier": address.peerIdentifier,
          "code": "cleanup.pending"
        ] as NSDictionary)
      }
    }
    if cleanup.peerIdentifiers.isEmpty && cleanup.notificationDesiredStates.isEmpty {
      pendingCancellationCleanup.removeValue(forKey: operationIdentifier)
      return ["state": "released", "failures": []] as NSDictionary
    }
    pendingCancellationCleanup[operationIdentifier] = cleanup
    scheduleCancellationCleanupRetry(operationIdentifier)
    return ["state": "retryable", "failures": failures] as NSDictionary
  }

  func clearCancellationCleanup(forPeerIdentifier peerIdentifier: String) {
    for (operationIdentifier, var cleanup) in pendingCancellationCleanup {
      cleanup.peerIdentifiers.remove(peerIdentifier)
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
      if cleanup.peerIdentifiers.isEmpty && cleanup.notificationDesiredStates.isEmpty {
        pendingCancellationCleanup.removeValue(forKey: operationIdentifier)
        pendingCancellationCleanupRetryScheduled.remove(operationIdentifier)
      } else {
        pendingCancellationCleanup[operationIdentifier] = cleanup
      }
    }
  }

  func clearCancellationCleanup(forNotificationAddress address: CharacteristicAddress) {
    for (operationIdentifier, var cleanup) in pendingCancellationCleanup {
      cleanup.notificationDesiredStates.removeValue(forKey: address)
      cleanup.notificationAwaitingCallbacks.remove(address)
      if cleanup.peerIdentifiers.isEmpty && cleanup.notificationDesiredStates.isEmpty {
        pendingCancellationCleanup.removeValue(forKey: operationIdentifier)
        pendingCancellationCleanupRetryScheduled.remove(operationIdentifier)
      } else {
        pendingCancellationCleanup[operationIdentifier] = cleanup
      }
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
    guard let resolved = resolve(address) else { return }
    resolved.peripheral.setNotifyValue(desiredState, for: resolved.characteristic)
    for (operationIdentifier, var cleanup) in pendingCancellationCleanup {
      guard cleanup.notificationDesiredStates[address] != nil else { continue }
      cleanup.notificationAwaitingCallbacks.insert(address)
      pendingCancellationCleanup[operationIdentifier] = cleanup
      scheduleCancellationCleanupRetry(operationIdentifier)
    }
  }

  private func scheduleCancellationCleanupRetry(_ operationIdentifier: String) {
    // Queue-confined retained ownership is the automatic retry owner.  It stays
    // protocol-reachable through retryCancellationCleanup until CoreBluetooth's
    // physical callback confirms the required CCCD state.
    guard pendingCancellationCleanupRetryScheduled.insert(operationIdentifier).inserted else { return }
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
      if peripheral.state == .connected || peripheral.state == .connecting || peripheral.state == .disconnecting {
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
        self.central.delegate = nil
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
      if entry.peripheral.state == .disconnected {
        pendingDisconnect.removeValue(forKey: entry.identifier)?.completion(nil)
        continue
      }
      central.cancelPeripheralConnection(entry.peripheral)
      if entry.peripheral.state == .disconnected {
        pendingDisconnect.removeValue(forKey: entry.identifier)?.completion(nil)
      }
    }
  }
}
