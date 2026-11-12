// ios/UnifiedBleRustRadioAdapter.swift
//
// The Apple `MobilePlatformRadio` (docs/MOBILE_RUST_WIRE.md, "Platform radio
// interface"): the process-owned Rust mobile host asks, this adapter drives
// the OS through `OwnedCoreBluetoothProtocolRadio` and answers every request
// exactly once through `MobileCoreHost.complete`. Unsolicited CoreBluetooth
// facts (advertisements, link loss, service changes, notifications, adapter
// state incl. authorization, state restoration) reach Rust through
// `MobileCoreHost.ingest`.
//
// The adapter holds no BLE policy: ownership, leases, budgets, cancellation
// admission, delivery-mode property checks and failure identities live in
// Rust. It only translates, keeps the request-id → operation-id table that
// cancellation needs, and keeps the subscription-id → (instance, epoch)
// table that stamps notification values.

import CoreBluetooth
import CoreFoundation
import Foundation

/// The OS radio seam the adapter drives. `OwnedCoreBluetoothProtocolRadio`
/// is the production driver; the Apple harness scripts one.
protocol UnifiedBleRustRadioDriver: AnyObject {
  /// Serial queue every driver callback and every adapter mutation runs on.
  var workQueue: DispatchQueue { get }
  /// The peer's existing graph gate, queried only on the work queue.
  func orderedGraphGate(peer: String) -> OwnedOrderedGraphPermitGate?
  func adapterSnapshot(completion: @escaping (NSDictionary) -> Void)
  func prepareForOperation(operationIdentifier: String, completion: @escaping (NSDictionary?, NSError?) -> Void)
  func resolveDirectoryPeer(peerIdentifier: String, completion: @escaping (MobilePeerName?, NSError?) -> Void)
  func connectedPeerSnapshots(services: [String], completion: @escaping ([MobilePeerName]?, NSError?) -> Void)
  func restoredPeerSnapshots(completion: @escaping ([NSDictionary]) -> Void)
  func writeLimits(peerIdentifier: String, completion: @escaping (NSDictionary?, NSError?) -> Void)
  func startScan(
    serviceUUIDs: [String],
    allowDuplicates: Bool,
    operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  )
  func stopScan(operationIdentifier: String, completion: @escaping (NSError?) -> Void)
  func connect(peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSError?) -> Void)
  func disconnect(peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSError?) -> Void)
  func discover(
    peerIdentifier: String,
    operationIdentifier: String,
    completion: @escaping (NSDictionary?, NSError?) -> Void
  )
  func readCharacteristic(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    operationIdentifier: String,
    completion: @escaping (NSData?, OwnedCoreBluetoothReadProvenance, NSError?) -> Void
  )
  func readRssi(
    peerIdentifier: String,
    operationIdentifier: String,
    completion: @escaping (NSNumber?, NSError?) -> Void
  )
  func write(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    value: NSData,
    withResponse: Bool,
    operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  )
  func subscribe(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    subscriptionIdentifier: String,
    operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  )
  func unsubscribe(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    subscriptionIdentifier: String,
    operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  )
  func readDescriptor(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    descriptorUUID: String,
    descriptorOccurrence: Int,
    operationIdentifier: String,
    completion: @escaping (NSData?, NSError?) -> Void
  )
  func writeDescriptor(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    descriptorUUID: String,
    descriptorOccurrence: Int,
    value: NSData,
    operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  )
  func cancelOperation(_ operationIdentifier: String, completion: @escaping (NSDictionary) -> Void)
}

extension UnifiedBleRustRadioDriver {
  func orderedGraphGate(peer: String) -> OwnedOrderedGraphPermitGate? {
    (self as? OwnedCoreBluetoothProtocolRadio)?.orderedGraphPermitGates[peer]
  }
}

/// Reads the Rust-core platform adapter (`UnifiedBleRustRadioAdapter`) needs
/// beyond the Native Protocol v2 surface. Every read runs on the radio queue
/// and answers asynchronously, so a caller on a Rust executor thread never
/// blocks on CoreBluetooth.
extension OwnedCoreBluetoothProtocolRadio: UnifiedBleRustRadioDriver {
  /// The serial queue every CoreBluetooth object is confined to.
  var workQueue: DispatchQueue { queue }

  /// Explicit scan/connect admission only. ASK starts the existing process
  /// central without requesting global permission, then awaits its actual
  /// state. Rust's original request budget/cancellation owns this waiter.
  func prepareForOperation(operationIdentifier: String, completion: @escaping (NSDictionary?, NSError?) -> Void) {
    prepareForOperation(operationIdentifier: operationIdentifier, owner: nil, completion: completion)
  }

  func prepareForOperation(operationIdentifier: String, owner: AnyObject?, completion: @escaping (NSDictionary?, NSError?) -> Void) {
    queue.async {
      guard self.requireUsable({ completion(nil, $0) }) else { return }
      let authorization = OwnedCoreBluetoothProtocolRadioSupport.currentAuthorizationWord()
      let accessorySetup = OwnedCoreBluetoothProtocolRadioSupport.accessorySetupConfigured(info: Bundle.main.infoDictionary ?? [:])
      if accessorySetup && !["denied", "restricted", "unavailable"].contains(authorization) {
        _ = self.ensureCentral()
      }
      if let central = self.central,
         let failure = OwnedCoreBluetoothProtocolRadioSupport.operationReadinessFailure(state: central.state) {
        completion(nil, failure); return
      }
      self.radioPreparation.start(operationIdentifier, owner: owner, snapshot: self.snapshotOnQueue(),
                                  waitForInitialState: accessorySetup, completion: completion)
    }
  }

  func adapterSnapshot(completion: @escaping (NSDictionary) -> Void) {
    queue.async {
      completion(self.snapshotOnQueue())
    }
  }

  /// `{peerIdentifier, name, connected}` for every peripheral CoreBluetooth
  /// restored into this process, read live (a restored link may have dropped).
  func restoredPeerSnapshots(completion: @escaping ([NSDictionary]) -> Void) {
    queue.async {
      completion(OwnedCoreBluetoothProtocolRadioSupport.restoredPeerSnapshots(
        identifiers: self.restoredPeerIdentifiers, peripherals: self.peripheralByIdentifier
      ))
    }
  }

  func resolveDirectoryPeer(peerIdentifier: String, completion: @escaping (MobilePeerName?, NSError?) -> Void) {
    queue.async {
      guard self.requireUsable({ completion(nil, $0) }) else { return }
      guard let identifier = OwnedCoreBluetoothKnownPeerLookup.identifier(peerIdentifier) else { completion(nil, self.error(code: 1005, message: "Invalid CoreBluetooth identifier")); return }
      guard let central = self.centralForUse(or: { completion(nil, $0) }) else { return }
      let peers = central.retrievePeripherals(withIdentifiers: [identifier])
      guard peers.count <= 1, peers.allSatisfy({ $0.identifier == identifier }) else { completion(nil, self.error(code: 1035, message: "CoreBluetooth identifier lookup returned inconsistent identity")); return }
      completion(peers.first.map { MobilePeerName(peerId: $0.identifier.uuidString, name: $0.name) }, nil)
    }
  }

  func connectedPeerSnapshots(services: [String], completion: @escaping ([MobilePeerName]?, NSError?) -> Void) {
    queue.async {
      guard self.requireUsable({ completion(nil, $0) }) else { return }
      guard !services.isEmpty else { completion(nil, self.error(code: 1034, message: "System-connected retrieval requires services")); return }
      guard let central = self.centralForUse(or: { completion(nil, $0) }) else { return }
      let peripherals = central.retrieveConnectedPeripherals(withServices: services.map { CBUUID(string: $0) })
      guard peripherals.count <= 4096 else { completion(nil, self.error(code: 1035, message: "System-connected directory capacity exceeded")); return }
      // No delegate assignment, owner-cache insertion or connection request.
      let peers = peripherals.filter { $0.state == .connected }.map { MobilePeerName(peerId: $0.identifier.uuidString, name: $0.name) }
      completion(peers, nil)
    }
  }

  /// CoreBluetooth's per-mode single-write limits and write-without-response
  /// readiness for one connected peripheral:
  /// `{withResponse, withoutResponse, canSendWithoutResponse}`.
  func writeLimits(peerIdentifier: String, completion: @escaping (NSDictionary?, NSError?) -> Void) {
    queue.async {
      guard self.requireUsable({ error in completion(nil, error) }) else { return }
      guard let peripheral = self.peripheralByIdentifier[peerIdentifier], peripheral.state == .connected else {
        completion(nil, self.error(code: 1033, message: "The requested peripheral is not connected"))
        return
      }
      completion([
        "withResponse": peripheral.maximumWriteValueLength(for: .withResponse),
        "withoutResponse": peripheral.maximumWriteValueLength(for: .withoutResponse),
        "canSendWithoutResponse": peripheral.canSendWriteWithoutResponse
      ] as NSDictionary, nil)
    }
  }
}

/// Where answers and facts go: the installed `MobileCoreHost`.
protocol UnifiedBleRustRadioSink: AnyObject {
  /// `"delivered"` | `"late"` | `"mismatched"`.
  func complete(requestId: UInt64, completion: MobileRadioCompletion) -> String
  /// `"accepted"` | `"dropped-advertisement"` | `"dropped-notification"` |
  /// `"dropped-control"` | `"closed"`.
  func ingest(ingress: MobileRadioIngress) -> String
}

/// Implemented only by a Rust binding with explicit, acknowledged scan loss.
/// Older bindings do not conform, so their missing evidence is retained.
protocol UnifiedBleRustScanLossSink: UnifiedBleRustRadioSink {
  func ingestScanLoss(_ loss: OwnedScanLoss) -> String
}

/// A process-native lease for scan-failure recovery.  The foreign Rust
/// handles never cross the React Native bridge: the embedding host can use
/// this Swift-only holder, while JavaScript only receives ordinary public
/// BLE/recording results.
///
/// The holder is fenced by the adapter generation that issued it.  A later
/// host installation cannot reuse a recovery object from an earlier binding.
public final class UnifiedBleRustScanFailureRecoveryLease {
  private weak var adapter: UnifiedBleRustRadioAdapter?
  private let generation: UInt64
  private let recovery: MobileScanFailureRecovery

  fileprivate init(adapter: UnifiedBleRustRadioAdapter, generation: UInt64,
                   recovery: MobileScanFailureRecovery) {
    self.adapter = adapter
    self.generation = generation
    self.recovery = recovery
  }

  func requireLive(_ operation: String) throws {
    guard let adapter, adapter.scanFailureRecoveryLeaseIsLive(generation) else {
      throw MobileCoreError.Failed(code: "lifecycle.destroyed", domain: "restoration",
        operation: operation, detail: "scan recovery binding is no longer active")
    }
  }

  public func owners() throws -> [MobileScanFailureRecoveryOwner] {
    try requireLive("scan.failure-recovery.owners")
    return recovery.owners()
  }

  public func pendingCleanupReceipts() throws -> [MobileScanFailureCleanupReceipt] {
    try requireLive("scan.failure-recovery.pending-cleanup")
    return try recovery.pendingCleanupReceipts()
  }
}

/// Versioned, idempotent admission. An unknown reply is ambiguous and must
/// be reconciled by retrying the same receipt before sending a later one.
protocol UnifiedBleRustAcknowledgedFailureSink: UnifiedBleRustRadioSink {
  func notificationFailureIngressVersion(offered: UInt32) -> UInt32
  func ingestNotificationFailureV1(sequence: UInt64, instance: MobileInstance, epoch: UInt64, failure: MobilePlatformFailure) -> String
}

/// An explicitly configured host-owned durable journal for source failures
/// which arrive after the adapter's bounded in-memory receipts are occupied.
/// `sequence` is an adapter-monotonic, idempotent source-arrival identity; the implementation
/// must retain the exact record before answering `"committed"`. Durable append
/// is not source acknowledgment: replay/export/acknowledgment must retain this
/// sequence behind every smaller sequence for the same source, including an
/// in-memory V1 receipt which has not reached Rust yet.
protocol UnifiedBleRustNotificationFailureSpool: AnyObject {
  func appendNotificationFailureV1(sequence: UInt64, instance: MobileInstance,
                                   epoch: UInt64, failure: MobilePlatformFailure) -> String
  /// Returns the persisted maximum source sequence as an unsigned decimal,
  /// or `"unsupported"` when this host intentionally has no durable spool.
  /// A configured spool must not admit a route while its prior sequence cannot
  /// be recovered, because a fresh counter could collide after process restart.
  func notificationFailureSpoolResumeSequenceV1(instance: MobileInstance, epoch: UInt64) -> String
  /// Returns the next exact FIFO page after `afterSequence`. The JSON envelope
  /// is versioned by the Rust boundary so Swift must fail closed on malformed
  /// or unknown records rather than acknowledging a different source fact.
  func notificationFailureSpoolPageV1(instance: MobileInstance, epoch: UInt64,
                                      afterSequence: UInt64, maxItems: UInt32) -> String
  /// Deletes precisely the record Rust returned after the V1 ingress has
  /// accepted it. A lost reply is deliberately retried with the same record.
  func acknowledgeNotificationFailureSpoolV1(sequence: UInt64, instance: MobileInstance,
                                             epoch: UInt64, failure: MobilePlatformFailure) -> String
  /// V2 is one FIFO across all callback routes. `captureOrdinal` is durable
  /// source identity, deliberately distinct from the fresh V1 sequence the
  /// current process assigns while replaying it into Rust.
  func notificationFailureSpoolResumeGlobalV2() -> String
  func notificationFailureSpoolPageGlobalV2(afterCaptureOrdinal: UInt64,
                                            maxItems: UInt32) -> String
  func acknowledgeNotificationFailureSpoolGlobalV2(captureOrdinal: UInt64) -> String
  func acknowledgeNotificationFailureSpoolGlobalV3(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                   instance: MobileInstance, epoch: UInt64,
                                                   failure: MobilePlatformFailure) -> String
  func retireNotificationFailureSpoolAcknowledgementV3(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                        instance: MobileInstance, epoch: UInt64,
                                                        failure: MobilePlatformFailure) -> String
  func acknowledgeNotificationFailureSpoolGlobalV4(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                   instance: MobileInstance, epoch: UInt64,
                                                   failure: MobilePlatformFailure) -> String
  func confirmNotificationFailureSpoolAcknowledgementRetirementV4(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                                   instance: MobileInstance, epoch: UInt64,
                                                                   failure: MobilePlatformFailure,
                                                                   retirementCapability: String) -> String
  func notificationFailureSpoolPendingRetirementsV4(maxItems: UInt32) -> String
}

extension UnifiedBleRustNotificationFailureSpool {
  func notificationFailureSpoolResumeSequenceV1(instance: MobileInstance, epoch: UInt64) -> String { "unsupported" }
  func notificationFailureSpoolPageV1(instance: MobileInstance, epoch: UInt64,
                                      afterSequence: UInt64, maxItems: UInt32) -> String { "unsupported" }
  func acknowledgeNotificationFailureSpoolV1(sequence: UInt64, instance: MobileInstance,
                                             epoch: UInt64, failure: MobilePlatformFailure) -> String { "unsupported" }
  func notificationFailureSpoolResumeGlobalV2() -> String { "unsupported" }
  func notificationFailureSpoolPageGlobalV2(afterCaptureOrdinal: UInt64,
                                            maxItems: UInt32) -> String { "unsupported" }
  func acknowledgeNotificationFailureSpoolGlobalV2(captureOrdinal: UInt64) -> String { "unsupported" }
  func acknowledgeNotificationFailureSpoolGlobalV3(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                   instance: MobileInstance, epoch: UInt64,
                                                   failure: MobilePlatformFailure) -> String { "unsupported" }
  func retireNotificationFailureSpoolAcknowledgementV3(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                        instance: MobileInstance, epoch: UInt64,
                                                        failure: MobilePlatformFailure) -> String { "unsupported" }
  func acknowledgeNotificationFailureSpoolGlobalV4(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                   instance: MobileInstance, epoch: UInt64,
                                                   failure: MobilePlatformFailure) -> String { "unsupported" }
  func confirmNotificationFailureSpoolAcknowledgementRetirementV4(captureOrdinal: UInt64, sourceSequence: UInt64,
                                                                   instance: MobileInstance, epoch: UInt64,
                                                                   failure: MobilePlatformFailure,
                                                                   retirementCapability: String) -> String { "unsupported" }
  func notificationFailureSpoolPendingRetirementsV4(maxItems: UInt32) -> String { "unsupported" }
}

extension MobileCoreHost: UnifiedBleRustAcknowledgedFailureSink {}
extension MobileCoreHost: UnifiedBleRustNotificationFailureSpool {}

/// Answer bookkeeping the host does not see: a completion Rust refused as
/// mismatched is an adapter defect, and a fact ingested after shutdown is
/// lost by construction. Both are counted here and logged, never dropped
/// silently.
struct UnifiedBleRustRadioAdapterCounters: Equatable {
  var mismatchedCompletions: UInt64 = 0
  var ingressAfterClose: UInt64 = 0
  var cancelledRequests: UInt64 = 0
  var unknownConnectionLosses: UInt64 = 0
}

