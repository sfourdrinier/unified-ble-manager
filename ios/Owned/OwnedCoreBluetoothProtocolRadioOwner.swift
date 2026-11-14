// ios/Owned/OwnedCoreBluetoothProtocolRadioOwner.swift

import Foundation
import CoreBluetooth


/// Implemented by borrowers whose native owner has a cross-queue fence.
/// The supplied closure only mutates queue-confined borrower/delegate state.
protocol OwnedCoreBluetoothFencedBorrower: AnyObject {
  func admitRadioBorrower(_ attach: () -> Bool) -> Bool
  func didReleaseRadioBorrower()
}

/** Queue-confined attachment/release state, split from CoreBluetooth for executable race tests. */
final class OwnedCoreBluetoothBorrowerReleaseCoordinator {
  typealias Completion = (NSError?) -> Void

  enum ReleaseAdmission {
    case noBorrower
    case joined
    case start(UUID)
  }

  private struct Borrower {
    // Keep the identity alive until its cleanup succeeds, even when a caller
    // has dropped its last reference while a native callback is outstanding.
    let owner: AnyObject
    var ticket: UUID?
    var retiring = false
    var completions: [Completion] = []
  }
  private var borrowers: [ObjectIdentifier: Borrower] = [:]

  var releaseInProgress: Bool { borrowers.values.contains { $0.ticket != nil } }
  var borrowerCount: Int { borrowers.count }

  func admits(_ candidate: AnyObject) -> Bool {
    guard let borrower = borrowers[ObjectIdentifier(candidate)], borrower.owner === candidate else { return false }
    return !borrower.retiring
  }

  func attach(_ candidate: AnyObject) -> Bool {
    let identity = ObjectIdentifier(candidate)
    guard borrowers[identity] == nil else { return false }
    borrowers[identity] = Borrower(owner: candidate)
    return true
  }

  func beginRelease(_ candidate: AnyObject, completion: @escaping Completion) -> ReleaseAdmission {
    let identity = ObjectIdentifier(candidate)
    guard var borrower = borrowers[identity], borrower.owner === candidate else { return .noBorrower }
    borrower.completions.append(completion)
    if borrower.ticket != nil {
      borrowers[identity] = borrower
      return .joined
    }
    let ticket = UUID()
    borrower.retiring = true
    borrower.ticket = ticket
    borrowers[identity] = borrower
    return .start(ticket)
  }

  func finish(_ candidate: AnyObject, ticket: UUID, error: NSError?) -> [Completion]? {
    let identity = ObjectIdentifier(candidate)
    guard var borrower = borrowers[identity], borrower.owner === candidate,
          borrower.ticket == ticket else { return nil }
    let pending = borrower.completions
    if error == nil {
      borrowers.removeValue(forKey: identity)
    } else {
      borrower.ticket = nil
      borrower.completions.removeAll()
      borrowers[identity] = borrower
    }
    return pending
  }
}

