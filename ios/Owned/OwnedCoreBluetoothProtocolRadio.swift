// ios/Owned/OwnedCoreBluetoothProtocolRadio.swift
import CoreBluetooth
import Foundation

@objc public protocol OwnedCoreBluetoothProtocolRadioDelegate: NSObjectProtocol {
  func protocolRadioDidUpdateAdapterState(_ snapshot: NSDictionary)
  func protocolRadioDidReceiveAdvertisement(_ advertisement: NSDictionary)
  func protocolRadioDidDisconnectPeer(_ peerIdentifier: String, error: NSError?)
  func protocolRadioDidModifyServices(_ peerIdentifier: String)
  func protocolRadioDidReceiveNotification(_ subscriptionIdentifier: String, value: NSData)
  /// A terminal notification failure; the host must close the matching stream.
  /// Optional for binary/source compatibility with existing delegates.
  @objc optional func protocolRadioDidFailNotification(_ subscriptionIdentifier: String, error: NSError)
  /// `willRestoreState` peers (Native Protocol v2 reads `restorationPeerIdentifiers` instead).
  @objc optional func protocolRadioDidRestorePeers(_ peers: [NSDictionary])
  /// CoreBluetooth says the without-response queue can accept another write.
  /// Optional so existing protocol-radio delegates keep compiling.
  @objc optional func protocolRadioDidBecomeReadyToSendWriteWithoutResponse(_ peerIdentifier: String)
}

/// A rejected observation belongs to the scan generation that observed it.
/// `exactCount == nil` means the exact count is unavailable (for example,
/// duplicate filtering or counter exhaustion).
struct OwnedScanLoss {
  let generation: UUID
  let exactCount: UInt64?
}

/// Returning false retains the loss for retry. This is a separate control path
/// from advertisements, so saturated data ingress cannot consume its capacity.
protocol OwnedScanLossReceiver: AnyObject {
  func receiveScanLoss(_ loss: OwnedScanLoss) -> Bool
}

/// A receiver can report Rust data-ingress refusal for the actual observation.
protocol OwnedScanAdvertisementReceiver: AnyObject {
  func receiveScanAdvertisement(_ advertisement: NSDictionary, generation: UUID) -> Bool
}

/// Snapshots a subscription's delivery target before a callback is deferred.
protocol OwnedDeferredNotificationFailureReceiver: AnyObject {
  func captureNotificationFailure(_ identifier: String) -> ((NSError) -> Void)?
}

/// Keeps a borrower attached while captured failures still own its sink.
protocol OwnedNotificationFailureRetention: AnyObject {
  func hasPendingNotificationFailureRetention() -> Bool
}

/**
 * Direct CoreBluetooth radio façade for Native Protocol v2.
 *
 * Control records cross the JSI boundary in C++, while this class receives and produces native
 * Data only. Every mutable CoreBluetooth object stays confined to its serial radio queue.
 */
@objc(OwnedCoreBluetoothProtocolRadio)
public final class OwnedCoreBluetoothProtocolRadio: NSObject, CBPeripheralDelegate {
  static let maximumBinaryPayloadBytes = 512 * 1024
  private static let radioQueue = DispatchQueue(
    label: "com.sfourdrinier.unifiedblemanager.unified-protocol-radio"
  )

  @objc public weak var delegate: OwnedCoreBluetoothProtocolRadioDelegate?
  final class NotificationRoute {
    // A route can be removed while a notification callback is still owed.
    // ObjectIdentifier alone is not a lifetime identity: its address may be
    // reused by a replacement route before the callback arrives.
    let identity = UUID()
    weak var borrower: OwnedCoreBluetoothProtocolRadioDelegate?
    var claimIdentity: UUID?
    var orderedClaim: OrderedNotificationClaim?
    init(_ borrower: OwnedCoreBluetoothProtocolRadioDelegate) { self.borrower = borrower }
  }
  var notificationRoutes: [String: NotificationRoute] = [:]
  let borrowerRelease = OwnedCoreBluetoothBorrowerReleaseCoordinator()
  var borrowerReleaseRetryScheduled: [ObjectIdentifier: UUID] = [:]

  let queue: DispatchQueue
  /// The process central, allocated on first explicit need (finding 179:
  /// first allocation prompts while undecided in ordinary non-ASK apps).
  /// ASK explicit operations use accessory-scoped authorization. Never deallocated.
  var central: CBCentralManager?
  var centralDelegate: OwnedCoreBluetoothCentralDelegate!
  let restoreIdentifierKey: String?
  let showPowerAlert: NSNumber?
  /// The Apple permission request; its exchange stays single-flight across
  /// requests. Created eagerly: the closures only run on explicit request.
  private lazy var prompter = ApplePermissionPrompter(
    currentAuthorization: OwnedCoreBluetoothProtocolRadioSupport.currentAuthorizationWord,
    ensureCentral: { [unowned self] in _ = self.ensureCentral() },
    schedule: { [unowned self] delayMs, work in
      let item = DispatchWorkItem(block: work)
      self.queue.asyncAfter(deadline: .now() + .milliseconds(Int(delayMs)), execute: item)
      return { item.cancel() }
    },
    makeError: { [unowned self] code, message in self.error(code: code, message: message) },
    accessorySetupConfigured: {
      OwnedCoreBluetoothProtocolRadioSupport.accessorySetupConfigured(info: Bundle.main.infoDictionary ?? [:])
    }
  )
  var peripheralByIdentifier = [String: CBPeripheral]()
  var servicesByPeer = [String: [CBService]]()
  var pendingConnect = [String: PendingVoid]()
  struct ConnectionClaim {
    let owner: AnyObject
    let route: OwnedCoreBluetoothProtocolRadioDelegate?
    var pending: PendingVoid?
    let identity = UUID()
  }
  var connectionClaims: [String: [ObjectIdentifier: ConnectionClaim]] = [:]
  var releasingConnectionClaims: [String: Set<ObjectIdentifier>] = [:]
  var releasingConnectionOperations: [String: [ObjectIdentifier: String]] = [:]
  var releasingConnectionTickets: [String: [ObjectIdentifier: UUID]] = [:]
  var disconnectOwners: [String: (owner: AnyObject, operation: String)] = [:]
  struct ConnectionCancellation {
    let owner: AnyObject
    let physicalOperation: String
  }
  var connectionCancellationKeys: [ObjectIdentifier: [String: ConnectionCancellation]] = [:]
  /// Physical cleanup identities for borrower operations without a GATT slot.
  var fallbackCancellationKeys: [ObjectIdentifier: [String: ConnectionCancellation]] = [:]
  /// Captured at delegate ingress so callbacks deferred by a graph permit
  /// cannot settle a later connection generation.
  var connectionEpochs: [String: UUID] = [:]
  var pendingConnectionTerminals: [String: UUID] = [:]
  /// A connect or restored attachment can still produce a terminal callback,
  /// even when the peripheral has already changed its visible state.
  var unfencedConnections = Set<String>()
  /// A disconnect resolves only from CoreBluetooth's terminal delegate callback.
  var pendingDisconnect = [String: PendingVoid]()
  var pendingDiscovery = [String: PendingDiscovery]()
  /// Characteristic reads per characteristic, in request order.
  struct ScopedGattOperation {
    let owner: AnyObject
    let claim: UUID
    let peer: String
    let physical: String
  }
  var scopedGattOperations: [ObjectIdentifier: [String: ScopedGattOperation]] = [:]
  var pendingRead = [CharacteristicAddress: OwnedCoreBluetoothReadLane<PendingCharacteristicRead>]()
  var readObjects = [CharacteristicAddress: OwnedCoreBluetoothReadObjectFence]()
  var retiredReads = [OwnedCoreBluetoothReadObjectFence]()
  let pendingRssi = OwnedCoreBluetoothCallbackLedger<String, (NSNumber?, NSError?)>()
  let pendingWrite = OwnedCoreBluetoothCallbackLedger<CharacteristicAddress, NSError?>()
  let descriptorOperations = OwnedCoreBluetoothDescriptorOperations()
  var pendingNotify = [CharacteristicAddress: PendingNotify]()
  var pendingNotifyJoins = [CharacteristicAddress: [String: PendingNotify]]()
  /// The platform object which issued the in-flight transition. An address can
  /// be reused after rediscovery, so it cannot attribute callbacks by itself.
  var notificationTransitionObjects = [CharacteristicAddress: ObjectIdentifier]()
  var notificationTransitionTickets = [CharacteristicAddress: UUID]()
  var notificationGenerations = [CharacteristicAddress: String]()
  var retiredNotificationOwners = [ObjectIdentifier: RetiredNotificationOwner<CBPeripheral, CBCharacteristic>]()
  /// Optional queue-confined writer for deterministic notification lifecycle tests.
  var notificationStateWriter: ((CharacteristicAddress, Bool) -> Bool)?
  var pendingCancellationCleanup = [String: PendingCancellationCleanup]()
  /// Invalidated paths cannot safely address a newly discovered characteristic.
  var invalidatedNotificationCleanup = Set<CharacteristicAddress>()
  /// At most one automatic retry may be queued for a cancelled operation.  The
  /// retained cleanup entry, rather than an unbounded timer fan-out, is the
  /// source of truth for the physical CCCD transition still in flight.
  var pendingCancellationCleanupRetryScheduled = Set<String>()
  var subscriptions = [CharacteristicAddress: String]()
  /// Additional logical owners sharing an already enabled physical CCCD.
  var sharedSubscriptions = [CharacteristicAddress: Set<String>]()
  /// Current source per connected peer; invalidated on disconnect or database change.
  var orderedSources = [String: OwnedOrderedNotificationSource]()
  /// Installed by the ordered-group bridge.  Native callback failures must
  /// enter the same bounded positional ledger as explicit bridge captures.
  var orderedFailureCapture: ((OrderedNotificationRoute, OwnedOrderedNotificationIngress.NativeFailure, OrderedNotificationClaim?) -> (() -> Void)?)?
  /// A graph gate can be full while a delegate callback has already captured
  /// its exact source and borrower. Keep that callback outside the ordinary
  /// FIFO only until it can be appended behind the accepted prefix; it is
  /// never delivered synchronously ahead of that prefix.
  struct RetainedNotificationReplay {
    let id: UUID
    let gate: OwnedOrderedGraphPermitGate
    let replay: () -> Void
  }
  /// A retained replay only represents a request that was already accepted by
  /// the graph, or the one request that discovered that graph was full.  It
  /// therefore has the same upper bound as the graph's request capacity; do
  /// not introduce an unrelated smaller loss limit here.
  static let retainedNotificationReplayLimit = 1_024
  var retainedNotificationReplays: [String: [RetainedNotificationReplay]] = [:]
  var retainedNotificationReplayRetryScheduled = Set<String>()
  let orderedSnapshotIdentities = OwnedGattSnapshotTracker()
  let orderedGraphAuthorities = OwnedOrderedGraphAuthorities()
  var orderedGraphPermitGates: [String: OwnedOrderedGraphPermitGate] = [:]
  var orderedPermitSources: [String: OwnedOrderedNotificationSource] = [:]
  struct LostDisconnect { let error: NSError? }
  var orderedLostDisconnects: [String: LostDisconnect] = [:]
  var orderedOverflowDisconnectPending = Set<String>()
  var orderedConfirmedDisconnected = Set<String>()
  struct PendingOverflowCentralState { let apply: () -> Void }
  var pendingOverflowCentralState: PendingOverflowCentralState?
  struct ScanLease {
    let owner: AnyObject
    let route: OwnedCoreBluetoothProtocolRadioDelegate?
    let operationIdentifier: String
    let generation: UUID
    let services: Set<String>
    let allowDuplicates: Bool
    // Only a positively matched observation consumes the first-delivery slot.
    var deliveredPeers: Set<String> = []
  }
  var scanLeases: [ObjectIdentifier: ScanLease] = [:]
  struct PendingScanCallback {
    let peripheral: CBPeripheral
    let snapshot: NSDictionary
    let claims: [(ObjectIdentifier, UUID)]
    let bytes: Int
  }
  struct PendingScanLoss {
    let route: OwnedCoreBluetoothProtocolRadioDelegate
    let generation: UUID
    var exactCount: UInt64?
    var revision: UUID
  }
  // One reserved loss slot per admitted scan owner, including stopped owners
  // whose loss is awaiting acknowledgement.
  static let scanOwnerLimit = 128
  static let scanCallbackLimit = 1024
  static let scanCallbackByteLimit = 4 * 1024 * 1024
  var pendingScanCallbacks: [PendingScanCallback] = []
  var pendingScanCallbackBytes = 0
  var pendingScanLosses: [ObjectIdentifier: PendingScanLoss] = [:]
  var scanDeliveringOwners = Set<ObjectIdentifier>()
  var scanDrainScheduled = false
  var scanDraining = false
  var scanRetryScheduled = false
  var physicalScanActive = false
  var restoredPeerIdentifiers = [String]()
  var destroyed = false
  let radioPreparation = AppleRadioPreparation()

  @objc public convenience init(restoreIdentifierKey: String?) {
    self.init(restoreIdentifierKey: restoreIdentifierKey, showPowerAlert: nil)
  }

  @objc public init(restoreIdentifierKey: String?, showPowerAlert: NSNumber?) {
    queue = Self.radioQueue
    self.restoreIdentifierKey = restoreIdentifierKey
    self.showPowerAlert = showPowerAlert
    super.init()
    #if os(iOS)
    if OwnedCoreBluetoothProtocolRadioSupport.restorationConfigured(restoreIdentifierKey: restoreIdentifierKey) {
      centralDelegate = OwnedCoreBluetoothRestoringCentralDelegate(radio: self)
    } else {
      centralDelegate = OwnedCoreBluetoothCentralDelegate(radio: self)
    }
    #else
    centralDelegate = OwnedCoreBluetoothCentralDelegate(radio: self)
    #endif
  }

