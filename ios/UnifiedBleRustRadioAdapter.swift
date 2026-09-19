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
import Foundation

/// The OS radio seam the adapter drives. `OwnedCoreBluetoothProtocolRadio`
/// is the production driver; the Apple harness scripts one.
protocol UnifiedBleRustRadioDriver: AnyObject {
  /// Serial queue every driver callback and every adapter mutation runs on.
  var workQueue: DispatchQueue { get }
  func adapterSnapshot(completion: @escaping (NSDictionary) -> Void)
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

/// Reads the Rust-core platform adapter (`UnifiedBleRustRadioAdapter`) needs
/// beyond the Native Protocol v2 surface. Every read runs on the radio queue
/// and answers asynchronously, so a caller on a Rust executor thread never
/// blocks on CoreBluetooth.
extension OwnedCoreBluetoothProtocolRadio: UnifiedBleRustRadioDriver {
  /// The serial queue every CoreBluetooth object is confined to.
  var workQueue: DispatchQueue { queue }

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

extension MobileCoreHost: UnifiedBleRustRadioSink {}

/// Answer bookkeeping the host does not see: a completion Rust refused as
/// mismatched is an adapter defect, and a fact ingested after shutdown is
/// lost by construction. Both are counted here and logged, never dropped
/// silently.
struct UnifiedBleRustRadioAdapterCounters: Equatable {
  var mismatchedCompletions: UInt64 = 0
  var ingressAfterClose: UInt64 = 0
  var cancelledRequests: UInt64 = 0
}

final class UnifiedBleRustRadioAdapter: NSObject, MobilePlatformRadio, OwnedCoreBluetoothProtocolRadioDelegate,
  @unchecked Sendable
{
  private struct Subscription {
    let identifier: String
    let instance: MobileInstance
    let epoch: UInt64
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

  private let driver: UnifiedBleRustRadioDriver
  private let queue: DispatchQueue
  // Everything below is confined to `queue`.
  private var sink: UnifiedBleRustRadioSink?
  private var inFlight = [UInt64: String]()
  private var subscriptionsById = [String: Subscription]()
  private var subscriptionByInstance = [InstanceKey: String]()
  private var announcedRestoredPeers = Set<String>()
  private var pendingRestoredPeers = [MobileRestoredPeer]()
  private var scanActive = false
  private var counters = UnifiedBleRustRadioAdapterCounters()

  init(driver: UnifiedBleRustRadioDriver) {
    self.driver = driver
    queue = driver.workQueue
    super.init()
  }

  /// Connects the installed host. Restoration facts CoreBluetooth delivered
  /// before the host existed are ingested now, exactly once per peer.
  func bind(sink: UnifiedBleRustRadioSink) {
    queue.async {
      self.sink = sink
      self.driver.restoredPeerSnapshots { snapshots in
        self.announceRestored(snapshots.compactMap(Self.restoredPeer))
      }
    }
  }

  func adapterCounters(completion: @escaping (UnifiedBleRustRadioAdapterCounters) -> Void) {
    queue.async { completion(self.counters) }
  }

  // MARK: - MobilePlatformRadio

  func submit(request: MobileRadioRequest) {
    queue.async { self.start(request) }
  }

  /// Rust dropped its wait (budget expiry, `op.cancel`, abort). The OS work is
  /// withdrawn and the request is answered `cancelled` so every request keeps
  /// exactly one answer; Rust counts it as a late completion.
  func cancel(requestId: UInt64) {
    queue.async {
      guard let operationIdentifier = self.inFlight.removeValue(forKey: requestId) else { return }
      self.counters.cancelledRequests += 1
      self.driver.cancelOperation(operationIdentifier) { _ in }
      self.deliver(requestId, Self.failed("cancelled", "cancelled by the Rust owner", dispatched: true))
    }
  }

  // MARK: - Requests

  private func start(_ request: MobileRadioRequest) {
    let id = Self.requestId(of: request)
    let operationIdentifier = "ubm-rust-\(id)"
    inFlight[id] = operationIdentifier
    switch request {
    case .adapterState:
      driver.adapterSnapshot { snapshot in self.finish(id, .adapter(snapshot: Self.adapterSnapshot(snapshot))) }
    case let .startScan(_, serviceUuids, deviceAddresses, scanMode, callbackType, legacy):
      guard deviceAddresses.isEmpty, scanMode == nil, callbackType == nil, legacy == nil else {
        return finish(id, Self.unsupported("CoreBluetooth has no address filter or Android scan settings"))
      }
      whenReady(id) {
        self.driver.startScan(serviceUUIDs: serviceUuids, allowDuplicates: true, operationIdentifier: operationIdentifier) { error in
          if error == nil { self.scanActive = true }
          self.finishUnit(id, error, verb: .startScan)
        }
      }
    case .stopScan:
      driver.stopScan(operationIdentifier: operationIdentifier) { error in
        // No scan running is the state a stop asks for: CoreBluetooth
        // already stopped it (adapter off, reported as `ScanFailed`).
        let noScanActive = error.map { $0.domain == Self.ownedDomain && $0.code == 1004 } ?? false
        if error == nil || noScanActive { self.scanActive = false }
        self.finishUnit(id, noScanActive ? nil : error, verb: .stopScan)
      }
    case let .connect(_, peerId, autoConnect, preferredPhy):
      guard !autoConnect else {
        return finish(id, Self.unsupported("CoreBluetooth has no when-available (autoConnect) connection intent"))
      }
      // Rust refuses a PHY preference on Apple before any effect; should one
      // ever arrive, it is refused here too, never connected without.
      guard preferredPhy.isEmpty else {
        return finish(id, Self.unsupported("CoreBluetooth has no LE PHY control"))
      }
      whenReady(id) {
        self.driver.connect(peerIdentifier: peerId, operationIdentifier: operationIdentifier) { error in
          self.finishUnit(id, error, verb: .connect)
        }
      }
    case let .disconnect(_, peerId):
      driver.disconnect(peerIdentifier: peerId, operationIdentifier: operationIdentifier) { error in
        self.finishUnit(id, error, verb: .disconnect)
      }
    case let .discover(_, peerId):
      driver.discover(peerIdentifier: peerId, operationIdentifier: operationIdentifier) { snapshot, error in
        if let error { return self.finish(id, Self.failure(error, verb: .discover)) }
        guard let services = snapshot.flatMap(Self.services) else {
          return self.finish(id, Self.platformFailure("CoreBluetooth returned an unreadable discovery snapshot"))
        }
        self.finish(id, .discovered(services: services))
      }
    case let .read(_, instance):
      guard let path = Self.path(instance) else { return finish(id, Self.stalePath) }
      driver.readCharacteristic(
        peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
        characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
        operationIdentifier: operationIdentifier
      ) { value, provenance, error in
        if let error { return self.finish(id, Self.failure(error, verb: .read)) }
        guard let value else { return self.finish(id, Self.platformFailure("CoreBluetooth answered the read without a value")) }
        self.finish(id, .read(value: value as Data, provenance: provenance.wire))
      }
    case let .write(_, instance, value, withResponse):
      write(id, operationIdentifier, instance, value, withResponse)
    case let .readDescriptor(_, instance, descriptorUuid, descriptorOccurrence):
      guard let path = Self.path(instance), let descriptor = Int(exactly: descriptorOccurrence) else {
        return finish(id, Self.stalePath)
      }
      driver.readDescriptor(
        peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
        characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
        descriptorUUID: descriptorUuid, descriptorOccurrence: descriptor, operationIdentifier: operationIdentifier
      ) { value, error in
        self.finishBytes(id, value, error, verb: .readDescriptor)
      }
    case let .writeDescriptor(_, instance, descriptorUuid, descriptorOccurrence, value):
      guard let path = Self.path(instance), let descriptor = Int(exactly: descriptorOccurrence) else {
        return finish(id, Self.stalePath)
      }
      driver.writeDescriptor(
        peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
        characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
        descriptorUUID: descriptorUuid, descriptorOccurrence: descriptor, value: value as NSData,
        operationIdentifier: operationIdentifier
      ) { error in
        self.finishUnit(id, error, verb: .writeDescriptor)
      }
    case let .enableNotifications(_, instance, epoch, requested, _):
      enable(id, operationIdentifier, instance, epoch, requested)
    case let .disableNotifications(_, instance):
      disable(instance, operationIdentifier: operationIdentifier) { failure in
        self.finish(id, failure ?? .unit)
      }
    case let .readMtu(_, peerId):
      // Apple has no MTU readout: the ATT MTU CoreBluetooth negotiated is
      // `maximumWriteValueLength(.withResponse) + 3` (frozen wire rule).
      driver.writeLimits(peerIdentifier: peerId) { limits, error in
        if let error { return self.finish(id, Self.failure(error, verb: .readMtu)) }
        guard let withResponse = (limits?["withResponse"] as? NSNumber)?.intValue,
              let mtu = UInt16(exactly: withResponse + 3) else {
          return self.finish(id, Self.platformFailure("CoreBluetooth reported no write limit"))
        }
        self.finish(id, .mtu(mtu: mtu))
      }
    case let .readWriteLimits(_, peerId):
      // CoreBluetooth's own per-type answer: with response it performs the
      // long write itself; a command must fit one ATT payload.
      driver.writeLimits(peerIdentifier: peerId) { limits, error in
        if let error { return self.finish(id, Self.failure(error, verb: .readWriteLimits)) }
        guard let withResponse = (limits?["withResponse"] as? NSNumber)?.intValue,
              let withoutResponse = (limits?["withoutResponse"] as? NSNumber)?.intValue,
              let withResponseLimit = UInt16(exactly: withResponse), withResponseLimit > 0,
              let withoutResponseLimit = UInt16(exactly: withoutResponse), withoutResponseLimit > 0 else {
          return self.finish(id, Self.platformFailure("CoreBluetooth reported no write limit"))
        }
        self.finish(id, .writeLimits(withResponse: withResponseLimit, withoutResponse: withoutResponseLimit))
      }
    case let .readRssi(_, peerId):
      driver.readRssi(peerIdentifier: peerId, operationIdentifier: operationIdentifier) { rssi, error in
        if let error { return self.finish(id, Self.failure(error, verb: .readRssi)) }
        guard let value = rssi.flatMap(Self.rssi) else {
          return self.finish(id, Self.platformFailure("CoreBluetooth reported the RSSI as unavailable"))
        }
        self.finish(id, .rssi(rssi: value))
      }
    case .requestMtu:
      finish(id, Self.unsupported("CoreBluetooth has no caller-directed ATT MTU request"))
    case .requestConnectionPriority:
      finish(id, Self.unsupported("CoreBluetooth has no connection priority control"))
    case .readPhy, .requestPhy:
      finish(id, Self.unsupported("CoreBluetooth exposes no LE PHY control"))
    case .securityState, .createBond, .cancelBond, .bondedPeers:
      finish(id, Self.unsupported("CoreBluetooth exposes no bond state or pairing control"))
    case .acquireBackground, .releaseBackground, .updateBackgroundNotification:
      finish(id, Self.unsupported("Connected-device foreground service is Android-only"))
    case .associateCompanion:
      finish(id, Self.unsupported("Companion device association is Android-only"))
    case .observePresence, .stopPresence:
      finish(id, Self.unsupported("Companion device presence observation is Android-only; Apple restoration arrives through willRestoreState"))
    case .close:
      close(id)
    }
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
      self.driver.write(
        peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
        characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
        value: value as NSData, withResponse: withResponse, operationIdentifier: operationIdentifier
      ) { error in
        self.finishUnit(id, error, verb: .write)
      }
    }
    if withResponse { return perform() }
    // CoreBluetooth truncates an oversize write without response and drops
    // one it cannot queue, both without an error. Refuse both instead.
    driver.writeLimits(peerIdentifier: instance.peerId) { limits, error in
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
    guard let path = Self.path(instance) else { return finish(id, Self.stalePath) }
    // CoreBluetooth writes the CCCD mode itself and never reports it. Rust
    // stops a delivery requirement at the property check on Apple; a hard
    // requirement reaching the radio cannot be honoured here.
    if let requested {
      return finish(id, Self.unsupported("CoreBluetooth cannot be told to write \(requested)"))
    }
    let key = InstanceKey(instance)
    let subscriptionIdentifier = "ubm-rust-sub-\(id)"
    let previous = subscriptionByInstance[key]
    // Registered before the OS call: a value can arrive before
    // didUpdateNotificationState confirms the enable.
    subscriptionsById[subscriptionIdentifier] = Subscription(
      identifier: subscriptionIdentifier, instance: instance, epoch: epoch
    )
    driver.subscribe(
      peerIdentifier: instance.peerId, serviceUUID: instance.serviceUuid, serviceOccurrence: path.service,
      characteristicUUID: instance.characteristicUuid, characteristicOccurrence: path.characteristic,
      subscriptionIdentifier: subscriptionIdentifier, operationIdentifier: operationIdentifier
    ) { error in
      if let error {
        self.subscriptionsById.removeValue(forKey: subscriptionIdentifier)
        return self.finish(id, Self.failure(error, verb: .enableNotifications))
      }
      if let previous, previous != subscriptionIdentifier { self.subscriptionsById.removeValue(forKey: previous) }
      self.subscriptionByInstance[key] = subscriptionIdentifier
      self.finish(id, .notifyEnabled(delivery: "unknown"))
    }
  }

  /// Clears the CCCD this adapter enabled. A path CoreBluetooth already
  /// tore down (link lost, services invalidated) holds no CCCD any more, so
  /// that answer is the released state, not a failure.
  private func disable(
    _ instance: MobileInstance,
    operationIdentifier: String,
    completion: @escaping (MobileRadioCompletion?) -> Void
  ) {
    let key = InstanceKey(instance)
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
      let alreadyReleased = error.map { $0.domain == Self.ownedDomain && ($0.code == 1017 || $0.code == 1019) } ?? false
      if error == nil || alreadyReleased {
        self.subscriptionByInstance.removeValue(forKey: key)
        self.subscriptionsById.removeValue(forKey: subscriptionIdentifier)
        return completion(nil)
      }
      completion(error.map { Self.failure($0, verb: .disableNotifications) })
    }
  }

  /// Teardown: disable every CCCD this adapter enabled, one at a time, and
  /// name every instance that did not release.
  private func close(_ id: UInt64) {
    var remaining = subscriptionsById.values.filter { subscriptionByInstance[InstanceKey($0.instance)] == $0.identifier }
    var failures = [MobileCloseFailure]()
    func next() {
      guard let subscription = remaining.popLast() else {
        return finish(id, .closed(failures: failures))
      }
      disable(subscription.instance, operationIdentifier: "ubm-rust-close-\(id)") { failure in
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
  private func whenReady(_ id: UInt64, _ body: @escaping () -> Void) {
    driver.adapterSnapshot { snapshot in
      if let failure = Self.readinessFailure(Self.adapterSnapshot(snapshot)) {
        return self.finish(id, failure)
      }
      body()
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
    guard inFlight.removeValue(forKey: id) != nil else { return }
    deliver(id, completion)
  }

  private func deliver(_ id: UInt64, _ completion: MobileRadioCompletion) {
    guard let sink else {
      NSLog("[UnifiedBleRustRadioAdapter] request %llu answered before a host was bound", id)
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
    ingest(.adapterState(snapshot: adapter))
    // CoreBluetooth ends every scan when the adapter leaves powered-on and
    // never resumes it: report the loss and clear the radio's scan owner.
    guard scanActive, adapter.power != "on" else { return }
    scanActive = false
    ingest(.scanFailed(detail: "CoreBluetooth stopped scanning: adapter power is \(adapter.power)"))
    driver.stopScan(operationIdentifier: "ubm-rust-scan-lost") { _ in }
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
    ingest(.connection(peerId: peerIdentifier, connected: false, status: error.flatMap { Int32(exactly: $0.code) }))
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

  func protocolRadioDidRestorePeers(_ peers: [NSDictionary]) {
    announceRestored(peers.compactMap(Self.restoredPeer))
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
  }

  // MARK: - Translation

  enum Verb {
    case startScan, stopScan, connect, disconnect, discover, read, write, readDescriptor, writeDescriptor
    case enableNotifications, disableNotifications, readMtu, readWriteLimits, readRssi
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
    case "not-determined": return failed("permission-not-determined", reason, dispatched: false)
    default: break
    }
    switch adapter.power {
    case "on": return nil
    case "off": return failed("adapter-off", reason, dispatched: false)
    case "resetting": return failed("adapter-resetting", reason, dispatched: false)
    default: return failed("adapter-unavailable", reason, dispatched: false)
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
      result.append(MobileGattService(uuid: uuid, occurrence: occurrence, characteristics: mapped))
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
    case let .adapterState(id), let .stopScan(id), let .bondedPeers(id), let .close(id):
      return id
    case let .startScan(id, _, _, _, _, _), let .connect(id, _, _, _), let .disconnect(id, _), let .discover(id, _),
      let .read(id, _), let .write(id, _, _, _), let .readDescriptor(id, _, _, _), let .writeDescriptor(id, _, _, _, _),
      let .enableNotifications(id, _, _, _, _), let .disableNotifications(id, _), let .readMtu(id, _),
      let .readWriteLimits(id, _),
      let .requestMtu(id, _, _), let .readRssi(id, _), let .requestConnectionPriority(id, _, _), let .readPhy(id, _),
      let .requestPhy(id, _, _, _), let .securityState(id, _), let .createBond(id, _, _), let .cancelBond(id, _),
      let .acquireBackground(id, _, _), let .releaseBackground(id, _), let .updateBackgroundNotification(id, _, _, _),
      let .associateCompanion(id, _, _),
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