extension OwnedCoreBluetoothProtocolRadio {
  /// Explicit borrower entrypoints are internal until all radio resources have
  /// borrower-scoped teardown. The public route binds to the attached owner.
  func startScan(borrower: OwnedCoreBluetoothProtocolRadioDelegate, serviceUUIDs: [String],
                 allowDuplicates: Bool, operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    queue.async {
      if borrower is OwnedCoreBluetoothFencedBorrower && !self.borrowerRelease.admits(borrower) {
        completion(self.error(code: 1014, message: "Scan borrower is retiring or detached")); return
      }
      self.startScanOnQueue(owner: borrower, route: borrower, serviceUUIDs: serviceUUIDs,
        allowDuplicates: allowDuplicates, operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  func startScanOnQueue(owner: AnyObject, route: OwnedCoreBluetoothProtocolRadioDelegate?, serviceUUIDs: [String],
                        allowDuplicates: Bool, operationIdentifier: String, completion: (NSError?) -> Void) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard requireUsable(completion) else { return }
    let identity = ObjectIdentifier(owner)
    guard pendingScanLosses[identity] == nil, !scanDeliveringOwners.contains(identity) else {
      completion(error(code: 1014, message: "Previous scan callback or loss delivery is pending")); return
    }
    guard scanLeases[identity] == nil else {
      completion(error(code: 1001, message: "A Native Protocol v2 scan is already active")); return
    }
    let reservedOwners = Set(scanLeases.keys).union(pendingScanLosses.keys).union(scanDeliveringOwners)
    guard reservedOwners.count < Self.scanOwnerLimit else {
      completion(error(code: 1014, message: "Scan owner control capacity exhausted")); return
    }
    guard let parsed = OwnedCoreBluetoothProtocolRadioSupport.parseUUIDs(serviceUUIDs) else {
      completion(error(code: 1002, message: "A scan service UUID is invalid")); return
    }
    guard let central = centralForUse(or: completion) else { return }
    guard central.state == .poweredOn else {
      completion(error(code: 1003, message: "CoreBluetooth is not powered on")); return
    }
    scanLeases[identity] = ScanLease(owner: owner, route: route, operationIdentifier: operationIdentifier,
      generation: UUID(), services: Set(parsed.map { OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID($0.uuidString) }),
      allowDuplicates: allowDuplicates)
    reconcileScan()
    completion(nil)
  }

  func updateScan(borrower: OwnedCoreBluetoothProtocolRadioDelegate, serviceUUIDs: [String],
                  allowDuplicates: Bool, completion: @escaping (NSError?) -> Void) {
    queue.async {
      guard self.requireUsable(completion) else { return }
      let key = ObjectIdentifier(borrower)
      self.drainScanCallbacks()
      guard self.pendingScanLosses[key] == nil, !self.scanDeliveringOwners.contains(key) else {
        completion(self.error(code: 1014, message: "Previous scan loss delivery is pending")); return
      }
      guard let lease = self.scanLeases[key] else {
        completion(self.error(code: 1004, message: "No Native Protocol v2 scan is active")); return
      }
      guard let parsed = OwnedCoreBluetoothProtocolRadioSupport.parseUUIDs(serviceUUIDs) else {
        completion(self.error(code: 1002, message: "A scan service UUID is invalid")); return
      }
      guard let central = self.centralForUse(or: completion) else { return }
      guard central.state == .poweredOn else {
        completion(self.error(code: 1003, message: "CoreBluetooth is not powered on")); return
      }
      self.scanLeases[key] = ScanLease(owner: lease.owner, route: lease.route,
        operationIdentifier: lease.operationIdentifier, generation: UUID(),
        services: Set(parsed.map { OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID($0.uuidString) }),
        allowDuplicates: allowDuplicates)
      self.reconcileScan()
      completion(nil)
    }
  }

  func stopScan(borrower: OwnedCoreBluetoothProtocolRadioDelegate, completion: @escaping (NSError?) -> Void) {
    queue.async {
      guard self.requireUsable(completion) else { return }
      self.stopScanOnQueue(owner: borrower, completion: completion)
    }
  }

  func stopScanOnQueue(owner: AnyObject, completion: (NSError?) -> Void) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !scanDraining else {
      completion(error(code: 1014, message: "Scan callbacks are being delivered; retry stop")); return
    }
    drainScanCallbacks()
    guard scanLeases.removeValue(forKey: ObjectIdentifier(owner)) != nil else {
      completion(error(code: 1004, message: "No Native Protocol v2 scan is active")); return
    }
    reconcileScan()
    completion(nil)
  }

  func cancelScan(borrower: OwnedCoreBluetoothProtocolRadioDelegate, operationIdentifier: String) {
    cancelScan(owner: borrower, operationIdentifier: operationIdentifier)
  }

  func cancelScan(owner: AnyObject, operationIdentifier: String) {
    dispatchPrecondition(condition: .onQueue(queue))
    if scanDraining {
      queue.async { self.cancelScan(owner: owner, operationIdentifier: operationIdentifier) }
      return
    }
    let key = ObjectIdentifier(owner)
    guard scanLeases[key]?.operationIdentifier == operationIdentifier else { return }
    drainScanCallbacks()
    guard scanLeases[key]?.operationIdentifier == operationIdentifier else { return }
    scanLeases.removeValue(forKey: key)
    reconcileScan()
  }

  func clearAllScanLeases() {
    dispatchPrecondition(condition: .onQueue(queue))
    scanLeases.removeAll()
    // Restoration can leave a physical scan with no lease in this process.
    if !physicalScanActive, central?.isScanning == true { central?.stopScan() }
    reconcileScan()
  }

  /// Drain this borrower's observed callbacks and retain its loss route until
  /// the receiver acknowledges every loss. Other owners remain untouched.
  func releaseScanLease(owner: AnyObject) -> NSError? {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !scanDraining else { return error(code: 1014, message: "Scan callbacks are being delivered; retry release") }
    drainScanCallbacks()
    let key = ObjectIdentifier(owner)
    guard pendingScanLosses[key] == nil, !scanDeliveringOwners.contains(key) else {
      return error(code: 1014, message: "Scan loss delivery is pending; retry release")
    }
    if scanLeases.removeValue(forKey: key) != nil { reconcileScan() }
    return nil
  }

  func scanAdapterStateChanged(_ state: CBManagerState) {
    dispatchPrecondition(condition: .onQueue(queue))
    if scanDraining {
      queue.async { self.scanAdapterStateChanged(state) }
      return
    }
    switch state {
    case .unknown, .resetting, .unsupported, .unauthorized, .poweredOff:
      drainScanCallbacks()
      physicalScanActive = false
      // Callback work queued before the adapter fence cannot belong to a
      // restarted physical scan, even if operation identifiers are reused.
      for (key, lease) in scanLeases {
        scanLeases[key] = ScanLease(owner: lease.owner, route: lease.route,
          operationIdentifier: lease.operationIdentifier, generation: UUID(),
          services: lease.services, allowDuplicates: lease.allowDuplicates)
      }
    case .poweredOn:
      if !scanLeases.isEmpty && !physicalScanActive { reconcileScan() }
    @unknown default: break
    }
  }

  /// CoreBluetooth replaces the physical scan configuration on scanForPeripherals.
  /// Its scan and stop methods provide no asynchronous success/error callback;
  /// readiness refusals are returned before the logical claim is installed.
  func reconcileScan() {
    dispatchPrecondition(condition: .onQueue(queue))
    // An old generation's loss reserves that borrower's control slot. Pause
    // only that claim until it is acknowledged; other claims can recover.
    let leases = scanLeases.filter { key, lease in
      guard let loss = pendingScanLosses[key] else { return true }
      return loss.generation == lease.generation
    }.map(\.value)
    guard !leases.isEmpty else {
      if physicalScanActive { central?.stopScan() }
      physicalScanActive = false
      return
    }
    guard let central = central, central.state == .poweredOn else { physicalScanActive = false; return }
    let unrestricted = leases.contains { $0.services.isEmpty }
    let services = Set(leases.flatMap { $0.services }).sorted().map { CBUUID(string: $0) }
    central.scanForPeripherals(withServices: unrestricted ? nil : services,
      options: [CBCentralManagerScanOptionAllowDuplicatesKey: leases.contains { $0.allowDuplicates }])
    physicalScanActive = true
  }

  func connectionClaimRoutes(peer: String? = nil) -> [ObjectIdentifier: OwnedCoreBluetoothProtocolRadioDelegate] {
    var routes: [ObjectIdentifier: OwnedCoreBluetoothProtocolRadioDelegate] = [:]
    for (identifier, claims) in connectionClaims where peer == nil || identifier == peer {
      for claim in claims.values {
        if let route = claim.route { routes[ObjectIdentifier(route)] = route }
      }
    }
    return routes
  }

  /// Retire only this borrower's subscriptions. A final physical disable must
  /// settle before the connection claim can report successful release.
  func retireConnectionNotifications(owner: AnyObject, peer: String, completion: @escaping (NSError?) -> Void) {
    guard let claimIdentity = connectionClaims[peer]?[ObjectIdentifier(owner)]?.identity else {
      completion(error(code: 1007, message: "This borrower does not own the connection")); return
    }
    let owns: (String) -> Bool = { self.notificationRoute($0) === owner }
    if pendingNotify.contains(where: { $0.key.peerIdentifier == peer && owns($0.value.subscriptionIdentifier) }) ||
       pendingNotifyJoins.contains(where: { $0.key.peerIdentifier == peer && $0.value.keys.contains(where: owns) }) {
      completion(error(code: 1018, message: "Notification transition is still pending for this connection claim")); return
    }
    var owned: [(CharacteristicAddress, String, NotificationRoute?)] = []
    for (address, primary) in subscriptions where address.peerIdentifier == peer {
      if owns(primary) { owned.append((address, primary, notificationRoutes[primary])) }
      for identifier in sharedSubscriptions[address] ?? [] where owns(identifier) { owned.append((address, identifier, notificationRoutes[identifier])) }
    }
    if owned.contains(where: { pendingNotify[$0.0] != nil || pendingNotifyJoins[$0.0]?.isEmpty == false }) {
      completion(error(code: 1018, message: "A shared notification transition is still pending")); return
    }
    // Keep retired identifiers reserved until the entire release settles. If a
    // later disable fails, those subscriptions cannot be restored without an
    // unobservable gap; terminate their streams before returning the failure.
    var retired: [(address: CharacteristicAddress, identifier: String, route: NotificationRoute?, borrower: OwnedCoreBluetoothProtocolRadioDelegate?, receiver: ((NSError) -> Void)?, source: OwnedOrderedNotificationSource?)] = []
    func finish(_ failure: NSError?) {
      let loss = failure.map { cause in
        NSError(domain: "com.sfourdrinier.unifiedblemanager.corebluetooth", code: 1039,
          userInfo: [NSLocalizedDescriptionKey: "Notification subscription was retired before connection cleanup failed",
                     NSUnderlyingErrorKey: cause])
      }
      for entry in retired {
        // Another operation may have explicitly retired and replaced this
        // identifier while an asynchronous disable was outstanding. The
        // captured ordered claim remains valid evidence even in that case.
        let routeCurrent = self.notificationRoutes[entry.identifier] === entry.route &&
          (entry.route != nil || self.delegate === entry.borrower)
        if routeCurrent { self.notificationRoutes.removeValue(forKey: entry.identifier) }
        if let loss, let cause = failure {
          // A source can contain multiple claims on the same physical route.
          // Use the token retained at enable, including when the CCCD remains
          // active for another borrower. Legacy unbound routes are safe to
          // report only when no other logical connection or subscription exists.
          let orderedClaim = entry.route?.orderedClaim
          let legacyExclusive = routeCurrent && self.connectionClaims[peer]?.count == 1 &&
            self.subscriptions[entry.address] == nil && (self.sharedSubscriptions[entry.address] ?? []).isEmpty
          if (orderedClaim != nil || legacyExclusive),
             let source = entry.source, self.orderedSources[peer] === source,
             orderedClaim == nil || orderedClaim?.generation == source.generation {
            let route = self.orderedRoute(for: entry.address)
            let nativeFailure = OwnedOrderedNotificationIngress.NativeFailure(error: cause)
            let report = self.orderedFailureCapture?(route, nativeFailure, orderedClaim)
              ?? source.captureFailure(route: route, failure: nativeFailure, claim: orderedClaim)
            if let gate = self.orderedGraphPermitGates[peer] {
              if !gate.enqueue(reject: report, report) { gate.breakForOverflow() }
            } else { report() }
          }
          if let receiver = entry.receiver { receiver(loss) }
          else if routeCurrent { entry.borrower?.protocolRadioDidFailNotification?(entry.identifier, error: loss) }
        }
      }
      completion(failure)
    }
    func next() {
      guard self.connectionClaims[peer]?[ObjectIdentifier(owner)]?.identity == claimIdentity else {
        finish(self.error(code: 1020, message: "Connection claim changed during notification cleanup")); return
      }
      guard let (address, identifier, route) = owned.popLast() else { finish(nil); return }
      guard self.notificationRoutes[identifier] === route,
            route != nil || self.delegate === owner else {
        finish(self.error(code: 1020, message: "Notification route changed during connection cleanup")); return
      }
      let borrower = self.notificationRoute(identifier)
      let receiver = (borrower as? OwnedDeferredNotificationFailureReceiver)?.captureNotificationFailure(identifier)
      if borrower is OwnedDeferredNotificationFailureReceiver && receiver == nil {
        finish(self.error(code: 1014, message: "Notification failure retention is unavailable; retry connection cleanup")); return
      }
      let source = self.orderedSources[peer]
      if self.updateActiveSharedSubscription(address: address, enabled: false, subscriptionIdentifier: identifier, completion: { failure in
        if let failure { finish(failure); return }
        retired.append((address, identifier, route, borrower, receiver, source))
        next()
      }) { return }
      self.setNotify(peerIdentifier: address.peerIdentifier, serviceUUID: address.serviceUUID,
        serviceOccurrence: address.serviceOccurrence, characteristicUUID: address.characteristicUUID,
        characteristicOccurrence: address.characteristicOccurrence, enabled: false, subscriptionIdentifier: identifier,
        operationIdentifier: "claim-disconnect-\(UUID().uuidString)",
        expectedRelease: (owner: owner, claim: claimIdentity, route: route)) { failure in
          if let failure { finish(failure); return }
          retired.append((address, identifier, route, borrower, receiver, source))
          next()
        }
    }
    next()
  }

  // Routes are installed before an enable can call CoreBluetooth. A value may
  // arrive before the enable completion. An identifier can only be retired by
  // the borrower that installed it.
  func installNotificationRoute(_ identifier: String, borrower: OwnedCoreBluetoothProtocolRadioDelegate) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard notificationRoutes[identifier] == nil else { return false }
    notificationRoutes[identifier] = NotificationRoute(borrower)
    return true
  }