  @objc public func adapterSnapshot() -> NSDictionary {
    queue.sync { self.snapshotOnQueue() }
  }

  /// Finding 179: the Apple permission request on the process radio (like
  /// Android's Expo module). Decided words answer at once; otherwise the
  /// central allocation prompts and the waiter reports the decision.
  /// `timeoutMs` bounds an unanswered prompt; concurrent requests refuse.
  @objc public func requestPermission(
    timeoutMs: NSNumber,
    completion: @escaping (NSDictionary?, NSError?) -> Void
  ) {
    queue.async {
      guard self.prompter.start(timeoutMs: timeoutMs.uint64Value, completion: completion) else {
        completion(nil, self.error(code: 1037, message: "A Bluetooth permission request is already in progress"))
        return
      }
    }
  }

  /// Supplies native startup restoration state for canonical protocol-journal append.
  @objc public func restorationPeerIdentifiers() -> [String] {
    queue.sync { restoredPeerIdentifiers }
  }

  /// Consumes only the OS-provided restoration identifiers after an authenticated adoption succeeds.
  @objc public func consumeRestorationPeerIdentifiers() {
    queue.async {
      self.restoredPeerIdentifiers.removeAll()
    }
  }

  /// Releases one JavaScript protocol client while preserving the process-owned restoration central.
  @objc public func releaseProtocolClient(completion: @escaping (NSError?) -> Void) {
    releaseProtocolClient(scanOwner: nil, completion: completion)
  }

  func releaseProtocolClient(scanOwner: AnyObject?, completion: @escaping (NSError?) -> Void) {
    queue.async {
      self.drainScanCallbacks()
      guard scanOwner.map({ self.pendingScanLosses[ObjectIdentifier($0)] == nil }) ?? self.pendingScanLosses.isEmpty else {
        completion(self.error(code: 1014, message: "Scan loss delivery is pending; retry release"))
        return
      }
      var notificationCleanupBlocked = false
      self.performOrderedGlobalBarrier(peerOperation: { peer in
        if self.restoredPeerIdentifiers.contains(peer), !self.retireNotificationsForRelease(peer) {
          notificationCleanupBlocked = true
          return
        }
        self.resetOrderedPeer(peer, cause: .shutdown)
      }, onFailure: {
        completion(self.error(code: 1014, message: "Ordered lifecycle control capacity exhausted"))
      }) {
      guard !self.destroyed else {
        completion(self.error(code: 1021, message: "The Native Protocol v2 CoreBluetooth radio was destroyed"))
        return
      }
      // A graph barrier can wait while more scan callbacks arrive.
      self.drainScanCallbacks()
      guard scanOwner.map({ self.pendingScanLosses[ObjectIdentifier($0)] == nil }) ?? self.pendingScanLosses.isEmpty else {
        completion(self.error(code: 1014, message: "Scan loss delivery is pending; retry release"))
        return
      }
      let restored = Set(self.restoredPeerIdentifiers)
      if notificationCleanupBlocked || self.retiredNotificationOwners.values.contains(where: { restored.contains($0.address.peerIdentifier) }) {
        completion(self.error(code: 1039, message: "Restored notification cleanup is pending; retry release"))
        return
      }
      if let scanOwner {
        if let failure = self.releaseScanLease(owner: scanOwner) { completion(failure); return }
      } else {
        self.clearAllScanLeases()
      }
      self.failAllPendingOperationsOnDestroy()
      self.invalidateAllOrderedSources(.shutdown)
      self.disableActiveNotifications()
      self.subscriptions.removeAll()
      self.sharedSubscriptions.removeAll()
      let restoredIdentifiers = Set(self.restoredPeerIdentifiers)
      self.completeAfterPendingDisconnects(
        preserving: restoredIdentifiers,
        destroyRadio: false,
        completion: completion
      )
      }
    }
  }