final class UnifiedBleRustRadioAdapter: NSObject, MobilePlatformRadio, OwnedCoreBluetoothProtocolRadioDelegate, OwnedCoreBluetoothFencedBorrower, OwnedScanLossReceiver, OwnedScanAdvertisementReceiver, OwnedDeferredNotificationFailureReceiver, OwnedNotificationFailureRetention,
  @unchecked Sendable
{
  private struct Subscription {
    let identifier: String
    let instance: MobileInstance
    let epoch: UInt64
  }

  // Slots exist before the route can reach the driver. A slot remains busy
  // until both its captured callback and its sink receipt have settled.
  private final class CapturedFailureSlot {
    let key = UUID()
    /// The route reserves one separate terminal receipt after its ordinary
    /// callback capacity. CoreBluetooth cannot be back-pressured, so this
    /// preserves the first callback that arrives after ordinary receipts are
    /// all retained instead of treating that source failure as a log-only
    /// observation.
    let overflow: Bool
    var busy = false
    var ingress: MobileRadioIngress?
    var sequence: UInt64?
    var refusalLogged = false
    var settled = false
    var captureReleased = false
    init(overflow: Bool) { self.overflow = overflow }
  }
  private final class NotificationReceiptRoute {
    let sink: UnifiedBleRustRadioSink
    let subscription: Subscription
    let reservation: OwnedNotificationReceiptBudget.Reservation
    let slots: [CapturedFailureSlot]
    /// Once the individually retained receipts are occupied, CoreBluetooth
    /// cannot be paused. Keep one durable accounting record instead of
    /// replacing a source failure or reducing later callbacks to a log.
    var coalescedOverflow: CoalescedNotificationFailureOverflow?
    var retired = false
    init(sink: UnifiedBleRustRadioSink, subscription: Subscription,
         reservation: OwnedNotificationReceiptBudget.Reservation, ordinaryCount: Int) {
      self.sink = sink; self.subscription = subscription; self.reservation = reservation
      slots = (0..<ordinaryCount).map { _ in CapturedFailureSlot(overflow: false) }
        + [CapturedFailureSlot(overflow: true)]
    }
  }
  private final class CoalescedNotificationFailureOverflow {
    let firstSequence: UInt64?
    var lastSequence: UInt64?
    let first: MobilePlatformFailure
    var last: MobilePlatformFailure
    var count: UInt64 = 1
    var refusalLogged = false
    init(sequence: UInt64?, failure: MobilePlatformFailure) {
      firstSequence = sequence; lastSequence = sequence; first = failure; last = failure
    }
    func append(sequence: UInt64?, failure: MobilePlatformFailure) {
      lastSequence = sequence
      last = failure
      if count < UInt64.max { count += 1 }
    }
  }
  private final class CapturedFailureLease {
    let release: () -> Void
    init(release: @escaping () -> Void) { self.release = release }
    deinit { release() }
  }

  private struct NotificationFailureSpoolRecord {
    /// Stable durable capture identity. This is the only ACK key.
    let captureOrdinal: UInt64
    /// Original adapter/source identity retained for diagnostics. It must not
    /// be reused as the V1 delivery receipt after a process restart.
    let sourceSequence: UInt64
    let route: String
    let epoch: UInt64
    let instance: MobileInstance
    let failure: MobilePlatformFailure
  }
  private enum NotificationFailureSpoolPageError: Error {
    case reason(String)
  }

  private struct InstanceKey: Hashable {
    let peerId: String
    let serviceUuid: String
    let serviceOccurrence: UInt64
    let characteristicUuid: String
    let characteristicOccurrence: UInt64

    init(_ instance: MobileInstance) {
      peerId = instance.peerId
      serviceUuid = OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(instance.serviceUuid)
      serviceOccurrence = instance.serviceOccurrence
      characteristicUuid = OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(instance.characteristicUuid)
      characteristicOccurrence = instance.characteristicOccurrence
    }
  }

  private struct SubscriptionKey: Hashable {
    let instance: InstanceKey
    let epoch: UInt64
  }
  private struct OrderedActivationKey: Hashable {
    let subscription: SubscriptionKey
    let connection: String
    let source: String
    let group: UInt64
    let member: UInt64
    let token: UInt64
    let memberEpoch: UInt64
  }
  private enum OrderedEnableState {
    case pending, enabled, released
    case uncertain(MobilePlatformFailure)
  }

  // The submission gate lets an exact cancellation beat a marker request that
  // is waiting on the radio queue. No CoreBluetooth call runs under this lock.
  private struct MarkerKey: Hashable {
    let source: String
    let group: UInt64
    let marker: UInt64
  }
  /// A marker that was admitted by Rust but is still fenced by a retained
  /// CoreBluetooth failure capture. The original request has already received
  /// its pending receipt; this host-owned record is what retries the exact
  /// physical operation once the capture boundary settles.
  private struct PendingMarkerControl {
    let key: MarkerKey
    let connection: String
    let snapshot: OwnedGattSnapshotIdentity
    let instance: MobileInstance
    let value: Data
    let withResponse: Bool
    let operationIdentifier: String
  }
  final class MarkerAdmissionGate {
    private let lock = NSLock()
    private var cancelled = false
    private var admitted = false
    func cancel() {
      lock.lock(); defer { lock.unlock() }
      if !admitted { cancelled = true }
    }
    func claim() -> Bool {
      lock.lock(); defer { lock.unlock() }
      guard !cancelled else { return false }
      admitted = true
      return true
    }
  }
  private let markerGateLock = NSLock()
  private var markerGates = [MarkerKey: MarkerAdmissionGate]()
  private func markerGate(_ key: MarkerKey, create: Bool) -> MarkerAdmissionGate? {
    markerGateLock.lock(); defer { markerGateLock.unlock() }
    if let gate = markerGates[key] { return gate }
    // Match the host's bounded group ownership; overload refuses admission.
    guard create, markerGates.count < 128 else { return nil }
    let gate = MarkerAdmissionGate()
    markerGates[key] = gate
    return gate
  }
  private func removeMarkerGate(_ key: MarkerKey) {
    markerGateLock.lock(); defer { markerGateLock.unlock() }
    markerGates.removeValue(forKey: key)
  }

  private enum SynchronousReserve { case pending, issued(UInt64), notCreated }
  private let reserveResultLock = NSLock()
  private var reserveResults: [UInt64: SynchronousReserve] = [:]
  private func storeReserveResult(_ id: UInt64, allocate: () -> UInt64?) -> Bool {
    reserveResultLock.lock()
    guard !reserveResults.keys.contains(id), reserveResults.count < 128 else { reserveResultLock.unlock(); return false }
    reserveResults[id] = .pending
    reserveResultLock.unlock()
    let value = allocate()
    reserveResultLock.lock()
    reserveResults[id] = value.map(SynchronousReserve.issued) ?? .notCreated
    reserveResultLock.unlock()
    return true
  }
  private func reserveUnderOwner(_ reserve: () -> Bool) -> (admitted: Bool, stored: Bool) {
    var stored = false
    guard let owner = orderedGroups?.ownerRetirement else { return (true, reserve()) }
    let admitted = owner.admitLocalReservation { stored = reserve() }
    return (admitted, stored)
  }

  private func takeReserveResult(_ id: UInt64) -> SynchronousReserve? {
    reserveResultLock.lock(); defer { reserveResultLock.unlock() }
    return reserveResults.removeValue(forKey: id)
  }

  private let permitAdmissionLock = NSLock()
  private var permitAdmissions: [UInt64: MarkerAdmissionGate] = [:]

  private func reservePermitAdmission(_ id: UInt64) -> Bool {
    permitAdmissionLock.lock(); defer { permitAdmissionLock.unlock() }
    guard !permitAdmissions.keys.contains(id), permitAdmissions.count < 128 else { return false }
    permitAdmissions[id] = MarkerAdmissionGate()
    return true
  }
  private func permitAdmission(_ id: UInt64, remove: Bool = false) -> MarkerAdmissionGate? {
    permitAdmissionLock.lock(); defer { permitAdmissionLock.unlock() }
    return remove ? permitAdmissions.removeValue(forKey: id) : permitAdmissions[id]
  }

  private let driver: UnifiedBleRustRadioDriver
  private let queue: DispatchQueue
  private let onRestoredPeer: (String) -> Void
  // Queue-confined after construction. The production host is created only
  // after the adapter, so binding installs the host-owned spool in FIFO order
  // with the radio sink before any request can enable notifications.
  private var notificationFailureSpool: UnifiedBleRustNotificationFailureSpool?
  private var notificationFailureSpoolConfigurationAuthority: MobileNotificationFailureSpoolConfigurationAuthority?
  // Claimed during private host installation and retained only by this native
  // adapter. React Native never receives this authority or a recovery handle.
  private var scanFailureRecoveryConfigurationAuthority: MobileScanFailureRecoveryConfigurationAuthority?
  private var scanFailureRecoveryGeneration: UInt64 = 0
  // Configuration is admitted separately from the sidecar's durable state.
  // The host can only see rows already retained by Rust; this fence also owns
  // native routes, retained callback receipts, and an adapter-side replay
  // operation which may still hold the previous spool handle.
  private var notificationFailureSpoolReconfigurationInProgress = false
  private let orderedGroups: UnifiedBleRustOrderedGroupBridge?
  private let controlSinkLock = NSLock()
  private var controlSink: UnifiedBleRustRadioSink?
  // Everything below is confined to `queue`.
  private var sink: UnifiedBleRustRadioSink?
  private var inFlight = [UInt64: String]()
  private var deferredRequestIDs = Set<UInt64>()
  private var deferredOpenIDs = Set<UInt64>()
  private var requestPeers: [UInt64: String] = [:]
  private var pendingMarkerControls: [MarkerKey: PendingMarkerControl] = [:]
  private var pendingMarkerRetryScheduled = false
  private var pendingMarkerRetryDelayMilliseconds = 20
  private var orderedEnables = [UInt64: SubscriptionKey]()
  private var orderedEnableRequests = [UInt64: OrderedActivationKey]()
  private var orderedEnableReceipts = [OrderedActivationKey: OrderedEnableState]()
  private var subscriptionsById = [String: Subscription]()
  // Four ordinary retained callbacks plus one separately reserved terminal
  // receipt. The terminal receipt is consumed only when ordinary capacity is
  // already retained, so the first unaccepted native failure is not silently
  // lost under a full sink.
  private static let ordinaryNotificationReceiptsPerRoute = 4
  private static let overflowNotificationReceiptsPerRoute = 1
  private static let notificationReceiptsPerRoute = ordinaryNotificationReceiptsPerRoute + overflowNotificationReceiptsPerRoute
  // The adapter admits up to 64 notification routes. Reserve every ordinary
  // and terminal receipt for each admitted route up front; a smaller shared
  // pool would silently lower that established route limit (for example,
  // 256 only admitted 51 routes at five receipts each).
  private static let maximumNotificationReceiptRoutes = 64
  private lazy var notificationReceiptBudget = OwnedNotificationReceiptBudget(
    queue: queue,
    capacity: Self.maximumNotificationReceiptRoutes * Self.notificationReceiptsPerRoute)
  private var notificationReceiptRoutes: [String: NotificationReceiptRoute] = [:]
  private var capturedFailureSlots: [UUID: (route: NotificationReceiptRoute, slot: CapturedFailureSlot)] = [:]
  // This is settlement order, not dictionary or capture order: captures may
  // be discarded without reporting a failure.
  private var capturedFailureReady: [UUID] = []
  /// Stable source identities for newly captured callbacks. They are written
  /// into the durable global stream and survive only as diagnostics.
  private var nextFailureSequence: UInt64? = 1
  /// V1 ingress receipts are per-process. A restored durable capture begins
  /// again at one, even when its persistent capture/source ordinals are much
  /// larger. A lost spool ACK retries the same allocated receipt.
  private var nextFailureDeliverySequence: UInt64? = 1
  private var globalSpoolAfterCaptureOrdinal: UInt64 = 0
  private var globalSpoolPending = false
  private var globalSpoolInFlight: (record: NotificationFailureSpoolRecord, deliverySequence: UInt64,
                                    ingressAccepted: Bool)?
  private var capturedFailureRetrying = false
  private var capturedFailureRetryScheduled = false
  private var capturedFailureRetryDelayMilliseconds = 250
  private var subscriptionByInstance = [SubscriptionKey: String]()
  private var announcedRestoredPeers = Set<String>()
  private var pendingRestoredPeers = [MobileRestoredPeer]()
  private var establishedGenerations = [String: String]()
  private var establishedNativeClaims = [String: UUID]()

  private func currentConnection(_ peer: String, generation: String) -> Bool {
    guard establishedGenerations[peer] == generation else { return false }
    guard let radio = driver as? OwnedCoreBluetoothProtocolRadio else { return true }
    guard let identity = establishedNativeClaims[peer] else { return false }
    return radio.connectionClaimIsCurrent(owner: self, peer: peer, identity: identity)
  }

  /// Retain a capture-fenced marker on the radio queue. There is at most one
  /// record per exact bridge receipt, so a repeated pending status cannot
  /// schedule duplicate physical writes.
  private func retainPendingMarker(_ control: PendingMarkerControl) {
    dispatchPrecondition(condition: .onQueue(queue))
    pendingMarkerControls[control.key] = control
    schedulePendingMarkerRetry()
  }

  private func schedulePendingMarkerRetry() {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !pendingMarkerControls.isEmpty, !pendingMarkerRetryScheduled else { return }
    pendingMarkerRetryScheduled = true
    let delay = pendingMarkerRetryDelayMilliseconds
    pendingMarkerRetryDelayMilliseconds = min(delay * 2, 250)
    queue.asyncAfter(deadline: .now() + .milliseconds(delay)) { [weak self] in
      guard let self else { return }
      self.pendingMarkerRetryScheduled = false
      self.retryPendingMarkers()
    }
  }

  /// Attempts only controls whose exact receipt is still pending. The bridge
  /// re-checks source generation, capture ownership and cancellation before
  /// the CoreBluetooth write admission closure executes.
  private func retryPendingMarkers() {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let bridge = orderedGroups, let radio = driver as? OwnedCoreBluetoothProtocolRadio else {
      pendingMarkerControls.removeAll()
      return
    }
    for key in Array(pendingMarkerControls.keys) {
      guard let control = pendingMarkerControls[key] else { continue }
      do {
        switch try bridge.retryWaitingMarker(groupID: key.group, opaqueSourceGeneration: key.source, id: key.marker) {
        case .waiting:
          continue
        case .settled:
          pendingMarkerControls.removeValue(forKey: key)
          continue
        case .ready:
          break
        }
        try dispatchPendingMarker(control, bridge: bridge, radio: radio)
        pendingMarkerControls.removeValue(forKey: key)
        pendingMarkerRetryDelayMilliseconds = 20
      } catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {
        // The queue-local admission closure observed a newer capture. Keep the
        // same receipt and retry it; no physical write has been dispatched.
      } catch {
        let failure = Self.markerFailure(error as NSError, dispatched: false)
        _ = try? bridge.markerRejected(groupID: key.group, id: key.marker, failure: failure)
        pendingMarkerControls.removeValue(forKey: key)
      }
    }
    schedulePendingMarkerRetry()
  }

  private func dispatchPendingMarker(_ control: PendingMarkerControl,
                                     bridge: UnifiedBleRustOrderedGroupBridge,
                                     radio: OwnedCoreBluetoothProtocolRadio) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    guard currentConnection(control.instance.peerId, generation: control.connection),
          radio.orderedSnapshotIdentities.matches(control.instance.peerId, control.snapshot) else {
      throw NSError(domain: "ubm.marker.stale", code: 1,
        userInfo: [NSLocalizedDescriptionKey: "marker connection claim is no longer current"])
    }
    try bridge.validateMarkerTarget(groupID: control.key.group, opaqueSourceGeneration: control.key.source,
      peer: control.instance.peerId, snapshot: control.snapshot)
    guard let path = Self.path(control.instance) else {
      throw NSError(domain: "ubm.marker.stale", code: 1,
        userInfo: [NSLocalizedDescriptionKey: "marker characteristic occurrence is no longer current"])
    }
    let address = CharacteristicAddress(peerIdentifier: control.instance.peerId,
      serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(control.instance.serviceUuid),
      serviceOccurrence: path.service,
      characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(control.instance.characteristicUuid),
      characteristicOccurrence: path.characteristic)
    _ = try radio.writeWithOrderedMarker(address: address, value: control.value, withResponse: control.withResponse,
      operationIdentifier: control.operationIdentifier, admit: {
        guard self.currentConnection(control.instance.peerId, generation: control.connection),
              radio.orderedSnapshotIdentities.matches(control.instance.peerId, control.snapshot) else {
          throw NSError(domain: "ubm.marker.stale", code: 1,
            userInfo: [NSLocalizedDescriptionKey: "marker connection claim is no longer current"])
        }
        try bridge.checkMarkerCaptureOrder(groupID: control.key.group, opaqueSourceGeneration: control.key.source)
        guard self.markerGate(control.key, create: false)?.claim() == true else {
          throw NSError(domain: "ubm.marker.cancelled", code: 1,
            userInfo: [NSLocalizedDescriptionKey: "marker cancelled before admission"])
        }
        let ordinal = try bridge.admitMarker(groupID: control.key.group, opaqueSourceGeneration: control.key.source, id: control.key.marker)
        try bridge.markerAdmitted(groupID: control.key.group, id: control.key.marker, ordinal: ordinal)
        return ordinal
      }, completion: { error in
        let failure = error.map { Self.markerFailure($0, dispatched: true) }
        do { try bridge.markerSettled(groupID: control.key.group, id: control.key.marker, failure: failure) }
        catch { NSLog("[UnifiedBleRustRadioAdapter] marker retry settlement failed: %@", String(describing: error)) }
      })
    _ = try bridge.markerStarted(groupID: control.key.group, id: control.key.marker)
  }

  private var scanActive = false
  private var counters = UnifiedBleRustRadioAdapterCounters()

  private func newOperationIdentifier(_ requestID: UInt64) -> String {
    "ubm-rust-\(UUID().uuidString)-\(requestID)"
  }

  private func subscriptionIdentifier(_ operationIdentifier: String) -> String {
    "ubm-rust-sub-\(operationIdentifier)"
  }

  func startOwnedScan(serviceUUIDs: [String], operationIdentifier: String,
                              completion: @escaping (NSError?) -> Void) {
    if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
      radio.startScan(borrower: self, serviceUUIDs: serviceUUIDs, allowDuplicates: true,
                      operationIdentifier: operationIdentifier, completion: completion)
    } else {
      driver.startScan(serviceUUIDs: serviceUUIDs, allowDuplicates: true,
                       operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  func stopOwnedScan(operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
      radio.stopScan(borrower: self, completion: completion)
    } else {
      driver.stopScan(operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  private func forgetSubscription(_ identifier: String) {
    subscriptionsById.removeValue(forKey: identifier)
    (driver as? OwnedCoreBluetoothProtocolRadio)?.removeNotificationRoute(identifier, borrower: self)
    if let route = notificationReceiptRoutes[identifier] {
      route.retired = true
      retireNotificationReceiptRouteIfIdle(route)
    }
  }

  private func retireNotificationReceiptRouteIfIdle(_ route: NotificationReceiptRoute) {
    guard route.retired, !route.slots.contains(where: { $0.busy }), route.coalescedOverflow == nil else { return }
    guard notificationReceiptRoutes[route.subscription.identifier] === route else { return }
    notificationReceiptRoutes.removeValue(forKey: route.subscription.identifier)
    route.reservation.release()
  }

  private func releaseCapturedFailure(_ slot: CapturedFailureSlot, route: NotificationReceiptRoute) {
    guard slot.busy, slot.captureReleased, slot.ingress == nil else { return }
    capturedFailureSlots.removeValue(forKey: slot.key)
    slot.busy = false
    slot.settled = false
    slot.captureReleased = false
    slot.sequence = nil
    slot.refusalLogged = false
    retireNotificationReceiptRouteIfIdle(route)
  }

  private func orderedActivationKey(_ activation: MobileOrderedActivation) -> OrderedActivationKey? {
    guard case let .opaque(source) = activation.sourceGeneration, !activation.connectionGeneration.isEmpty else { return nil }
    return OrderedActivationKey(subscription: SubscriptionKey(instance: InstanceKey(activation.instance), epoch: activation.activationEpoch),
      connection: activation.connectionGeneration, source: source, group: activation.groupId,
      member: activation.member.memberId, token: activation.member.sourceMember, memberEpoch: activation.member.epoch)
  }

  private func releaseOrderedActivations(peer: String, connection: String) {
    for key in Array(orderedEnableReceipts.keys) where key.subscription.instance.peerId == peer && key.connection == connection {
      orderedEnableReceipts[key] = .released
    }
  }

  init(driver: UnifiedBleRustRadioDriver, onRestoredPeer: @escaping (String) -> Void = { _ in },
       onOrderedGroupWake: @escaping (UInt64, String) -> Void = { _, _ in },
       orderedGroupBridge: UnifiedBleRustOrderedGroupBridge? = nil,
       notificationFailureSpool: UnifiedBleRustNotificationFailureSpool? = nil) {
    self.driver = driver
    self.onRestoredPeer = onRestoredPeer
    self.notificationFailureSpool = notificationFailureSpool
    queue = driver.workQueue
    if let orderedGroupBridge {
      orderedGroups = orderedGroupBridge
    } else if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
      orderedGroups = UnifiedBleRustOrderedGroupBridge(queue: driver.workQueue, factory: { routes, capacity, bytes, wake in
        let addresses = routes.map { route in
          CharacteristicAddress(peerIdentifier: route.peer,
            serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(route.serviceUUID),
            serviceOccurrence: route.serviceOccurrence,
            characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(route.characteristicUUID),
            characteristicOccurrence: route.characteristicOccurrence)
        }
        do {
          return try radio.createOrderedSourceGroup(addresses: addresses, capacity: capacity, byteCapacity: bytes, wake: wake)
        } catch let error as OwnedOrderedSourceFactoryNoEffect {
          throw UnifiedBleRustOrderedGroupBridge.FactoryNoEffect(underlying: error.underlying)
        }
      }, normalize: { route in
        OrderedNotificationRoute(peer: route.peer,
          serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(route.serviceUUID),
          serviceOccurrence: route.serviceOccurrence,
          characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(route.characteristicUUID),
          characteristicOccurrence: route.characteristicOccurrence)
      }, validateSnapshot: { peer, identity in
        radio.orderedSnapshotIdentities.matches(peer, identity)
      }, wake: onOrderedGroupWake)
    } else {
      orderedGroups = nil
    }
    super.init()
    if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
      radio.orderedFailureCapture = { [weak self] route, failure, claim in
        self?.orderedGroups?.capturePhysicalFailure(route: route, failure: failure, claim: claim)
      }
    }
    if orderedGroupBridge == nil, let radio = driver as? OwnedCoreBluetoothProtocolRadio {
      orderedGroups?.setSnapshotValidator { [weak self, weak radio] peer, identity in
        guard let self, let radio, let generation = self.establishedGenerations[peer],
              self.currentConnection(peer, generation: generation) else { return false }
        return radio.orderedSnapshotIdentities.matches(peer, identity)
      }
    }
  }

  func admitRadioBorrower(_ attach: () -> Bool) -> Bool {
    guard let owner = orderedGroups?.ownerRetirement else { return attach() }
    return owner.admitBorrower(self, attach: attach)
  }

  func didReleaseRadioBorrower() {
    orderedGroups?.ownerRetirement.releaseBorrower(self)
  }

  /// The Rust binding must schedule group operations on `workQueue`.
  func orderedGroupsOnRadioQueue() -> UnifiedBleRustOrderedGroupBridge? {
    dispatchPrecondition(condition: .onQueue(queue))
    return orderedGroups
  }

  /// Connects one installed host for this adapter's lifetime. An outstanding
  /// callback belongs to that host even if another installation is attempted.
  /// Replacing the route requires a joined retirement, not a sink assignment.
  @discardableResult
  func bind(sink: UnifiedBleRustRadioSink) -> Bool {
    if let owner = orderedGroups?.ownerRetirement {
      return owner.admitAttachment { ticket in bindAdmitted(sink: sink, owner: owner, ticket: ticket) }
    }
    return bindAdmitted(sink: sink, owner: nil, ticket: nil).accepted
  }

  func installNotificationFailureSpool(_ spool: UnifiedBleRustNotificationFailureSpool?) {
    queue.async { [self] in
      notificationFailureSpool = spool
      retryCapturedNotificationFailures()
    }
  }

  /// The host gives this opaque capability to the production binding exactly
  /// once. Keeping it on the adapter makes changing the durable spool follow
  /// the same route/callback/replay fence as every other adapter-owned change.
  @discardableResult
  func installNotificationFailureSpoolConfigurationAuthority(_ authority: MobileNotificationFailureSpoolConfigurationAuthority) -> Bool {
    queue.sync {
      // A host binding owns exactly one non-forgeable configuration authority.
      // Replacing it would let a second binding change this adapter's spool
      // after the first binding had established its callback/replay fence.
      guard notificationFailureSpoolConfigurationAuthority == nil else { return false }
      notificationFailureSpoolConfigurationAuthority = authority
      return true
    }
  }

  /// Installs the single host-bound scan-recovery authority before the adapter
  /// is published. A later trusted recording-policy owner may use it with an
  /// opaque recording id; neither sidecar paths nor credentials cross Swift.
  @discardableResult
  func installScanFailureRecoveryConfigurationAuthority(_ authority: MobileScanFailureRecoveryConfigurationAuthority) -> Bool {
    queue.sync {
      guard scanFailureRecoveryConfigurationAuthority == nil else { return false }
      scanFailureRecoveryConfigurationAuthority = authority
      scanFailureRecoveryGeneration &+= 1
      return true
    }
  }

  /// Attaches only a recording already selected by the native embedding.
  /// This deliberately returns a Swift-only holder: neither recording ids nor
  /// recovery capabilities are TurboModule values.
  func openScanFailureRecovery(recordingId: String) throws -> UnifiedBleRustScanFailureRecoveryLease {
    let state: (MobileScanFailureRecoveryConfigurationAuthority, UInt64)? = queue.sync {
      guard let authority = scanFailureRecoveryConfigurationAuthority else { return nil }
      return (authority, scanFailureRecoveryGeneration)
    }
    guard let (authority, generation) = state else {
      throw MobileCoreError.Failed(code: "capability.unsupported", domain: "restoration",
        operation: "scan.failure-recovery.open", detail: "native scan recovery is unavailable")
    }
    let recovery = try authority.attachRecordingV2(recordingId: recordingId)
    guard scanFailureRecoveryLeaseIsLive(generation) else {
      throw MobileCoreError.Failed(code: "lifecycle.destroyed", domain: "restoration",
        operation: "scan.failure-recovery.open", detail: "scan recovery binding changed while opening")
    }
    return UnifiedBleRustScanFailureRecoveryLease(adapter: self, generation: generation, recovery: recovery)
  }

  fileprivate func scanFailureRecoveryLeaseIsLive(_ generation: UInt64) -> Bool {
    queue.sync {
      scanFailureRecoveryConfigurationAuthority != nil && scanFailureRecoveryGeneration == generation
    }
  }

  /// Runs a spool configuration change only after the adapter has retired all
  /// sources that could append through its current host binding. The closure
  /// deliberately runs off the radio queue: configuring the Rust sidecar can
  /// perform I/O, while this queue-local reservation prevents a new route or
  /// callback capture from being admitted until it completes.
  func withNotificationFailureSpoolReconfiguration(_ configure: () -> (result: String, installed: Bool)) -> (admitted: Bool, result: String?) {
    let admitted = queue.sync { () -> Bool in
      guard !notificationFailureSpoolReconfigurationInProgress,
            notificationReceiptRoutes.isEmpty,
            capturedFailureSlots.isEmpty,
            capturedFailureReady.isEmpty,
            !globalSpoolPending,
            globalSpoolInFlight == nil else { return false }
      notificationFailureSpoolReconfigurationInProgress = true
      return true
    }
    guard admitted else { return (false, nil) }
    let configured = configure()
    queue.sync {
      if configured.installed {
        // Capture ordinals belong to the installed spool, not the adapter.
        // Do not reset the V1 ingress delivery sequence: the Rust receipt
        // stream remains continuous across an A -> B replacement.
        globalSpoolAfterCaptureOrdinal = 0
        globalSpoolPending = false
        globalSpoolInFlight = nil
      }
      notificationFailureSpoolReconfigurationInProgress = false
      retryCapturedNotificationFailures()
    }
    return (true, configured.result)
  }

  /// The only adapter route to change the Rust spool.  Keep acquisition of
  /// the opaque authority and the sidecar call inside the same reservation so
  /// no caller can accidentally use a valid authority outside the fence.
  func reconfigureNotificationFailureSpoolV2(recordingId: String, maxBytes: String,
                                             maxRecords: String) -> (admitted: Bool, result: String?) {
    withNotificationFailureSpoolReconfiguration { [weak self] in
      guard let self else {
        return ("{\"status\":\"error\",\"error\":{\"domain\":\"lifecycle\",\"code\":\"lifecycle.invalid-state\"}}", false)
      }
      let authority = self.queue.sync { self.notificationFailureSpoolConfigurationAuthority }
      guard let authority else {
        return ("{\"status\":\"error\",\"error\":{\"domain\":\"lifecycle\",\"code\":\"lifecycle.invalid-state\"}}", false)
      }
      let result = authority.configureV2(recordingId: recordingId, maxBytes: maxBytes, maxRecords: maxRecords)
      return (result, Self.notificationFailureSpoolConfigured(result))
    }
  }

  private static func notificationFailureSpoolConfigured(_ result: String) -> Bool {
    guard let data = result.data(using: .utf8),
          let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return false }
    return body["status"] as? String == "configured"
  }

  private func bindAdmitted(sink: UnifiedBleRustRadioSink, owner: UnifiedBleRustOrderedGroupBridge.OwnerRetirement?,
                            ticket: UUID?) -> (accepted: Bool, queued: Bool) {
    controlSinkLock.lock()
    if let existing = controlSink {
      let same = existing === sink
      controlSinkLock.unlock()
      if !same { NSLog("[UnifiedBleRustRadioAdapter] refused replacement of a live host binding") }
      return (same, false)
    }
    controlSink = sink
    controlSinkLock.unlock()
    queue.async { [self] in
      if let owner, let ticket, !owner.beginAttachment(ticket) { return }
      self.sink = sink
      self.driver.restoredPeerSnapshots { snapshots in
        if let owner, let ticket, !owner.beginAttachmentCallback(ticket) {
          NSLog("[UnifiedBleRustRadioAdapter] ignored stale restored-peer attachment callback")
          return
        }
        self.announceRestored(snapshots.compactMap(Self.restoredPeer))
        if let owner, let ticket { owner.finishAttachment(ticket) }
      }
    }
    return (true, true)
  }

  /// Capture the binding at group installation; another adapter sharing the
  /// bridge cannot replace this group's callback destination.
  private func orderedWakeRoute() -> ((UInt64, String) -> Void)? {
    guard let sink else { return nil }
    return { [weak self, sink] groupID, generation in
      guard let self else { return }
      let status = sink.ingest(ingress: .orderedWake(generation: .opaque(value: generation), groupId: groupID))
      guard status != "accepted" else { return }
      NSLog("[UnifiedBleRustRadioAdapter] ordered group wake was refused: %@", status)
      do {
        try self.orderedGroups?.failWake(groupID: groupID, opaqueSourceGeneration: generation)
        let failureStatus = sink.ingest(ingress: .orderedWakeFailure(generation: .opaque(value: generation), groupId: groupID))
        if failureStatus != "accepted" {
          NSLog("[UnifiedBleRustRadioAdapter] ordered wake failure report was refused: %@", failureStatus)
        }
      } catch {
        NSLog("[UnifiedBleRustRadioAdapter] ordered wake failure could not be retained: %@", String(describing: error))
      }
    }
  }

  func adapterCounters(completion: @escaping (UnifiedBleRustRadioAdapterCounters) -> Void) {
    queue.async { completion(self.counters) }
  }

  // MARK: - MobilePlatformRadio

  func orderedAdmissionOwner(version: UInt32) -> String {
    orderedGroups?.admissionOwner(version: version) ?? ""
  }

  /// Exact source and notification teardown remain available after admission
  /// is fenced. Their identity and cleanup checks still apply.
  private static func isFencedSourceSettlement(_ request: MobileRadioRequest) -> Bool {
    switch request {
    case .orderedRecoverOpen, .orderedAcknowledgeOpen,
         .orderedRetire, .orderedClose, .orderedDrain, .orderedAcknowledgeBatch, .orderedDrainV2, .orderedAcknowledgeBatchV2,
         .orderedSeal, .orderedConfirmCleanup, .orderedRemove, .orderedForgetRemovalReceipt,
         .orderedEnableSettlement, .orderedDisableNotifications, .orderedForgetEnableReceipt,
         .orderedGraphAuthorityReserveStatus, .orderedGraphAuthorityReserveFence, .orderedGraphAuthorityReserveForget,
         .orderedGraphAuthorityStatus, .orderedGraphAuthorityFenceOpen, .orderedGraphAuthorityForgetOpen,
         .orderedGraphAuthorityClose, .orderedGraphAuthorityForget,
         .orderedGraphPermitReserveStatus, .orderedGraphPermitReserveFence, .orderedGraphPermitForgetRequest,
         .orderedGraphPermitRelease, .orderedGraphPermitForget,
         .orderedMarkerWriteSettlement, .orderedCancelMarkerWrite, .orderedForgetMarkerWriteReceipt:
      return true
    default:
      return false
    }
  }

  func submit(request: MobileRadioRequest) {
    switch request {
    case let .ownerFence(id, owner, retirement), let .ownerStatus(id, owner, retirement):
      guard let bridge = orderedGroups else {
        deliverControl(id, Self.unsupported("ordered owner unavailable")); return
      }
      let isFence: Bool
      if case .ownerFence = request { isFence = true } else { isFence = false }
      let reply = bridge.ownerRetirement.control(owner: owner, expected: bridge.admissionOwner(version: 3),
                                                 retirement: retirement, fence: isFence)
      let state: MobileOwnerRetirementState
      switch reply {
      case .pending:
        // Until all native and relay cleanup paths are joined, retain a
        // conservative pending receipt even when the queue appears empty.
        state = .fencedPending(outstandingClasses: [.attachments, .reservations, .nativeWork, .callbacks, .cleanup], failures: [])
      case .wrongOwner: state = .rejected(reason: .wrongOwner)
      case .conflicting: state = .rejected(reason: .conflictingRetirement)
      case .malformed: state = .rejected(reason: .malformed)
      }
      deliverControl(id, .ownerRetirement(ownerIncarnation: owner, retirementId: retirement, state: state))
      return
    default: break
    }
    if let bridge = orderedGroups, !bridge.ownerRetirement.admitsWork() {
      switch request {
      case let .orderedOpenV2(id, value):
        do {
          let identity = try UnifiedBleRustOrderedRadioAdapter.identity(value)
          guard id == identity.requestID else { throw UnifiedBleRustOrderedGroupBridge.AdmissionSlots.Error.identity }
          let outcome = try bridge.admissionSlots.fence(identity, failure: .init(kind: "cancelled", detail: "native owner fenced before admission"))
          deliverControl(id, UnifiedBleRustOrderedRadioAdapter.result(identity, outcome))
        } catch { deliverControl(id, Self.failed("platform", "invalid ordered open fence: \(error)", dispatched: false)) }
        return
      case .orderedRecoverOpenV2, .orderedAcknowledgeOpenV2:
        // Existing receipts remain queryable and retireable after the fence.
        break
      default:
        if !Self.isFencedSourceSettlement(request) {
          deliverControl(Self.requestId(of: request), Self.failed("cancelled", "native owner is fenced", dispatched: false))
          return
        }
      }
    }
    var openAdmission: (UnifiedBleRustOrderedGroupBridge.OpenRequest, UnifiedBleRustOrderedGroupBridge.OpenToken)?
    var slotAdmission: UnifiedBleRustOrderedGroupBridge.AdmissionSlots.Identity?
    switch request {
    case let .orderedOpenV2(id, value):
      guard let bridge = orderedGroups else { deliverControl(id, Self.failed("unsupported", "ordered source unavailable", dispatched: false)); return }
      do {
        let identity = try UnifiedBleRustOrderedRadioAdapter.identity(value)
        guard id == identity.requestID else { throw UnifiedBleRustOrderedGroupBridge.AdmissionSlots.Error.identity }
        switch try bridge.admissionSlots.reserve(identity) {
        case .reserved: slotAdmission = identity
        case .pending: deliverControl(id, UnifiedBleRustOrderedRadioAdapter.result(identity, .pending)); return
        case let .terminal(outcome): deliverControl(id, UnifiedBleRustOrderedRadioAdapter.result(identity, outcome)); return
        }
      } catch { deliverControl(id, Self.failed("platform", "invalid ordered open admission: \(error)", dispatched: false)); return }
    case let .orderedRecoverOpenV2(id, value):
      guard let bridge = orderedGroups else { deliverControl(id, Self.failed("unsupported", "ordered source unavailable", dispatched: false)); return }
      do {
        let identity = try UnifiedBleRustOrderedRadioAdapter.identity(value)
        let outcome = try bridge.admissionSlots.fence(identity, failure: .init(kind: "cancelled", detail: "ordered open fenced before admission"))
        deliverControl(id, UnifiedBleRustOrderedRadioAdapter.result(identity, outcome))
      } catch { deliverControl(id, Self.failed("platform", "invalid ordered open fence: \(error)", dispatched: false)) }
      return
    case let .orderedAcknowledgeOpenV2(id, value):
      guard let bridge = orderedGroups else { deliverControl(id, Self.failed("unsupported", "ordered source unavailable", dispatched: false)); return }
      do {
        let identity = try UnifiedBleRustOrderedRadioAdapter.identity(value)
        let retired = try bridge.admissionSlots.retire(identity)
        deliverControl(id, .orderedOpenRetiredV2(slotIdentity: .init(ownerIncarnation: retired.owner.uuidString.lowercased(), slot: retired.slot, slotGeneration: retired.generation)))
      } catch { deliverControl(id, Self.failed("platform", "ordered open not retired: \(error)", dispatched: false)) }
      return
    case let .orderedOpen(id, groupID, _, _, _, members, _, _):
      if let bridge = orderedGroups {
        let key = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: id, groupID: groupID,
          peer: members.first?.instance.peerId ?? "", fingerprint: AnyHashable(request))
        let refusal = MobilePlatformFailure(kind: "busy", gattStatus: nil, nativeDomain: nil, nativeCode: nil,
          detail: "ordered open admission unavailable", dispatched: false)
        let refusalAnswer = MobileRadioCompletion.orderedOpenFailed(failure: refusal, ownership: .notCreated)
        do { openAdmission = (key, try bridge.reserveOpen(key, refusal: .completed(AnyHashable(refusalAnswer)))) }
        catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionConflict {
          // An existing receipt may belong to this exact request. A duplicate
          // submission cannot claim that its original execution had no effect.
          queue.async { self.deliver(id, Self.failed("busy", "ordered open identity is already owned", dispatched: true)) }
          return
        }
        catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionUnavailable {
          queue.async { self.deliver(id, refusalAnswer) }
          return
        }
        catch {
          let failure = MobilePlatformFailure(kind: "busy", gattStatus: nil, nativeDomain: nil, nativeCode: nil,
            detail: "ordered open admission receipt unavailable", dispatched: true)
          queue.async { self.deliver(id, .orderedOpenFailed(failure: failure, ownership: .indeterminate(recoveryKey: groupID))) }
          return
        }
      }
    case let .orderedGraphAuthorityReserve(id, token):
      let radio = driver as? OwnedCoreBluetoothProtocolRadio
      let result = reserveUnderOwner { storeReserveResult(id, allocate: { radio?.orderedGraphAuthorities.reserve(request: token) }) }
      guard result.admitted else {
        deliverControl(id, Self.failed("cancelled", "native owner fenced before reservation", dispatched: false)); return
      }
      guard result.stored else {
        queue.async { self.deliver(id, Self.failed("busy", "ordered reserve admission capacity exhausted", dispatched: false)) }
        return
      }
    case let .orderedGraphPermitReserve(id, authority, token, peer, connection, snapshot):
      let identity = OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let key = OwnedOrderedGraphPermitRegistry.Request(authority: authority, request: token, peer: peer, connection: connection, snapshot: identity)
      let radio = driver as? OwnedCoreBluetoothProtocolRadio
      let result = reserveUnderOwner { storeReserveResult(id, allocate: { radio?.orderedGraphAuthorities.issuePermit(key) }) }
      guard result.admitted else {
        deliverControl(id, Self.failed("cancelled", "native owner fenced before reservation", dispatched: false)); return
      }
      guard result.stored else {
        queue.async { self.deliver(id, Self.failed("busy", "ordered reserve admission capacity exhausted", dispatched: false)) }
        return
      }
    case let .orderedGraphAuthorityOpen(id, receipt):
      guard reservePermitAdmission(id) else {
        _ = (driver as? OwnedCoreBluetoothProtocolRadio)?.orderedGraphAuthorities.cancelOpen(receipt: receipt)
        queue.async { self.deliver(id, Self.failed("busy", "ordered admission capacity exhausted", dispatched: false)) }
        return
      }
    case let .orderedGraphPermitAcquire(id, _, _, _, _, _):
      guard reservePermitAdmission(id) else {
        queue.async { self.deliver(id, Self.failed("busy", "ordered admission capacity exhausted", dispatched: false)) }
        return
      }
    case let .orderedMarkerWrite(_, .opaque(source), group, marker, _, _, _, _, _):
      _ = markerGate(MarkerKey(source: source, group: group, marker: marker), create: true)
    case let .orderedCancelMarkerWrite(_, .opaque(source), group, marker):
      markerGate(MarkerKey(source: source, group: group, marker: marker), create: false)?.cancel()
    default: break
    }
    let admission = openAdmission
    let slot = slotAdmission
    queue.async { self.start(request, openAdmission: admission, slotAdmission: slot) }
  }

  /// Rust dropped its wait (budget expiry, `op.cancel`, abort). The OS work is
  /// withdrawn and the request is answered `cancelled` so every request keeps
  /// exactly one answer; Rust counts it as a late completion.
  func cancel(requestId: UInt64) {
    if orderedGroups?.admissionSlots.cancel(requestID: requestId,
      failure: .init(kind: "cancelled", detail: "ordered open cancelled before admission")) == true { return }
    orderedGroups?.cancelOpen(requestID: requestId)
    permitAdmission(requestId)?.cancel()
    queue.async { self.cancelOnQueue(requestId) }
  }

  private func cancelOnQueue(_ requestId: UInt64) {
    if deferredRequestIDs.remove(requestId) != nil {
      requestPeers.removeValue(forKey: requestId)
      guard inFlight.removeValue(forKey: requestId) != nil else { return }
      counters.cancelledRequests += 1
      if deferredOpenIDs.remove(requestId) != nil {
        deliver(requestId, UnifiedBleRustOrderedRadioAdapter.fencedOpen())
      } else {
        deliver(requestId, Self.failed("cancelled", "cancelled before native admission", dispatched: false))
      }
      return
    }
    if let peer = requestPeers[requestId], let gate = driver.orderedGraphGate(peer: peer), gate.isHolding {
      guard let operationIdentifier = inFlight[requestId] else { return }
      gate.enqueue(reject: {
        guard self.inFlight[requestId] == operationIdentifier else { return }
        self.finish(requestId, Self.failed("busy", "ordered graph deferred cancellation capacity exhausted", dispatched: true))
      }) {
        guard self.inFlight[requestId] == operationIdentifier else { return }
        self.cancelOnQueue(requestId)
      }
      return
    }
    // Ordered activation owns its eventual native result even if Rust's
    // waiter disappears. The host retains the late exact-epoch completion.
    if orderedEnables[requestId] != nil { return }
    requestPeers.removeValue(forKey: requestId)
    guard let operationIdentifier = inFlight.removeValue(forKey: requestId) else { return }
    counters.cancelledRequests += 1
    let identifier = subscriptionIdentifier(operationIdentifier)
    let cleanup: (NSDictionary) -> Void = { result in
      // Cancellation requests cleanup; only the native owner's release
      // receipt establishes that this exact route can no longer call back.
      if result["state"] as? String == "released" {
        self.forgetSubscription(identifier)
      } else if self.notificationReceiptRoutes[identifier] != nil {
        NSLog("[UnifiedBleRustRadioAdapter] notification cancellation cleanup remains unresolved: %@",
          (result["state"] as? String) ?? "unrecognized response")
      }
    }
    if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
      radio.cancelOperation(operationIdentifier, borrower: self, completion: cleanup)
    } else {
      driver.cancelOperation(operationIdentifier, completion: cleanup)
    }
    deliver(requestId, Self.failed("cancelled", "cancelled by the Rust owner", dispatched: true))
  }

  // MARK: - Requests

  private func orderedControlPeer(_ request: MobileRadioRequest) -> String? {
    let group: UInt64
    switch request {
    case let .connect(_, peer, _, _, _), let .disconnect(_, peer), let .discover(_, peer): return peer
    case let .read(_, instance), let .write(_, instance, _, _), let .readDescriptor(_, instance, _, _),
         let .writeDescriptor(_, instance, _, _, _), let .enableNotifications(_, instance, _, _, _),
         let .disableNotifications(_, instance, _): return instance.peerId
    case let .orderedOpenV2(_, identity): return identity.peerId
    case .orderedRecoverOpenV2, .orderedAcknowledgeOpenV2: return nil
    case let .orderedOpen(_, _, _, _, _, members, _, _): return members.first?.instance.peerId
    case let .orderedEnableNotifications(_, activation, _, _), let .orderedDisableNotifications(_, activation),
         let .orderedEnableSettlement(_, activation), let .orderedForgetEnableReceipt(_, activation): return activation.instance.peerId
    // Recovery is a fence, not a graph operation: it must overtake a
    // deferred open instead of joining that open behind the permit.
    case .orderedRecoverOpen, .orderedAcknowledgeOpen: return nil
    case let .orderedRegister(_, _, id, _), let .orderedRetire(_, _, id, _),
         let .orderedDrain(_, _, id), let .orderedAcknowledgeBatch(_, _, id, _),
         let .orderedDrainV2(_, _, _, id), let .orderedAcknowledgeBatchV2(_, _, _, id, _),
         let .orderedClose(_, _, id), let .orderedSeal(_, _, id), let .orderedBarrier(_, _, id, _),
         let .orderedRetainClaim(_, _, id, _), let .orderedConfirmCleanup(_, _, id, _),
         let .orderedRemove(_, _, id), let .orderedForgetRemovalReceipt(_, _, id),
         let .orderedMarkerWrite(_, _, id, _, _, _, _, _, _), let .orderedMarkerWriteSettlement(_, _, id, _),
         let .orderedCancelMarkerWrite(_, _, id, _), let .orderedForgetMarkerWriteReceipt(_, _, id, _): group = id
    default: return nil
    }
    return orderedGroups?.peerForGroup(group)
  }

  private func start(_ request: MobileRadioRequest,
                     openAdmission: (UnifiedBleRustOrderedGroupBridge.OpenRequest, UnifiedBleRustOrderedGroupBridge.OpenToken)? = nil,
                     slotAdmission: UnifiedBleRustOrderedGroupBridge.AdmissionSlots.Identity? = nil) {
    // These requests allocated their receipt synchronously at submission.
    // Resolving that retained result is necessary to avoid hiding an effect.
    let resolvesReservation: Bool
    switch request {
    case .orderedGraphAuthorityReserve, .orderedGraphPermitReserve: resolvesReservation = true
    default: resolvesReservation = false
    }
    if let bridge = orderedGroups, !bridge.ownerRetirement.admitsWork(), !resolvesReservation,
       !Self.isFencedSourceSettlement(request) {
      let id = Self.requestId(of: request)
      requestPeers.removeValue(forKey: id)
      deferredRequestIDs.remove(id)
      deferredOpenIDs.remove(id)
      if let slotAdmission {
        do {
          let outcome = try bridge.admissionSlots.fence(slotAdmission, failure: .init(kind: "cancelled", detail: "native owner fenced before admission"))
          deliver(id, UnifiedBleRustOrderedRadioAdapter.result(slotAdmission, outcome))
        } catch { deliver(id, Self.failed("platform", "ordered open fence could not be retained", dispatched: false)) }
      } else if let (key, token) = openAdmission {
        let fenced = UnifiedBleRustOrderedRadioAdapter.fencedOpen()
        let outcome = bridge.cancelOpen(key, token: token, outcome: .completed(AnyHashable(fenced)))
        let answer: MobileRadioCompletion
        switch outcome {
        case .notCreated?: answer = fenced
        case let .completed(value)?:
          answer = (value.base as? MobileRadioCompletion) ?? Self.failed("platform", "ordered open receipt has an invalid answer", dispatched: true)
        default:
          answer = Self.failed("platform", "ordered open ownership could not be fenced", dispatched: true)
        }
        deliver(id, answer)
      } else {
        // An authority receipt may already exist even though its queued open
        // never ran. Settle that exact receipt, then release the admission.
        permitAdmission(id, remove: true)?.cancel()
        if case let .orderedGraphAuthorityOpen(_, receipt) = request,
           let radio = driver as? OwnedCoreBluetoothProtocolRadio {
          _ = radio.orderedGraphAuthorities.cancelOpen(receipt: receipt)
        }
        deliver(id, Self.failed("cancelled", "native owner fenced before admission", dispatched: false))
      }
      return
    }
    if let peer = orderedControlPeer(request), let gate = driver.orderedGraphGate(peer: peer), gate.isHolding {
      let bytes: Int
      switch request {
      case let .orderedMarkerWrite(_, _, _, _, _, _, _, value, _), let .write(_, _, value, _), let .writeDescriptor(_, _, _, _, value): bytes = value.count
      default: bytes = 0
      }
      let requestID = Self.requestId(of: request)
      let deferredOperation = newOperationIdentifier(requestID)
      inFlight[requestID] = deferredOperation
      requestPeers[requestID] = peer
      deferredRequestIDs.insert(requestID)
      if openAdmission != nil { deferredOpenIDs.insert(requestID) }
      gate.enqueue(bytes: bytes, reject: {
        guard self.inFlight[requestID] == deferredOperation else { return }
        let answer: MobileRadioCompletion
        if let slotAdmission, let bridge = self.orderedGroups {
          do {
            let outcome = try bridge.admissionSlots.fence(slotAdmission, failure: .init(kind: "busy", detail: "ordered graph deferred request capacity exhausted"))
            answer = UnifiedBleRustOrderedRadioAdapter.result(slotAdmission, outcome)
          } catch { answer = Self.failed("platform", "ordered open admission is stale", dispatched: false) }
        } else if let (key, token) = openAdmission {
          let failure = MobilePlatformFailure(kind: "busy", gattStatus: nil, nativeDomain: nil, nativeCode: nil,
            detail: "ordered graph deferred request capacity exhausted", dispatched: false)
          let refused = MobileRadioCompletion.orderedOpenFailed(failure: failure, ownership: .notCreated)
          let outcome = self.orderedGroups?.cancelOpen(key, token: token, outcome: .completed(AnyHashable(refused)))
          if case .notCreated? = outcome {
            answer = UnifiedBleRustOrderedRadioAdapter.fencedOpen()
          } else {
            answer = refused
          }
        } else {
          answer = Self.failed("busy", "ordered graph deferred request capacity exhausted", dispatched: false)
        }
        self.deferredRequestIDs.remove(requestID)
        self.deferredOpenIDs.remove(requestID)
        self.finish(requestID, answer)
      }) {
        guard self.inFlight[requestID] == deferredOperation,
              self.deferredRequestIDs.remove(requestID) != nil,
              self.inFlight.removeValue(forKey: requestID) != nil else { return }
        self.deferredOpenIDs.remove(requestID)
        self.start(request, openAdmission: openAdmission, slotAdmission: slotAdmission)
      }
      return
    }
    let id = Self.requestId(of: request)
    if let peer = orderedControlPeer(request) { requestPeers[id] = peer }
    let operationIdentifier = newOperationIdentifier(id)
    inFlight[id] = operationIdentifier
    if let slotAdmission, let bridge = orderedGroups {
      guard bridge.admissionSlots.begin(slotAdmission) else {
        do {
          let outcome = try bridge.admissionSlots.fence(slotAdmission, failure: .init(kind: "cancelled", detail: "ordered open fenced before admission"))
          return finish(id, UnifiedBleRustOrderedRadioAdapter.result(slotAdmission, outcome))
        } catch { return finish(id, Self.failed("platform", "ordered open admission is stale", dispatched: false)) }
      }
      let outcome: UnifiedBleRustOrderedGroupBridge.AdmissionSlots.Outcome
      if driver is OwnedCoreBluetoothProtocolRadio,
         !currentConnection(slotAdmission.peer, generation: slotAdmission.spec.connectionGeneration) {
        outcome = .failed(.init(kind: "path-stale", detail: "connection claim is no longer current"), .notCreated)
      } else {
        outcome = UnifiedBleRustOrderedRadioAdapter.execute(slotAdmission, bridge: bridge, wake: orderedWakeRoute())
      }
      guard bridge.admissionSlots.complete(slotAdmission, outcome: outcome) else {
        return finish(id, Self.failed("platform", "ordered open completion could not be retained", dispatched: true))
      }
      return finish(id, UnifiedBleRustOrderedRadioAdapter.result(slotAdmission, outcome))
    }
    if let (key, token) = openAdmission, let bridge = orderedGroups {
      guard bridge.beginOpen(key, token: token) else {
        return finish(id, UnifiedBleRustOrderedRadioAdapter.fencedOpen())
      }
      if case let .orderedOpen(_, _, connection, _, _, members, _, _) = request,
         driver is OwnedCoreBluetoothProtocolRadio,
         let peer = members.first?.instance.peerId,
         !currentConnection(peer, generation: connection) {
        let failure = MobilePlatformFailure(kind: "path-stale", gattStatus: nil, nativeDomain: nil, nativeCode: nil,
          detail: "connection claim is no longer current", dispatched: false)
        let answer = MobileRadioCompletion.orderedOpenFailed(failure: failure, ownership: .notCreated)
        bridge.completeOpen(key, token: token, outcome: .completed(AnyHashable(answer)))
        return finish(id, answer)
      }
      guard let answer = UnifiedBleRustOrderedRadioAdapter.handle(request, bridge: bridge, wake: orderedWakeRoute()) else {
        bridge.completeOpen(key, token: token, outcome: .indeterminate)
        return finish(id, Self.failed("platform", "ordered open mapping failed", dispatched: true))
      }
      let outcome: UnifiedBleRustOrderedGroupBridge.OpenOutcome
      switch answer {
      case .orderedOpened, .orderedOpenFailed(_, .created), .orderedOpenFailed(_, .notCreated):
        outcome = .completed(AnyHashable(answer))
      case .orderedOpenFailed(_, .indeterminate):
        outcome = .unresolved(AnyHashable(answer))
      default: outcome = .indeterminate
      }
      bridge.completeOpen(key, token: token, outcome: outcome)
      return finish(id, answer)
    }
    if let answer = UnifiedBleRustOrderedRadioAdapter.handle(request, bridge: orderedGroups, wake: orderedWakeRoute()) {
      return finish(id, answer)
    }
    switch request {
    case .ownerFence, .ownerStatus:
      finish(id, Self.unsupported("owner retirement is unavailable"))
    case .orderedGraphAuthorityReserve:
      guard case let .issued(receipt)? = takeReserveResult(id) else { return finish(id, Self.platformFailure("ordered authority receipt capacity exhausted")) }
      return finish(id, .orderedGraphAuthorityReserved(receiptId: receipt))
    case let .orderedGraphAuthorityReserveStatus(_, token), let .orderedGraphAuthorityReserveFence(_, token):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio else { return finish(id, Self.stalePath) }
      let status: MobileGraphReserveStatus
      if let receipt = radio.orderedGraphAuthorities.receipt(request: token) { status = .issued(receiptId: receipt) }
      else if case .orderedGraphAuthorityReserveFence = request { status = .notCreated }
      else { status = .pending }
      return finish(id, .orderedGraphReserveStatus(status: status))
    case let .orderedGraphAuthorityReserveForget(_, token):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, radio.orderedGraphAuthorities.receipt(request: token) == nil else { return finish(id, Self.platformFailure("ordered authority reserve receipt remains owned")) }
      return finish(id, .unit)
    case let .orderedGraphAuthorityOpen(_, receipt):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, let admission = permitAdmission(id, remove: true) else { return finish(id, Self.platformFailure("ordered authority is unavailable")) }
      if !admission.claim() {
        switch radio.orderedGraphAuthorities.cancelOpen(receipt: receipt) {
        case let .opened(authority)?, let .closed(authority)?: return finish(id, .orderedGraphAuthorityOpened(authorityId: authority))
        default: return finish(id, Self.failed("cancelled", "ordered authority open cancelled before admission", dispatched: false))
        }
      }
      guard let authority = radio.orderedGraphAuthorities.open(receipt: receipt) else { return finish(id, Self.platformFailure("ordered authority not created")) }
      return finish(id, .orderedGraphAuthorityOpened(authorityId: authority))
    case let .orderedGraphAuthorityStatus(_, receipt), let .orderedGraphAuthorityFenceOpen(_, receipt):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio else { return finish(id, Self.stalePath) }
      let status: OwnedOrderedGraphAuthorities.Status?
      if case .orderedGraphAuthorityFenceOpen = request { status = radio.orderedGraphAuthorities.cancelOpen(receipt: receipt) }
      else { status = radio.orderedGraphAuthorities.status(receipt: receipt) }
      guard let status else { return finish(id, Self.stalePath) }
      let mapped: MobileGraphAuthorityStatus
      switch status {
      case .pending: mapped = .pending
      case .notCreated: mapped = .notCreated
      case let .opened(authority): mapped = .opened(authorityId: authority)
      case let .closed(authority): mapped = .closed(authorityId: authority)
      }
      return finish(id, .orderedGraphAuthorityStatus(status: mapped))
    case let .orderedGraphAuthorityForgetOpen(_, receipt):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, radio.orderedGraphAuthorities.forgetReceipt(receipt) else { return finish(id, Self.platformFailure("ordered authority open receipt remains owned")) }
      return finish(id, .unit)
    case let .orderedGraphAuthorityClose(_, authority):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, radio.orderedGraphAuthorities.close(authority) else { return finish(id, Self.platformFailure("ordered authority still owns permits or is stale")) }
      return finish(id, .unit)
    case let .orderedGraphAuthorityForget(_, authority):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, radio.orderedGraphAuthorities.forget(authority) else { return finish(id, Self.platformFailure("ordered authority close receipt is unavailable")) }
      return finish(id, .unit)
    case .orderedGraphPermitReserve:
      guard case let .issued(permit)? = takeReserveResult(id) else { return finish(id, Self.platformFailure("ordered permit receipt capacity exhausted")) }
      return finish(id, .orderedGraphPermitReserved(permitId: permit))
    case let .orderedGraphPermitReserveStatus(_, authority, token, peer, connection, snapshot),
         let .orderedGraphPermitReserveFence(_, authority, token, peer, connection, snapshot):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, let registry = radio.orderedGraphAuthorities.permits(authority) else { return finish(id, Self.stalePath) }
      let identity = OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let key = OwnedOrderedGraphPermitRegistry.Request(authority: authority, request: token, peer: peer, connection: connection, snapshot: identity)
      guard !registry.requestConflicts(key) else { return finish(id, Self.stalePath) }
      let status: MobileGraphReserveStatus
      if let permit = registry.issuedPermit(request: key) { status = .issued(receiptId: permit) }
      else if case .orderedGraphPermitReserveFence = request { status = .notCreated }
      else { status = .pending }
      return finish(id, .orderedGraphReserveStatus(status: status))
    case let .orderedGraphPermitForgetRequest(_, authority, token, peer, connection, snapshot):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, let registry = radio.orderedGraphAuthorities.permits(authority) else { return finish(id, Self.stalePath) }
      let identity = OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let key = OwnedOrderedGraphPermitRegistry.Request(authority: authority, request: token, peer: peer, connection: connection, snapshot: identity)
      guard !registry.requestConflicts(key) else { return finish(id, Self.stalePath) }
      if registry.issuedPermit(request: key) != nil && !registry.forgetRequest(key) { return finish(id, Self.platformFailure("ordered permit reserve receipt remains owned")) }
      return finish(id, .unit)
    case let .orderedGraphPermitAcquire(_, peer, connection, snapshot, authority, permitID):
      guard let admission = permitAdmission(id, remove: true), let radio = driver as? OwnedCoreBluetoothProtocolRadio else {
        return finish(id, .orderedGraphPermit(state: .stale))
      }
      let identity = OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let key = OwnedOrderedGraphPermitGate.Key(authority: authority, peer: peer, connection: connection, snapshot: identity, id: permitID)
      guard let registry = radio.orderedGraphAuthorities.permits(authority) else { return finish(id, .orderedGraphPermit(state: .stale)) }
      switch registry.reserve(key) {
      case .accepted: break
      case .full: return finish(id, .orderedGraphPermit(state: .busy))
      case .stale, .foreign: return finish(id, .orderedGraphPermit(state: .stale))
      }
      if registry.isBusy(key) { return finish(id, .orderedGraphPermit(state: .busy)) }
      guard radio.orderedGraphPermitGates[peer] != nil || radio.orderedGraphPermitGates.count < 64 else {
        registry.markBusy(key)
        return finish(id, .orderedGraphPermit(state: .busy))
      }
      let gate = radio.orderedGraphGate(peer: peer)
      let result: OwnedOrderedGraphPermitGate.Acquire
      if admission.claim() {
        let valid = currentConnection(peer, generation: connection) && radio.orderedSnapshotIdentities.matches(peer, identity) && radio.orderedSources[peer] != nil
        result = gate.acquire(key, valid: valid)
      } else { result = gate.cancelBeforeAcquire(key) }
      if result == .acquired, let source = radio.orderedSources[peer] { radio.orderedPermitSources[peer] = source }
      switch result {
      case .acquired: return finish(id, .orderedGraphPermit(state: .acquired))
      case .notAcquired: return finish(id, Self.graphPermitNotAcquired())
      case .stale: return finish(id, .orderedGraphPermit(state: .stale))
      case .busy: return finish(id, .orderedGraphPermit(state: .busy))
      }
    case let .orderedGraphPermitCommit(_, peer, connection, snapshot, authority, permitID):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, let gate = radio.orderedGraphPermitGates[peer] else {
        return finish(id, .orderedGraphPermit(state: .stale))
      }
      let identity = OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let key = OwnedOrderedGraphPermitGate.Key(authority: authority, peer: peer, connection: connection, snapshot: identity, id: permitID)
      guard let registry = radio.orderedGraphAuthorities.permits(authority), registry.owns(key) else { return finish(id, .orderedGraphPermit(state: .stale)) }
      switch gate.commit(key) {
      case .committed?: return finish(id, .orderedGraphPermit(state: .committed))
      case .invalidated?: return finish(id, Self.graphPermitInvalidated())
      default: return finish(id, .orderedGraphPermit(state: .stale))
      }
    case let .orderedGraphPermitRelease(_, peer, connection, snapshot, authority, permitID):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio, let gate = radio.orderedGraphPermitGates[peer] else {
        return finish(id, .orderedGraphPermit(state: .stale))
      }
      let identity = OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let key = OwnedOrderedGraphPermitGate.Key(authority: authority, peer: peer, connection: connection, snapshot: identity, id: permitID)
      guard let registry = radio.orderedGraphAuthorities.permits(authority), registry.owns(key) else { return finish(id, .orderedGraphPermit(state: .stale)) }
      let result = gate.release(key)
      switch result {
      case .released?: return finish(id, .orderedGraphPermit(state: .released))
      default: return finish(id, .orderedGraphPermit(state: .stale))
      }
    case let .orderedGraphPermitForget(_, peer, connection, snapshot, authority, permitID):
      guard let radio = driver as? OwnedCoreBluetoothProtocolRadio else { return finish(id, .orderedGraphPermit(state: .stale)) }
      let identity = OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let key = OwnedOrderedGraphPermitGate.Key(authority: authority, peer: peer, connection: connection, snapshot: identity, id: permitID)
      guard let registry = radio.orderedGraphAuthorities.permits(authority) else { return finish(id, .orderedGraphPermit(state: .stale)) }
      if registry.alreadyForgotten(key) { return finish(id, .orderedGraphPermit(state: .forgotten)) }
      if registry.owns(key), let gate = radio.orderedGraphPermitGates[peer], !registry.isBusy(key) {
        guard gate.forget(key) else { return finish(id, .orderedGraphPermit(state: .stale)) }
        radio.cleanupOrderedGraphGateIfIdle(peer)
      }
      guard registry.forget(key) else { return finish(id, .orderedGraphPermit(state: .stale)) }
      return finish(id, .orderedGraphPermit(state: .forgotten))
    case let .orderedMarkerWrite(_, generation, groupID, markerID, connection, snapshot, instance, value, withResponse):
      guard case let .opaque(source) = generation else { return finish(id, Self.stalePath) }
      let markerKey = MarkerKey(source: source, group: groupID, marker: markerID)
      guard let bridge = orderedGroups, let path = Self.path(instance) else {
        removeMarkerGate(markerKey)
        return finish(id, Self.stalePath)
      }
      let route = OrderedNotificationRoute(peer: instance.peerId,
        serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(instance.serviceUuid), serviceOccurrence: path.service,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(instance.characteristicUuid), characteristicOccurrence: path.characteristic)
      let identity = OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision)
      let input = UnifiedBleRustOrderedGroupBridge.MarkerInput(connection: connection, snapshot: identity, route: route, value: value, withResponse: withResponse)
      do {
        if let status = try bridge.beginMarker(groupID: groupID, generation: source, id: markerID, input: input) {
          return finish(id, .orderedMarkerWriteStatus(status: Self.markerStatus(status)))
        }
        guard markerGate(markerKey, create: false) != nil else {
          let failure = UnifiedBleRustOrderedGroupBridge.MarkerFailure(kind: "platform", gattStatus: nil, domain: nil, code: nil, detail: "marker admission capacity exhausted", dispatched: false)
          return finish(id, .orderedMarkerWriteStatus(status: Self.markerStatus(try bridge.markerRejected(groupID: groupID, id: markerID, failure: failure))))
        }
        guard let radio = driver as? OwnedCoreBluetoothProtocolRadio,
              currentConnection(instance.peerId, generation: connection) else {
          let failure = UnifiedBleRustOrderedGroupBridge.MarkerFailure(kind: "path-stale", gattStatus: nil, domain: nil, code: nil, detail: "marker write target is stale or unavailable", dispatched: false)
          return finish(id, .orderedMarkerWriteStatus(status: Self.markerStatus(try bridge.markerRejected(groupID: groupID, id: markerID, failure: failure))))
        }
        do {
          try bridge.validateMarkerTarget(groupID: groupID, opaqueSourceGeneration: source, peer: instance.peerId, snapshot: identity)
          let address = CharacteristicAddress(peerIdentifier: route.peer, serviceUUID: route.serviceUUID,
            serviceOccurrence: route.serviceOccurrence, characteristicUUID: route.characteristicUUID, characteristicOccurrence: route.characteristicOccurrence)
          _ = try radio.writeWithOrderedMarker(address: address, value: value, withResponse: withResponse,
            operationIdentifier: operationIdentifier, admit: {
              guard self.currentConnection(instance.peerId, generation: connection),
                    radio.orderedSnapshotIdentities.matches(instance.peerId, identity) else {
                throw NSError(domain: "ubm.marker.stale", code: 1,
                  userInfo: [NSLocalizedDescriptionKey: "marker connection claim is no longer current"])
              }
              try bridge.checkMarkerCaptureOrder(groupID: groupID, opaqueSourceGeneration: source)
              let key = MarkerKey(source: source, group: groupID, marker: markerID)
              guard self.markerGate(key, create: false)?.claim() == true else {
                throw NSError(domain: "ubm.marker.cancelled", code: 1,
                  userInfo: [NSLocalizedDescriptionKey: "marker cancelled before admission"])
              }
              let ordinal = try bridge.admitMarker(groupID: groupID, opaqueSourceGeneration: source, id: markerID)
              try bridge.markerAdmitted(groupID: groupID, id: markerID, ordinal: ordinal)
              return ordinal
            },
            completion: { error in
              let failure = error.map { Self.markerFailure($0, dispatched: true) }
              do { try bridge.markerSettled(groupID: groupID, id: markerID, failure: failure) }
              catch { NSLog("[UnifiedBleRustRadioAdapter] marker settlement failed: %@", String(describing: error)) }
            })
          return finish(id, .orderedMarkerWriteStatus(status: Self.markerStatus(try bridge.markerStarted(groupID: groupID, id: markerID))))
        } catch {
          if case UnifiedBleRustOrderedGroupBridge.Failure.capturePending = error {
            retainPendingMarker(PendingMarkerControl(key: markerKey, connection: connection, snapshot: identity,
              instance: instance, value: value, withResponse: withResponse, operationIdentifier: operationIdentifier))
            return finish(id, .orderedMarkerWriteStatus(status: Self.markerStatus(try bridge.markerStatus(groupID: groupID, generation: source, id: markerID))))
          }
          let requiresAck: Bool
          switch error {
          case UnifiedBleRustOrderedGroupBridge.Failure.source(.controlOverflow),
               UnifiedBleRustOrderedGroupBridge.Failure.source(.markerOrdinalExhausted): requiresAck = true
          default: requiresAck = false
          }
          let native = error as NSError
          let failure = Self.markerFailure(native, dispatched: false)
          return finish(id, .orderedMarkerWriteStatus(status: Self.markerStatus(try bridge.markerRejected(groupID: groupID, id: markerID, failure: failure, requiresAcknowledgement: requiresAck))))
        }
      } catch {
        removeMarkerGate(markerKey)
        return finish(id, Self.platformFailure("marker transaction refused"))
      }
    case let .orderedMarkerWriteSettlement(_, generation, groupID, markerID):
      guard case let .opaque(source) = generation, let bridge = orderedGroups else { return finish(id, Self.stalePath) }
      do { return finish(id, .orderedMarkerWriteStatus(status: Self.markerStatus(try bridge.markerStatus(groupID: groupID, generation: source, id: markerID)))) }
      catch { return finish(id, Self.stalePath) }
    case let .orderedCancelMarkerWrite(_, generation, groupID, markerID):
      guard case let .opaque(source) = generation, let bridge = orderedGroups else { return finish(id, Self.stalePath) }
      do {
        let status = try bridge.cancelWaitingMarker(groupID: groupID, generation: source, id: markerID)
          ?? bridge.markerStatus(groupID: groupID, generation: source, id: markerID)
        pendingMarkerControls.removeValue(forKey: MarkerKey(source: source, group: groupID, marker: markerID))
        return finish(id, .orderedMarkerWriteStatus(status: Self.markerStatus(status)))
      }
      catch { return finish(id, Self.stalePath) }
    case let .orderedForgetMarkerWriteReceipt(_, generation, groupID, markerID):
      guard case let .opaque(source) = generation, let bridge = orderedGroups else { return finish(id, Self.stalePath) }
      do { try bridge.forgetMarker(groupID: groupID, generation: source, id: markerID); let key = MarkerKey(source: source, group: groupID, marker: markerID); pendingMarkerControls.removeValue(forKey: key); removeMarkerGate(key); return finish(id, .unit) }
      catch { return finish(id, Self.platformFailure("marker receipt remains owned")) }
    case .orderedOpenV2, .orderedRecoverOpenV2, .orderedAcknowledgeOpenV2,
         .orderedOpen, .orderedRecoverOpen, .orderedAcknowledgeOpen, .orderedRegister, .orderedRetire, .orderedDrain, .orderedAcknowledgeBatch, .orderedDrainV2, .orderedAcknowledgeBatchV2,
         .orderedClose, .orderedSeal, .orderedBarrier, .orderedRetainClaim, .orderedConfirmCleanup, .orderedRemove, .orderedForgetRemovalReceipt:
      return finish(id, Self.platformFailure("ordered request was not handled"))
    case .adapterState:
      driver.adapterSnapshot { snapshot in
        guard self.inFlight[id] == operationIdentifier else { return }
        self.finish(id, .adapter(snapshot: Self.adapterSnapshot(snapshot)))
      }
    case let .startScan(_, serviceUuids, deviceAddresses, scanMode, callbackType, legacy, reportDelayMs, phy):
      guard deviceAddresses.isEmpty, scanMode == nil, callbackType == nil, legacy == nil,
            reportDelayMs == nil, phy == nil else {
        return finish(id, Self.unsupported("CoreBluetooth has no address filter or Android scan settings"))
      }
      whenReady(id, verb: .startScan) {
        self.startOwnedScan(serviceUUIDs: serviceUuids, operationIdentifier: operationIdentifier) { error in
          guard self.inFlight[id] == operationIdentifier else { return }
          if error == nil { self.scanActive = true }
          self.finishUnit(id, error, verb: .startScan)
        }
      }
    case .stopScan:
      stopOwnedScan(operationIdentifier: operationIdentifier) { error in
        guard self.inFlight[id] == operationIdentifier else { return }
        // No scan running is the state a stop asks for: CoreBluetooth
        // already stopped it (adapter off, reported as `ScanFailed`).
        let noScanActive = error.map { $0.domain == Self.ownedDomain && $0.code == 1004 } ?? false
        if error == nil || noScanActive { self.scanActive = false }
        self.finishUnit(id, noScanActive ? nil : error, verb: .stopScan)
      }
    case let .connect(_, peerId, autoConnect, preferredPhy, expectedGeneration):
      guard !expectedGeneration.isEmpty else {
        return finish(id, Self.platformFailure("connect requires an admitted core generation"))
      }
      guard !autoConnect else {
        return finish(id, Self.unsupported("CoreBluetooth has no when-available (autoConnect) connection intent"))
      }
      // Rust refuses a PHY preference on Apple before any effect; should one
      // ever arrive, it is refused here too, never connected without.
      guard preferredPhy.isEmpty else {
        return finish(id, Self.unsupported("CoreBluetooth has no LE PHY control"))
      }
      whenReady(id, verb: .connect) {
        let connected: (@escaping (NSError?) -> Void) -> Void = { completion in
          if let radio = self.driver as? OwnedCoreBluetoothProtocolRadio {
            radio.connect(borrower: self, peerIdentifier: peerId, operationIdentifier: operationIdentifier, completion: completion)
          } else {
            self.driver.connect(peerIdentifier: peerId, operationIdentifier: operationIdentifier, completion: completion)
          }
        }
        connected { error in
          guard self.inFlight[id] == operationIdentifier else { return }
          if error == nil {
            if let radio = self.driver as? OwnedCoreBluetoothProtocolRadio {
              guard let claim = radio.connectionClaims[peerId]?[ObjectIdentifier(self)],
                    radio.connectionClaimIsCurrent(owner: self, peer: peerId, identity: claim.identity) else {
                return self.finish(id, Self.failed("path-stale", "connection claim is no longer current", dispatched: true))
              }
              self.establishedNativeClaims[peerId] = claim.identity
            }
            self.establishedGenerations[peerId] = expectedGeneration
          }
          self.finishUnit(id, error, verb: .connect)
        }
      }
    case let .disconnect(_, peerId):
      let generation = establishedGenerations[peerId]
      let nativeClaim = establishedNativeClaims[peerId]
      let disconnected: (@escaping (NSError?) -> Void) -> Void = { completion in
        if let radio = self.driver as? OwnedCoreBluetoothProtocolRadio {
          radio.disconnect(borrower: self, peerIdentifier: peerId, operationIdentifier: operationIdentifier, completion: completion)
        } else {
          self.driver.disconnect(peerIdentifier: peerId, operationIdentifier: operationIdentifier, completion: completion)
        }
      }
      disconnected { error in
        if error == nil, let generation, self.establishedGenerations[peerId] == generation,
           self.establishedNativeClaims[peerId] == nativeClaim {
          self.establishedGenerations.removeValue(forKey: peerId)
          self.establishedNativeClaims.removeValue(forKey: peerId)
          self.orderedGroups?.disconnectGroups(peer: peerId)
          self.releaseOrderedActivations(peer: peerId, connection: generation)
          let retired = self.subscriptionsById.values.filter { $0.instance.peerId == peerId }
          for subscription in retired {
            let key = SubscriptionKey(instance: InstanceKey(subscription.instance), epoch: subscription.epoch)
            if self.subscriptionByInstance[key] == subscription.identifier { self.subscriptionByInstance.removeValue(forKey: key) }
            self.forgetSubscription(subscription.identifier)
          }
        }
        guard self.inFlight[id] == operationIdentifier else { return }
        self.finishUnit(id, error, verb: .disconnect)
      }
    case let .discover(_, peerId):
      let discover: (@escaping (NSDictionary?, NSError?) -> Void) -> Void = { completion in
        if let radio = self.driver as? OwnedCoreBluetoothProtocolRadio {
          radio.discover(borrower: self, peerIdentifier: peerId, operationIdentifier: operationIdentifier, completion: completion)
        } else {
          self.driver.discover(peerIdentifier: peerId, operationIdentifier: operationIdentifier, completion: completion)
        }
      }
      discover { snapshot, error in
        guard self.inFlight[id] == operationIdentifier else { return }
        if let error { return self.finish(id, Self.failure(error, verb: .discover)) }
        guard let services = snapshot.flatMap(Self.services) else {
          return self.finish(id, Self.platformFailure("CoreBluetooth returned an unreadable discovery snapshot"))
        }
        guard let owner = snapshot?["nativeSnapshotOwner"] as? String,
              let attachment = snapshot?["nativeSnapshotAttachment"] as? NSNumber,
              let revision = snapshot?["nativeSnapshotRevision"] as? NSNumber else {
          return self.finish(id, Self.platformFailure("CoreBluetooth discovery omitted native graph identity"))
        }
        self.finish(id, .discoveredScoped(services: services, identity: MobileGattSnapshotIdentity(owner: owner, attachment: attachment.uint64Value, revision: revision.uint64Value)))
      }
    case let .read(_, instance):
      guard let path = Self.path(instance) else { return finish(id, Self.stalePath) }
      let completion: (NSData?, OwnedCoreBluetoothReadProvenance, NSError?) -> Void = { value, provenance, error in
        guard self.inFlight[id] == operationIdentifier else { return }
        if let error { return self.finish(id, Self.failure(error, verb: .read)) }
        guard let value else { return self.finish(id, Self.platformFailure("CoreBluetooth answered the read without a value")) }
        self.finish(id, .read(value: value as Data, provenance: provenance.wire))
      }
      if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
        radio.readCharacteristic(borrower: self, peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid,
          serviceOccurrence: path.service, characteristicUUID: instance.characteristicUuid,
          characteristicOccurrence: path.characteristic, operationIdentifier: operationIdentifier, completion: completion)
      } else {
        driver.readCharacteristic(peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid,
          serviceOccurrence: path.service, characteristicUUID: instance.characteristicUuid,
          characteristicOccurrence: path.characteristic, operationIdentifier: operationIdentifier, completion: completion)
      }
    case let .write(_, instance, value, withResponse):
      write(id, operationIdentifier, instance, value, withResponse)
    case let .readDescriptor(_, instance, descriptorUuid, descriptorOccurrence):
      guard let path = Self.path(instance), let descriptor = Int(exactly: descriptorOccurrence) else {
        return finish(id, Self.stalePath)
      }
      let completion: (NSData?, NSError?) -> Void = { value, error in
        guard self.inFlight[id] == operationIdentifier else { return }
        self.finishBytes(id, value, error, verb: .readDescriptor)
      }
      if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
        radio.readDescriptor(borrower: self, peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid,
          serviceOccurrence: path.service, characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
          descriptorUUID: descriptorUuid, descriptorOccurrence: descriptor, operationIdentifier: operationIdentifier, completion: completion)
      } else {
        driver.readDescriptor(peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
          characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
          descriptorUUID: descriptorUuid, descriptorOccurrence: descriptor, operationIdentifier: operationIdentifier, completion: completion)
      }
    case let .writeDescriptor(_, instance, descriptorUuid, descriptorOccurrence, value):
      guard let path = Self.path(instance), let descriptor = Int(exactly: descriptorOccurrence) else {
        return finish(id, Self.stalePath)
      }
      let completion: (NSError?) -> Void = { error in
        guard self.inFlight[id] == operationIdentifier else { return }
        self.finishUnit(id, error, verb: .writeDescriptor)
      }
      if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
        radio.writeDescriptor(borrower: self, peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid,
          serviceOccurrence: path.service, characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
          descriptorUUID: descriptorUuid, descriptorOccurrence: descriptor, value: value as NSData, operationIdentifier: operationIdentifier, completion: completion)
      } else {
        driver.writeDescriptor(peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
          characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
          descriptorUUID: descriptorUuid, descriptorOccurrence: descriptor, value: value as NSData, operationIdentifier: operationIdentifier, completion: completion)
      }
    case let .enableNotifications(_, instance, epoch, requested, _):
      enable(id, operationIdentifier, instance, epoch, requested)
    case let .orderedEnableNotifications(_, activation, requested, _):
      guard let key = orderedActivationKey(activation), currentConnection(activation.instance.peerId, generation: activation.connectionGeneration),
            let bridge = orderedGroups else { return finish(id, Self.stalePath) }
      do {
        try bridge.validateSource(groupID: activation.groupId, opaqueSourceGeneration: key.source)
        let member = try bridge.expectedMember(groupID: activation.groupId, memberID: activation.member.memberId, sourceMember: activation.member.sourceMember, epoch: activation.member.epoch)
        guard let path = Self.path(activation.instance), member.route.peer == activation.instance.peerId,
              OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(member.route.serviceUUID) == OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(activation.instance.serviceUuid), member.route.serviceOccurrence == path.service,
              OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(member.route.characteristicUUID) == OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(activation.instance.characteristicUuid), member.route.characteristicOccurrence == path.characteristic else { return finish(id, Self.stalePath) }
        try bridge.validateCurrentMember(groupID: activation.groupId, expected: member)
      } catch { return finish(id, Self.stalePath) }
      guard orderedEnableReceipts.count < 64, orderedEnableReceipts[key] == nil,
            !orderedEnables.values.contains(key.subscription), subscriptionByInstance[key.subscription] == nil else {
        return finish(id, Self.platformFailure("ordered enable ownership capacity exhausted"))
      }
      orderedEnables[id] = key.subscription
      orderedEnableRequests[id] = key
      orderedEnableReceipts[key] = .pending
      enable(id, operationIdentifier, activation.instance, activation.activationEpoch, requested)
    case let .orderedEnableSettlement(_, activation):
      guard let key = orderedActivationKey(activation), let state = orderedEnableReceipts[key] else { return finish(id, Self.stalePath) }
      let status: MobileOrderedEnableStatus
      switch state {
      case .pending: status = .pending
      case .enabled: status = .enabled(delivery: "unknown")
      case .released: status = .released(failure: nil)
      case let .uncertain(failure): status = .uncertain(failure: failure)
      }
      finish(id, .orderedEnableStatus(status: status))
    case let .orderedDisableNotifications(_, activation):
      guard let key = orderedActivationKey(activation), let state = orderedEnableReceipts[key] else { return finish(id, Self.stalePath) }
      if case .released = state { return finish(id, .unit) }
      guard case .enabled = state, establishedGenerations[activation.instance.peerId] == key.connection else {
        return finish(id, Self.platformFailure("ordered enable settlement remains pending"))
      }
      disable(activation.instance, epoch: activation.activationEpoch, operationIdentifier: operationIdentifier) { failure in
        guard self.inFlight[id] == operationIdentifier else { return }
        self.finish(id, failure ?? .unit)
      }
    case let .orderedForgetEnableReceipt(_, activation):
      guard let key = orderedActivationKey(activation) else { return finish(id, Self.stalePath) }
      guard let state = orderedEnableReceipts[key] else { return finish(id, .unit) }
      guard case .released = state else { return finish(id, Self.platformFailure("ordered enable cleanup remains pending")) }
      orderedEnableReceipts.removeValue(forKey: key)
      finish(id, .unit)
    case let .disableNotifications(_, instance, epoch):
      disable(instance, epoch: epoch, operationIdentifier: operationIdentifier) { failure in
        guard self.inFlight[id] == operationIdentifier else { return }
        self.finish(id, failure ?? .unit)
      }
    case .readMtu:
      // CoreBluetooth does not expose the negotiated ATT MTU.
      // `maximumWriteValueLength` can include a long write, so it is not one ATT PDU.
      finish(id, .mtu(mtu: nil))
    case let .readWriteLimits(_, peerId):
      // CoreBluetooth's own per-type answer: with response it performs the
      // long write itself; a command must fit one ATT payload.
      driver.writeLimits(peerIdentifier: peerId) { limits, error in
        guard self.inFlight[id] == operationIdentifier else { return }
        if let error { return self.finish(id, Self.failure(error, verb: .readWriteLimits)) }
        guard let withResponse = (limits?["withResponse"] as? NSNumber)?.intValue,
              let withoutResponse = (limits?["withoutResponse"] as? NSNumber)?.intValue,
              let withResponseLimit = UInt16(exactly: withResponse), withResponseLimit > 0,
              let withoutResponseLimit = UInt16(exactly: withoutResponse), withoutResponseLimit > 0 else {
          return self.finish(id, Self.platformFailure("CoreBluetooth reported no write limit"))
        }
        self.finish(id, .writeLimits(withResponse: withResponseLimit, withoutResponse: withoutResponseLimit))
      }
    case let .readWriteReadiness(_, peerId):
      // The initial observation is CoreBluetooth's current queue flag.
      // Later edges arrive through peripheralIsReady(toSendWriteWithoutResponse:).
      driver.writeLimits(peerIdentifier: peerId) { limits, error in
        guard self.inFlight[id] == operationIdentifier else { return }
        if let error { return self.finish(id, Self.failure(error, verb: .readWriteReadiness)) }
        guard let ready = (limits?["canSendWithoutResponse"] as? NSNumber)?.boolValue else {
          return self.finish(id, Self.platformFailure("CoreBluetooth reported no write-without-response readiness"))
        }
        self.finish(id, .ready(ready: ready))
      }
    case let .readRssi(_, peerId):
      let completion: (NSNumber?, NSError?) -> Void = { rssi, error in
        guard self.inFlight[id] == operationIdentifier else { return }
        if let error { return self.finish(id, Self.failure(error, verb: .readRssi)) }
        guard let value = rssi.flatMap(Self.rssi) else {
          return self.finish(id, Self.platformFailure("CoreBluetooth reported the RSSI as unavailable"))
        }
        self.finish(id, .rssi(rssi: value))
      }
      if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
        radio.readRssi(borrower: self, peerIdentifier: peerId, operationIdentifier: operationIdentifier, completion: completion)
      } else {
        driver.readRssi(peerIdentifier: peerId, operationIdentifier: operationIdentifier, completion: completion)
      }
    case .requestMtu:
      finish(id, Self.unsupported("CoreBluetooth has no caller-directed ATT MTU request"))
    case .requestConnectionPriority, .requestSubrate:
      finish(id, Self.unsupported("CoreBluetooth has no connection priority control"))
    case .readPhy, .requestPhy:
      finish(id, Self.unsupported("CoreBluetooth exposes no LE PHY control"))
    case let .resolvePeer(_, peerId):
      whenReady(id, verb: .resolvePeer) {
        self.driver.resolveDirectoryPeer(peerIdentifier: peerId) { peer, error in
          guard self.inFlight[id] == operationIdentifier else { return }
          if let error { return self.finish(id, Self.failure(error, verb: .resolvePeer)) }
          self.finish(id, .resolvedPeer(peer: peer))
        }
      }
    case let .connectedPeers(_, services):
      guard !services.isEmpty else { return finish(id, Self.unsupported("CoreBluetooth requires a service-scoped system-connected query")) }
      whenReady(id, verb: .connectedPeers) {
        self.driver.connectedPeerSnapshots(services: services) { peers, error in
          guard self.inFlight[id] == operationIdentifier else { return }
          if let error { return self.finish(id, Self.failure(error, verb: .connectedPeers)) }
          guard let peers else { return self.finish(id, Self.platformFailure("CoreBluetooth directory returned no result")) }
          self.finish(id, .connectedPeers(peers: peers))
        }
      }
    case .securityState, .createBond, .cancelBond, .bondedPeers:
      finish(id, Self.unsupported("CoreBluetooth exposes no bond state or pairing control"))
    case .acquireBackground, .releaseBackground, .updateBackgroundNotification:
      finish(id, Self.unsupported("Connected-device foreground service is Android-only"))
    case .associateCompanion:
      finish(id, Self.unsupported("Companion device association is Android-only"))
    case .listCompanion, .disassociateCompanion:
      finish(id, Self.unsupported("Companion device associations are Android-only"))
    case .observePresence, .stopPresence:
      finish(id, Self.unsupported("Companion device presence observation is Android-only; Apple restoration arrives through willRestoreState"))
    case .close:
      close(id)
    }
  }

  private static func graphPermitNotAcquired() -> MobileRadioCompletion {
    .orderedGraphPermitNotAcquired(failure: MobilePlatformFailure(kind: "cancelled", gattStatus: nil,
      nativeDomain: nil, nativeCode: nil, detail: "native ordered graph permit was cancelled before admission", dispatched: false))
  }

  private static func graphPermitInvalidated() -> MobileRadioCompletion {
    .orderedGraphPermitInvalidated(failure: MobilePlatformFailure(kind: "platform", gattStatus: nil,
      nativeDomain: nil, nativeCode: nil, detail: "native ordered graph permit was invalidated", dispatched: false))
  }

  private static func markerFailure(_ error: NSError, dispatched: Bool) -> UnifiedBleRustOrderedGroupBridge.MarkerFailure {
    if case let .failed(kind, gattStatus, domain, code, detail, _) = failure(error, verb: .write) {
      return .init(kind: kind, gattStatus: gattStatus, domain: domain, code: code, detail: detail, dispatched: dispatched)
    }
    return .init(kind: "platform", gattStatus: nil, domain: error.domain, code: Int64(error.code),
      detail: "marker write failed", dispatched: dispatched)
  }

  private static func markerStatus(_ status: UnifiedBleRustOrderedGroupBridge.MarkerStatus) -> MobileOrderedMarkerWriteStatus {
    func failure(_ value: UnifiedBleRustOrderedGroupBridge.MarkerFailure) -> MobilePlatformFailure {
      MobilePlatformFailure(kind: value.kind, gattStatus: value.gattStatus, nativeDomain: value.domain,
        nativeCode: value.code, detail: value.detail, dispatched: value.dispatched)
    }
    let admission: MobileOrderedMarkerAdmission
    switch status.admission {
    case .pending: admission = .pending
    case .notAdmitted: admission = .notAdmitted
    case let .admitted(ordinal): admission = .admitted(ordinal: ordinal)
    case let .rejected(error): admission = .rejected(failure: failure(error))
    }
    let write: MobileOrderedMarkerWriteState
    switch status.write {
    case .initiating: write = .initiating
    case let .notStarted(error): write = .notStarted(failure: error.map(failure))
    case let .started(error): write = .started(failure: error.map(failure))
    case let .settled(error): write = .settled(failure: error.map(failure))
    }
    return MobileOrderedMarkerWriteStatus(admission: admission, write: write)
  }

  private func write(
    _ id: UInt64,
    _ operationIdentifier: String,
    _ instance: MobileInstance,
    _ value: Data,
    _ withResponse: Bool
  ) {
    guard let path = Self.path(instance) else { return finish(id, Self.stalePath) }
    let perform = {
      let completion: (NSError?) -> Void = { error in
        guard self.inFlight[id] == operationIdentifier else { return }
        self.finishUnit(id, error, verb: .write)
      }
      if let radio = self.driver as? OwnedCoreBluetoothProtocolRadio {
        radio.write(borrower: self, peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid,
          serviceOccurrence: path.service, characteristicUUID: instance.characteristicUuid,
          characteristicOccurrence: path.characteristic, value: value as NSData, withResponse: withResponse,
          operationIdentifier: operationIdentifier, completion: completion)
      } else {
        self.driver.write(peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid,
          serviceOccurrence: path.service, characteristicUUID: instance.characteristicUuid,
          characteristicOccurrence: path.characteristic, value: value as NSData, withResponse: withResponse,
          operationIdentifier: operationIdentifier, completion: completion)
      }
    }
    if withResponse || driver is OwnedCoreBluetoothProtocolRadio { return perform() }
    // CoreBluetooth truncates an oversize write without response and drops
    // one it cannot queue, both without an error. Refuse both instead.
    driver.writeLimits(peerIdentifier: instance.peerId) { limits, error in
      guard self.inFlight[id] == operationIdentifier else { return }
      if let error { return self.finish(id, Self.failure(error, verb: .write)) }
      guard let maximum = (limits?["withoutResponse"] as? NSNumber)?.intValue,
            let canSend = (limits?["canSendWithoutResponse"] as? NSNumber)?.boolValue else {
        return self.finish(id, Self.platformFailure("CoreBluetooth reported no write-without-response limit"))
      }
      guard value.count <= maximum else {
        return self.finish(id, Self.platformFailure(
          "\(value.count) bytes exceed CoreBluetooth's write-without-response limit of \(maximum)"
        ))
      }
      guard canSend else {
        return self.finish(id, Self.failed(
          "busy", "CoreBluetooth's write-without-response queue is full", dispatched: false
        ))
      }
      perform()
    }
  }

  private func enable(
    _ id: UInt64,
    _ operationIdentifier: String,
    _ instance: MobileInstance,
    _ epoch: UInt64,
    _ requested: String?
  ) {
    guard !notificationFailureSpoolReconfigurationInProgress else {
      if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
      return finish(id, Self.failed("busy", "notification failure spool reconfiguration is in progress", dispatched: false))
    }
    guard let path = Self.path(instance) else {
      if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
      return finish(id, Self.stalePath)
    }
    // CoreBluetooth writes the CCCD mode itself and never reports it. Rust
    // stops a delivery requirement at the property check on Apple; a hard
    // requirement reaching the radio cannot be honoured here.
    if let requested {
      if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
      return finish(id, Self.unsupported("CoreBluetooth cannot be told to write \(requested)"))
    }
    let key = SubscriptionKey(instance: InstanceKey(instance), epoch: epoch)
    let subscriptionIdentifier = subscriptionIdentifier(operationIdentifier)
    let previous = subscriptionByInstance[key]
    if let spool = notificationFailureSpool {
      let persisted = spool.notificationFailureSpoolResumeSequenceV1(instance: instance, epoch: epoch)
      if let maximum = UInt64(persisted) {
        guard maximum < UInt64.max else {
          if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
          return finish(id, Self.platformFailure("notification failure spool sequence is exhausted"))
        }
        let resumed = maximum + 1
        if let next = nextFailureSequence, next < resumed { nextFailureSequence = resumed }
      } else if persisted != "unsupported" {
        if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
        return finish(id, Self.platformFailure("notification failure spool cannot resume its source sequence: \(persisted)"))
      }
    }
    guard notificationReceiptRoutes[subscriptionIdentifier] == nil else {
      if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
      return finish(id, Self.platformFailure("notification receipt route identity is already owned"))
    }
    guard let capturedSink = sink,
          let reservation = notificationReceiptBudget.reserve(Self.notificationReceiptsPerRoute) else {
      if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
      return finish(id, Self.failed("busy", "notification failure receipt capacity exhausted", dispatched: false))
    }
    let subscription = Subscription(identifier: subscriptionIdentifier, instance: instance, epoch: epoch)
    let receiptRoute = NotificationReceiptRoute(sink: capturedSink, subscription: subscription,
      reservation: reservation, ordinaryCount: Self.ordinaryNotificationReceiptsPerRoute)
    notificationReceiptRoutes[subscriptionIdentifier] = receiptRoute
    if let spoolFailure = primeNotificationFailureSpool() {
      receiptRoute.retired = true
      retireNotificationReceiptRouteIfIdle(receiptRoute)
      if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
      return finish(id, Self.platformFailure("notification failure spool cannot recover its pending source prefix: \(spoolFailure)"))
    }
    if let radio = driver as? OwnedCoreBluetoothProtocolRadio,
       !radio.installNotificationRoute(subscriptionIdentifier, borrower: self) {
      if let route = notificationReceiptRoutes[subscriptionIdentifier] {
        route.retired = true
        retireNotificationReceiptRouteIfIdle(route)
      }
      if let activation = orderedEnableRequests[id] { orderedEnableReceipts[activation] = .released }
      return finish(id, Self.platformFailure("notification route identity is already owned"))
    }
    if let radio = driver as? OwnedCoreBluetoothProtocolRadio, let activation = orderedEnableRequests[id] {
      let route = OrderedNotificationRoute(peer: instance.peerId,
        serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(instance.serviceUuid), serviceOccurrence: path.service,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(instance.characteristicUuid), characteristicOccurrence: path.characteristic)
      radio.notificationRoutes[subscriptionIdentifier]?.orderedClaim = OrderedNotificationClaim(
        generation: activation.source, route: route, token: activation.token)
    }
    // Registered before the OS call: a value can arrive before
    // didUpdateNotificationState confirms the enable.
    subscriptionsById[subscriptionIdentifier] = subscription
    driver.subscribe(
      peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
      characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
      subscriptionIdentifier: subscriptionIdentifier, operationIdentifier: operationIdentifier
    ) { error in
      guard self.inFlight[id] == operationIdentifier else {
        // A cancelled request's late completion is not a native cleanup
        // receipt. Its exact route remains owned by cancellation cleanup.
        return
      }
      if let error {
        if let activation = self.orderedEnableRequests[id], let state = self.orderedEnableReceipts[activation], case .released = state {
          self.forgetSubscription(subscriptionIdentifier)
        } else if error.domain == Self.ownedDomain && Self.ownedRefusedBeforeSending(error.code, verb: .enableNotifications) {
          self.forgetSubscription(subscriptionIdentifier)
          if let activation = self.orderedEnableRequests[id] { self.orderedEnableReceipts[activation] = .released }
        } else if let activation = self.orderedEnableRequests[id], self.orderedEnableReceipts[activation] != nil {
          self.orderedEnableReceipts[activation] = .uncertain(MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: error.domain, nativeCode: Int64(error.code), detail: "native enable failed without authoritative release", dispatched: true))
        }
        return self.finish(id, Self.failure(error, verb: .enableNotifications))
      }
      if let activation = self.orderedEnableRequests[id] {
        if self.orderedEnableReceipts[activation] == nil {
          self.forgetSubscription(subscriptionIdentifier)
          return self.finish(id, .orderedEnableReleased(kind: "platform", gattStatus: nil, nativeDomain: nil, nativeCode: nil, detail: "connection retired", dispatched: true))
        }
        if case .released? = self.orderedEnableReceipts[activation] {
          self.forgetSubscription(subscriptionIdentifier)
          return self.finish(id, .orderedEnableReleased(kind: "platform", gattStatus: nil, nativeDomain: nil, nativeCode: nil, detail: "connection retired", dispatched: true))
        }
      }
      if let previous, previous != subscriptionIdentifier { self.forgetSubscription(previous) }
      self.subscriptionByInstance[key] = subscriptionIdentifier
      if let activation = self.orderedEnableRequests[id], let state = self.orderedEnableReceipts[activation], case .released = state {} else if let activation = self.orderedEnableRequests[id], self.orderedEnableReceipts[activation] != nil { self.orderedEnableReceipts[activation] = .enabled }
      self.finish(id, .notifyEnabled(delivery: "unknown"))
      self.retryCapturedNotificationFailures()
    }
  }

  /// Clears the CCCD this adapter enabled. A stale path does not prove that
  /// CoreBluetooth has delivered the disconnect fence or released the CCCD.
  private func disable(
    _ instance: MobileInstance,
    epoch: UInt64,
    operationIdentifier: String,
    completion: @escaping (MobileRadioCompletion?) -> Void
  ) {
    let key = SubscriptionKey(instance: InstanceKey(instance), epoch: epoch)
    guard let subscriptionIdentifier = subscriptionByInstance[key] else {
      return completion(Self.failed(
        "path-stale", "this radio enabled no notifications on the instance", dispatched: false
      ))
    }
    guard let path = Self.path(instance) else { return completion(Self.stalePath) }
    driver.unsubscribe(
      peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
      characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
      subscriptionIdentifier: subscriptionIdentifier, operationIdentifier: operationIdentifier
    ) { error in
      if error == nil {
        if self.subscriptionByInstance[key] == subscriptionIdentifier {
          self.subscriptionByInstance.removeValue(forKey: key)
        }
        self.forgetSubscription(subscriptionIdentifier)
        if error == nil {
          for activation in Array(self.orderedEnableReceipts.keys) where activation.subscription == key {
            self.orderedEnableReceipts[activation] = .released
          }
        }
        return completion(nil)
      }
      completion(error.map { Self.failure($0, verb: .disableNotifications) })
    }
  }

  /// Teardown: disable every CCCD this adapter enabled, one at a time, and
  /// name every instance that did not release.
  private func close(_ id: UInt64) {
    guard let operationIdentifier = inFlight[id] else { return }
    var remaining = subscriptionsById.values.filter { subscriptionByInstance[SubscriptionKey(instance: InstanceKey($0.instance), epoch: $0.epoch)] == $0.identifier }
    var failures = [MobileCloseFailure]()
    func next() {
      guard let subscription = remaining.popLast() else {
        guard self.inFlight[id] == operationIdentifier else { return }
        return finish(id, .closed(failures: failures))
      }
      disable(subscription.instance, epoch: subscription.epoch, operationIdentifier: self.newOperationIdentifier(id)) { failure in
        if case let .failed(kind, _, _, _, detail, _)? = failure {
          failures.append(MobileCloseFailure(instance: subscription.instance, detail: "\(kind): \(detail)"))
        }
        next()
      }
    }
    next()
  }

  /// Legacy `assertAdapterReady`: a scan or connect against an adapter that
  /// cannot serve it fails with the adapter's reason instead of being handed
  /// to CoreBluetooth (which ignores it with an API-misuse log).
  private func whenReady(_ id: UInt64, verb: Verb, _ body: @escaping () -> Void) {
    guard let operationIdentifier = inFlight[id] else { return }
    let completion: (NSDictionary?, NSError?) -> Void = { snapshot, error in
      guard self.inFlight[id] == operationIdentifier else { return }
      if let error { return self.finish(id, Self.failure(error, verb: verb)) }
      guard let snapshot else { return self.finish(id, Self.platformFailure("CoreBluetooth preparation returned no state")) }
      if let failure = Self.readinessFailure(Self.adapterSnapshot(snapshot)) {
        return self.finish(id, failure)
      }
      body()
    }
    if let radio = driver as? OwnedCoreBluetoothProtocolRadio {
      radio.prepareForOperation(operationIdentifier: operationIdentifier, owner: self, completion: completion)
    } else {
      driver.prepareForOperation(operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  private func finishUnit(_ id: UInt64, _ error: NSError?, verb: Verb) {
    finish(id, error.map { Self.failure($0, verb: verb) } ?? .unit)
  }

  private func finishBytes(_ id: UInt64, _ value: NSData?, _ error: NSError?, verb: Verb) {
    if let error { return finish(id, Self.failure(error, verb: verb)) }
    guard let value else { return finish(id, Self.platformFailure("CoreBluetooth answered the read without a value")) }
    finish(id, .bytes(value: value as Data))
  }

  /// Answers one request. A request `cancel` already answered is not
  /// answered twice.
  private func finish(_ id: UInt64, _ completion: MobileRadioCompletion) {
    requestPeers.removeValue(forKey: id)
    orderedEnables.removeValue(forKey: id)
    orderedEnableRequests.removeValue(forKey: id)
    guard inFlight.removeValue(forKey: id) != nil else { return }
    deliver(id, completion)
  }

  /// Slot control must remain available when the radio queue is occupied.
  private func deliverControl(_ id: UInt64, _ completion: MobileRadioCompletion) {
    controlSinkLock.lock(); let target = controlSink; controlSinkLock.unlock()
    guard let target else {
      NSLog("[UnifiedBleRustRadioAdapter] control request %llu answered before a host was bound", id)
      return
    }
    if target.complete(requestId: id, completion: completion) == "mismatched" {
      queue.async { self.counters.mismatchedCompletions += 1 }
      NSLog("[UnifiedBleRustRadioAdapter] Rust refused control answer for request %llu", id)
    }
  }

  private func deliver(_ id: UInt64, _ completion: MobileRadioCompletion) {
    guard let sink else {
      // The fence may cancel the queued callback installation while requests
      // still require their exact no-effect answers on the retained route.
      deliverControl(id, completion)
      return
    }
    if sink.complete(requestId: id, completion: completion) == "mismatched" {
      counters.mismatchedCompletions += 1
      NSLog("[UnifiedBleRustRadioAdapter] Rust refused the answer shape for request %llu", id)
    }
  }

  private func ingest(_ ingress: MobileRadioIngress) {
    guard let sink else {
      counters.ingressAfterClose += 1
      NSLog("[UnifiedBleRustRadioAdapter] platform fact arrived before a host was bound")
      return
    }
    if sink.ingest(ingress: ingress) == "closed" {
      counters.ingressAfterClose += 1
    }
  }

  // MARK: - OwnedCoreBluetoothProtocolRadioDelegate (radio queue)

  func protocolRadioDidUpdateAdapterState(_ snapshot: NSDictionary) {
    let adapter = Self.adapterSnapshot(snapshot)
    if adapter.power != "on" {
      establishedGenerations.removeAll()
      establishedNativeClaims.removeAll()
    }
    ingest(.adapterState(snapshot: adapter))
    // CoreBluetooth ends every scan when the adapter leaves powered-on and
    // never resumes it: report the loss and clear the radio's scan owner.
    guard scanActive, adapter.power != "on" else { return }
    scanActive = false
    ingest(.scanFailed(detail: "CoreBluetooth stopped scanning: adapter power is \(adapter.power)"))
    stopOwnedScan(operationIdentifier: newOperationIdentifier(0)) { _ in }
  }

  func receiveScanLoss(_ loss: OwnedScanLoss) -> Bool {
    guard let sink = sink as? UnifiedBleRustScanLossSink else { return false }
    return sink.ingestScanLoss(loss) == "accepted"
  }

  func receiveScanAdvertisement(_ advertisement: NSDictionary, generation: UUID) -> Bool {
    let parsed = Self.advertisement(advertisement)
    if let dropped = parsed.dropped { ingest(.dropped(ingressClass: "advertisement", detail: dropped)) }
    guard let value = parsed.advertisement, let sink else { return false }
    let status = sink.ingest(ingress: .advertisement(advertisement: value))
    if status == "closed" { counters.ingressAfterClose += 1 }
    return status == "accepted"
  }

  func protocolRadioDidReceiveAdvertisement(_ advertisement: NSDictionary) {
    let parsed = Self.advertisement(advertisement)
    if let dropped = parsed.dropped {
      ingest(.dropped(ingressClass: "advertisement", detail: dropped))
    }
    if let value = parsed.advertisement {
      ingest(.advertisement(advertisement: value))
    }
  }

  func protocolRadioDidDisconnectPeer(_ peerIdentifier: String, error: NSError?) {
    guard let expectedGeneration = establishedGenerations.removeValue(forKey: peerIdentifier) else {
      counters.unknownConnectionLosses += 1
      NSLog("[UnifiedBleRustRadioAdapter] CoreBluetooth reported an unowned disconnect for %@", peerIdentifier)
      return
    }
    establishedNativeClaims.removeValue(forKey: peerIdentifier)
    orderedGroups?.disconnectGroups(peer: peerIdentifier)
    releaseOrderedActivations(peer: peerIdentifier, connection: expectedGeneration)
    ingest(.connectionScoped(
      peerId: peerIdentifier,
      expectedGeneration: expectedGeneration,
      connected: false,
      status: error.flatMap { Int32(exactly: $0.code) }
    ))
  }

  func protocolRadioDidModifyServices(_ peerIdentifier: String) {
    ingest(.servicesChanged(peerId: peerIdentifier))
  }

  func protocolRadioDidReceiveNotification(_ subscriptionIdentifier: String, value: NSData) {
    guard let subscription = subscriptionsById[subscriptionIdentifier] else {
      return ingest(.dropped(
        ingressClass: "notification",
        detail: "CoreBluetooth delivered a value for subscription \(subscriptionIdentifier), which this radio does not own"
      ))
    }
    ingest(.notification(instance: subscription.instance, epoch: subscription.epoch, value: value as Data))
  }

  /// The CoreBluetooth callback path may use the one independently reserved
  /// overflow receipt. Internal cleanup callers use the same path: refusing
  /// a cleanup before that receipt is available keeps the physical owner and
  /// asks the caller to retry rather than discarding the failure.
  func captureNotificationFailure(_ identifier: String) -> ((NSError) -> Void)? {
    captureNotificationFailure(identifier, allowOverflow: true)
  }

  func captureNotificationFailure(_ identifier: String, allowOverflow: Bool) -> ((NSError) -> Void)? {
    guard let route = notificationReceiptRoutes[identifier], !route.retired,
          subscriptionsById[identifier] != nil,
          let slot = route.slots.first(where: { !$0.busy && (allowOverflow || !$0.overflow) }) else { return nil }
    let subscription = route.subscription, key = slot.key
    slot.busy = true
    capturedFailureSlots[key] = (route, slot)
    let lease = CapturedFailureLease { [weak self] in
      guard let self else { return }
      self.queue.async { [weak self] in
        guard let self, let current = self.capturedFailureSlots[key], current.slot === slot else { return }
        slot.captureReleased = true
        self.releaseCapturedFailure(slot, route: current.route)
      }
    }
    return { [self, lease] error in
      _ = lease
      guard !slot.settled else { return }
      slot.settled = true
      let failure = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: error.domain,
        nativeCode: Int64(error.code), detail: error.localizedDescription, dispatched: true)
      guard let sourceSequence = self.nextFailureSequence,
            let deliverySequence = self.nextFailureDeliverySequence else {
        preconditionFailure("notification failure sequence exhausted")
      }
      self.nextFailureSequence = sourceSequence == UInt64.max ? nil : sourceSequence + 1
      self.nextFailureDeliverySequence = deliverySequence == UInt64.max ? nil : deliverySequence + 1
      slot.sequence = deliverySequence
      slot.ingress = .notificationSourceFailed(version: 1, instance: subscription.instance,
        epoch: subscription.epoch, failure: failure)
      capturedFailureReady.append(key)
      retryCapturedNotificationFailures()
    }
  }

  func hasPendingNotificationFailureRetention() -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    return !capturedFailureSlots.isEmpty || notificationReceiptRoutes.values.contains {
      $0.coalescedOverflow != nil
    } || globalSpoolPending
  }

  private func recordCoalescedNotificationFailure(_ identifier: String, error: NSError) {
    guard let route = notificationReceiptRoutes[identifier] else {
      ingest(.dropped(ingressClass: "control", detail: "CoreBluetooth failed an unowned notification subscription \(identifier)"))
      return
    }
    let failure = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: error.domain,
      nativeCode: Int64(error.code), detail: error.localizedDescription, dispatched: true)
    guard let sequence = nextFailureSequence else {
      preconditionFailure("notification failure sequence exhausted")
    }
    nextFailureSequence = sequence == UInt64.max ? nil : sequence + 1
    // Once volatile fallback owns an earlier sequence, do not let a later
    // callback commit directly to the durable spool and become externally
    // replayable ahead of that uncommitted prefix.
    if route.coalescedOverflow == nil, let spool = notificationFailureSpool {
      let status = spool.appendNotificationFailureV1(sequence: sequence, instance: route.subscription.instance,
        epoch: route.subscription.epoch, failure: failure)
      if status == "committed" {
        globalSpoolPending = true
        retryCapturedNotificationFailures()
        return
      }
      if status == "closed" { counters.ingressAfterClose += 1 }
    }
    if let overflow = route.coalescedOverflow {
      overflow.append(sequence: sequence, failure: failure)
    } else {
      route.coalescedOverflow = CoalescedNotificationFailureOverflow(sequence: sequence, failure: failure)
    }
    retryCapturedNotificationFailures()
  }

  private func coalescedNotificationFailureDetail(_ overflow: CoalescedNotificationFailureOverflow,
                                                   route: NotificationReceiptRoute) -> String {
    func describe(_ failure: MobilePlatformFailure) -> String {
      let domain = failure.nativeDomain ?? "unknown"
      let code = failure.nativeCode.map(String.init) ?? "unknown"
      return "\(domain) (\(code)); detail=\(failure.detail)"
    }
    let instance = route.subscription.instance
    return "notification source failure retention saturated; coalesced count=\(overflow.count); source=\(instance.peerId)/\(instance.serviceUuid)[\(instance.serviceOccurrence)]/\(instance.characteristicUuid)[\(instance.characteristicOccurrence)]; epoch=\(route.subscription.epoch); first-sequence=\(overflow.firstSequence.map(String.init) ?? "unknown"); last-sequence=\(overflow.lastSequence.map(String.init) ?? "unknown"); first=\(describe(overflow.first)); last=\(describe(overflow.last)); intermediate native failures were not individually retained"
  }

  private func routeHasReadyCapturedFailure(_ route: NotificationReceiptRoute) -> Bool {
    capturedFailureReady.contains { capturedFailureSlots[$0]?.route === route }
  }

  private func notificationFailureSpoolPage(_ encoded: String) -> Result<[NotificationFailureSpoolRecord], NotificationFailureSpoolPageError> {
    guard let data = encoded.data(using: .utf8),
          let page = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let version = page["version"] as? Int, (version == 2 || version == 4),
          let rows = page["records"] as? [[String: Any]] else {
      return .failure(.reason("malformed-page"))
    }
    guard rows.count <= 1 else { return .failure(.reason("page-exceeds-requested-bound")) }
    var records: [NotificationFailureSpoolRecord] = []
    records.reserveCapacity(rows.count)
    for row in rows {
      guard let captureOrdinalText = row["captureOrdinal"] as? String, let captureOrdinal = UInt64(captureOrdinalText),
            let sourceSequenceText = row["sourceSequence"] as? String, let sourceSequence = UInt64(sourceSequenceText),
            let route = row["route"] as? String,
            let epochText = row["epoch"] as? String, let epoch = UInt64(epochText),
            let failureBody = row["failure"] as? [String: Any],
            let instanceBody = failureBody["instance"] as? [String: Any],
            let peerId = instanceBody["peerId"] as? String,
            let serviceUuid = instanceBody["serviceUuid"] as? String,
            let serviceOccurrence = instanceBody["serviceOccurrence"] as? UInt64,
            let characteristicUuid = instanceBody["characteristicUuid"] as? String,
            let characteristicOccurrence = instanceBody["characteristicOccurrence"] as? UInt64,
            let kind = failureBody["kind"] as? String,
            let detail = failureBody["detail"] as? String,
            let dispatched = failureBody["dispatched"] as? Bool else {
        return .failure(.reason("malformed-record"))
      }
      let gattStatus = failureBody["gattStatus"] as? Int32
      let nativeDomain = failureBody["nativeDomain"] as? String
      let nativeCode = failureBody["nativeCode"] as? Int64
      let instance = MobileInstance(peerId: peerId, serviceUuid: serviceUuid,
        serviceOccurrence: serviceOccurrence, characteristicUuid: characteristicUuid,
        characteristicOccurrence: characteristicOccurrence)
      guard captureOrdinal > globalSpoolAfterCaptureOrdinal else {
        return .failure(.reason("mismatched-record"))
      }
      records.append(NotificationFailureSpoolRecord(captureOrdinal: captureOrdinal, sourceSequence: sourceSequence,
        route: route, epoch: epoch, instance: instance,
        failure: MobilePlatformFailure(kind: kind, gattStatus: gattStatus, nativeDomain: nativeDomain,
          nativeCode: nativeCode, detail: detail, dispatched: dispatched)))
    }
    return .success(records)
  }

  /// Returns the earliest V4 receipt whose source ACK committed before the
  /// prior process stopped. A capability proves only the sidecar ACK; the
  /// receipt remains until this process's fresh sink accepts it.
  private func pendingNotificationFailureRetirementV4(_ spool: UnifiedBleRustNotificationFailureSpool) -> Result<(record: NotificationFailureSpoolRecord, capability: String)?, NotificationFailureSpoolPageError> {
    let encoded = spool.notificationFailureSpoolPendingRetirementsV4(maxItems: 1)
    if encoded == "unsupported" { return .success(nil) }
    guard let data = encoded.data(using: .utf8),
          let page = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          page["version"] as? Int == 4,
          let rows = page["records"] as? [[String: Any]] else {
      return .failure(.reason(encoded == "closed" ? "closed" : "malformed-v4-retirements"))
    }
    guard case let .success(records) = notificationFailureSpoolPage(encoded), records.count == rows.count else {
      return .failure(.reason("malformed-v4-retirements"))
    }
    guard let record = records.first else { return .success(nil) }
    guard let capability = rows[0]["retirementCapability"] as? String, !capability.isEmpty else {
      return .failure(.reason("malformed-v4-retirements"))
    }
    return .success((record, capability))
  }

  private func primeNotificationFailureSpool() -> String? {
    guard let spool = notificationFailureSpool else { return nil }
    switch pendingNotificationFailureRetirementV4(spool) {
    case let .failure(.reason(reason)): return reason
    case .success(.some):
      globalSpoolPending = true
      return nil
    case .success(nil): break
    }
    let resume = spool.notificationFailureSpoolResumeGlobalV2()
    if resume == "unsupported" { return nil }
    guard let resumeData = resume.data(using: .utf8),
          let resumeBody = try? JSONSerialization.jsonObject(with: resumeData) as? [String: Any],
          resumeBody["version"] as? Int == 2,
          let highestText = resumeBody["highestCaptureOrdinal"] as? String,
          UInt64(highestText) != nil else {
      return resume == "closed" || resume == "invalid" ? resume : "malformed-resume"
    }
    let page = spool.notificationFailureSpoolPageGlobalV2(afterCaptureOrdinal: globalSpoolAfterCaptureOrdinal,
      maxItems: 1)
    if page == "unsupported" { return nil }
    if page == "closed" || page == "invalid" { return page }
    switch notificationFailureSpoolPage(page) {
    case let .success(records): globalSpoolPending = !records.isEmpty; return nil
    case let .failure(.reason(reason)): return reason
    }
  }

  @discardableResult
  private func replayNotificationFailureSpool(blockedSinks: inout Set<ObjectIdentifier>) -> Bool {
    guard globalSpoolPending, capturedFailureSlots.isEmpty,
          let spool = notificationFailureSpool, let sink else { return false }
    let sinkIdentity = ObjectIdentifier(sink)
    guard !blockedSinks.contains(sinkIdentity) else { return false }
    guard let acknowledged = sink as? UnifiedBleRustAcknowledgedFailureSink,
          acknowledged.notificationFailureIngressVersion(offered: 1) == 1 else {
      blockedSinks.insert(sinkIdentity)
      return false
    }
    let pending: (record: NotificationFailureSpoolRecord, capability: String)?
    switch pendingNotificationFailureRetirementV4(spool) {
    case let .failure(.reason(reason)):
      NSLog("[UnifiedBleRustRadioAdapter] durable notification failure spool page retained for retry: %@", reason)
      blockedSinks.insert(sinkIdentity)
      return false
    case let .success(value):
      pending = value
    }
    let records: [NotificationFailureSpoolRecord]
    if let pending {
      records = [pending.record]
    } else {
      let page = spool.notificationFailureSpoolPageGlobalV2(afterCaptureOrdinal: globalSpoolAfterCaptureOrdinal,
        maxItems: 1)
      if page == "closed" { counters.ingressAfterClose += 1; blockedSinks.insert(sinkIdentity); return false }
      guard page != "unsupported", page != "invalid" else { blockedSinks.insert(sinkIdentity); return false }
      switch notificationFailureSpoolPage(page) {
      case let .failure(.reason(reason)):
        NSLog("[UnifiedBleRustRadioAdapter] durable notification failure spool page retained for retry: %@", reason)
        blockedSinks.insert(sinkIdentity)
        return false
      case let .success(value):
        records = value
      }
    }
    guard let record = records.first else {
      globalSpoolPending = false
      globalSpoolInFlight = nil
      return true
    }
    let deliverySequence: UInt64
    let shouldOfferIngress: Bool
    if let inFlight = globalSpoolInFlight {
        guard inFlight.record.captureOrdinal == record.captureOrdinal else {
          blockedSinks.insert(sinkIdentity)
          return false
        }
        deliverySequence = inFlight.deliverySequence
        shouldOfferIngress = !inFlight.ingressAccepted
    } else {
        guard let next = nextFailureDeliverySequence else {
          preconditionFailure("notification failure delivery sequence exhausted")
        }
        deliverySequence = next
        nextFailureDeliverySequence = next == UInt64.max ? nil : next + 1
        globalSpoolInFlight = (record, deliverySequence, false)
        shouldOfferIngress = true
    }
    if shouldOfferIngress {
        let status = acknowledged.ingestNotificationFailureV1(sequence: deliverySequence, instance: record.instance,
          epoch: record.epoch, failure: record.failure)
        guard status == "accepted" else {
          if status == "closed" { counters.ingressAfterClose += 1 }
          blockedSinks.insert(sinkIdentity)
          return false
        }
        globalSpoolInFlight = (record, deliverySequence, true)
    }
    let capability: String
    if let pending {
        capability = pending.capability
    } else {
        let acknowledgement = spool.acknowledgeNotificationFailureSpoolGlobalV4(
          captureOrdinal: record.captureOrdinal, sourceSequence: record.sourceSequence,
          instance: record.instance, epoch: record.epoch, failure: record.failure)
        guard let acknowledgementData = acknowledgement.data(using: .utf8),
              let acknowledgementBody = try? JSONSerialization.jsonObject(with: acknowledgementData) as? [String: Any],
              acknowledgementBody["version"] as? Int == 4,
              acknowledgementBody["status"] as? String == "acknowledged",
              let issued = acknowledgementBody["retirementCapability"] as? String, !issued.isEmpty else {
          // The exact V1 operation is idempotent. Keep the row and re-offer it
          // after a lost/ambiguous acknowledgment rather than skipping ahead.
          blockedSinks.insert(sinkIdentity)
          return false
        }
        capability = issued
    }
    // The capability is persisted by the sidecar before it was returned;
    // final confirmation follows the fresh sink's exact acceptance.
    let confirmed = spool.confirmNotificationFailureSpoolAcknowledgementRetirementV4(
        captureOrdinal: record.captureOrdinal, sourceSequence: record.sourceSequence,
        instance: record.instance, epoch: record.epoch, failure: record.failure,
        retirementCapability: capability)
    guard confirmed == "confirmed" else { blockedSinks.insert(sinkIdentity); return false }
    globalSpoolAfterCaptureOrdinal = record.captureOrdinal
    globalSpoolInFlight = nil
    return true
  }

  private func retryCapturedNotificationFailures() {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !capturedFailureRetrying else { return }
    capturedFailureRetrying = true
    defer { capturedFailureRetrying = false }
    var blockedSinks: Set<ObjectIdentifier> = []
    var index = 0
    while index < capturedFailureReady.count {
      let key = capturedFailureReady[index]
      guard let entry = capturedFailureSlots[key], let ingress = entry.slot.ingress else {
        preconditionFailure("ready notification failure lost its retained receipt")
      }
      let slot = entry.slot, sink = entry.route.sink
      let sinkIdentity = ObjectIdentifier(sink)
      if blockedSinks.contains(sinkIdentity) { index += 1; continue }
      guard case let .notificationSourceFailed(_, instance, epoch, failure) = ingress else {
        preconditionFailure("captured notification failure changed kind")
      }
      let status: String
      if let acknowledged = sink as? UnifiedBleRustAcknowledgedFailureSink,
         acknowledged.notificationFailureIngressVersion(offered: 1) == 1,
         let sequence = slot.sequence {
        status = acknowledged.ingestNotificationFailureV1(sequence: sequence, instance: instance, epoch: epoch, failure: failure)
      } else {
        status = "unsupported"
      }
      if status == "accepted" {
        guard slot.sequence != nil else { preconditionFailure("accepted failure has no receipt sequence") }
        capturedFailureReady.remove(at: index)
        slot.ingress = nil
        releaseCapturedFailure(slot, route: entry.route)
      } else {
        blockedSinks.insert(sinkIdentity)
        index += 1
        if status == "closed" { counters.ingressAfterClose += 1 }
        if !slot.refusalLogged {
          slot.refusalLogged = true
          NSLog("[UnifiedBleRustRadioAdapter] captured notification failure retained for retry: %@", status)
        }
      }
    }
    // Drain one globally ordered durable suffix only after every earlier
    // in-memory receipt settled. Capture order is never reconstructed from
    // route dictionaries, whose iteration order is intentionally unspecified.
    _ = replayNotificationFailureSpool(blockedSinks: &blockedSinks)
    for route in notificationReceiptRoutes.values where route.coalescedOverflow != nil && !routeHasReadyCapturedFailure(route) && !globalSpoolPending {
      let sinkIdentity = ObjectIdentifier(route.sink)
      guard !blockedSinks.contains(sinkIdentity), let overflow = route.coalescedOverflow else { continue }
      let status = route.sink.ingest(ingress: .dropped(ingressClass: "notification",
        detail: coalescedNotificationFailureDetail(overflow, route: route)))
      if status == "accepted" {
        route.coalescedOverflow = nil
        retireNotificationReceiptRouteIfIdle(route)
      } else {
        blockedSinks.insert(sinkIdentity)
        if status == "closed" { counters.ingressAfterClose += 1 }
        if !overflow.refusalLogged {
          overflow.refusalLogged = true
          NSLog("[UnifiedBleRustRadioAdapter] coalesced notification failure overflow retained for retry: %@", status)
        }
      }
    }
    guard hasPendingNotificationFailureRetention() else {
      capturedFailureRetryDelayMilliseconds = 250
      return
    }
    guard !capturedFailureRetryScheduled else { return }
    capturedFailureRetryScheduled = true
    let delay = capturedFailureRetryDelayMilliseconds
    capturedFailureRetryDelayMilliseconds = min(delay * 2, 4_000)
    queue.asyncAfter(deadline: .now() + .milliseconds(delay)) { [self] in
      capturedFailureRetryScheduled = false
      retryCapturedNotificationFailures()
    }
  }

  func protocolRadioDidFailNotification(_ subscriptionIdentifier: String, error: NSError) {
    guard subscriptionsById[subscriptionIdentifier] != nil else {
      if notificationReceiptRoutes[subscriptionIdentifier] != nil {
        recordCoalescedNotificationFailure(subscriptionIdentifier, error: error)
        return
      }
      return ingest(.dropped(ingressClass: "control", detail: "CoreBluetooth failed an unowned notification subscription \(subscriptionIdentifier)"))
    }
    // A process restart may leave an older durable source suffix. New native
    // callbacks join that suffix rather than taking an in-memory receipt that
    // Rust could observe first.
    if globalSpoolPending {
      recordCoalescedNotificationFailure(subscriptionIdentifier, error: error)
      return
    }
    guard let captured = captureNotificationFailure(subscriptionIdentifier) else {
      recordCoalescedNotificationFailure(subscriptionIdentifier, error: error)
      return
    }
    captured(error)
  }

  func protocolRadioDidRestorePeers(_ peers: [NSDictionary]) {
    announceRestored(peers.compactMap(Self.restoredPeer))
  }

  func protocolRadioDidBecomeReadyToSendWriteWithoutResponse(_ peerIdentifier: String) {
    ingest(.writeReadiness(peerId: peerIdentifier, ready: true))
  }

  private func announceRestored(_ peers: [MobileRestoredPeer]) {
    let fresh = peers.filter { !announcedRestoredPeers.contains($0.peerId) }
    guard !fresh.isEmpty else { return }
    guard sink != nil else {
      pendingRestoredPeers.append(contentsOf: fresh.filter { peer in
        !pendingRestoredPeers.contains { $0.peerId == peer.peerId }
      })
      return
    }
    let batch = pendingRestoredPeers + fresh.filter { peer in !pendingRestoredPeers.contains { $0.peerId == peer.peerId } }
    pendingRestoredPeers.removeAll()
    announcedRestoredPeers.formUnion(batch.map(\.peerId))
    ingest(.restored(peers: batch))
    for peer in batch { onRestoredPeer(peer.peerId) }
  }

  // MARK: - Translation

  enum Verb {
    case startScan, stopScan, connect, disconnect, discover, read, write, readDescriptor, writeDescriptor
    case enableNotifications, disableNotifications, readMtu, readWriteLimits, readWriteReadiness, readRssi, connectedPeers, resolvePeer
  }

  static let ownedDomain = "com.sfourdrinier.unifiedblemanager.corebluetooth"
  private static let stalePath = failed("path-stale", "occurrence index out of range", dispatched: false)

  static func unsupported(_ detail: String) -> MobileRadioCompletion {
    failed("unsupported", detail, dispatched: false)
  }

  static func platformFailure(_ detail: String) -> MobileRadioCompletion {
    failed("platform", detail, dispatched: false)
  }

  /// `dispatched: false` means CoreBluetooth was never handed the request,
  /// so a failed write reports commit `not-dispatched`.
  /// `nativeError` is the `NSError` CoreBluetooth or the owned radio failed
  /// with: its domain and code are the failure's legacy identity (113).
  static func failed(
    _ kind: String, _ detail: String, gattStatus: Int32? = nil, nativeError: NSError? = nil, dispatched: Bool
  ) -> MobileRadioCompletion {
    .failed(
      kind: kind, gattStatus: gattStatus, nativeDomain: nativeError?.domain,
      nativeCode: nativeError.map { Int64($0.code) }, detail: detail, dispatched: dispatched
    )
  }

  /// Owned-radio codes raised at admission, before any CoreBluetooth call:
  /// unknown or unconnected peripheral, stale path, oversize payload, an
  /// operation already pending on the same attribute, unowned subscription.
  static func ownedRefusedBeforeSending(_ code: Int, verb: Verb) -> Bool {
    switch code {
    case 1001, 1002, 1003, 1004, 1005, 1006, 1007, 1008, 1009, 1010, 1012, 1013, 1014, 1017, 1018, 1019,
      1022, 1023, 1024, 1028, 1029, 1033, 1034:
      return true
    case 1025: return true
    case 1026: return verb == .readDescriptor
    case 1027: return verb == .writeDescriptor
    default: return false
    }
  }

  /// One CoreBluetooth or owned-radio error → one platform failure kind.
  /// Rust maps the kind to the contract identity per request.
  static func failure(_ error: NSError, verb: Verb) -> MobileRadioCompletion {
    let detail = "\(error.domain)#\(error.code): \(error.localizedDescription)"
    switch error.domain {
    case OwnedCoreBluetoothProtocolRadioSupport.centralStateErrorDomain where error.code == CBManagerState.unauthorized.rawValue:
      return failed("permission-denied", detail, nativeError: error, dispatched: false)
    case CBATTErrorDomain:
      return failed("gatt-status", detail, gattStatus: Int32(exactly: error.code), nativeError: error, dispatched: true)
    case CBErrorDomain:
      return failed(coreBluetoothKind(error.code), detail, nativeError: error, dispatched: true)
    case ownedDomain:
      return failed(
        ownedKind(error.code, verb: verb), detail, nativeError: error,
        dispatched: !ownedRefusedBeforeSending(error.code, verb: verb)
      )
    default:
      return failed("platform", detail, nativeError: error, dispatched: true)
    }
  }

  static func coreBluetoothKind(_ code: Int) -> String {
    switch code {
    case 2: return "path-stale"  // invalidHandle
    case 3, 7: return "not-connected"  // notConnected, peripheralDisconnected
    case 5: return "cancelled"  // operationCancelled
    case 11: return "busy"  // connectionLimitReached
    case 12: return "peer-unknown"  // unknownDevice
    case 13: return "unsupported"  // operationNotSupported
    default: return "platform"
    }
  }

  static func ownedKind(_ code: Int, verb: Verb) -> String {
    switch code {
    case 1005, 1007: return "peer-unknown"
    case 1008, 1016, 1020, 1022, 1033: return "not-connected"
    case 1010, 1013, 1017, 1019, 1028: return "path-stale"
    case 1001, 1006, 1009, 1014, 1018, 1023, 1024, 1029: return "busy"
    case 1003: return "adapter-off"
    case 1034: return "permission-not-determined"
    case 1025: return verb == .readDescriptor ? "path-stale" : "busy"
    case 1026: return verb == .readDescriptor ? "busy" : (verb == .discover ? "path-stale" : "platform")
    case 1027: return verb == .writeDescriptor ? "platform" : "path-stale"
    default: return "platform"
    }
  }

  static func readinessFailure(_ adapter: MobileAdapterSnapshot) -> MobileRadioCompletion? {
    let reason = "CoreBluetooth reports availability \(adapter.availability), authorization \(adapter.authorization), power \(adapter.power)"
    if adapter.availability == "unsupported" || adapter.power == "unsupported" {
      return failed("adapter-unavailable", reason, dispatched: false)
    }
    switch adapter.authorization {
    case "denied": return failed("permission-denied", reason, dispatched: false)
    case "restricted": return failed("permission-restricted", reason, dispatched: false)
    case "unavailable": return failed("adapter-unavailable", reason, dispatched: false)
    default: break
    }
    switch adapter.power {
    case "on": return nil
    case "off": return failed("adapter-off", reason, dispatched: false)
    case "resetting": return failed("adapter-resetting", reason, dispatched: false)
    default:
      if adapter.authorization == "not-determined" {
        return failed("permission-not-determined", reason, dispatched: false)
      }
      return failed("adapter-unavailable", reason, dispatched: false)
    }
  }

  static func adapterSnapshot(_ snapshot: NSDictionary) -> MobileAdapterSnapshot {
    let authorization = snapshot["authorization"] as? String ?? "unknown"
    return MobileAdapterSnapshot(
      availability: snapshot["availability"] as? String ?? "unknown",
      authorization: authorization == "notDetermined" ? "not-determined" : authorization,
      power: snapshot["power"] as? String ?? "unknown",
      safeReason: snapshot["safeReason"] as? String
    )
  }

  /// CoreBluetooth reports 127 when no RSSI is available.
  static func rssi(_ value: NSNumber) -> Int16? {
    let raw = value.intValue
    return raw == 127 ? nil : Int16(exactly: raw)
  }

  static func advertisement(_ record: NSDictionary) -> (advertisement: MobileAdvertisement?, dropped: String?) {
    guard let peerId = record["peerIdentifier"] as? String, !peerId.isEmpty else {
      return (nil, "CoreBluetooth advertisement without a peripheral identifier")
    }
    var dropped: String?
    var manufacturerData = [MobileManufacturerData]()
    if let section = record["manufacturerData"] as? Data {
      if section.count < 2 {
        dropped = "manufacturer data section of \(section.count) byte(s) from \(peerId) is shorter than its 2-byte company id; section omitted"
      } else {
        let companyId = UInt16(section[section.startIndex]) | (UInt16(section[section.startIndex + 1]) << 8)
        manufacturerData.append(MobileManufacturerData(companyId: companyId, payload: section.dropFirst(2)))
      }
    }
    let serviceData = ((record["serviceData"] as? [String: Data]) ?? [:])
      .sorted { $0.key < $1.key }
      .map { MobileServiceData(uuid: $0.key, payload: $0.value) }
    let advertisement = MobileAdvertisement(
      peerId: peerId,
      address: nil,
      localName: record["localName"] as? String,
      rssi: (record["rssi"] as? NSNumber).flatMap(rssi),
      txPowerLevel: (record["txPower"] as? NSNumber).flatMap { Int16(exactly: $0.intValue) },
      serviceUuids: record["serviceUUIDs"] as? [String] ?? [],
      manufacturerData: manufacturerData,
      serviceData: serviceData,
      connectable: (record["connectable"] as? NSNumber)?.boolValue,
      solicitedServiceUuids: record["solicitedServiceUUIDs"] as? [String],
      overflowServiceUuids: record["overflowServiceUUIDs"] as? [String]
    )
    return (advertisement, dropped)
  }

  static func services(_ snapshot: NSDictionary) -> [MobileGattService]? {
    guard let services = snapshot["services"] as? [NSDictionary] else { return nil }
    var result = [MobileGattService]()
    for service in services {
      guard let uuid = service["uuid"] as? String, let occurrence = unsigned(service["occurrence"]),
            let characteristics = service["characteristics"] as? [NSDictionary] else { return nil }
      let primary: Bool?
      if service["primary"] is NSNull || service["primary"] == nil { primary = nil }
      else if let value = service["primary"] as? NSNumber,
              CFGetTypeID(value) == CFBooleanGetTypeID() { primary = value.boolValue }
      else { return nil }
      var includes: [MobileGattDescriptor]?
      if service["includedServices"] is NSNull || service["includedServices"] == nil { includes = nil }
      else if let references = service["includedServices"] as? [NSDictionary] {
        var parsed = [MobileGattDescriptor]()
        for reference in references {
          guard let target = reference["uuid"] as? String, let targetOccurrence = unsigned(reference["occurrence"]) else { return nil }
          parsed.append(MobileGattDescriptor(uuid: target, occurrence: targetOccurrence))
        }
        includes = parsed
      } else { return nil }
      var mapped = [MobileGattCharacteristic]()
      for characteristic in characteristics {
        guard let characteristicUuid = characteristic["uuid"] as? String,
              let characteristicOccurrence = unsigned(characteristic["occurrence"]),
              let descriptors = characteristic["descriptors"] as? [NSDictionary] else { return nil }
        var mappedDescriptors = [MobileGattDescriptor]()
        for descriptor in descriptors {
          guard let descriptorUuid = descriptor["uuid"] as? String,
                let descriptorOccurrence = unsigned(descriptor["occurrence"]) else { return nil }
          mappedDescriptors.append(MobileGattDescriptor(uuid: descriptorUuid, occurrence: descriptorOccurrence))
        }
        mapped.append(MobileGattCharacteristic(
          uuid: characteristicUuid,
          occurrence: characteristicOccurrence,
          properties: MobileGattProperties(
            read: flag(characteristic["readable"]),
            write: flag(characteristic["writableWithResponse"]),
            writeWithoutResponse: flag(characteristic["writableWithoutResponse"]),
            notify: flag(characteristic["notifiable"]),
            indicate: flag(characteristic["indicatable"])
          ),
          descriptors: mappedDescriptors
        ))
      }
      result.append(MobileGattService(uuid: uuid, occurrence: occurrence, primary: primary, includedServices: includes, characteristics: mapped))
    }
    return result
  }

  static func restoredPeer(_ record: NSDictionary) -> MobileRestoredPeer? {
    guard let peerId = record["peerIdentifier"] as? String, !peerId.isEmpty else { return nil }
    return MobileRestoredPeer(
      peerId: peerId,
      name: record["name"] as? String,
      connected: (record["connected"] as? NSNumber)?.boolValue ?? false
    )
  }

  static func requestId(of request: MobileRadioRequest) -> UInt64 {
    switch request {
    case let .adapterState(id), let .stopScan(id), let .bondedPeers(id), let .close(id),
         let .ownerFence(id, _, _), let .ownerStatus(id, _, _):
      return id
    case let .resolvePeer(id, _), let .connectedPeers(id, _), let .startScan(id, _, _, _, _, _, _, _), let .connect(id, _, _, _, _), let .disconnect(id, _), let .discover(id, _),
      let .read(id, _), let .write(id, _, _, _), let .readDescriptor(id, _, _, _), let .writeDescriptor(id, _, _, _, _),
      let .enableNotifications(id, _, _, _, _), let .orderedEnableNotifications(id, _, _, _), let .orderedDisableNotifications(id, _), let .orderedEnableSettlement(id, _), let .orderedForgetEnableReceipt(id, _), let .disableNotifications(id, _, _),
      let .orderedGraphAuthorityReserve(id, _), let .orderedGraphAuthorityReserveStatus(id, _), let .orderedGraphAuthorityReserveFence(id, _), let .orderedGraphAuthorityReserveForget(id, _), let .orderedGraphAuthorityOpen(id, _), let .orderedGraphAuthorityStatus(id, _), let .orderedGraphAuthorityFenceOpen(id, _), let .orderedGraphAuthorityForgetOpen(id, _), let .orderedGraphAuthorityClose(id, _), let .orderedGraphAuthorityForget(id, _),
      let .orderedGraphPermitReserve(id, _, _, _, _, _), let .orderedGraphPermitReserveStatus(id, _, _, _, _, _), let .orderedGraphPermitReserveFence(id, _, _, _, _, _), let .orderedGraphPermitForgetRequest(id, _, _, _, _, _),
      let .orderedGraphPermitAcquire(id, _, _, _, _, _), let .orderedGraphPermitCommit(id, _, _, _, _, _), let .orderedGraphPermitRelease(id, _, _, _, _, _), let .orderedGraphPermitForget(id, _, _, _, _, _),
      let .orderedMarkerWrite(id, _, _, _, _, _, _, _, _), let .orderedMarkerWriteSettlement(id, _, _, _), let .orderedCancelMarkerWrite(id, _, _, _), let .orderedForgetMarkerWriteReceipt(id, _, _, _),
      let .orderedOpenV2(id, _), let .orderedRecoverOpenV2(id, _), let .orderedAcknowledgeOpenV2(id, _),
      let .orderedOpen(id, _, _, _, _, _, _, _), let .orderedRecoverOpen(id, _, _, _), let .orderedAcknowledgeOpen(id, _, _, _, _), let .orderedRegister(id, _, _, _), let .orderedRetire(id, _, _, _),
      let .orderedDrain(id, _, _), let .orderedAcknowledgeBatch(id, _, _, _), let .orderedDrainV2(id, _, _, _), let .orderedAcknowledgeBatchV2(id, _, _, _, _), let .orderedClose(id, _, _), let .orderedSeal(id, _, _),
      let .orderedRetainClaim(id, _, _, _), let .orderedConfirmCleanup(id, _, _, _), let .orderedRemove(id, _, _), let .orderedForgetRemovalReceipt(id, _, _), let .orderedBarrier(id, _, _, _),
      let .readMtu(id, _),
      let .readWriteLimits(id, _), let .readWriteReadiness(id, _),
      let .requestMtu(id, _, _), let .readRssi(id, _), let .requestConnectionPriority(id, _, _), let .requestSubrate(id, _, _), let .readPhy(id, _),
      let .requestPhy(id, _, _, _), let .securityState(id, _), let .createBond(id, _, _), let .cancelBond(id, _),
      let .acquireBackground(id, _, _), let .releaseBackground(id, _), let .updateBackgroundNotification(id, _, _, _),
      let .associateCompanion(id, _, _, _), let .listCompanion(id),
      let .disassociateCompanion(id, _),
      let .observePresence(id, _), let .stopPresence(id, _):
      return id
    }
  }

  private static func path(_ instance: MobileInstance) -> (service: Int, characteristic: Int)? {
    guard let service = Int(exactly: instance.serviceOccurrence),
          let characteristic = Int(exactly: instance.characteristicOccurrence) else { return nil }
    return (service, characteristic)
  }

  private static func unsigned(_ value: Any?) -> UInt64? {
    (value as? NSNumber).flatMap { UInt64(exactly: $0.int64Value) }
  }

  private static func flag(_ value: Any?) -> Bool {
    (value as? NSNumber)?.boolValue ?? false
  }
}