  func removeNotificationRoute(_ identifier: String, borrower: OwnedCoreBluetoothProtocolRadioDelegate) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let route = notificationRoutes[identifier], route.borrower === borrower else { return }
    notificationRoutes.removeValue(forKey: identifier)
  }

  func notificationRoute(_ identifier: String) -> OwnedCoreBluetoothProtocolRadioDelegate? {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let route = notificationRoutes[identifier] else { return delegate }
    if route.borrower == nil { NSLog("[UnifiedBle] notification route lost its borrower: %@", identifier) }
    return route.borrower
  }

  /**
   * Claims the sole JavaScript event route for this process-owned radio.
   * A second TurboModule must not steal callbacks from an attached borrower.
   */
  @objc public func attachDelegateIfAvailable(_ candidate: OwnedCoreBluetoothProtocolRadioDelegate) -> Bool {
    queue.sync {
      let attach = {
        // The coordinator supports independent identities, but the radio's
        // global cleanup and event route are not borrower-scoped yet.
        guard self.borrowerRelease.borrowerCount == 0 else { return false }
        guard self.borrowerRelease.attach(candidate) else { return false }
        self.delegate = candidate
        return true
      }
      if let fenced = candidate as? OwnedCoreBluetoothFencedBorrower {
        return fenced.admitRadioBorrower(attach)
      }
      return attach()
    }
  }

  /**
   * Releases one borrower without opening an attachment gap during physical
   * cleanup. Concurrent releases join the same terminal cleanup result.
   */
  @objc public func releaseBorrowerIfCurrent(
    _ candidate: OwnedCoreBluetoothProtocolRadioDelegate,
    completion: @escaping (NSError?) -> Void
  ) {
    queue.async {
      let ticket: UUID
      switch self.borrowerRelease.beginRelease(candidate, completion: completion) {
      case .noBorrower:
        completion(nil)
        return
      case .joined:
        return
      case let .start(value):
        ticket = value
      }
      let finish: (NSError?) -> Void = { error in
        guard let completions = self.borrowerRelease.finish(candidate, ticket: ticket, error: error) else { return }
        if error == nil { self.borrowerReleaseRetryScheduled.removeValue(forKey: ObjectIdentifier(candidate)) }
        if error == nil, let current = self.delegate, current === candidate {
          self.delegate = nil
        }
        if error == nil {
          self.notificationRoutes = self.notificationRoutes.filter { $0.value.borrower !== candidate }
          let key = ObjectIdentifier(candidate)
          self.scopedGattOperations.removeValue(forKey: key)
          for peer in Array(self.connectionClaims.keys) {
            self.connectionClaims[peer]?.removeValue(forKey: key)
            if self.connectionClaims[peer]?.isEmpty == true { self.connectionClaims.removeValue(forKey: peer) }
          }
        }
        if error == nil { (candidate as? OwnedCoreBluetoothFencedBorrower)?.didReleaseRadioBorrower() }
        if error != nil {
          self.scheduleBorrowerReleaseRetry(candidate)
        }
        for pendingCompletion in completions {
          pendingCompletion(error)
        }
      }
      if (candidate as? OwnedNotificationFailureRetention)?.hasPendingNotificationFailureRetention() == true {
        finish(self.error(code: 1014, message: "Notification failure delivery is pending; retry borrower release")); return
      }
      if let failure = self.releaseScanLease(owner: candidate) { finish(failure); return }
      // The attachment guard stays closed. If internal callers ever attach a
      // second borrower, refuse the still-global non-scan teardown rather
      // than destroying the other borrower's resources.
      guard self.borrowerRelease.borrowerCount == 1 else {
        finish(self.error(code: 1014, message: "Non-scan borrower cleanup is not yet scoped")); return
      }
      self.releaseProtocolClient(scanOwner: candidate) { failure in
        if failure == nil, (candidate as? OwnedNotificationFailureRetention)?.hasPendingNotificationFailureRetention() == true {
          finish(self.error(code: 1014, message: "Notification failure delivery is pending; retry borrower release"))
        } else { finish(failure) }
      }
    }
  }

  private func scheduleBorrowerReleaseRetry(_ candidate: OwnedCoreBluetoothProtocolRadioDelegate) {
    let identity = ObjectIdentifier(candidate)
    guard borrowerReleaseRetryScheduled[identity] == nil else { return }
    // A successful release can be followed by another attachment of the same
    // object before this timer fires. Only the still-current retry may act.
    let retry = UUID()
    borrowerReleaseRetryScheduled[identity] = retry
    queue.asyncAfter(deadline: .now() + .seconds(1)) {
      guard self.borrowerReleaseRetryScheduled[identity] == retry else { return }
      self.borrowerReleaseRetryScheduled.removeValue(forKey: identity)
      self.releaseBorrowerIfCurrent(candidate) { _ in }
    }
  }
}