  @objc public func startScan(
    serviceUUIDs: [String], allowDuplicates: Bool, operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  ) {
    queue.async {
      let owner: AnyObject = self.delegate ?? self
      self.startScanOnQueue(owner: owner, route: self.delegate, serviceUUIDs: serviceUUIDs,
        allowDuplicates: allowDuplicates, operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  @objc public func stopScan(operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    queue.async {
      guard self.requireUsable(completion) else { return }
      let owner: AnyObject = self.delegate ?? self
      self.stopScanOnQueue(owner: owner, completion: completion)
    }
  }

  @objc public func connect(
    peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSError?) -> Void
  ) {
    queue.async {
      let owner: AnyObject = self.delegate ?? self
      self.connectOnQueue(owner: owner, route: self.delegate, peerIdentifier: peerIdentifier,
        operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  func connect(borrower: OwnedCoreBluetoothProtocolRadioDelegate, peerIdentifier: String,
               operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    queue.async {
      if borrower is OwnedCoreBluetoothFencedBorrower && !self.borrowerRelease.admits(borrower) {
        completion(self.error(code: 1014, message: "Connection borrower is retiring or detached")); return
      }
      self.connectOnQueue(owner: borrower, route: borrower, peerIdentifier: peerIdentifier,
        operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  func connectOnQueue(owner: AnyObject, route: OwnedCoreBluetoothProtocolRadioDelegate?, peerIdentifier: String,
                      operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    guard requireUsable(completion) else { return }
    guard let identifier = OwnedCoreBluetoothKnownPeerLookup.identifier(peerIdentifier) else {
      completion(error(code: 1005, message: "The requested CoreBluetooth peripheral is unknown")); return
    }
    guard !orderedOverflowDisconnectPending.contains(peerIdentifier), pendingDisconnect[peerIdentifier] == nil,
          pendingConnectionTerminals[peerIdentifier] == nil,
          !pendingCancellationCleanup.values.contains(where: { $0.peerIdentifiers.contains(peerIdentifier) }) else {
      completion(error(code: 1025, message: "The previous connection generation is still being fenced")); return
    }
    let key = ObjectIdentifier(owner)
    guard releasingConnectionClaims[peerIdentifier]?.contains(key) != true else {
      completion(error(code: 1024, message: "Connection claim cleanup is pending")); return
    }
    if let existing = connectionClaims[peerIdentifier]?[key] {
      if existing.pending != nil { completion(error(code: 1006, message: "A connection is already pending for this borrower")) }
      else if peripheralByIdentifier[peerIdentifier]?.state == .connected { completion(nil) }
      else { completion(error(code: 1025, message: "The previous connection generation is awaiting its terminal callback")) }
      return
    }
    if pendingConnect[peerIdentifier] != nil {
      guard connectionClaims[peerIdentifier]?.isEmpty == false else {
        completion(error(code: 1006, message: "A connection is already pending for this peripheral")); return
      }
      connectionClaims[peerIdentifier, default: [:]][key] = ConnectionClaim(owner: owner, route: route,
        pending: PendingVoid(operationIdentifier: operationIdentifier, completion: completion))
      return
    }
    guard let central = centralForUse(or: completion) else { return }
    let cached = peripheralByIdentifier[peerIdentifier]
    guard let peripheral = OwnedCoreBluetoothKnownPeerLookup.resolve(identifier: identifier, cached: cached,
      retrieve: { central.retrievePeripherals(withIdentifiers: [$0]) }, identifierOf: { $0.identifier }) else {
      completion(error(code: 1005, message: "The requested CoreBluetooth peripheral is unknown")); return
    }
    guard connectionGenerationAvailable(peerIdentifier, isConnected: peripheral.state == .connected) else {
      completion(error(code: 1025, message: "The previous connection generation is awaiting its terminal callback")); return
    }
    orderedConfirmedDisconnected.remove(peerIdentifier)
    if connectionEpochs[peerIdentifier] == nil { connectionEpochs[peerIdentifier] = UUID() }
    peripheralByIdentifier[peerIdentifier] = peripheral
    peripheral.delegate = self
    let connected = !OwnedCoreBluetoothKnownPeerLookup.requiresConnection(
      hasCachedPeripheral: cached != nil, isConnected: peripheral.state == .connected)
    connectionClaims[peerIdentifier, default: [:]][key] = ConnectionClaim(owner: owner, route: route,
      pending: connected ? nil : PendingVoid(operationIdentifier: operationIdentifier, completion: completion))
    unfencedConnections.insert(peerIdentifier)
    if connected { completion(nil); return }
    // The physical operation has its own identity; caller IDs are only unique
    // within a borrower and must never identify the shared platform request.
    pendingConnect[peerIdentifier] = PendingVoid(operationIdentifier: "connect-physical-\(UUID().uuidString)") { [weak self] failure in
      self?.settleConnectionClaims(peerIdentifier, failure: failure)
    }
    central.connect(peripheral, options: nil)
  }

  func settleConnectionClaims(_ peer: String, failure: NSError?) {
    guard let claims = connectionClaims[peer] else { return }
    if failure != nil { connectionClaims.removeValue(forKey: peer) }
    else {
      for (key, var claim) in claims { claim.pending = nil; connectionClaims[peer]?[key] = claim }
    }
    for claim in claims.values { claim.pending?.completion(failure) }
  }

  @objc public func disconnect(
    peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSError?) -> Void
  ) {
    queue.async {
      let owner: AnyObject = self.delegate ?? self
      // A restored connection may predate any logical connect call.
      if self.connectionClaims[peerIdentifier] == nil, self.pendingDisconnect[peerIdentifier] == nil,
         self.peripheralByIdentifier[peerIdentifier] != nil {
        self.connectionClaims[peerIdentifier] = [ObjectIdentifier(owner): ConnectionClaim(owner: owner, route: self.delegate, pending: nil)]
      }
      self.disconnectOnQueue(owner: owner, peerIdentifier: peerIdentifier,
        operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  func disconnect(borrower: OwnedCoreBluetoothProtocolRadioDelegate, peerIdentifier: String,
                  operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    queue.async { self.disconnectOnQueue(owner: borrower, peerIdentifier: peerIdentifier,
      operationIdentifier: operationIdentifier, completion: completion) }
  }

  func disconnectOnQueue(owner: AnyObject, peerIdentifier: String, operationIdentifier: String,
                         completion: @escaping (NSError?) -> Void) {
    guard requireUsable(completion) else { return }
    guard let peripheral = peripheralByIdentifier[peerIdentifier] else {
      completion(error(code: 1007, message: "The requested CoreBluetooth peripheral is unknown")); return
    }
    guard pendingDisconnect[peerIdentifier] == nil else {
      completion(error(code: 1024, message: "A disconnect is already pending for this peripheral")); return
    }
    let key = ObjectIdentifier(owner)
    guard let claim = connectionClaims[peerIdentifier]?[key], claim.owner === owner else {
      completion(error(code: 1007, message: "This borrower does not own the connection")); return
    }
    if (connectionClaims[peerIdentifier]?.count ?? 0) > 1 &&
       (pendingCancellationCleanup.values.contains(where: { cleanup in
         cleanup.notificationDesiredStates.keys.contains(where: { $0.peerIdentifier == peerIdentifier }) ||
           cleanup.notificationAwaitingCallbacks.contains(where: { $0.peerIdentifier == peerIdentifier }) ||
           cleanup.retiredNotificationObjects.contains(where: { self.retiredNotificationOwners[$0]?.address.peerIdentifier == peerIdentifier })
       }) || retiredNotificationOwners.values.contains(where: { $0.address.peerIdentifier == peerIdentifier })) {
      completion(error(code: 1018, message: "Peer notification cleanup is unresolved; connection claim remains owned")); return
    }
    if peripheral.state != .disconnected && connectionClaims[peerIdentifier]?.count == 1 {
      guard centralForUse(or: completion) != nil else { return }
    }
    guard releasingConnectionClaims[peerIdentifier]?.contains(key) != true else {
      completion(error(code: 1024, message: "Connection claim cleanup is already pending")); return
    }
    releasingConnectionClaims[peerIdentifier, default: []].insert(key)
    releasingConnectionOperations[peerIdentifier, default: [:]][key] = operationIdentifier
    let releaseTicket = UUID()
    releasingConnectionTickets[peerIdentifier, default: [:]][key] = releaseTicket
    retireConnectionNotifications(owner: owner, peer: peerIdentifier) { failure in
      guard self.releasingConnectionTickets[peerIdentifier]?[key] == releaseTicket else {
        completion(self.error(code: 1016, message: "Connection cleanup was superseded")); return
      }
      self.releasingConnectionTickets[peerIdentifier]?.removeValue(forKey: key)
      if self.releasingConnectionTickets[peerIdentifier]?.isEmpty == true { self.releasingConnectionTickets.removeValue(forKey: peerIdentifier) }
      self.releasingConnectionClaims[peerIdentifier]?.remove(key)
      self.releasingConnectionOperations[peerIdentifier]?.removeValue(forKey: key)
      if self.releasingConnectionOperations[peerIdentifier]?.isEmpty == true { self.releasingConnectionOperations.removeValue(forKey: peerIdentifier) }
      if self.releasingConnectionClaims[peerIdentifier]?.isEmpty == true { self.releasingConnectionClaims.removeValue(forKey: peerIdentifier) }
      guard self.connectionClaims[peerIdentifier]?[key]?.identity == claim.identity else {
        completion(self.error(code: 1016, message: "Connection claim changed during cleanup")); return
      }
      guard failure == nil else { completion(failure); return }
      guard self.connectionClaims[peerIdentifier]?[key]?.owner === owner else {
        completion(self.error(code: 1016, message: "Connection claim ended during cleanup")); return
      }
      self.finishConnectionDisconnect(owner: owner, claim: claim, peripheral: peripheral,
        peerIdentifier: peerIdentifier, operationIdentifier: operationIdentifier, completion: completion)
    }
  }

  func finishConnectionDisconnect(owner: AnyObject, claim: ConnectionClaim, peripheral: CBPeripheral,
                                  peerIdentifier: String, operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    let key = ObjectIdentifier(owner)
    if let pending = pendingDiscovery[peerIdentifier], pending.owner === owner,
       pending.claimIdentity == claim.identity {
      failDiscovery(peerIdentifier, pending: pending,
        error: error(code: 1020, message: "Connection claim released during discovery"))
    }
    retireScopedGatt(owner: owner, claim: claim.identity, peer: peerIdentifier)
    connectionClaims[peerIdentifier]?.removeValue(forKey: key)
    claim.pending?.completion(error(code: 1020, message: "Connection claim released while connecting"))
    if connectionClaims[peerIdentifier]?.isEmpty == false { completion(nil); return }
    connectionClaims.removeValue(forKey: peerIdentifier)
    guard peripheral.state != .disconnected || unfencedConnections.contains(peerIdentifier) else {
      servicesByPeer.removeValue(forKey: peerIdentifier); completion(nil); return
    }
    unfencedConnections.insert(peerIdentifier)
    disconnectOwners[peerIdentifier] = (owner, operationIdentifier)
    pendingDisconnect[peerIdentifier] = PendingVoid(operationIdentifier: "disconnect-physical-\(UUID().uuidString)", completion: completion)
    let disconnectIdentity = pendingDisconnect[peerIdentifier]?.identity
    queue.asyncAfter(deadline: .now() + .seconds(5)) {
      guard let pending = self.pendingDisconnect[peerIdentifier], pending.identity == disconnectIdentity else { return }
      self.pendingDisconnect[peerIdentifier] = PendingVoid(operationIdentifier: pending.operationIdentifier, completion: { _ in })
      self.disconnectOwners.removeValue(forKey: peerIdentifier)
      pending.completion(self.error(code: 1026, message: "CoreBluetooth disconnect confirmation timed out; the connection remains fenced"))
    }
    if peripheral.state != .disconnected { central?.cancelPeripheralConnection(peripheral) }
  }

  func connectionGenerationAvailable(_ peerIdentifier: String, isConnected: Bool) -> Bool {
    pendingDisconnect[peerIdentifier] == nil &&
      !pendingCancellationCleanup.values.contains(where: { $0.peerIdentifiers.contains(peerIdentifier) }) &&
      (!unfencedConnections.contains(peerIdentifier) || isConnected)
  }

  @objc public func discover(
    peerIdentifier: String,
    operationIdentifier: String,
    completion: @escaping (NSDictionary?, NSError?) -> Void
  ) {
    discoverOwned(owner: nil, peerIdentifier: peerIdentifier, operationIdentifier: operationIdentifier, completion: completion)
  }

  func discover(borrower: OwnedCoreBluetoothProtocolRadioDelegate, peerIdentifier: String,
                operationIdentifier: String, completion: @escaping (NSDictionary?, NSError?) -> Void) {
    discoverOwned(owner: borrower, peerIdentifier: peerIdentifier, operationIdentifier: operationIdentifier, completion: completion)
  }

  private func discoverOwned(owner: AnyObject?, peerIdentifier: String, operationIdentifier: String,
                             completion: @escaping (NSDictionary?, NSError?) -> Void) {
    queue.async {
      guard self.requireUsable({ error in completion(nil, error) }) else { return }
      var claimIdentity: UUID?
      if let owner {
        guard let claim = self.connectionClaims[peerIdentifier]?[ObjectIdentifier(owner)], claim.owner === owner,
              claim.pending == nil, self.releasingConnectionClaims[peerIdentifier]?.contains(ObjectIdentifier(owner)) != true,
              !(owner is OwnedCoreBluetoothFencedBorrower) || self.borrowerRelease.admits(owner) else {
          completion(nil, self.error(code: 1007, message: "This borrower does not own an active connection claim")); return
        }
        claimIdentity = claim.identity
      }
      guard !self.orderedOverflowDisconnectPending.contains(peerIdentifier) else {
        completion(nil, self.error(code: 1014, message: "The previous connection is being fenced after ordered source overflow"))
        return
      }
      guard let peripheral = self.peripheralByIdentifier[peerIdentifier], peripheral.state == .connected else {
        completion(nil, self.error(code: 1008, message: "The requested peripheral is not connected"))
        return
      }
      guard self.pendingDiscovery[peerIdentifier] == nil else {
        completion(nil, self.error(code: 1009, message: "A discovery operation is already pending for this peripheral"))
        return
      }
      guard self.orderedSnapshotIdentities.ensureAttachment(peerIdentifier) else {
        completion(nil, self.error(code: 1009, message: "Native attachment identity exhausted"))
        return
      }
      if let identity = self.orderedSnapshotIdentities.current(peerIdentifier) {
        do {
          guard let snapshot = try OwnedCoreBluetoothProtocolRadioSupport.discoverySnapshot(self.servicesByPeer[peerIdentifier] ?? []).mutableCopy() as? NSMutableDictionary else {
            completion(nil, self.error(code: 1009, message: "Native discovery snapshot could not be copied")); return
          }
          snapshot["nativeSnapshotOwner"] = identity.owner
          snapshot["nativeSnapshotAttachment"] = NSNumber(value: identity.attachment)
          snapshot["nativeSnapshotRevision"] = NSNumber(value: identity.revision)
          completion(snapshot, nil)
        } catch { completion(nil, error as NSError) }
        return
      }
      let physical: String
      if let owner {
        guard let id = self.registerScopedGatt(owner: owner, peer: peerIdentifier, operation: operationIdentifier) else {
          completion(nil, self.error(code: 1014, message: "Operation identifier is already owned")); return
        }
        physical = id
      } else { physical = operationIdentifier }
      peripheral.delegate = self
      self.pendingDiscovery[peerIdentifier] = PendingDiscovery(
        operationIdentifier: physical,
        completion: { snapshot, failure in
          if let owner {
            let current = self.scopedGattClaimIsCurrent(owner: owner, peer: peerIdentifier, operation: operationIdentifier, physical: physical)
            self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical)
            if !current && failure == nil {
              completion(nil, self.error(code: 1020, message: "Connection claim changed before discovery callback")); return
            }
          }
          completion(snapshot, failure)
        }, owner: owner, claimIdentity: claimIdentity
      )
      peripheral.discoverServices(nil)
    }
  }

  /// Reads one characteristic, also while it notifies. Reads of one
  /// characteristic are serialized; each completion carries what
  /// CoreBluetooth can say the value is ([OwnedCoreBluetoothReadProvenance]).
  /// A value that completes a read while the characteristic can notify still
  /// reaches its subscription. An ambiguous completion blocks later reads on
  /// that object until disconnect or adapter invalidation.
  @objc public func readCharacteristic(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    operationIdentifier: String,
    completion: @escaping (NSData?, OwnedCoreBluetoothReadProvenance, NSError?) -> Void
  ) {
    readCharacteristicOwned(owner: nil, peerIdentifier: peerIdentifier, serviceUUID: serviceUUID,
      serviceOccurrence: serviceOccurrence, characteristicUUID: characteristicUUID,
      characteristicOccurrence: characteristicOccurrence, operationIdentifier: operationIdentifier, completion: completion)
  }

  func readCharacteristic(borrower: OwnedCoreBluetoothProtocolRadioDelegate, peerIdentifier: String,
    serviceUUID: String, serviceOccurrence: Int, characteristicUUID: String, characteristicOccurrence: Int,
    operationIdentifier: String, completion: @escaping (NSData?, OwnedCoreBluetoothReadProvenance, NSError?) -> Void) {
    readCharacteristicOwned(owner: borrower, peerIdentifier: peerIdentifier, serviceUUID: serviceUUID,
      serviceOccurrence: serviceOccurrence, characteristicUUID: characteristicUUID,
      characteristicOccurrence: characteristicOccurrence, operationIdentifier: operationIdentifier, completion: completion)
  }

  private func readCharacteristicOwned(owner: AnyObject?, peerIdentifier: String,
    serviceUUID: String, serviceOccurrence: Int, characteristicUUID: String, characteristicOccurrence: Int,
    operationIdentifier: String, completion: @escaping (NSData?, OwnedCoreBluetoothReadProvenance, NSError?) -> Void) {
    queue.async {
      guard self.requireUsable({ error in completion(nil, .readResponse, error) }) else { return }
      if let owner, !self.hasActiveGattClaim(owner: owner, peer: peerIdentifier) {
        completion(nil, .readResponse, self.error(code: 1007, message: "This borrower does not own an active connection claim")); return
      }
      let address = CharacteristicAddress(
        peerIdentifier: peerIdentifier,
        serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(serviceUUID),
        serviceOccurrence: serviceOccurrence,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(characteristicUUID),
        characteristicOccurrence: characteristicOccurrence
      )
      guard let resolved = self.resolve(address) else {
        completion(nil, .readResponse, self.error(code: 1010, message: "The generation-bound characteristic path is stale"))
        return
      }
      guard self.notificationObjectIsAvailable(resolved.characteristic) else {
        completion(nil, .readResponse, self.error(code: 1010, message: "The previous characteristic generation is still being cleaned up"))
        return
      }
      guard self.readCallbackObjectIsAvailable(peripheral: resolved.peripheral, characteristic: resolved.characteristic),
            self.readObjects[address].map({ $0.matches(peripheral: resolved.peripheral, object: resolved.characteristic) }) ?? true else {
        completion(nil, .readResponse, self.error(code: 1010, message: "The previous characteristic read callback is still outstanding"))
        return
      }
      guard self.readObjects[address]?.cancelledOperation == nil ||
            self.readObjects[address]?.unattributable != true else {
        completion(nil, .readOrNotification, self.error(code: 1010,
          message: "Notifications prevent attribution of the cancelled characteristic read callback"))
        return
      }
      guard self.readObjects[address]?.ambiguousCompletion != true else {
        completion(nil, .readOrNotification, self.error(code: 1010,
          message: "The previous read response cannot be distinguished from notifications until disconnect"))
        return
      }
      if let owner, self.scopedGattOperations[ObjectIdentifier(owner)]?[operationIdentifier] != nil {
        completion(nil, .readResponse, self.error(code: 1014, message: "Operation identifier is already owned")); return
      }
      if self.readObjects[address] == nil {
        guard self.readObjects.count + self.retiredReads.count < 256 else {
          completion(nil, .readResponse, self.error(code: 1010, message: "Characteristic read callback capacity exhausted"))
          return
        }
        self.readObjects[address] = OwnedCoreBluetoothReadObjectFence(
          peripheral: resolved.peripheral, object: resolved.characteristic, cancelledOperation: nil,
          unattributable: resolved.characteristic.isNotifying || self.subscriptions[address] != nil ||
            self.pendingNotify[address] != nil || self.hasPendingCancellationCleanup(for: address))
      }
      let physical: String
      if let owner {
        guard let id = self.registerScopedGatt(owner: owner, peer: peerIdentifier, operation: operationIdentifier) else {
          completion(nil, .readResponse, self.error(code: 1014, message: "Operation identifier is already owned")); return
        }
        physical = id
      } else { physical = operationIdentifier }
      let waiter = PendingCharacteristicRead(operationIdentifier: physical) { value, provenance, failure in
        if let owner {
          let current = self.scopedGattClaimIsCurrent(owner: owner, peer: peerIdentifier, operation: operationIdentifier, physical: physical)
          self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical)
          if !current && failure == nil {
            completion(nil, provenance, self.error(code: 1020, message: "Connection claim changed before read callback")); return
          }
        }
        completion(value, provenance, failure)
      }
      if self.pendingRead[address, default: OwnedCoreBluetoothReadLane()].admit(waiter) {
        resolved.peripheral.readValue(for: resolved.characteristic)
      }
    }
  }

  /// Native Protocol v2 read (`UnifiedBleProtocolAppleExecution.mm`). That wire
  /// cannot carry a read's provenance, so a value CoreBluetooth cannot
  /// attribute to the read response fails with 1031 there instead of being
  /// reported as one.
  @objc public func read(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    operationIdentifier: String,
    completion: @escaping (NSData?, NSError?) -> Void
  ) {
    readCharacteristic(
      peerIdentifier: peerIdentifier,
      serviceUUID: serviceUUID,
      serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID,
      characteristicOccurrence: characteristicOccurrence,
      operationIdentifier: operationIdentifier
    ) { value, provenance, error in
      guard error != nil || provenance == .readResponse else {
        completion(nil, self.error(code: 1031, message: "The read value may be a notification; this protocol cannot report that"))
        return
      }
      completion(value, error)
    }
  }

  @objc public func readRssi(peerIdentifier: String, operationIdentifier: String,
    completion: @escaping (NSNumber?, NSError?) -> Void) {
    readRssiOwned(owner: nil, peerIdentifier: peerIdentifier, operationIdentifier: operationIdentifier, completion: completion)
  }

  func readRssi(borrower: OwnedCoreBluetoothProtocolRadioDelegate, peerIdentifier: String, operationIdentifier: String,
    completion: @escaping (NSNumber?, NSError?) -> Void) {
    readRssiOwned(owner: borrower, peerIdentifier: peerIdentifier, operationIdentifier: operationIdentifier, completion: completion)
  }

  private func readRssiOwned(owner: AnyObject?, peerIdentifier: String, operationIdentifier: String,
    completion: @escaping (NSNumber?, NSError?) -> Void) {
    queue.async {
      guard self.requireUsable({ error in completion(nil, error) }) else { return }
      if let owner, !self.hasActiveGattClaim(owner: owner, peer: peerIdentifier) {
        completion(nil, self.error(code: 1007, message: "This borrower does not own an active connection claim")); return
      }
      guard let peripheral = self.peripheralByIdentifier[peerIdentifier], peripheral.state == .connected else {
        completion(nil, self.error(code: 1022, message: "The requested peripheral is not connected")); return
      }
      let physical: String
      if let owner {
        guard let id = self.registerScopedGatt(owner: owner, peer: peerIdentifier, operation: operationIdentifier) else {
          completion(nil, self.error(code: 1014, message: "Operation identifier is already owned")); return
        }
        physical = id
      } else { physical = operationIdentifier }
      guard self.pendingRssi.admit(peerIdentifier, peer: peerIdentifier, peripheral: peripheral,
        object: peripheral, operation: physical, completion: { value in
          if let owner {
            let current = self.scopedGattClaimIsCurrent(owner: owner, peer: peerIdentifier, operation: operationIdentifier, physical: physical)
            self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical)
            if !current && value.1 == nil {
              completion(nil, self.error(code: 1020, message: "Connection claim changed before RSSI callback")); return
            }
          }
          completion(value.0, value.1)
        }) else {
        if let owner { self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical) }
        completion(nil, self.error(code: 1023, message: self.pendingRssi.hasCapacity
          ? "An RSSI callback is still outstanding for this peripheral" : "RSSI callback capacity exhausted")); return
      }
      peripheral.delegate = self
      peripheral.readRSSI()
    }
  }

  @objc public func write(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    value: NSData,
    withResponse: Bool,
    operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  ) {
    writeOwned(owner: nil, peerIdentifier: peerIdentifier, serviceUUID: serviceUUID, serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID, characteristicOccurrence: characteristicOccurrence, value: value,
      withResponse: withResponse, operationIdentifier: operationIdentifier, completion: completion)
  }

  func write(borrower: OwnedCoreBluetoothProtocolRadioDelegate, peerIdentifier: String, serviceUUID: String,
    serviceOccurrence: Int, characteristicUUID: String, characteristicOccurrence: Int, value: NSData, withResponse: Bool,
    operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    writeOwned(owner: borrower, peerIdentifier: peerIdentifier, serviceUUID: serviceUUID, serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID, characteristicOccurrence: characteristicOccurrence, value: value,
      withResponse: withResponse, operationIdentifier: operationIdentifier, completion: completion)
  }

  private func writeOwned(owner: AnyObject?, peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int,
    characteristicUUID: String, characteristicOccurrence: Int, value: NSData, withResponse: Bool,
    operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    queue.async {
      guard self.requireUsable(completion) else { return }
      if let owner, !self.hasActiveGattClaim(owner: owner, peer: peerIdentifier) {
        completion(self.error(code: 1007, message: "This borrower does not own an active connection claim")); return
      }
      guard value.length <= Self.maximumBinaryPayloadBytes else {
        completion(self.error(code: 1012, message: "The native binary payload exceeds the protocol limit"))
        return
      }
      let address = CharacteristicAddress(
        peerIdentifier: peerIdentifier,
        serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(serviceUUID),
        serviceOccurrence: serviceOccurrence,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(characteristicUUID),
        characteristicOccurrence: characteristicOccurrence
      )
      guard let resolved = self.resolve(address) else {
        completion(self.error(code: 1013, message: "The generation-bound characteristic path is stale"))
        return
      }
      let writeType: CBCharacteristicWriteType = withResponse ? .withResponse : .withoutResponse
      if owner != nil && !withResponse {
        let maximum = resolved.peripheral.maximumWriteValueLength(for: .withoutResponse)
        guard value.length <= maximum else {
          completion(self.error(code: 1012, message: "\(value.length) bytes exceed CoreBluetooth's write-without-response limit of \(maximum)")); return
        }
        guard resolved.peripheral.canSendWriteWithoutResponse else {
          completion(self.error(code: 1014, message: "The write without response cannot be queued")); return
        }
      }
      if withResponse {
        let physical: String
        if let owner {
          guard let id = self.registerScopedGatt(owner: owner, peer: peerIdentifier, operation: operationIdentifier) else {
            completion(self.error(code: 1014, message: "Operation identifier is already owned")); return
          }
          physical = id
        } else { physical = operationIdentifier }
        guard self.pendingWrite.admit(address, peer: peerIdentifier, peripheral: resolved.peripheral,
          object: resolved.characteristic, operation: physical, completion: { failure in
            if let owner {
              let current = self.scopedGattClaimIsCurrent(owner: owner, peer: peerIdentifier, operation: operationIdentifier, physical: physical)
              self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical)
              if !current && failure == nil {
                completion(self.error(code: 1020, message: "Connection claim changed before write callback")); return
              }
            }
            completion(failure)
          }) else {
          if let owner { self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical) }
          completion(self.error(code: 1014, message: self.pendingWrite.hasCapacity
            ? "A write callback is still outstanding for this characteristic" : "Write callback capacity exhausted"))
          return
        }
      }
      resolved.peripheral.writeValue(value as Data, for: resolved.characteristic, type: writeType)
      if !withResponse {
        completion(nil)
      }
    }
  }

  /// Runs wholly on the CoreBluetooth delegate queue. Admission is performed
  /// only after write validation, immediately before the platform write call.
  func writeWithOrderedMarker(
    address: CharacteristicAddress, value: Data, withResponse: Bool,
    operationIdentifier: String, admit: () throws -> UInt64,
    completion: @escaping (NSError?) -> Void
  ) throws -> UInt64 {
    dispatchPrecondition(condition: .onQueue(queue))
    var validationError: NSError?
    guard requireUsable({ validationError = $0 }) else {
      throw validationError ?? error(code: 1013, message: "The radio is unavailable")
    }
    guard value.count <= Self.maximumBinaryPayloadBytes else {
      throw error(code: 1012, message: "The native binary payload exceeds the protocol limit")
    }
    guard let resolved = resolve(address) else {
      throw error(code: 1013, message: "The generation-bound characteristic path is stale")
    }
    if withResponse {
      guard pendingWrite.canAdmit(address, peripheral: resolved.peripheral, object: resolved.characteristic) else {
        throw error(code: 1014, message: pendingWrite.hasCapacity
          ? "A write callback is still outstanding for this characteristic" : "Write callback capacity exhausted")
      }
    } else {
      guard value.count <= resolved.peripheral.maximumWriteValueLength(for: .withoutResponse),
            resolved.peripheral.canSendWriteWithoutResponse else {
        throw error(code: 1014, message: "The write without response cannot be queued")
      }
    }
    return try OwnedOrderedWriteDispatch.start(queue: queue, admit: admit, write: { finish in
      if withResponse {
        guard pendingWrite.admit(address, peer: address.peerIdentifier, peripheral: resolved.peripheral,
          object: resolved.characteristic, operation: operationIdentifier, settleOnCancel: true,
          completion: finish) else { preconditionFailure("validated write admission changed on the radio queue") }
      }
      resolved.peripheral.writeValue(value, for: resolved.characteristic, type: withResponse ? .withResponse : .withoutResponse)
      if !withResponse { finish(nil) }
    }, completion: completion)
  }

  @objc public func subscribe(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    subscriptionIdentifier: String,
    operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  ) {
    setNotify(
      peerIdentifier: peerIdentifier,
      serviceUUID: serviceUUID,
      serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID,
      characteristicOccurrence: characteristicOccurrence,
      enabled: true,
      subscriptionIdentifier: subscriptionIdentifier,
      operationIdentifier: operationIdentifier,
      completion: completion
    )
  }

  @objc public func unsubscribe(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    subscriptionIdentifier: String,
    operationIdentifier: String,
    completion: @escaping (NSError?) -> Void
  ) {
    setNotify(
      peerIdentifier: peerIdentifier,
      serviceUUID: serviceUUID,
      serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID,
      characteristicOccurrence: characteristicOccurrence,
      enabled: false,
      subscriptionIdentifier: subscriptionIdentifier,
      operationIdentifier: operationIdentifier,
      completion: completion
    )
  }

  @objc public func destroy(completion: @escaping (NSError?) -> Void) {
    queue.async {
      self.drainScanCallbacks()
      guard self.pendingScanLosses.isEmpty else {
        completion(self.error(code: 1014, message: "Scan loss delivery is pending; retry destroy"))
        return
      }
      self.performOrderedGlobalBarrier(peerOperation: { self.resetOrderedPeer($0, cause: .shutdown) }, onFailure: {
        completion(self.error(code: 1014, message: "Ordered lifecycle control capacity exhausted"))
      }) {
      guard !self.destroyed else {
        completion(nil)
        return
      }
      // The barrier may have waited behind an unrelated held graph permit.
      self.drainScanCallbacks()
      guard self.pendingScanLosses.isEmpty else {
        completion(self.error(code: 1014, message: "Scan loss delivery is pending; retry destroy"))
        return
      }
      self.destroyed = true
      self.orderedSnapshotIdentities.reset()
      self.clearAllScanLeases()
      self.prompter.abandon()
      self.failAllPendingOperationsOnDestroy()
      self.invalidateAllOrderedSources(.shutdown)
      self.disableActiveNotifications()
      self.subscriptions.removeAll()
      self.sharedSubscriptions.removeAll()
      self.completeAfterPendingDisconnects(
        preserving: [],
        destroyRadio: true,
        completion: completion
      )
      }
    }
  }

  public func centralManagerDidUpdateState(_ central: CBCentralManager) {
    let state = central.state
    let snapshot = OwnedCoreBluetoothProtocolRadioSupport.adapterSnapshotDictionary(central: central)
    routeCentralState(state, snapshot: snapshot)
  }

  func routeCentralState(_ state: CBManagerState, snapshot: NSDictionary) {
    let claimRoutes = connectionClaimRoutes()
    let apply = { self.applyCentralState(state, snapshot: snapshot, capturedClaims: claimRoutes) }
    let event = PendingOverflowCentralState(apply: apply)
    // Only ingress advances the pending state. An older accepted barrier may
    // finish after a newer event has already overflowed.
    if pendingOverflowCentralState != nil { pendingOverflowCentralState = event }
    let complete = {
      if self.pendingOverflowCentralState != nil { self.flushOverflowCentralStateIfReady() }
      else { apply() }
    }
    let fail = {
      if self.pendingOverflowCentralState == nil { self.pendingOverflowCentralState = event }
    }
    switch state {
    case .unknown, .resetting, .unsupported, .unauthorized:
      performOrderedGlobalBarrier(peerOperation: { peer in
        self.resetOrderedPeer(peer, cause: .adapterLost)
        let failure = self.error(code: 1016, message: "CoreBluetooth adapter invalidated the previous connection")
        self.pendingConnect.removeValue(forKey: peer)?.completion(failure)
        self.pendingDisconnect.removeValue(forKey: peer)?.completion(failure)
        self.clearTerminalCallbackDebts(for: peer)
        self.clearCancellationCleanup(forPeerIdentifier: peer)
        self.recoverAfterAdapterInvalidation(peer)
      }, onFailure: fail, operation: complete)
    case .poweredOff:
      performOrderedGlobalBarrier(peerOperation: { self.resetOrderedPeer($0, cause: .adapterLost) }, onFailure: fail, operation: complete)
    case .poweredOn:
      performOrderedGlobalBarrier(invalidating: false, onFailure: fail, operation: complete)
    @unknown default:
      performOrderedGlobalBarrier(invalidating: false, onFailure: fail, operation: complete)
    }
  }

  private func applyCentralState(_ state: CBManagerState, snapshot: NSDictionary, capturedClaims: [ObjectIdentifier: OwnedCoreBluetoothProtocolRadioDelegate]) {
    scanAdapterStateChanged(state)
    // Capture routes before calling out: a receiver may synchronously change
    // its scan claim. The legacy delegate can also own a scan, but must see
    // each state transition only once.
    var recipients = capturedClaims
    recipients.merge(connectionClaimRoutes()) { _, current in current }
    if let delegate = delegate { recipients[ObjectIdentifier(delegate)] = delegate }
    for lease in scanLeases.values {
      if let route = lease.route { recipients[ObjectIdentifier(route)] = route }
    }
    for route in recipients.values { route.protocolRadioDidUpdateAdapterState(snapshot) }
    prompter.authorizationChanged()
    if let failure = OwnedCoreBluetoothProtocolRadioSupport.operationReadinessFailure(state: state) {
      radioPreparation.failAll(failure)
    } else {
      radioPreparation.update(snapshot)
    }
  }

  #if os(iOS)
  public func centralManager(_ central: CBCentralManager, willRestoreState dictionary: [String: Any]) {
    let peripherals = (dictionary[CBCentralManagerRestoredStatePeripheralsKey] as? [CBPeripheral]) ?? []
    for peripheral in peripherals {
      let identifier = peripheral.identifier.uuidString
      peripheralByIdentifier[identifier] = peripheral
      if peripheral.state != .disconnected {
        unfencedConnections.insert(identifier)
        if connectionEpochs[identifier] == nil { connectionEpochs[identifier] = UUID() }
      }
      if !restoredPeerIdentifiers.contains(identifier) {
        restoredPeerIdentifiers.append(identifier)
      }
      peripheral.delegate = self
    }
    delegate?.protocolRadioDidRestorePeers?(OwnedCoreBluetoothProtocolRadioSupport.restoredPeerSnapshots(
      identifiers: restoredPeerIdentifiers, peripherals: peripheralByIdentifier
    ))
  }
  #endif

  public func centralManager(
    _ central: CBCentralManager,
    didDiscover peripheral: CBPeripheral,
    advertisementData: [String: Any],
    rssi RSSI: NSNumber
  ) {
    guard physicalScanActive, !scanLeases.isEmpty else { return }
    let snapshot = OwnedCoreBluetoothProtocolRadioSupport.advertisementDictionary(peripheral: peripheral,
      advertisementData: advertisementData, rssi: RSSI)
    // CoreBluetooth does not identify which scan filter matched and its
    // advertisement dictionary need not contain every service it matched.
    // Capture claim generations before deferral; duplicate suppression only
    // starts after visible evidence matches the individual claim.
    var claims: [(ObjectIdentifier, UUID)] = []
    for (key, lease) in scanLeases {
      if let loss = pendingScanLosses[key] {
        // A previous generation is paused until its retained loss is paid;
        // this physical observation belongs only to the other live claims.
        if loss.generation == lease.generation { recordScanLoss(key, generation: lease.generation) }
      }
      else { claims.append((key, lease.generation)) }
    }
    guard !claims.isEmpty else { scheduleScanDrain(); return }
    let bytes = scanCallbackSize(snapshot, claimCount: claims.count)
    guard pendingScanCallbacks.count < Self.scanCallbackLimit,
          bytes <= Self.scanCallbackByteLimit - pendingScanCallbackBytes else {
      for (key, generation) in claims { recordScanLoss(key, generation: generation) }
      scheduleScanDrain()
      return
    }
    pendingScanCallbacks.append(PendingScanCallback(peripheral: peripheral, snapshot: snapshot, claims: claims, bytes: bytes))
    pendingScanCallbackBytes += bytes
    scheduleScanDrain()
  }

  /// Conservative accounting for retained Foundation container and payload bytes.
  private func scanCallbackSize(_ snapshot: NSDictionary, claimCount: Int) -> Int {
    let limit = Self.scanCallbackByteLimit
    func add(_ a: Int, _ b: Int) -> Int { b > limit - min(a, limit) ? limit + 1 : a + b }
    func size(_ value: Any) -> Int {
      if let data = value as? Data { return add(64, data.count) }
      if let string = value as? String { return add(64, string.utf8.count) }
      if let dictionary = value as? NSDictionary {
        return dictionary.reduce(64) { add($0, add(size($1.key), size($1.value))) }
      }
      if let array = value as? NSArray { return array.reduce(64) { add($0, size($1)) } }
      return 64
    }
    guard claimCount <= limit / 64 else { return limit + 1 }
    return add(size(snapshot), add(128, claimCount * 64))
  }

  private func scheduleScanDrain() {
    guard !scanDrainScheduled else { return }
    scanDrainScheduled = true
    queue.async { self.drainScanCallbacks() }
  }

  func drainScanCallbacks() {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !scanDraining else { return }
    let pausedBefore = Set<ObjectIdentifier>(scanLeases.compactMap { key, lease in
      guard let loss = pendingScanLosses[key], loss.generation != lease.generation else { return nil }
      return key
    })
    scanDraining = true
    defer { scanDraining = false }
    scanDrainScheduled = false
    let callbacks = pendingScanCallbacks
    pendingScanCallbacks.removeAll(keepingCapacity: true)
    pendingScanCallbackBytes = 0
    for callback in callbacks {
      deliverAdvertisement(callback.peripheral, snapshot: callback.snapshot, claims: callback.claims)
    }
    for key in Array(pendingScanLosses.keys) {
      guard let loss = pendingScanLosses[key],
            let receiver = loss.route as? OwnedScanLossReceiver,
            receiver.receiveScanLoss(OwnedScanLoss(generation: loss.generation, exactCount: loss.exactCount)),
            var current = pendingScanLosses[key], current.generation == loss.generation else { continue }
      if current.revision == loss.revision {
        pendingScanLosses.removeValue(forKey: key)
      } else {
        // A synchronous receiver may reenter and reject more observations
        // before acknowledging this snapshot. Retain only the new debt.
        if let total = current.exactCount, let delivered = loss.exactCount, total >= delivered {
          current.exactCount = total - delivered
        } else {
          current.exactCount = nil
        }
        pendingScanLosses[key] = current
      }
    }
    let pausedAfter = Set<ObjectIdentifier>(scanLeases.compactMap { key, lease in
      guard let loss = pendingScanLosses[key], loss.generation != lease.generation else { return nil }
      return key
    })
    if pausedBefore != pausedAfter || (!physicalScanActive && !scanLeases.isEmpty) { reconcileScan() }
    if !pendingScanLosses.isEmpty && !scanRetryScheduled {
      scanRetryScheduled = true
      queue.asyncAfter(deadline: .now() + .milliseconds(250)) {
        self.scanRetryScheduled = false
        self.drainScanCallbacks()
      }
    }
  }

  private func recordScanLoss(_ key: ObjectIdentifier, generation: UUID, capturedLease: ScanLease? = nil) {
    guard let lease = capturedLease ?? scanLeases[key], lease.generation == generation, let route = lease.route else { return }
    if var loss = pendingScanLosses[key] {
      guard loss.generation == generation else { return }
      if lease.allowDuplicates, let count = loss.exactCount, count < UInt64.max { loss.exactCount = count + 1 }
      else { loss.exactCount = nil }
      loss.revision = UUID()
      pendingScanLosses[key] = loss
    } else {
      pendingScanLosses[key] = PendingScanLoss(route: route, generation: generation,
        exactCount: lease.allowDuplicates ? 1 : nil, revision: UUID())
    }
  }

  private func deliverAdvertisement(_ peripheral: CBPeripheral, snapshot: NSDictionary,
                                    claims: [(ObjectIdentifier, UUID)]) {
    if unfencedConnections.contains(peripheral.identifier.uuidString),
       let current = peripheralByIdentifier[peripheral.identifier.uuidString], current !== peripheral { return }
    let peer = peripheral.identifier.uuidString
    let advertised = Set((snapshot["serviceUUIDs"] as? [String]) ?? [])
      .union((snapshot["overflowServiceUUIDs"] as? [String]) ?? [])
      .union(((snapshot["serviceData"] as? [String: NSData]) ?? [:]).keys)
    for (key, generation) in claims {
      guard let lease = scanLeases[key], lease.generation == generation, let route = lease.route else { continue }
      if !lease.allowDuplicates && lease.deliveredPeers.contains(peer) { continue }
      let matched = lease.services.isEmpty || !lease.services.isDisjoint(with: advertised)
      // Buffered values precede the overflow control; they are still eligible
      // for delivery. Rust ingress refusal is separately counted as loss.
      peripheralByIdentifier[peer] = peripheral
      peripheral.delegate = self
      let accepted: Bool
      if let receiver = route as? OwnedScanAdvertisementReceiver {
        scanDeliveringOwners.insert(key)
        accepted = receiver.receiveScanAdvertisement(snapshot, generation: generation)
        if !accepted { recordScanLoss(key, generation: generation, capturedLease: lease) }
        scanDeliveringOwners.remove(key)
      } else {
        route.protocolRadioDidReceiveAdvertisement(snapshot)
        accepted = true
      }
      if accepted && !lease.allowDuplicates && matched,
         var current = scanLeases[key], current.generation == generation {
        current.deliveredPeers.insert(peer)
        scanLeases[key] = current
      }
    }
  }

  public func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
    let identifier = peripheral.identifier.uuidString
    guard peripheralByIdentifier[identifier] === peripheral else { return }
    if let gate = orderedGraphPermitGates[identifier], gate.isHolding {
      let epoch = connectionEpochs[identifier]
      gate.invalidateAndEnqueue {
        guard self.connectionEpochs[identifier] == epoch else { return }
        self.centralManager(central, didConnect: peripheral)
      }
      return
    }
    if orderedOverflowDisconnectPending.contains(identifier) {
      central.cancelPeripheralConnection(peripheral)
      return
    }
    orderedConfirmedDisconnected.remove(identifier)
    unfencedConnections.insert(identifier)
    guard orderedSnapshotIdentities.attach(identifier) else {
      pendingConnect.removeValue(forKey: identifier)?.completion(error(code: 1009, message: "Native attachment identity exhausted"))
      central.cancelPeripheralConnection(peripheral)
      return
    }
    peripheralByIdentifier[identifier] = peripheral
    peripheral.delegate = self
    invalidateOrderedSource(for: identifier, cause: .sourceReplaced)
    orderedSources[identifier] = OwnedOrderedNotificationSource(queue: queue, generation: UUID().uuidString)
    if let pending = pendingConnect.removeValue(forKey: identifier) {
      pending.completion(nil)
    }
    if pendingDisconnect[identifier] != nil {
      central.cancelPeripheralConnection(peripheral)
    }
    if pendingCancellationCleanup.values.contains(where: { $0.peerIdentifiers.contains(identifier) }) {
      central.cancelPeripheralConnection(peripheral)
    }
  }

  public func centralManager(_ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?) {
    let identifier = peripheral.identifier.uuidString
    guard peripheralByIdentifier[identifier] === peripheral else { return }
    if let gate = orderedGraphPermitGates[identifier], gate.isHolding {
      let failure = error as NSError?
      let epoch = connectionEpochs[identifier]
      let terminal = UUID()
      pendingConnectionTerminals[identifier] = terminal
      if !gate.enqueue({
        guard self.pendingConnectionTerminals[identifier] == terminal else { return }
        self.pendingConnectionTerminals.removeValue(forKey: identifier)
        guard self.connectionEpochs[identifier] == epoch else { return }
        self.centralManager(central, didFailToConnect: peripheral, error: failure)
      }) {
        pendingConnectionTerminals.removeValue(forKey: identifier)
      }
      return
    }
    unfencedConnections.remove(identifier)
    connectionEpochs.removeValue(forKey: identifier)
    clearCancellationCleanup(forPeerIdentifier: identifier)
    let failure = error as NSError? ?? self.error(code: 1015, message: "CoreBluetooth failed to connect")
    if let pending = pendingConnect.removeValue(forKey: identifier) {
      pending.completion(failure)
    }
    disconnectOwners.removeValue(forKey: identifier)
    if let pending = pendingDisconnect.removeValue(forKey: identifier) {
      pending.completion(nil)
    }
  }

  public func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
    let identifier = peripheral.identifier.uuidString
    guard let current = peripheralByIdentifier[identifier], current === peripheral else { return }
    if let gate = orderedGraphPermitGates[identifier], gate.isHolding {
      let failure = error as NSError?
      let epoch = connectionEpochs[identifier]
      let terminal = UUID()
      pendingConnectionTerminals[identifier] = terminal
      if !gate.invalidateAndEnqueue({
        guard self.pendingConnectionTerminals[identifier] == terminal else { return }
        self.pendingConnectionTerminals.removeValue(forKey: identifier)
        guard self.connectionEpochs[identifier] == epoch else { return }
        self.centralManager(central, didDisconnectPeripheral: peripheral, error: failure)
      }) {
        pendingConnectionTerminals.removeValue(forKey: identifier)
        orderedLostDisconnects[identifier] = LostDisconnect(error: failure)
      }
      return
    }
    completeObservedDisconnect(identifier, error: error as NSError?)
  }

  func completeObservedDisconnect(_ identifier: String, error: NSError?) {
    unfencedConnections.remove(identifier)
    connectionEpochs.removeValue(forKey: identifier)
    pendingConnectionTerminals.removeValue(forKey: identifier)
    orderedOverflowDisconnectPending.remove(identifier)
    if orderedGraphPermitGates[identifier] != nil { orderedConfirmedDisconnected.insert(identifier) }
    orderedLostDisconnects.removeValue(forKey: identifier)
    orderedSnapshotIdentities.disconnect(identifier)
    invalidateOrderedSource(for: identifier, cause: .connectionLost)
    clearCancellationCleanup(forPeerIdentifier: identifier)
    let explicitDisconnect = pendingDisconnect.removeValue(forKey: identifier)
    disconnectOwners.removeValue(forKey: identifier)
    let claims = connectionClaims.removeValue(forKey: identifier) ?? [:]
    servicesByPeer.removeValue(forKey: identifier)
    if let pending = pendingConnect.removeValue(forKey: identifier) {
      pending.completion(error ?? self.error(code: 1015, message: "CoreBluetooth disconnected while connecting"))
    }
    if let pending = pendingDiscovery.removeValue(forKey: identifier), !pending.cancelled && !pending.completionDelivered {
      pending.completion(nil, error ?? self.error(code: 1016, message: "CoreBluetooth disconnected during discovery"))
    }
    failPendingGATT(for: identifier, error: error)
    clearTerminalCallbackDebts(for: identifier)
    if let explicitDisconnect {
      explicitDisconnect.completion(nil)
      return
    }
    if claims.isEmpty {
      delegate?.protocolRadioDidDisconnectPeer(identifier, error: error)
    } else {
      for claim in claims.values {
        if let pending = claim.pending {
          pending.completion(error ?? self.error(code: 1015, message: "CoreBluetooth disconnected while connecting"))
        } else {
          claim.route?.protocolRadioDidDisconnectPeer(identifier, error: error)
        }
      }
    }
  }

  public func peripheral(_ peripheral: CBPeripheral, didModifyServices invalidatedServices: [CBService]) {
    let identifier = peripheral.identifier.uuidString
    guard peripheralByIdentifier[identifier] === peripheral else { return }
    if let gate = orderedGraphPermitGates[identifier], gate.isHolding {
      let services = Array(invalidatedServices)
      gate.invalidateAndEnqueue { self.peripheral(peripheral, didModifyServices: services) }
      return
    }
    orderedSnapshotIdentities.invalidate(identifier)
    invalidateOrderedSource(for: identifier, cause: .serviceChanged)
    let invalidated = Set(invalidatedServices.map(ObjectIdentifier.init))
    var addresses = Set(subscriptions.keys.filter { $0.peerIdentifier == identifier })
    addresses.formUnion(pendingNotify.keys.filter { $0.peerIdentifier == identifier })
    for cleanup in pendingCancellationCleanup.values {
      addresses.formUnion(cleanup.notificationDesiredStates.keys.filter { $0.peerIdentifier == identifier })
    }
    var retiredThisChange = [RetiredNotificationOwner<CBPeripheral, CBCharacteristic>]()
    for address in addresses {
      guard let resolved = resolve(address), let service = resolved.characteristic.service else { continue }
      let invalid = invalidated.contains(ObjectIdentifier(service))
      let awaiting = pendingNotify[address] != nil || pendingCancellationCleanup.values.contains {
        $0.notificationAwaitingCallbacks.contains(address)
      }
      var owners = sharedSubscriptions[address] ?? []
      if let primary = subscriptions[address] { owners.insert(primary) }
      if let pending = pendingNotify[address] { owners.insert(pending.subscriptionIdentifier) }
      let owner = RetiredNotificationOwner(generation: notificationGenerations[address] ?? UUID().uuidString, address: address,
        peripheral: resolved.peripheral, characteristic: resolved.characteristic,
        subscriptions: owners,
        phase: invalid ? .invalidated : (awaiting ? .awaitingOriginal : .stalled))
      retiredNotificationOwners[ObjectIdentifier(resolved.characteristic)] = owner
      retiredThisChange.append(owner)
    }
    clearNotificationOwnership(forPeerIdentifier: identifier)
    for owner in retiredThisChange {
      let object = ObjectIdentifier(owner.characteristic)
      transferNotificationCleanup(address: owner.address, toRetiredObject: object)
      invalidatedNotificationCleanup.remove(owner.address)
      if case .stalled = owner.phase { retryRetiredNotification(owner) }
    }
    servicesByPeer.removeValue(forKey: identifier)
    invalidateDiscovery(identifier, services: invalidatedServices)
    failPendingGATT(for: identifier, error: self.error(code: 1027, message: "CoreBluetooth services changed"))
    var recipients = connectionClaimRoutes(peer: identifier)
    if let delegate = delegate { recipients[ObjectIdentifier(delegate)] = delegate }
    for route in recipients.values { route.protocolRadioDidModifyServices(identifier) }
  }

  public func peripheral(_ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic, error: Error?) {
    if consumeRetiredReadCallback(peripheral: peripheral, characteristic: characteristic) { return }
    guard peripheralByIdentifier[peripheral.identifier.uuidString] === peripheral else { return }
    guard notificationObjectIsAvailable(characteristic) else { return }
    guard let address = address(for: characteristic, peerIdentifier: peripheral.identifier.uuidString) else { return }
    if let gate = orderedGraphPermitGates[address.peerIdentifier], gate.isHolding {
      let value = characteristic.value.map { Data($0) }
      let notifying = characteristic.isNotifying
      let failure = error as NSError?
      deliverCharacteristicSnapshot(peer: address.peerIdentifier, byteCount: value?.count ?? 0) {
        self.handleCharacteristicValueUpdate(address: address, peripheral: peripheral, characteristic: characteristic, error: failure,
          snapshotValue: value, snapshotIsNotifying: notifying, hasSnapshot: true)
      }
      return
    }
    handleCharacteristicValueUpdate(address: address, peripheral: peripheral, characteristic: characteristic, error: error)
  }

  func notificationObjectIsAvailable(_ object: AnyObject) -> Bool {
    Self.notificationObjectIsAvailable(object, retired: Set(retiredNotificationOwners.keys))
  }

  func readCallbackObjectIsAvailable(peripheral: AnyObject, characteristic: AnyObject) -> Bool {
    !retiredReads.contains { $0.matches(peripheral: peripheral, object: characteristic) }
  }

  static func notificationObjectIsAvailable(_ object: AnyObject, retired: Set<ObjectIdentifier>) -> Bool {
    !retired.contains(ObjectIdentifier(object))
  }

  public func peripheral(_ peripheral: CBPeripheral, didReadRSSI RSSI: NSNumber, error: Error?) {
    let peerIdentifier = peripheral.identifier.uuidString
    if let gate = orderedGraphPermitGates[peerIdentifier], gate.isHolding {
      let value = NSNumber(value: RSSI.int64Value), failure = error as NSError?
      gate.enqueue { self.peripheral(peripheral, didReadRSSI: value, error: failure) }
      return
    }
    if let operation = pendingRssi.complete(peripheral: peripheral, object: peripheral, value: (RSSI, error as NSError?)) {
      refreshCallbackCancellationCleanup(operation)
    }
  }

  public func peripheral(_ peripheral: CBPeripheral, didWriteValueFor characteristic: CBCharacteristic, error: Error?) {
    if let gate = orderedGraphPermitGates[peripheral.identifier.uuidString], gate.isHolding {
      let failure = error as NSError?
      gate.enqueue { self.peripheral(peripheral, didWriteValueFor: characteristic, error: failure) }
      return
    }
    if let operation = pendingWrite.complete(peripheral: peripheral, object: characteristic, value: error as NSError?) {
      refreshCallbackCancellationCleanup(operation)
    }
  }

  public func peripheralIsReady(toSendWriteWithoutResponse peripheral: CBPeripheral) {
    let peer = peripheral.identifier.uuidString
    guard peripheralByIdentifier[peer] === peripheral else { return }
    let claims = connectionClaims[peer] ?? [:]
    let legacy = claims.isEmpty ? delegate : nil
    let epoch = connectionEpochs[peer]
    let deliver = {
      guard self.peripheralByIdentifier[peer] === peripheral, self.connectionEpochs[peer] == epoch else { return }
      if let legacy, self.connectionClaims[peer]?.isEmpty != false, self.delegate === legacy {
        legacy.protocolRadioDidBecomeReadyToSendWriteWithoutResponse?(peer)
      }
      for (key, captured) in claims {
        guard let current = self.connectionClaims[peer]?[key], current.identity == captured.identity else { continue }
        current.route?.protocolRadioDidBecomeReadyToSendWriteWithoutResponse?(peer)
      }
    }
    if let gate = orderedGraphPermitGates[peer], gate.isHolding {
      gate.enqueue(deliver)
      return
    }
    deliver()
  }

  public func peripheral(_ peripheral: CBPeripheral, didUpdateNotificationStateFor characteristic: CBCharacteristic, error: Error?) {
    let notifying = characteristic.isNotifying
    if let gate = orderedGraphPermitGates[peripheral.identifier.uuidString], gate.isDeferringNotificationIngress {
      let failure = error as NSError?
      let object = ObjectIdentifier(characteristic)
      let retired = retiredNotificationOwners[object]
      let address = address(for: characteristic, peerIdentifier: peripheral.identifier.uuidString)
      let ticket = address.flatMap { notificationTransitionTickets[$0] }
      let pending = address.flatMap { pendingNotify[$0] }
      let unsolicited = address.map { pending == nil && cancellationDesiredState(forNotificationAddress: $0) == nil } ?? false
      var identifiers = retired?.subscriptions ?? []
      if let pending { identifiers.insert(pending.subscriptionIdentifier) }
      if let address {
        identifiers.formUnion(pendingNotifyJoins[address]?.keys.map { $0 } ?? [])
        identifiers.formUnion(sharedSubscriptions[address] ?? [])
        if let established = subscriptions[address] { identifiers.insert(established) }
      }
      let successfulDisable = address.map { address in
        guard failure == nil, !notifying, let pending, !pending.enabled,
              self.notificationClaimIsCurrent(pending, address: address) else { return false }
        return self.cancellationDesiredState(forNotificationAddress: address) != true &&
          self.subscriptions[address] == pending.subscriptionIdentifier &&
          (self.sharedSubscriptions[address] ?? []).isEmpty
      } ?? false
      let observedFailure = failure ?? (!successfulDisable && !notifying && address.map {
        subscriptions[$0] != nil || !(sharedSubscriptions[$0] ?? []).isEmpty
      } == true ? self.error(code: 1039, message: "CoreBluetooth disabled an owned notification subscription") : nil)
      var retainedFailures: [(Bool) -> Void] = []
      var retainedReceivers: [(NSError) -> Void] = []
      var retentionRefused = false
      if let failure = observedFailure, let address, let source = orderedSources[address.peerIdentifier] {
        let nativeFailure = OwnedOrderedNotificationIngress.NativeFailure(error: failure)
        if unsolicited && (subscriptions[address] != nil || !(sharedSubscriptions[address] ?? []).isEmpty) {
          if let retained = source.captureDeferredFailure(route: orderedRoute(for: address), failure: nativeFailure) {
            retainedFailures.append(retained)
          }
        } else {
          var seen = Set<OrderedNotificationClaim>()
          for identifier in identifiers {
            guard let route = notificationRoutes[identifier] else { continue }
            if identifier == pending?.subscriptionIdentifier, let expected = pending?.routeIdentity,
               route.identity != expected { continue }
            guard let claim = route.orderedClaim, claim.generation == source.generation,
                  seen.insert(claim).inserted else { continue }
            if let retained = source.captureDeferredFailure(claim: claim, failure: nativeFailure) { retainedFailures.append(retained) }
          }
        }
      }
      if observedFailure != nil {
        for identifier in identifiers {
          guard let route = notificationRoutes[identifier] else { continue }
          if identifier == pending?.subscriptionIdentifier, let expected = pending?.routeIdentity,
             route.identity != expected { continue }
          if let receiver = route.borrower as? OwnedDeferredNotificationFailureReceiver {
            if let retained = receiver.captureNotificationFailure(identifier) { retainedReceivers.append(retained) }
            else { retentionRefused = true }
          } else if let borrower = route.borrower {
            // Retain the observed delegate itself. Looking the identifier up
            // again after replay could deliver the old error to a new owner.
            retainedReceivers.append { error in
              borrower.protocolRadioDidFailNotification?(identifier, error: error)
            }
          }
        }
      }
      var settled = false
      let settle: (Bool) -> Void = { report in
        guard !settled else { return }
        settled = true
        for retained in retainedFailures { retained(report) }
        if report, let failure = observedFailure { for receiver in retainedReceivers { receiver(failure) } }
        if report, retainedFailures.isEmpty, retainedReceivers.isEmpty, let failure = observedFailure {
          NSLog("[UnifiedBle] deferred notification callback lost its owner: %@ (%ld)", failure.domain, failure.code)
        }
      }
      if retentionRefused {
        if let failure = observedFailure {
          NSLog("[UnifiedBle] notification failure retention exhausted; failing peer closed: %@ (%ld)", failure.domain, failure.code)
        }
        settle(true)
        gate.breakForOverflow()
        return
      }
      // `enqueue` may invoke the overflow hook before it invokes `reject`.
      // Keep the terminal state with the callback so that the latter cannot
      // recreate an observation which the hook has just delivered.
      var replayStarted = false
      let replay = {
        guard !replayStarted else { return }
        replayStarted = true
        if let retired {
          guard self.retiredNotificationOwners[object] === retired, retired.peripheral === peripheral else {
            settle(true)
            return
          }
          settle(false)
          self.handleRetiredNotificationCallback(retired, isNotifying: notifying, error: failure)
          return
        }
        guard let address, self.notificationTransitionTickets[address] == ticket,
              self.address(for: characteristic, peerIdentifier: peripheral.identifier.uuidString) == address else {
          settle(true)
          return
        }
        defer { settle(false) }
        self.handleNotificationCallback(peripheral, characteristic: characteristic, isNotifying: notifying, error: failure,
          capturedUnsolicitedFailure: unsolicited && observedFailure != nil ? { settle(true) } : nil)
      }
      let peer = peripheral.identifier.uuidString
      // Retain before asking the bounded graph FIFO. Its overflow hook may
      // run synchronously on refusal, so the exact observation must already
      // be reachable when that hook drains the accepted prefix.
      guard let retainedID = retainNotificationReplay(peer: peer, gate: gate, replay: replay) else {
        // This cannot be a normal, ordered delivery any more.  Break the
        // graph only after its prefix; the retained source/receiver stays
        // reachable through the graph's failure path.
        gate.breakForOverflow()
        return
      }
      let accepted = gate.enqueue(reject: {
        // A later graph reset can reject an already queued request after its
        // provisional retained entry was removed.  Put the exact callback
        // back on the durable post-prefix route unless it has already run.
        guard !replayStarted else { return }
        guard self.retainNotificationReplay(peer: peer, gate: gate, replay: replay) != nil else {
          gate.breakForOverflow()
          return
        }
      }, replay)
      if accepted { removeRetainedNotificationReplay(peer: peer, id: retainedID) }
      if !accepted { gate.breakForOverflow() }
      return
    }
    handleNotificationCallback(peripheral, characteristic: characteristic, isNotifying: notifying, error: error)
  }

  private func handleNotificationCallback(_ peripheral: CBPeripheral, characteristic: CBCharacteristic, isNotifying: Bool, error: Error?,
    capturedUnsolicitedFailure: (() -> Void)? = nil) {
    if let owner = retiredNotificationOwners[ObjectIdentifier(characteristic)] {
      guard owner.peripheral === peripheral else { return }
      handleRetiredNotificationCallback(owner, isNotifying: isNotifying, error: error)
      return
    }
    guard peripheralByIdentifier[peripheral.identifier.uuidString] === peripheral else { return }
    guard let address = address(for: characteristic, peerIdentifier: peripheral.identifier.uuidString) else { return }
    if let expected = notificationTransitionObjects[address], expected != ObjectIdentifier(characteristic) { return }
    notificationTransitionObjects.removeValue(forKey: address)
    handleNotificationStateUpdate(address: address, isNotifying: isNotifying, error: error,
      capturedUnsolicitedFailure: capturedUnsolicitedFailure)
  }

  func retryRetiredNotification(_ owner: RetiredNotificationOwner<CBPeripheral, CBCharacteristic>) {
    guard case .stalled = owner.phase else { return }
    owner.phase = .awaitingDisable
    owner.peripheral.setNotifyValue(false, for: owner.characteristic)
  }

  func handleRetiredNotificationCallback(_ owner: RetiredNotificationOwner<CBPeripheral, CBCharacteristic>,
    isNotifying: Bool, error: Error?) {
    switch owner.callback(isNotifying: isNotifying, failed: error != nil) {
    case .none: return
    case .disable:
      retryRetiredNotification(owner)
    case .released:
      let object = ObjectIdentifier(owner.characteristic)
      retiredNotificationOwners.removeValue(forKey: object)
      clearRetiredNotificationCleanup(object)
    case .retry:
      let object = ObjectIdentifier(owner.characteristic)
      for (operation, cleanup) in pendingCancellationCleanup where cleanup.retiredNotificationObjects.contains(object) {
        _ = retryCancellationCleanupOnQueue(operation)
      }
    }
  }

  /// The delegate's operation-owned answer, separate from CB object lookup.
  func handleNotificationStateUpdate(address: CharacteristicAddress, isNotifying: Bool, error: Error?,
    capturedUnsolicitedFailure: (() -> Void)? = nil) {
    // A callback resolved through a replacement database cannot settle the old owner.
    guard !invalidatedNotificationCleanup.contains(address) else { return }
    let pending = pendingNotify.removeValue(forKey: address)
    let desiredCancellationState = cancellationDesiredState(forNotificationAddress: address)
    if pending == nil && desiredCancellationState == nil {
      if let failure = error as NSError? {
        if let capturedUnsolicitedFailure { capturedUnsolicitedFailure() }
        else { reportNotificationFailure(address: address, error: failure) }
      } else if !isNotifying && subscriptions[address] != nil {
        if let capturedUnsolicitedFailure { capturedUnsolicitedFailure() }
        else { reportNotificationFailure(address: address, error: self.error(code: 1039, message: "CoreBluetooth disabled an owned notification subscription")) }
      }
      return
    }
    if let pending, !pending.enabled, !notificationClaimIsCurrent(pending, address: address) {
      // The callback is still owed to the old physical transition. It cannot
      // retire ownership subsequently installed under another claim.
      if !isNotifying && subscriptions[address] != nil {
        // The off transition is nevertheless a real gap for every surviving
        // physical subscriber, even though the stale stop owns none of them.
        reportNotificationFailure(address: address, error: self.error(code: 1039,
          message: "Notifications were disabled by an earlier connection claim"))
        var cleanup = pendingCancellationCleanup[pending.cleanupIdentifier] ?? PendingCancellationCleanup()
        cleanup.notificationDesiredStates[address] = true
        if writeNotificationState(true, for: address) { cleanup.notificationAwaitingCallbacks.insert(address) }
        pendingCancellationCleanup[pending.cleanupIdentifier] = cleanup
      }
      pending.completion(self.error(code: 1020, message: "Connection claim changed before notification callback"))
      return
    }
    if !isNotifying && subscriptions[address] != nil &&
        (desiredCancellationState == true || (pending?.enabled == false && error != nil)) {
      // Logical ownership survived, but the physical CCCD went off. The
      // number of missed values is unknowable; end ordered consumers honestly.
      let gap = self.error(code: 1039, message: "Notifications were disabled while an owned subscription was being restored")
      reportNotificationFailure(address: address, error: gap)
    }

    if let pending, pending.enabled, !notificationClaimIsCurrent(pending, address: address) {
      // This callback still owns its physical debt, but cannot grant a new
      // connection generation a subscription it did not request.
      let failure = self.error(code: 1020, message: "Connection claim changed before notification callback")
      if error == nil && isNotifying { settleQueuedNotificationJoins(address: address, isNotifying: true) }
      let joins = pendingNotifyJoins.removeValue(forKey: address) ?? [:]
      if subscriptions[address] == nil {
        var cleanup = pendingCancellationCleanup[pending.cleanupIdentifier] ?? PendingCancellationCleanup()
        cleanup.notificationDesiredStates[address] = false
        if writeNotificationState(false, for: address) { cleanup.notificationAwaitingCallbacks.insert(address) }
        pendingCancellationCleanup[pending.cleanupIdentifier] = cleanup
        _ = retryCancellationCleanupOnQueue(pending.cleanupIdentifier)
      }
      pending.completion(failure)
      for join in joins.values { join.completion(failure) }
      return
    }
    if let pending {
      if pending.enabled {
        if error == nil && isNotifying {
          if subscriptions[address] == nil { subscriptions[address] = pending.subscriptionIdentifier }
          else { sharedSubscriptions[address, default: []].insert(pending.subscriptionIdentifier) }
        }
      } else if error == nil && !isNotifying {
        subscriptions.removeValue(forKey: address)
        notificationGenerations.removeValue(forKey: address)
        if pendingNotifyJoins[address]?.isEmpty != false { notificationTransitionTickets.removeValue(forKey: address) }
      }
      let answer = error as NSError? ?? (isNotifying == pending.enabled ? nil : self.error(
        code: 1039,
        message: "CoreBluetooth's notification state callback did not reach the requested state"
      ))
      if pending.enabled && answer != nil && subscriptions[address] == nil {
        // A failed enable does not prove that CoreBluetooth left no CCCD
        // registration behind. Retain cleanup until a disable is confirmed.
        var cleanup = pendingCancellationCleanup[pending.cleanupIdentifier] ?? PendingCancellationCleanup()
        cleanup.notificationDesiredStates[address] = false
        if writeNotificationState(false, for: address) {
          cleanup.notificationAwaitingCallbacks.insert(address)
        }
        pendingCancellationCleanup[pending.cleanupIdentifier] = cleanup
        _ = retryCancellationCleanupOnQueue(pending.cleanupIdentifier)
      }
      pending.completion(answer)
      if pending.enabled {
        let joins = pendingNotifyJoins.removeValue(forKey: address) ?? [:]
        for join in joins.values {
          if !notificationClaimIsCurrent(join, address: address) {
            join.completion(self.error(code: 1020, message: "Connection claim changed before notification join"))
          } else {
            if answer == nil { sharedSubscriptions[address, default: []].insert(join.subscriptionIdentifier) }
            join.completion(answer)
          }
        }
      } else if desiredCancellationState == nil {
        if answer != nil && !isNotifying {
          // A failed stop preserves the old logical owner. Restore its physical
          // registration and retain the operation until that is confirmed.
          var cleanup = pendingCancellationCleanup[pending.cleanupIdentifier] ?? PendingCancellationCleanup()
          cleanup.notificationDesiredStates[address] = true
          pendingCancellationCleanup[pending.cleanupIdentifier] = cleanup
        }
        settleQueuedNotificationJoins(address: address, isNotifying: isNotifying)
        if answer != nil && !isNotifying {
          if pendingNotify[address] != nil {
            pendingCancellationCleanup[pending.cleanupIdentifier]?.notificationAwaitingCallbacks.insert(address)
          }
          _ = retryCancellationCleanupOnQueue(pending.cleanupIdentifier)
        }
      }
    }

    guard let desired = desiredCancellationState else { return }
    markCancellationNotificationCallbackReceived(for: address)
    if error == nil && isNotifying == desired {
      clearCancellationCleanup(forNotificationAddress: address)
      if desired { settleQueuedNotificationJoins(address: address, isNotifying: true) }
      else if subscriptions[address] == nil && pendingNotifyJoins[address]?.isEmpty != false {
        notificationTransitionTickets.removeValue(forKey: address)
      }
      return
    }

    // The cancelled CoreBluetooth operation reached a different physical state.
    // Keep the cleanup owner reachable and drive it back to its required state.
    resolvedNotifyStateNeedsReconciliation(address, desiredState: desired)
  }

  func setNotify(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    enabled: Bool,
    subscriptionIdentifier: String,
    operationIdentifier: String,
    expectedRelease: (owner: AnyObject, claim: UUID, route: NotificationRoute?)? = nil,
    completion: @escaping (NSError?) -> Void
  ) {
    queue.async {
      guard self.requireUsable(completion) else { return }
      if let expected = expectedRelease {
        guard self.connectionClaims[peerIdentifier]?[ObjectIdentifier(expected.owner)]?.identity == expected.claim,
              self.notificationRoutes[subscriptionIdentifier] === expected.route,
              expected.route != nil || self.delegate === expected.owner else {
          completion(self.error(code: 1020, message: "Notification ownership changed during connection cleanup")); return
        }
      }
      let address = CharacteristicAddress(
        peerIdentifier: peerIdentifier,
        serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(serviceUUID),
        serviceOccurrence: serviceOccurrence,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(characteristicUUID),
        characteristicOccurrence: characteristicOccurrence
      )
      if enabled, let route = self.notificationRoutes[subscriptionIdentifier] {
        guard let borrower = route.borrower,
              self.hasActiveGattClaim(owner: borrower, peer: peerIdentifier),
              let claim = self.connectionClaims[peerIdentifier]?[ObjectIdentifier(borrower)] else {
          completion(self.error(code: 1007, message: "This borrower does not own an active connection claim")); return
        }
        guard route.claimIdentity == nil || route.claimIdentity == claim.identity else {
          completion(self.error(code: 1020, message: "Notification route belongs to an earlier connection claim")); return
        }
        route.claimIdentity = claim.identity
      }
      if !enabled, let route = self.notificationRoutes[subscriptionIdentifier], let identity = route.claimIdentity {
        guard let borrower = route.borrower,
              self.connectionClaims[peerIdentifier]?[ObjectIdentifier(borrower)]?.identity == identity else {
          completion(self.error(code: 1020, message: "Notification route belongs to an earlier connection claim")); return
        }
      }
      if enabled, let borrower = self.notificationRoute(subscriptionIdentifier),
         self.releasingConnectionClaims[peerIdentifier]?.contains(ObjectIdentifier(borrower)) == true {
        completion(self.error(code: 1018, message: "Connection claim cleanup is pending")); return
      }
      if !enabled, let owner = self.retiredNotificationOwners.values.first(where: {
        $0.address == address && $0.subscriptions.contains(subscriptionIdentifier)
      }) {
        self.retryRetiredNotification(owner)
        completion(self.error(code: 1039, message: "The retired notification registration is still being cleaned up"))
        return
      }
      guard let resolved = self.resolve(address) else {
        completion(self.error(code: 1017, message: "The generation-bound characteristic path is stale"))
        return
      }
      guard self.retiredNotificationOwners[ObjectIdentifier(resolved.characteristic)] == nil else {
        completion(self.error(code: 1018, message: "The previous registration for this characteristic is still being cleaned up"))
        return
      }
      guard !self.retiredNotificationOwners.values.contains(where: {
        $0.address.peerIdentifier == address.peerIdentifier && $0.address.serviceUUID == address.serviceUUID
      }) else {
        completion(self.error(code: 1018, message: "A previous notification registration for this service is still being cleaned up"))
        return
      }
      guard !enabled || self.readCallbackObjectIsAvailable(peripheral: resolved.peripheral,
        characteristic: resolved.characteristic) else {
        completion(self.error(code: 1018, message: "The previous characteristic read callback is still outstanding"))
        return
      }
      if enabled && self.subscriptions[address] == nil && self.pendingNotify[address] == nil {
        let owned = Set(self.subscriptions.keys).union(self.pendingNotify.keys)
        guard owned.count + self.retiredNotificationOwners.count < 256 else {
          completion(self.error(code: 1018, message: "Notification callback ownership capacity exhausted"))
          return
        }
      }
      if enabled && self.queueNotificationJoin(address: address, subscriptionIdentifier: subscriptionIdentifier,
        operationIdentifier: operationIdentifier, completion: completion) { return }
      guard self.notificationTransitionAvailable(for: address) else {
        completion(self.error(code: 1018, message: "A notification state change is already pending for this characteristic"))
        return
      }
      if self.updateActiveSharedSubscription(address: address, enabled: enabled,
        subscriptionIdentifier: subscriptionIdentifier, completion: completion) { return }
      if !enabled && self.subscriptions[address] != subscriptionIdentifier {
        completion(self.error(code: 1019, message: "The subscription does not own this characteristic"))
        return
      }
      self.pendingNotify[address] = PendingNotify(
        operationIdentifier: operationIdentifier,
        subscriptionIdentifier: subscriptionIdentifier,
        enabled: enabled,
        completion: completion,
        borrower: self.notificationRoutes[subscriptionIdentifier]?.borrower,
        claimIdentity: self.notificationRoutes[subscriptionIdentifier]?.borrower.flatMap {
          self.connectionClaims[peerIdentifier]?[ObjectIdentifier($0)]?.identity
        },
        routeIdentity: self.notificationRoutes[subscriptionIdentifier]?.identity
      )
      if enabled && self.notificationGenerations[address] == nil {
        self.notificationGenerations[address] = self.orderedSources[peerIdentifier]?.generation ?? UUID().uuidString
      }
      self.notificationTransitionObjects[address] = ObjectIdentifier(resolved.characteristic)
      self.notificationTransitionTickets[address] = UUID()
      if enabled { self.readObjects[address]?.unattributable = true }
      resolved.peripheral.setNotifyValue(enabled, for: resolved.characteristic)
    }
  }

  @discardableResult
  func queueNotificationJoin(address: CharacteristicAddress, subscriptionIdentifier: String,
    operationIdentifier: String, completion: @escaping (NSError?) -> Void) -> Bool {
    let pending = pendingNotify[address]
    guard !hasInvalidatedNotificationService(for: address) else { return false }
    guard pending != nil || cancellationDesiredState(forNotificationAddress: address) == true else { return false }
    guard pending?.subscriptionIdentifier != subscriptionIdentifier,
          subscriptions[address] != subscriptionIdentifier,
          sharedSubscriptions[address]?.contains(subscriptionIdentifier) != true,
          pendingNotifyJoins[address]?[subscriptionIdentifier] == nil else {
      completion(error(code: 1018, message: "The subscription already owns this characteristic"))
      return true
    }
    pendingNotifyJoins[address, default: [:]][subscriptionIdentifier] = PendingNotify(
      operationIdentifier: operationIdentifier, subscriptionIdentifier: subscriptionIdentifier,
      enabled: true, completion: completion, borrower: notificationRoutes[subscriptionIdentifier]?.borrower,
      claimIdentity: notificationRoutes[subscriptionIdentifier]?.borrower.flatMap {
        connectionClaims[address.peerIdentifier]?[ObjectIdentifier($0)]?.identity
      }, routeIdentity: notificationRoutes[subscriptionIdentifier]?.identity)
    return true
  }

  func notificationClaimIsCurrent(_ pending: PendingNotify, address: CharacteristicAddress) -> Bool {
    guard let borrower = pending.borrower else { return true }
    guard let identity = pending.claimIdentity,
          let route = notificationRoutes[pending.subscriptionIdentifier], route.borrower === borrower,
          let claim = connectionClaims[address.peerIdentifier]?[ObjectIdentifier(borrower)] else { return false }
    if let routeIdentity = pending.routeIdentity, route.identity != routeIdentity { return false }
    return claim.owner === borrower && claim.pending == nil && claim.identity == identity
  }

  /// A disable's callback must settle before a queued claimant can enable.
  func settleQueuedNotificationJoins(address: CharacteristicAddress, isNotifying: Bool) {
    guard var joins = pendingNotifyJoins[address], !joins.isEmpty else { return }
    if isNotifying {
      pendingNotifyJoins.removeValue(forKey: address)
      for join in joins.values {
        guard notificationClaimIsCurrent(join, address: address) else {
          join.completion(error(code: 1020, message: "Connection claim changed before notification join")); continue
        }
        if subscriptions[address] == nil { subscriptions[address] = join.subscriptionIdentifier }
        else { sharedSubscriptions[address, default: []].insert(join.subscriptionIdentifier) }
        join.completion(nil)
      }
      return
    }
    for (key, join) in joins where !notificationClaimIsCurrent(join, address: address) {
      joins.removeValue(forKey: key)
      join.completion(error(code: 1020, message: "Connection claim changed before notification join"))
    }
    pendingNotifyJoins[address] = joins.isEmpty ? nil : joins
    guard let key = joins.keys.sorted().first, let next = joins.removeValue(forKey: key) else { return }
    pendingNotifyJoins[address] = joins.isEmpty ? nil : joins
    pendingNotify[address] = next
    if notificationGenerations[address] == nil {
      notificationGenerations[address] = orderedSources[address.peerIdentifier]?.generation ?? UUID().uuidString
    }
    guard writeNotificationState(true, for: address) else {
      pendingNotify.removeValue(forKey: address)
      let failure = error(code: 1017, message: "The generation-bound characteristic path is stale")
      next.completion(failure)
      for join in joins.values { join.completion(failure) }
      pendingNotifyJoins.removeValue(forKey: address)
      return
    }
  }

  /// Handles only transitions that leave the physical registration enabled.
  @discardableResult
  func updateActiveSharedSubscription(address: CharacteristicAddress, enabled: Bool,
    subscriptionIdentifier: String, completion: (NSError?) -> Void) -> Bool {
    if enabled, let primary = subscriptions[address] {
      guard primary != subscriptionIdentifier,
            sharedSubscriptions[address]?.contains(subscriptionIdentifier) != true else {
        completion(error(code: 1018, message: "The subscription already owns this characteristic"))
        return true
      }
      sharedSubscriptions[address, default: []].insert(subscriptionIdentifier)
      completion(nil)
      return true
    }
    if !enabled, var others = sharedSubscriptions[address], !others.isEmpty {
      if others.remove(subscriptionIdentifier) != nil {
        sharedSubscriptions[address] = others.isEmpty ? nil : others
        completion(nil)
        return true
      }
      if subscriptions[address] == subscriptionIdentifier, let replacement = others.sorted().first {
        others.remove(replacement)
        subscriptions[address] = replacement
        sharedSubscriptions[address] = others.isEmpty ? nil : others
        completion(nil)
        return true
      }
    }
    return false
  }

  func notificationTransitionAvailable(for address: CharacteristicAddress) -> Bool {
    // An occurrence may shift among services with the same UUID after
    // rediscovery. Other service UUIDs cannot alias that old characteristic.
    pendingNotify[address] == nil && !hasPendingCancellationCleanup(for: address) &&
      !hasInvalidatedNotificationService(for: address)
  }

  func hasInvalidatedNotificationService(for address: CharacteristicAddress) -> Bool {
    invalidatedNotificationCleanup.contains {
      $0.peerIdentifier == address.peerIdentifier && $0.serviceUUID == address.serviceUUID
    }
  }

  @discardableResult
  func writeNotificationState(_ enabled: Bool, for address: CharacteristicAddress) -> Bool {
    if let writer = notificationStateWriter {
      let previous = notificationTransitionTickets[address]
      notificationTransitionTickets[address] = UUID()
      if writer(address, enabled) { return true }
      notificationTransitionTickets[address] = previous
      return false
    }
    guard let resolved = resolve(address) else { return false }
    notificationTransitionObjects[address] = ObjectIdentifier(resolved.characteristic)
    notificationTransitionTickets[address] = UUID()
    if enabled { readObjects[address]?.unattributable = true }
    resolved.peripheral.setNotifyValue(enabled, for: resolved.characteristic)
    return true
  }

  func resolve(_ address: CharacteristicAddress) -> (peripheral: CBPeripheral, characteristic: CBCharacteristic)? {
    guard let peripheral = peripheralByIdentifier[address.peerIdentifier], peripheral.state == .connected,
          let services = servicesByPeer[address.peerIdentifier] else { return nil }
    let matchingServices = services.filter {
      OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID($0.uuid.uuidString) == address.serviceUUID
    }
    guard address.serviceOccurrence >= 0, address.serviceOccurrence < matchingServices.count else { return nil }
    let service = matchingServices[address.serviceOccurrence]
    let matchingCharacteristics = (service.characteristics ?? []).filter {
      OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID($0.uuid.uuidString) == address.characteristicUUID
    }
    guard address.characteristicOccurrence >= 0, address.characteristicOccurrence < matchingCharacteristics.count else { return nil }
    return (peripheral, matchingCharacteristics[address.characteristicOccurrence])
  }

  func address(for characteristic: CBCharacteristic, peerIdentifier: String) -> CharacteristicAddress? {
    guard let service = characteristic.service,
          let services = servicesByPeer[peerIdentifier] else { return nil }
    let normalizedServiceUUID = OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(service.uuid.uuidString)
    let matchingServices = services.filter {
      OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID($0.uuid.uuidString) == normalizedServiceUUID
    }
    guard let serviceOccurrence = matchingServices.firstIndex(where: { $0 === service }) else { return nil }
    let matchingCharacteristics = (service.characteristics ?? []).filter {
      OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID($0.uuid.uuidString) ==
        OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(characteristic.uuid.uuidString)
    }
    guard let characteristicOccurrence = matchingCharacteristics.firstIndex(where: { $0 === characteristic }) else { return nil }
    return CharacteristicAddress(
      peerIdentifier: peerIdentifier,
      serviceUUID: normalizedServiceUUID,
      serviceOccurrence: serviceOccurrence,
      characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(characteristic.uuid.uuidString),
      characteristicOccurrence: characteristicOccurrence
    )
  }
  func clearNotificationOwnership(forPeerIdentifier peerIdentifier: String) {
    // An in-flight platform operation may still change the CCCD after its
    // caller is failed. Keep a cleanup owner through the database fence.
    for (address, pending) in pendingNotify where address.peerIdentifier == peerIdentifier {
      pendingNotify.removeValue(forKey: address)
      var cleanup = pendingCancellationCleanup[pending.cleanupIdentifier] ?? PendingCancellationCleanup()
      cleanup.notificationDesiredStates[address] = false
      cleanup.notificationAwaitingCallbacks.insert(address)
      pendingCancellationCleanup[pending.cleanupIdentifier] = cleanup
      pending.completion(error(code: 1027, message: "CoreBluetooth services changed"))
      for join in pendingNotifyJoins.removeValue(forKey: address)?.values ?? [:].values {
        join.completion(error(code: 1027, message: "CoreBluetooth services changed"))
      }
    }
    for (address, joins) in pendingNotifyJoins where address.peerIdentifier == peerIdentifier {
      pendingNotifyJoins.removeValue(forKey: address)
      for join in joins.values { join.completion(error(code: 1027, message: "CoreBluetooth services changed")) }
    }
    var notificationAddresses = Set(subscriptions.keys.filter { $0.peerIdentifier == peerIdentifier })
    notificationAddresses.formUnion(pendingNotify.keys.filter { $0.peerIdentifier == peerIdentifier })
    for cleanup in pendingCancellationCleanup.values {
      notificationAddresses.formUnion(cleanup.notificationDesiredStates.keys.filter { $0.peerIdentifier == peerIdentifier })
    }
    for address in notificationAddresses {
      if hasPendingCancellationCleanup(for: address) {
        // The old characteristic identity is no longer resolvable after this
        // fence. Retain ownership until disconnect; a rediscovered path may
        // denote a different characteristic and must not receive compensation.
        invalidatedNotificationCleanup.insert(address)
      } else if subscriptions[address] != nil {
        // The service may already be invalid. Retain ownership without an
        // unsafe write until disconnect proves the old CCCD is gone.
        let cleanupIdentifier = "service-change-\(UUID().uuidString)"
        var cleanup = PendingCancellationCleanup()
        cleanup.notificationDesiredStates[address] = false
        pendingCancellationCleanup[cleanupIdentifier] = cleanup
        invalidatedNotificationCleanup.insert(address)
      }
    }
    subscriptions = subscriptions.filter { $0.key.peerIdentifier != peerIdentifier }
    sharedSubscriptions = sharedSubscriptions.filter { $0.key.peerIdentifier != peerIdentifier }
    notificationGenerations = notificationGenerations.filter { $0.key.peerIdentifier != peerIdentifier }
  }

  func failPendingGATT(for peerIdentifier: String, error: NSError?) {
    let failure = error ?? self.error(code: 1020, message: "CoreBluetooth disconnected")
    for address in pendingRead.keys where address.peerIdentifier == peerIdentifier {
      guard let lane = pendingRead.removeValue(forKey: address) else { continue }
      if var owner = readObjects.removeValue(forKey: address), lane.updateOwed {
        owner.peerIdentifier = peerIdentifier
        retiredReads.append(owner)
      }
      for pending in lane.waiting {
        pending.completion(nil, .readResponse, failure)
      }
    }
    pendingRssi.retire(peer: peerIdentifier, failure: (nil, failure))
    pendingWrite.retire(peer: peerIdentifier, failure: failure)
    descriptorOperations.fail(peerIdentifier, error: failure)
    for (address, pending) in pendingNotify where address.peerIdentifier == peerIdentifier {
      pendingNotify.removeValue(forKey: address)
      pending.completion(failure)
      for join in pendingNotifyJoins.removeValue(forKey: address)?.values ?? [:].values { join.completion(failure) }
    }
    for (address, joins) in pendingNotifyJoins where address.peerIdentifier == peerIdentifier {
      pendingNotifyJoins.removeValue(forKey: address)
      for join in joins.values { join.completion(failure) }
    }
    subscriptions = subscriptions.filter { $0.key.peerIdentifier != peerIdentifier }
    sharedSubscriptions = sharedSubscriptions.filter { $0.key.peerIdentifier != peerIdentifier }
    notificationTransitionObjects = notificationTransitionObjects.filter { $0.key.peerIdentifier != peerIdentifier }
    notificationTransitionTickets = notificationTransitionTickets.filter { $0.key.peerIdentifier != peerIdentifier }
    notificationGenerations = notificationGenerations.filter { $0.key.peerIdentifier != peerIdentifier }
  }

  /// A disconnect delegate callback or adapter invalidation proves that the
  /// retired physical operations can no longer produce peripheral callbacks.
  func clearTerminalCallbackDebts(for peerIdentifier: String) {
    unfencedConnections.remove(peerIdentifier)
    retiredReads.removeAll { $0.peerIdentifier == peerIdentifier }
    pendingRssi.clearRetired(peer: peerIdentifier)
    pendingWrite.clearRetired(peer: peerIdentifier)
    descriptorOperations.pendingReads.clearRetired(peer: peerIdentifier)
    descriptorOperations.pendingWrites.clearRetired(peer: peerIdentifier)
    for operation in Array(pendingCancellationCleanup.keys) {
      refreshCallbackCancellationCleanup(operation)
    }
  }

  private func failAllPendingOperationsOnDestroy() {
    let failure = error(code: 1021, message: "The CoreBluetooth protocol radio was destroyed")
    radioPreparation.failAll(failure)
    let connects = pendingConnect
    let disconnects = pendingDisconnect
    let discoveries = pendingDiscovery
    let reads = pendingRead
    let notifications = pendingNotify
    let notifyJoins = pendingNotifyJoins

    pendingConnect.removeAll()
    pendingDisconnect.removeAll()
    disconnectOwners.removeAll()
    pendingDiscovery.removeAll()
    pendingRead.removeAll()
    for (address, lane) in reads {
      if var owner = readObjects.removeValue(forKey: address), lane.updateOwed {
        owner.peerIdentifier = address.peerIdentifier
        retiredReads.append(owner)
      }
    }
    pendingRssi.retireAll(failure: (nil, failure))
    pendingWrite.retireAll(failure: failure)
    descriptorOperations.failAll(failure)
    pendingNotify.removeAll()
    pendingNotifyJoins.removeAll()

    for pending in connects.values {
      pending.completion(failure)
    }
    for pending in disconnects.values {
      pending.completion(failure)
    }
    for pending in discoveries.values where !pending.cancelled && !pending.completionDelivered {
      pending.completion(nil, failure)
    }
    for lane in reads.values {
      for pending in lane.waiting {
        pending.completion(nil, .readResponse, failure)
      }
    }
    for pending in notifications.values {
      pending.completion(failure)
    }
    for joins in notifyJoins.values {
      for pending in joins.values { pending.completion(failure) }
    }
  }

  func requireUsable(_ completion: (NSError?) -> Void) -> Bool {
    guard !destroyed else {
      completion(error(code: 1021, message: "The Native Protocol v2 CoreBluetooth radio was destroyed"))
      return false
    }
    return true
  }

  func error(code: Int, message: String) -> NSError {
    NSError(
      domain: "com.sfourdrinier.unifiedblemanager.corebluetooth",
      code: code,
      userInfo: [NSLocalizedDescriptionKey: message]
    )
  }

}