/**
 * Process lifetime registry for CoreBluetooth centrals selected by immutable
 * native configuration. React Native may construct its TurboModule more than
 * once; those modules borrow the same radio rather than registering duplicate
 * CoreBluetooth restoration identifiers.
 */
@objc(OwnedCoreBluetoothProtocolRadioOwner)
public final class OwnedCoreBluetoothProtocolRadioOwner: NSObject {
  private struct Configuration: Hashable {
    let restoreIdentifierKey: String?
    let showPowerAlert: Bool?
  }

  private static let lock = NSLock()
  private static var radios = [Configuration: OwnedCoreBluetoothProtocolRadio]()

  @objc(acquireWithRestoreIdentifierKey:showPowerAlert:)
  public static func acquire(
    restoreIdentifierKey: String?,
    showPowerAlert: NSNumber?
  ) -> OwnedCoreBluetoothProtocolRadio {
    let configuration = Configuration(
      restoreIdentifierKey: restoreIdentifierKey,
      showPowerAlert: showPowerAlert?.boolValue
    )
    lock.lock()
    defer { lock.unlock() }
    if let existing = radios[configuration] { return existing }
    let created = OwnedCoreBluetoothProtocolRadio(
      restoreIdentifierKey: restoreIdentifierKey,
      showPowerAlert: showPowerAlert
    )
    radios[configuration] = created
    return created
  }

  /**
   * A borrower releases its own radio work only when it still owns callback
   * delivery. The process-owned central remains available for restoration and
   * a replacement TurboModule.
   */
  @objc(releaseBorrowerWithRadio:delegate:completion:)
  public static func releaseBorrower(
    radio: OwnedCoreBluetoothProtocolRadio,
    delegate: OwnedCoreBluetoothProtocolRadioDelegate,
    completion: @escaping (NSError?) -> Void
  ) {
    radio.releaseBorrowerIfCurrent(delegate, completion: completion)
  }
}

/// The factory failed before installing a group or publishing a new source.
/// The caller can classify this as no effect while retaining the original error.
struct OwnedOrderedSourceFactoryNoEffect: Error {
  let underlying: Error
}

/// Source-side operations only. Physical CCCD acquisition is a separate owner;
/// callers must install the route before starting that acquisition.
extension OwnedCoreBluetoothProtocolRadio {
  func orderedRoute(for address: CharacteristicAddress) -> OrderedNotificationRoute {
    OrderedNotificationRoute(
      peer: address.peerIdentifier, serviceUUID: address.serviceUUID,
      serviceOccurrence: address.serviceOccurrence, characteristicUUID: address.characteristicUUID,
      characteristicOccurrence: address.characteristicOccurrence
    )
  }

  /// A global lifecycle event applies its peer portion as each peer reaches
  /// the barrier; unrelated peers do not wait for a held permit.
  func performOrderedGlobalBarrier(invalidating: Bool = true, peerOperation: @escaping (String) -> Void = { _ in },
                                   onFailure: @escaping () -> Void = {}, operation: @escaping () -> Void) {
    dispatchPrecondition(condition: .onQueue(queue))
    let peers = Set(orderedSources.keys).union(peripheralByIdentifier.keys).union(pendingDiscovery.keys)
      .union(orderedSnapshotIdentities.peers()).union(orderedGraphPermitGates.keys).union(orderedOverflowDisconnectPending)
    let held = peers.compactMap { peer -> (String, OwnedOrderedGraphPermitGate)? in
      guard let gate = orderedGraphPermitGates[peer], gate.isHolding else { return nil }
      return (peer, gate)
    }
    let heldPeers = Set(held.map { $0.0 })
    for peer in peers where !heldPeers.contains(peer) { peerOperation(peer) }
    guard !held.isEmpty else { operation(); return }
    var remaining = held.count
    var failed = false
    for (peer, gate) in held {
      let accepted = gate.enqueueBarrier(invalidating: invalidating) { resume in
        gate.beginBarrierMutation()
        peerOperation(peer)
        gate.endBarrierMutation()
        remaining -= 1
        if remaining == 0 && !failed { operation() }
        resume()
        self.cleanupOrderedGraphGateIfIdle(peer)
        self.flushOverflowCentralStateIfReady()
      }
      if !accepted && !failed { failed = true; onFailure() }
    }
  }

  func flushOverflowCentralStateIfReady() {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let pending = pendingOverflowCentralState,
          !orderedGraphPermitGates.values.contains(where: { $0.isHolding || $0.hasOutstandingBarrier }) else { return }
    pendingOverflowCentralState = nil
    pending.apply()
  }

  func resetOrderedPeer(_ peer: String, cause: OwnedOrderedNotificationIngress.End) {
    orderedSnapshotIdentities.disconnect(peer)
    invalidateOrderedSource(for: peer, cause: cause)
    let failure = error(code: 1016, message: "CoreBluetooth graph was reset")
    failPendingGATT(for: peer, error: failure)
    if let pending = pendingDiscovery[peer] { failDiscovery(peer, pending: pending, error: failure) }
  }

  /// A central state below poweredOff invalidates every previously obtained
  /// peripheral object and its callback debts without a disconnect callback.
  func recoverAfterAdapterInvalidation(_ peer: String) {
    orderedOverflowDisconnectPending.remove(peer)
    let failure = error(code: 1016, message: "CoreBluetooth adapter invalidated the previous connection")
    pendingConnect.removeValue(forKey: peer)?.completion(failure)
    pendingDisconnect.removeValue(forKey: peer)?.completion(failure)
    disconnectOwners.removeValue(forKey: peer)
    connectionEpochs.removeValue(forKey: peer)
    pendingConnectionTerminals.removeValue(forKey: peer)
    if let claims = connectionClaims.removeValue(forKey: peer) {
      for claim in claims.values {
        if let pending = claim.pending { pending.completion(failure) }
        else { claim.route?.protocolRadioDidDisconnectPeer(peer, error: failure) }
      }
    }
    if let pending = pendingDiscovery.removeValue(forKey: peer), !pending.cancelled && !pending.completionDelivered {
      pending.completion(nil, failure)
    }
    peripheralByIdentifier.removeValue(forKey: peer)?.delegate = nil
    servicesByPeer.removeValue(forKey: peer)
    clearDiscoveryCancellationCleanup(forPeerIdentifier: peer)
    orderedLostDisconnects.removeValue(forKey: peer)
    orderedConfirmedDisconnected.remove(peer)
    clearCancellationCleanup(forPeerIdentifier: peer)
  }

  func cleanupOrderedGraphGateIfIdle(_ peer: String) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let gate = orderedGraphPermitGates[peer], gate.canDiscard else { return }
    orderedGraphPermitGates.removeValue(forKey: peer)
    orderedPermitSources.removeValue(forKey: peer)
    orderedConfirmedDisconnected.remove(peer)
  }

  func orderedGraphGate(peer: String) -> OwnedOrderedGraphPermitGate {
    dispatchPrecondition(condition: .onQueue(queue))
    if let gate = orderedGraphPermitGates[peer] { return gate }
    let failure: () -> Void = { [weak self] in self?.failOrderedGraphGate(peer, includePreparation: false) }
    let gate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 1024, maxBytes: 1_048_576,
      barrierOverflow: { [weak self] in self?.failOrderedGraphGate(peer, includePreparation: true) }, overflow: failure)
    orderedGraphPermitGates[peer] = gate
    return gate
  }

  private func failOrderedGraphGate(_ peer: String, includePreparation: Bool) {
    let failure = error(code: 1014, message: "Ordered graph deferred callback capacity exhausted")
    // The gate's reserved overflow action runs after every accepted prefix.
    // Execute exact retained notification callbacks now, before invalidating
    // their source. Re-enqueueing from inside the graph's drain would put
    // them behind this fail-stop, which clears that source first.
    flushRetainedNotificationReplays(peer: peer)
    orderedSnapshotIdentities.disconnect(peer)
    orderedPermitSources[peer]?.invalidate(.overflow)
    orderedSources.removeValue(forKey: peer)
    if let observed = orderedLostDisconnects.removeValue(forKey: peer) {
      completeObservedDisconnect(peer, error: observed.error)
    } else {
      pendingConnect.removeValue(forKey: peer)?.completion(failure)
      pendingDisconnect.removeValue(forKey: peer)?.completion(failure)
      failPendingGATT(for: peer, error: failure)
      if let pending = pendingDiscovery.removeValue(forKey: peer), !pending.cancelled && !pending.completionDelivered {
        pending.completion(nil, failure)
      }
      if let peripheral = peripheralByIdentifier[peer], !orderedConfirmedDisconnected.contains(peer) {
        // A disconnected-looking object is not itself the callback fence. Apple
        // guarantees no later peripheral delegate calls only after didDisconnect.
        // If neither that callback nor adapter invalidation arrives, the host
        // must restart the owning process; this does not revoke device bonds.
        orderedOverflowDisconnectPending.insert(peer)
        if peripheral.state != .disconnected { central?.cancelPeripheralConnection(peripheral) }
      } else {
        orderedOverflowDisconnectPending.remove(peer)
        servicesByPeer.removeValue(forKey: peer)
        clearCancellationCleanup(forPeerIdentifier: peer)
      }
    }
    if includePreparation { radioPreparation.failAll(failure) }
    flushOverflowCentralStateIfReady()
  }

  /// Shared delegate/test seam: callers capture immutable callback fields
  /// before supplying the operation.
  func deliverCharacteristicSnapshot(peer: String, byteCount: Int, operation: @escaping () -> Void) {
    dispatchPrecondition(condition: .onQueue(queue))
    if let gate = orderedGraphPermitGates[peer], gate.isHolding { gate.enqueue(bytes: byteCount, operation) }
    else { operation() }
  }

  /// Snapshot bytes and provenance before any deferred callback can mutate them.
  func admitOrderedCharacteristicValue(address: CharacteristicAddress, value: Data) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let source = orderedSources[address.peerIdentifier] else { return }
    let route = orderedRoute(for: address)
    let provenance: OwnedOrderedNotificationIngress.Provenance = pendingRead[address] == nil ? .notification : .readOrNotification
    let settle = source.reserveValue(route: route, bytes: value, provenance: provenance)
    if let gate = orderedGraphPermitGates[address.peerIdentifier] {
      if !gate.enqueue(bytes: value.count, reject: { settle(false) }, { settle(true) }) { gate.breakForOverflow() }
    } else { settle(true) }
  }

  /// A callback error on a subscribed route may be a notification failure,
  /// even when a read is outstanding. Preserve that failure for the group.
  func reportOrderedCharacteristicFailure(address: CharacteristicAddress, error: NSError) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard subscriptions[address] != nil || pendingNotify[address]?.enabled == true ||
            !(sharedSubscriptions[address] ?? []).isEmpty || !(pendingNotifyJoins[address] ?? [:]).isEmpty,
          let source = orderedSources[address.peerIdentifier] else { return }
    let route = orderedRoute(for: address)
    let failure = OwnedOrderedNotificationIngress.NativeFailure(error: error)
    let operation = orderedFailureCapture?(route, failure, nil)
      ?? source.captureFailure(route: route, failure: failure)
    if let gate = orderedGraphPermitGates[address.peerIdentifier] {
      if !gate.enqueue(reject: operation, operation) { gate.breakForOverflow() }
    } else { operation() }
  }

  /// A notification-state callback is already an immutable observation by the
  /// time this runs: its source retention and borrower delivery closures were
  /// captured on the CoreBluetooth queue. If the graph FIFO is full, retain
  /// that exact observation and append it only after the accepted prefix. A
  /// plain `reject` callback would run synchronously and violate FIFO order.
  func retainNotificationReplay(peer: String, gate: OwnedOrderedGraphPermitGate,
                               replay: @escaping () -> Void) -> UUID? {
    dispatchPrecondition(condition: .onQueue(queue))
    var retained = retainedNotificationReplays[peer] ?? []
    guard retained.count < Self.retainedNotificationReplayLimit else { return nil }
    let id = UUID()
    retained.append(RetainedNotificationReplay(id: id, gate: gate, replay: replay))
    retainedNotificationReplays[peer] = retained
    scheduleRetainedNotificationReplay(peer: peer)
    return id
  }

  func removeRetainedNotificationReplay(peer: String, id: UUID) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard var retained = retainedNotificationReplays[peer] else { return }
    retained.removeAll { $0.id == id }
    retainedNotificationReplays[peer] = retained.isEmpty ? nil : retained
  }

  private func scheduleRetainedNotificationReplay(peer: String, delayed: Bool = false) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard retainedNotificationReplays[peer]?.isEmpty == false,
          retainedNotificationReplayRetryScheduled.insert(peer).inserted else { return }
    let retry = { [weak self] in
      guard let self else { return }
      self.retainedNotificationReplayRetryScheduled.remove(peer)
      self.retryRetainedNotificationReplays(peer: peer)
    }
    if delayed { queue.asyncAfter(deadline: .now() + .milliseconds(10), execute: retry) }
    else { queue.async(execute: retry) }
  }

  private func retryRetainedNotificationReplays(peer: String) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard var retained = retainedNotificationReplays[peer], !retained.isEmpty else { return }
    while let first = retained.first {
      // A rejected request is intentionally retained and retried. Its source
      // and receiver closures remain exact; no caller sees a fabricated
      // success or a dropped callback while the graph is full.
      guard first.gate.enqueue(reject: {}, first.replay) else {
        retainedNotificationReplays[peer] = retained
        scheduleRetainedNotificationReplay(peer: peer, delayed: true)
        return
      }
      retained.removeFirst()
    }
    retainedNotificationReplays.removeValue(forKey: peer)
  }

  /// Called from the graph's ordered overflow action, after its accepted
  /// prefix has drained and before `failOrderedGraphGate` retires sources.
  /// This is deliberately a direct replay: appending to a currently-draining
  /// graph would run after the fail-stop that is executing this method.
  private func flushRetainedNotificationReplays(peer: String) {
    dispatchPrecondition(condition: .onQueue(queue))
    while var retained = retainedNotificationReplays[peer], !retained.isEmpty {
      let first = retained.removeFirst()
      retainedNotificationReplays[peer] = retained.isEmpty ? nil : retained
      first.replay()
    }
  }

  func reportNotificationFailure(address: CharacteristicAddress, error: NSError) {
    reportOrderedCharacteristicFailure(address: address, error: error)
    var owners = sharedSubscriptions[address] ?? []
    if let primary = subscriptions[address] { owners.insert(primary) }
    if let pending = pendingNotify[address], pending.enabled { owners.insert(pending.subscriptionIdentifier) }
    owners.formUnion(pendingNotifyJoins[address]?.keys.map { $0 } ?? [])
    for owner in owners.sorted() {
      guard let route = notificationRoute(owner) else { continue }
      if let receiver = route as? OwnedDeferredNotificationFailureReceiver,
         let retained = receiver.captureNotificationFailure(owner) {
        retained(error)
      } else {
        route.protocolRadioDidFailNotification?(owner, error: error)
      }
    }
  }

  /// Keep a physical owner until the original transition and a disable have
  /// both answered. A preserved restored connection has no disconnect fence.
  func retireNotificationsForRelease(_ peer: String) -> Bool {
    for owner in retiredNotificationOwners.values where owner.address.peerIdentifier == peer {
      retryRetiredNotification(owner)
    }
    var addresses = Set(subscriptions.keys.filter { $0.peerIdentifier == peer })
    addresses.formUnion(pendingNotify.keys.filter { $0.peerIdentifier == peer })
    for cleanup in pendingCancellationCleanup.values {
      addresses.formUnion(cleanup.notificationDesiredStates.keys.filter { $0.peerIdentifier == peer })
    }
    let resolved = addresses.compactMap { address -> (CharacteristicAddress, CBPeripheral, CBCharacteristic)? in
      guard let result = resolve(address) else { return nil }
      return (address, result.peripheral, result.characteristic)
    }
    guard resolved.count == addresses.count else { return false }
    for (address, peripheral, characteristic) in resolved {
      let object = ObjectIdentifier(characteristic)
      guard retiredNotificationOwners[object] == nil else { continue }
      let awaiting = pendingNotify[address] != nil || pendingCancellationCleanup.values.contains {
        $0.notificationAwaitingCallbacks.contains(address)
      }
      var owners = sharedSubscriptions[address] ?? []
      if let primary = subscriptions[address] { owners.insert(primary) }
      if let pending = pendingNotify[address] { owners.insert(pending.subscriptionIdentifier) }
      let owner = RetiredNotificationOwner(generation: notificationGenerations[address] ?? UUID().uuidString,
        address: address, peripheral: peripheral, characteristic: characteristic, subscriptions: owners,
        phase: awaiting ? .awaitingOriginal : .stalled)
      retiredNotificationOwners[object] = owner
      transferNotificationCleanup(address: address, toRetiredObject: object)
      if !awaiting { retryRetiredNotification(owner) }
    }
    return true
  }

  func invalidateOrderedSource(for peer: String, cause: OwnedOrderedNotificationIngress.End) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let source = orderedSources.removeValue(forKey: peer) else { return }
    let operation = { source.invalidate(cause) }
    if let gate = orderedGraphPermitGates[peer] { gate.invalidateAndEnqueue(operation) }
    else { operation() }
  }

  func invalidateAllOrderedSources(_ cause: OwnedOrderedNotificationIngress.End) {
    dispatchPrecondition(condition: .onQueue(queue))
    for peer in Array(orderedSources.keys) { invalidateOrderedSource(for: peer, cause: cause) }
  }

  /// Creates the immutable source route before any notification enable begins.
  func createOrderedSourceGroup(
    addresses: [CharacteristicAddress], capacity: Int, byteCapacity: Int,
    wake: @escaping () -> Void = {}
  ) throws -> (OwnedOrderedNotificationSource, OwnedOrderedNotificationIngress) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let peer = addresses.first?.peerIdentifier,
          addresses.allSatisfy({ address in
            guard address.peerIdentifier == peer, let resolved = resolve(address) else { return false }
            return notificationObjectIsAvailable(resolved.characteristic)
          }) else {
      throw OwnedOrderedSourceFactoryNoEffect(underlying: OwnedOrderedNotificationIngress.Failure.invalidMembers)
    }
    let existingSource = orderedSources[peer]
    let source = existingSource ?? OwnedOrderedNotificationSource(queue: queue, generation: UUID().uuidString)
    let group: OwnedOrderedNotificationIngress
    do {
      group = try OwnedOrderedNotificationIngress(
        queue: queue, generation: source.generation, members: addresses.map(orderedRoute),
        capacity: capacity, byteCapacity: byteCapacity, wake: wake
      )
    } catch {
      throw OwnedOrderedSourceFactoryNoEffect(underlying: error)
    }
    guard source.add(group) else {
      throw OwnedOrderedSourceFactoryNoEffect(underlying: OwnedOrderedNotificationIngress.Failure.inactive)
    }
    // Queue-confined and nonthrowing after installation: the returned pair is
    // the cleanup identity, and a new source is never visible on failure.
    if existingSource == nil { orderedSources[peer] = source }
    return (source, group)
  }

  func registerOrderedSourceMember(
    _ address: CharacteristicAddress, source: OwnedOrderedNotificationSource,
    group: OwnedOrderedNotificationIngress
  ) throws -> OrderedNotificationClaim {
    dispatchPrecondition(condition: .onQueue(queue))
    guard orderedSources[address.peerIdentifier] === source,
          let claim = source.makeClaim(route: orderedRoute(for: address)) else {
      throw OwnedOrderedNotificationIngress.Failure.inactive
    }
    try group.register(claim)
    return claim
  }
}
