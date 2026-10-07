// ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift

import CoreBluetooth
import Foundation
#if os(iOS) && !targetEnvironment(macCatalyst)
import AccessorySetupKit
#endif

/**
 * Pure CoreBluetooth projections, shared radio value types, injectable
 * permission coordination, and queue-confined central access for the Native
 * Protocol radio.
 *
 * This owns no radio state and never calls CoreBluetooth asynchronously; the
 * radio remains the sole owner of delegates, pending operations, and
 * queue-confined mutable state.
 */
enum OwnedCoreBluetoothProtocolRadioSupport {
  static let centralStateErrorDomain = "CoreBluetooth.CBManagerState"
  static func operationReadinessFailure(state: CBManagerState) -> NSError? {
    guard state == .unauthorized else { return nil }
    // A measured state callback, not an NSError emitted by Apple and not a
    // statement about CBManager.authorization's independent global scope.
    return NSError(domain: centralStateErrorDomain, code: state.rawValue,
      userInfo: [NSLocalizedDescriptionKey: "CoreBluetooth reported CBManagerState.unauthorized for this central"])
  }
  private static var startupAccessorySession: AnyObject?
  private static var startupAccessoryAuthorization: AppleAccessoryStartupAuthorization?
  static func advertisementDictionary(
    peripheral: CBPeripheral,
    advertisementData: [String: Any],
    rssi: NSNumber
  ) -> NSDictionary {
    let serviceUUIDs = (advertisementData[CBAdvertisementDataServiceUUIDsKey] as? [CBUUID])?.map {
      normalizedUUID($0.uuidString)
    }
    let solicitedServiceUUIDs = (advertisementData[CBAdvertisementDataSolicitedServiceUUIDsKey] as? [CBUUID])?.map {
      normalizedUUID($0.uuidString)
    }
    let overflowServiceUUIDs = (advertisementData[CBAdvertisementDataOverflowServiceUUIDsKey] as? [CBUUID])?.map {
      normalizedUUID($0.uuidString)
    }
    let serviceData = (advertisementData[CBAdvertisementDataServiceDataKey] as? [CBUUID: Data])?.reduce(into: [String: NSData]()) {
      $0[normalizedUUID($1.key.uuidString)] = $1.value as NSData
    }
    let manufacturerData = advertisementData[CBAdvertisementDataManufacturerDataKey] as? NSData
    var result: [String: Any] = [
      "peerIdentifier": peripheral.identifier.uuidString,
      "observedAt": DispatchTime.now().uptimeNanoseconds / 1_000_000,
      "localName": advertisementData[CBAdvertisementDataLocalNameKey] as? String ?? peripheral.name as Any,
      "rssi": rssi.intValue,
      "serviceUUIDs": serviceUUIDs as Any,
      "solicitedServiceUUIDs": solicitedServiceUUIDs as Any,
      "overflowServiceUUIDs": overflowServiceUUIDs as Any,
      "serviceData": serviceData as Any,
      "connectable": advertisementData[CBAdvertisementDataIsConnectable] as? Bool as Any,
      "txPower": advertisementData[CBAdvertisementDataTxPowerLevelKey] as? NSNumber as Any,
      "manufacturerData": manufacturerData as Any
    ]
    result["fieldProvenance"] = ["corebluetooth-advertisement"]
    return result as NSDictionary
  }

  static func discoverySnapshot(_ discoveredServices: [CBService]) throws -> NSDictionary {
    var services = [NSDictionary]()
    var serviceOccurrences = [String: Int]()
    for service in discoveredServices {
      let serviceUUID = normalizedUUID(service.uuid.uuidString)
      let serviceOccurrence = serviceOccurrences[serviceUUID, default: 0]
      serviceOccurrences[serviceUUID] = serviceOccurrence + 1
      var characteristics = [NSDictionary]()
      var characteristicOccurrences = [String: Int]()
      for characteristic in service.characteristics ?? [] {
        let characteristicUUID = normalizedUUID(characteristic.uuid.uuidString)
        let characteristicOccurrence = characteristicOccurrences[characteristicUUID, default: 0]
        characteristicOccurrences[characteristicUUID] = characteristicOccurrence + 1
        var descriptors = [NSDictionary]()
        var descriptorOccurrences = [String: Int]()
        for descriptor in characteristic.descriptors ?? [] {
          let descriptorUUID = normalizedUUID(descriptor.uuid.uuidString)
          let descriptorOccurrence = descriptorOccurrences[descriptorUUID, default: 0]
          descriptorOccurrences[descriptorUUID] = descriptorOccurrence + 1
          descriptors.append(["uuid": descriptorUUID, "occurrence": descriptorOccurrence] as NSDictionary)
        }
        characteristics.append([
          "uuid": characteristicUUID,
          "occurrence": characteristicOccurrence,
          "readable": characteristic.properties.contains(.read),
          "writableWithResponse": characteristic.properties.contains(.write),
          "writableWithoutResponse": characteristic.properties.contains(.writeWithoutResponse),
          "notifiable": characteristic.properties.contains(.notify),
          "indicatable": characteristic.properties.contains(.indicate),
          "descriptors": descriptors
        ] as NSDictionary)
      }
      services.append([
        "uuid": serviceUUID,
        "occurrence": serviceOccurrence,
        "primary": service.isPrimary,
        "includedServices": try (service.includedServices ?? []).map { target -> NSDictionary in
          guard let index = discoveredServices.firstIndex(where: { $0 === target }) else {
            throw NSError(domain: "UnifiedBle.Graph", code: 1,
              userInfo: [NSLocalizedDescriptionKey: "An included service is absent from the current graph"])
          }
          let uuid = normalizedUUID(target.uuid.uuidString)
          let occurrence = discoveredServices.prefix(index).filter { normalizedUUID($0.uuid.uuidString) == uuid }.count
          return ["uuid": uuid, "occurrence": occurrence] as NSDictionary
        },
        "characteristics": characteristics
      ] as NSDictionary)
    }
    return ["services": services] as NSDictionary
  }

  /// `{peerIdentifier, name, connected}` for each restored peripheral still held.
  static func restoredPeerSnapshots(identifiers: [String], peripherals: [String: CBPeripheral]) -> [NSDictionary] {
    identifiers.compactMap { identifier in
      guard let peripheral = peripherals[identifier] else { return nil }
      return [
        "peerIdentifier": identifier,
        "name": peripheral.name as Any,
        "connected": peripheral.state == .connected
      ] as NSDictionary
    }
  }

  static func adapterSnapshotDictionary(central: CBCentralManager) -> NSDictionary {
    let authorization = currentAuthorizationWord()
    let power: String
    switch central.state {
    case .poweredOn: power = "on"
    case .poweredOff: power = "off"
    case .resetting: power = "resetting"
    case .unsupported: power = "unsupported"
    case .unauthorized: power = "unknown"
    case .unknown: power = "unknown"
    @unknown default: power = "unknown"
    }
    return [
      "availability": central.state == .unsupported ? "unsupported" : "available",
      "authorization": authorization,
      "power": power,
      "safeReason": central.state == .poweredOn ? NSNull() : "CoreBluetooth has not reported a powered-on adapter"
    ] as NSDictionary
  }

  static func parseUUIDs(_ values: [String]) -> [CBUUID]? {
    var result = [CBUUID]()
    for value in values {
      let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
      guard !trimmed.isEmpty,
            trimmed.range(
              of: "^(?:[0-9A-Fa-f]{4}|[0-9A-Fa-f]{8}|[0-9A-Fa-f]{8}(?:-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12})$",
              options: .regularExpression
            ) != nil else {
        return nil
      }
      result.append(CBUUID(string: trimmed))
    }
    return result
  }

  static func normalizedUUID(_ value: String) -> String {
    let uppercased = value.uppercased()
    if uppercased.count == 4 {
      return "0000\(uppercased)-0000-1000-8000-00805F9B34FB"
    }
    if uppercased.count == 8 {
      return "\(uppercased)-0000-1000-8000-00805F9B34FB"
    }
    return uppercased
  }
}

/// What CoreBluetooth can say a characteristic read value is. It reports a
/// read response and a notification through the same `didUpdateValueFor`
/// callback, so while the characteristic can notify (it notifies, a
/// subscription is installed, a notification state change is in flight, or a
/// cancelled one is being undone) the value that completes a read is
/// `readOrNotification`; otherwise it is the `readResponse`.
@objc public enum OwnedCoreBluetoothReadProvenance: Int {
  case readResponse
  case readOrNotification

  /// The shared vocabulary's wire word.
  public var wire: String {
    switch self {
    case .readResponse: return "read-response"
    case .readOrNotification: return "read-or-notification"
    }
  }
}

enum OwnedCoreBluetoothReadNotifyProvenance {
  static func readProvenance(
    isNotifying: Bool,
    hasInstalledSubscription: Bool,
    pendingNotifyChange: Bool,
    pendingCancellationCleanup: Bool
  ) -> OwnedCoreBluetoothReadProvenance {
    isNotifying || hasInstalledSubscription || pendingNotifyChange || pendingCancellationCleanup
      ? .readOrNotification
      : .readResponse
  }

  /// A successful value update reaches the subscription that owns the
  /// characteristic, whether or not it also completes a read: a value that may
  /// be a notification is never withheld from the stream.
  static func deliversNotification(
    hasInstalledSubscription: Bool,
    pendingNotifyEnable: Bool,
    pendingCancellationCleanup: Bool,
    hasError: Bool,
    hasValue: Bool
  ) -> Bool {
    !hasError && hasValue && (hasInstalledSubscription || pendingNotifyEnable) && !pendingCancellationCleanup
  }
}

/// Reads of one characteristic, in request order. CoreBluetooth answers each
/// `readValue(for:)` with exactly one `didUpdateValueFor` (value or error) but
/// cannot say which update answers which read, so at most one `readValue` is
/// outstanding per characteristic and the next is issued only after the
/// previous one's update arrived. A read cancelled or timed out after its
/// `readValue` was issued leaves its update owed: that update is consumed
/// without completing a later read, so a later read never receives an answer
/// CoreBluetooth issued for an abandoned one.
struct OwnedCoreBluetoothReadLane<Waiter> {
  private(set) var inFlight: Waiter?
  private(set) var updateOwed = false
  private(set) var queued: [Waiter] = []

  var isIdle: Bool { !updateOwed && queued.isEmpty }

  /// Admits one read; `true` when the caller must issue `readValue(for:)` now.
  mutating func admit(_ waiter: Waiter) -> Bool {
    guard !updateOwed else {
      queued.append(waiter)
      return false
    }
    inFlight = waiter
    updateOwed = true
    return true
  }

  /// Consumes one value update: the read it completes (nil when the update
  /// answers an abandoned read or none is owed) and whether the caller must
  /// issue `readValue(for:)` for the next queued read.
  mutating func answer() -> (completed: Waiter?, issueNext: Bool) {
    guard updateOwed else { return (nil, false) }
    let completed = inFlight
    inFlight = nil
    updateOwed = false
    guard !queued.isEmpty else { return (completed, false) }
    inFlight = queued.removeFirst()
    updateOwed = true
    return (completed, true)
  }

  /// Abandons every read `matches` selects; the in-flight one keeps its
  /// update owed.
  mutating func cancel(where matches: (Waiter) -> Bool) {
    queued.removeAll(where: matches)
    if let current = inFlight, matches(current) { inFlight = nil }
  }

  /// Every read still waiting, in request order (disconnect, teardown).
  var waiting: [Waiter] { (inFlight.map { [$0] } ?? []) + queued }
}

/// Queue-confined central access for the radio (finding 179). These operate
/// on the radio's state but own none of it; the radio file keeps lifecycle,
/// operations, and teardown, and this file keeps the mechanics that both the
/// radio and the driver surface call.
extension OwnedCoreBluetoothProtocolRadio {
  /// The adapter snapshot without a queue hop; the caller is on the radio
  /// queue. Reading state never prompts: before the radio exists, power is
  /// unknown and the reason says the radio is created on request.
  func snapshotOnQueue() -> NSDictionary {
    if central == nil && (
      OwnedCoreBluetoothProtocolRadioSupport.currentAuthorizationWord() == "notDetermined" ||
      OwnedCoreBluetoothProtocolRadioSupport.accessorySetupConfigured(info: Bundle.main.infoDictionary ?? [:])
    ) {
      return OwnedCoreBluetoothProtocolRadioSupport.prePermissionSnapshot(
        authorization: OwnedCoreBluetoothProtocolRadioSupport.currentAuthorizationWord()
      )
    }
    return OwnedCoreBluetoothProtocolRadioSupport.adapterSnapshotDictionary(central: ensureCentral())
  }

  /// The central, allocating it. Only where the platform cannot prompt.
  func ensureCentral() -> CBCentralManager {
    if let central { return central }
    let created = OwnedCoreBluetoothProtocolRadioSupport.buildCentral(
      delegate: centralDelegate,
      queue: queue,
      restoreIdentifierKey: restoreIdentifierKey,
      showPowerAlert: showPowerAlert,
      restorationConfigured: OwnedCoreBluetoothProtocolRadioSupport.restorationConfigured(
        restoreIdentifierKey: restoreIdentifierKey
      )
    )
    central = created
    return created
  }

  /// The central for radio work, or `nil` while the user has not decided.
  func centralForUse() -> CBCentralManager? {
    if let central { return central }
    guard OwnedCoreBluetoothProtocolRadioSupport.currentAuthorizationWord() != "notDetermined" else { return nil }
    return ensureCentral()
  }

  /// The central for radio work, refusing permission-not-determined (179).
  func centralForUse(or completion: (NSError?) -> Void) -> CBCentralManager? {
    if let central = centralForUse() { return central }
    completion(error(code: 1034, message: "Bluetooth permission has not been requested"))
    return nil
  }
}

/// Exact identifier lookup; the caller supplies the already-admitted central's
/// retrieval and installs the returned object without inventing a radio event.
enum OwnedCoreBluetoothKnownPeerLookup {
  static func identifier(_ value: String) -> UUID? {
    guard let identifier = UUID(uuidString: value),
          identifier.uuidString == value else { return nil }
    return identifier
  }

  static func resolve<Peripheral>(
    identifier: UUID, cached: Peripheral?, retrieve: (UUID) -> [Peripheral],
    identifierOf: (Peripheral) -> UUID
  ) -> Peripheral? {
    if let cached { return cached }
    return retrieve(identifier).first { identifierOf($0) == identifier }
  }

  static func requiresConnection(hasCachedPeripheral: Bool, isConnected: Bool) -> Bool {
    // OS retrieval returns an object, not this central's connection admission.
    !hasCachedPeripheral || !isConnected
  }
}

/// Shared radio value types, used by the radio and its cancellation and
/// descriptor companions. Moved out of the radio file to keep it under its
/// line cap; behavior is unchanged.
struct CharacteristicAddress: Hashable {
  let peerIdentifier: String
  let serviceUUID: String
  let serviceOccurrence: Int
  let characteristicUUID: String
  let characteristicOccurrence: Int
}

struct PendingVoid {
  let operationIdentifier: String
  let completion: (NSError?) -> Void
}

struct PendingCharacteristicRead {
  let operationIdentifier: String
  let completion: (NSData?, OwnedCoreBluetoothReadProvenance, NSError?) -> Void
}

struct PendingRssi {
  let operationIdentifier: String
  let completion: (NSNumber?, NSError?) -> Void
}

struct PendingNotify {
  let operationIdentifier: String
  let subscriptionIdentifier: String
  let enabled: Bool
  let completion: (NSError?) -> Void
}

struct PendingDiscovery {
  let operationIdentifier: String
  let completion: (NSDictionary?, NSError?) -> Void
  var awaitingServices = true
  var cancelled = false
  var completionDelivered = false
  var characteristicCallbacks = Set<ObjectIdentifier>()
  var descriptorCallbacks = Set<ObjectIdentifier>()
  var includeCallbacks = Set<ObjectIdentifier>()
  var isDrained: Bool {
    !awaitingServices && characteristicCallbacks.isEmpty && descriptorCallbacks.isEmpty && includeCallbacks.isEmpty
  }
  mutating func consumeIncludes(_ service: CBService) -> Bool {
    includeCallbacks.remove(ObjectIdentifier(service)) != nil
  }
  mutating func consumeCharacteristics(_ service: CBService) -> Bool {
    characteristicCallbacks.remove(ObjectIdentifier(service)) != nil
  }
  mutating func consumeDescriptors(_ characteristic: CBCharacteristic) -> Bool {
    descriptorCallbacks.remove(ObjectIdentifier(characteristic)) != nil
  }
}

struct PendingCancellationCleanup {
  var discoveryPeers = Set<String>()
  var peerIdentifiers = Set<String>()
  /// The desired physical CCCD state after a cancelled notification transition.
  /// A cancelled subscribe must end disabled; a cancelled unsubscribe must restore
  /// the logically-installed subscription to enabled.
  var notificationDesiredStates = [CharacteristicAddress: Bool]()
  /// CoreBluetooth applies notification changes asynchronously.  Do not infer
  /// completion from the current `isNotifying` value: it can still describe the
  /// state before the cancelled operation's callback arrives.
  var notificationAwaitingCallbacks = Set<CharacteristicAddress>()
}

/// Finding 179: what an Apple Bluetooth permission request does with the
/// platform's authorization word, decided without allocating a
/// CBCentralManager. Allocation presents the system prompt while the word is
/// `notDetermined` (`CBManager.authorization` exists to check "before
/// allocating CBManager", per the CoreBluetooth headers), so only
/// `promptThenWait` may allocate; every other case answers or refuses from
/// the free class-property readout.
enum AppleBluetoothPermissionRequest {
  case answerGranted
  case answerDenied
  case refuseRestricted
  case refuseUnavailable
  case promptThenWait

  static func decision(authorization: String) -> AppleBluetoothPermissionRequest {
    switch authorization {
    case "granted": return .answerGranted
    case "denied": return .answerDenied
    case "restricted": return .refuseRestricted
    case "notDetermined": return .promptThenWait
    default: return .refuseUnavailable
    }
  }

  /// The Android-shaped result (`requested/granted/denied`,
  /// `recommendedSettingsTarget`): a grant needs no settings, a denial
  /// points at the app settings, exactly like Android's Expo module.
  static func result(granted: Bool) -> NSDictionary {
    [
      "requested": ["bluetooth"],
      "granted": granted ? ["bluetooth"] : [],
      "denied": granted ? [] : ["bluetooth"],
      "recommendedSettingsTarget": granted ? NSNull() : "app"
    ] as NSDictionary
  }
}

/// Finding 179: the single-flight authorization exchange behind one Apple
/// permission request (Android's prompt refuses a concurrent one the same
/// way). One waiter settles exactly once — with the decided word or its
/// timeout — and the exchange is reusable afterwards. Queue-confinement is
/// the caller's (the radio queue in production); `schedule` arms the timeout
/// and returns its cancellation.
final class ApplePermissionExchange {
  private let schedule: (UInt64, @escaping () -> Void) -> () -> Void
  private var cancelTimeout: (() -> Void)?
  private(set) var pending = false

  init(schedule: @escaping (UInt64, @escaping () -> Void) -> () -> Void) {
    self.schedule = schedule
  }

  /// Admits one waiter; `false` when one is already pending.
  @discardableResult
  func begin(timeoutMs: UInt64, onTimeout: @escaping () -> Void) -> Bool {
    guard !pending else { return false }
    pending = true
    cancelTimeout = schedule(timeoutMs) { [weak self] in
      guard let self, self.pending else { return }
      self.pending = false
      self.cancelTimeout = nil
      onTimeout()
    }
    return true
  }

  /// Settles the waiter with the decided word, or `nil` when none is
  /// pending (already decided or timed out: late answers are dropped).
  @discardableResult
  func finish(authorization: String) -> String? {
    guard pending else { return nil }
    pending = false
    let cancel = cancelTimeout
    cancelTimeout = nil
    cancel?()
    return authorization
  }
}

/// Queue-confined explicit-operation readiness. The existing Rust request owns
/// its deadline and cancellation; no independent timer or central is created.
final class AppleRadioPreparation {
  private var pending = [String: (NSDictionary?, NSError?) -> Void]()

  func start(_ identifier: String, snapshot: NSDictionary, waitForInitialState: Bool,
             completion: @escaping (NSDictionary?, NSError?) -> Void) {
    if waitForInitialState && Self.waiting(snapshot) { pending[identifier] = completion }
    else { completion(snapshot, nil) }
  }

  func update(_ snapshot: NSDictionary) {
    guard !Self.waiting(snapshot) else { return }
    let callbacks = pending
    pending.removeAll()
    for callback in callbacks.values { callback(snapshot, nil) }
  }

  func cancel(_ identifier: String, error: NSError) {
    pending.removeValue(forKey: identifier)?(nil, error)
  }

  func failAll(_ error: NSError) {
    let callbacks = pending
    pending.removeAll()
    for callback in callbacks.values { callback(nil, error) }
  }

  private static func waiting(_ snapshot: NSDictionary) -> Bool {
    if ["unsupported", "unavailable"].contains(snapshot["availability"] as? String) { return false }
    let authorization = snapshot["authorization"] as? String
    if ["denied", "restricted", "unavailable"].contains(authorization) { return false }
    let power = snapshot["power"] as? String
    return power == "unknown" || power == "resetting"
  }
}

/// Finding 179: one Apple Bluetooth permission request over injected seams,
/// so the harness drives it without allocating a `CBCentralManager` (which
/// would present the system prompt). The radio owns one and reuses it; the
/// exchange inside stays single-flight across requests.
final class ApplePermissionPrompter {
  private let accessorySetupConfigured: () -> Bool
  private let currentAuthorization: () -> String
  private let ensureCentral: () -> Void
  private let makeError: (Int, String) -> NSError
  private let exchange: ApplePermissionExchange
  private var completion: ((NSDictionary?, NSError?) -> Void)?

  init(
    currentAuthorization: @escaping () -> String,
    ensureCentral: @escaping () -> Void,
    schedule: @escaping (UInt64, @escaping () -> Void) -> () -> Void,
    makeError: @escaping (Int, String) -> NSError,
    accessorySetupConfigured: @escaping () -> Bool = { false }
  ) {
    self.currentAuthorization = currentAuthorization
    self.ensureCentral = ensureCentral
    self.makeError = makeError
    self.accessorySetupConfigured = accessorySetupConfigured
    self.exchange = ApplePermissionExchange(schedule: schedule)
  }

  /// Starts the request; `false` when one is already pending (Android's
  /// prompt refuses a concurrent one the same way).
  @discardableResult
  func start(timeoutMs: UInt64, completion: @escaping (NSDictionary?, NSError?) -> Void) -> Bool {
    let word = currentAuthorization()
    if accessorySetupConfigured() && AppleBluetoothPermissionRequest.decision(authorization: word) == .promptThenWait {
      completion(nil, makeError(1040,
        "AccessorySetupKit uses accessory-scoped authorization; this host has no global Bluetooth permission prompt"))
      return true
    }
    guard exchange.begin(timeoutMs: timeoutMs, onTimeout: { [weak self] in self?.completeTimeout() }) else {
      return false
    }
    self.completion = completion
    if AppleBluetoothPermissionRequest.decision(authorization: word) == .promptThenWait {
      ensureCentral()
      return true
    }
    complete(word: word)
    return true
  }

  /// Reports the fresh authorization word; a late word after a decision or
  /// timeout is dropped by the exchange.
  func authorizationChanged() {
    complete(word: currentAuthorization())
  }

  /// Fails a pending waiter without a platform answer (radio teardown).
  func abandon() {
    guard exchange.finish(authorization: "destroyed") != nil else { return }
    let completion = self.completion
    self.completion = nil
    completion?(nil, makeError(1021, "The Native Protocol v2 CoreBluetooth radio was destroyed"))
  }

  private func complete(word: String) {
    // A central may first report unknown/resetting while the user's permission
    // decision is still pending. Keep the same bounded waiter; this callback
    // is not a refusal or an authorization answer.
    let decision = AppleBluetoothPermissionRequest.decision(authorization: word)
    guard decision != .promptThenWait else { return }
    guard exchange.finish(authorization: word) != nil else { return }
    let completion = self.completion
    self.completion = nil
    switch decision {
    case .answerGranted:
      completion?(AppleBluetoothPermissionRequest.result(granted: true), nil)
    case .answerDenied:
      completion?(AppleBluetoothPermissionRequest.result(granted: false), nil)
    case .refuseRestricted:
      completion?(nil, makeError(1035, "iOS restrictions prevent Bluetooth use; the user cannot change this"))
    default:
      completion?(nil, makeError(1036, "This platform exposes no Bluetooth authorization to request"))
    }
  }

  private func completeTimeout() {
    let completion = self.completion
    self.completion = nil
    completion?(nil, makeError(1038, "The Bluetooth permission prompt was not answered in time"))
  }
}

extension OwnedCoreBluetoothProtocolRadioSupport {
  static func accessorySetupConfigured(info: [String: Any]) -> Bool {
    #if os(iOS) && !targetEnvironment(macCatalyst)
    if #available(iOS 18.0, *) {
      return (info["NSAccessorySetupKitSupports"] as? [String])?.contains("Bluetooth") == true
    }
    #endif
    return false
  }

  static func shouldCreateStartupCentral(
    restorationIdentifier: String?, accessorySetup: Bool, restorationLaunchIdentifiers: [String]
  ) -> Bool {
    guard let restorationIdentifier, !restorationIdentifier.isEmpty else { return false }
    return !accessorySetup || restorationLaunchIdentifiers.contains(restorationIdentifier)
  }

  static func authorizedBluetoothAccessory(authorized: Bool, bluetoothIdentifier: UUID?) -> Bool {
    authorized && bluetoothIdentifier != nil
  }

  /// Read the OS authorization list before restoring a configured ASK radio
  /// on an ordinary launch. Never prompt, choose, or infer an authorization.
  static func queryAuthorizedAccessories(
    completion: @escaping (Result<Bool, NSError>) -> Void,
    sessionFailure: @escaping (NSError) -> Void
  ) {
    #if os(iOS) && !targetEnvironment(macCatalyst)
    if #available(iOS 18.0, *) {
      DispatchQueue.main.async {
        if let active = startupAccessorySession as? ASAccessorySession {
          guard startupAccessoryAuthorization?.isActivated == true else {
            if startupAccessoryAuthorization?.join(completion: completion, sessionFailure: sessionFailure) == true {
              return
            }
            completion(.failure(NSError(domain: "UnifiedBleAccessoryStartup", code: 4,
              userInfo: [NSLocalizedDescriptionKey: "Accessory authorization activation waiter capacity exhausted or session retired"])))
            return
          }
          completion(.success(active.accessories.contains { accessory in
            authorizedBluetoothAccessory(authorized: accessory.state == .authorized, bluetoothIdentifier: accessory.bluetoothIdentifier)
          }))
          return
        }
        let session = ASAccessorySession()
        startupAccessorySession = session
        var deadline: DispatchWorkItem?
        let authorization = AppleAccessoryStartupAuthorization(
          completion: completion, sessionFailure: sessionFailure,
          activated: { deadline?.cancel() },
          retire: {
            deadline?.cancel()
            if startupAccessorySession === session {
              startupAccessorySession = nil
              startupAccessoryAuthorization = nil
            }
            session.invalidate()
          }
        )
        startupAccessoryAuthorization = authorization
        let timeout = DispatchWorkItem {
          authorization.fail(NSError(domain: "UnifiedBleAccessoryStartup", code: 1,
            userInfo: [NSLocalizedDescriptionKey: "Accessory authorization query timed out"]))
        }
        deadline = timeout
        DispatchQueue.main.asyncAfter(deadline: .now() + 10, execute: timeout)
        session.activate(on: .main) { event in
          if let error = event.error { authorization.fail(error as NSError); return }
          if event.eventType == .activated {
            authorization.activate(authorized: session.accessories.contains { accessory in
              authorizedBluetoothAccessory(authorized: accessory.state == .authorized, bluetoothIdentifier: accessory.bluetoothIdentifier)
            })
          } else if event.eventType == .invalidated {
            authorization.fail(NSError(domain: "UnifiedBleAccessoryStartup", code: 2,
              userInfo: [NSLocalizedDescriptionKey: "Accessory authorization session invalidated"]))
          }
        }
      }
      return
    }
    #endif
    completion(.failure(NSError(domain: "UnifiedBleAccessoryStartup", code: 3,
      userInfo: [NSLocalizedDescriptionKey: "AccessorySetupKit is unavailable on this platform"])))
  }

  /// A read-only snapshot of the current ASK authorization list. Reuse the
  /// process-owned activated session above; never allocate a CoreBluetooth
  /// central or open a picker to answer a directory query.
  static func queryAuthorizedAccessoryList(
    completion: @escaping (Result<String, NSError>) -> Void,
    sessionFailure: @escaping (NSError) -> Void
  ) {
    guard accessorySetupConfigured(info: Bundle.main.infoDictionary ?? [:]) else {
      completion(.failure(NSError(domain: "UnifiedBleAccessoryStartup", code: 3,
        userInfo: [NSLocalizedDescriptionKey: "AccessorySetupKit is not declared or available"])))
      return
    }
    queryAuthorizedAccessories(completion: { result in
      switch result {
      case .failure(let error): completion(.failure(error))
      case .success:
        #if os(iOS) && !targetEnvironment(macCatalyst)
        if #available(iOS 18.0, *), let session = startupAccessorySession as? ASAccessorySession,
           startupAccessoryAuthorization?.isActivated == true {
          do {
            let text = try AccessoryChoiceAdmission.authorizedListJson(session.accessories.map { accessory in
              (bluetoothIdentifier: accessory.bluetoothIdentifier,
               name: accessory.displayName,
               authorized: accessory.state == .authorized)
            })
            completion(.success(text))
          } catch {
            completion(.failure(error as NSError))
          }
          return
        }
        #endif
        completion(.failure(NSError(domain: "UnifiedBleAccessoryStartup", code: 5,
          userInfo: [NSLocalizedDescriptionKey: "Accessory authorization session no longer active"])))
      }
    }, sessionFailure: sessionFailure)
  }

  static func resumeAuthorizedAccessoryStartup(
    query: (@escaping (Result<Bool, NSError>) -> Void) -> Void,
    createCentral: @escaping () -> Void,
    failure: @escaping (NSError) -> Void
  ) {
    query { result in
      switch result {
      case .success(let authorized): if authorized { createCentral() }
      case .failure(let error): failure(error)
      }
    }
  }

  /// Whether the restore identifier needs its central early: `willRestoreState`
  /// only lands on a central created with it (finding 179 keeps legacy
  /// timing there, and the platform prompt with it).
  static func restorationConfigured(restoreIdentifierKey: String?) -> Bool {
    #if os(iOS)
    return restoreIdentifierKey?.isEmpty == false
    #else
    return false
    #endif
  }

  /// Allocates the process central. The one prompt trigger (finding 179):
  /// iOS presents the system Bluetooth prompt on first allocation while the
  /// user has not decided; once decided, allocation is silent.
  static func buildCentral(
    delegate: OwnedCoreBluetoothCentralDelegate,
    queue: DispatchQueue,
    restoreIdentifierKey: String?,
    showPowerAlert: NSNumber?,
    restorationConfigured: Bool
  ) -> CBCentralManager {
    var options = [String: Any]()
    if let showPowerAlert {
      options[CBCentralManagerOptionShowPowerAlertKey] = showPowerAlert
    }
    #if os(iOS)
    if restorationConfigured {
      options[CBCentralManagerOptionRestoreIdentifierKey] = restoreIdentifierKey
    }
    #endif
    return CBCentralManager(
      delegate: delegate,
      queue: queue,
      options: options.isEmpty ? nil : options
    )
  }

  /// The free authorization readout: no central is allocated, so no prompt
  /// can appear. Words match `adapterSnapshotDictionary`.
  static func currentAuthorizationWord() -> String {
    if #available(iOS 13.1, tvOS 13.1, *) {
      switch CBManager.authorization {
      case .allowedAlways: return "granted"
      case .denied: return "denied"
      case .restricted: return "restricted"
      case .notDetermined: return "notDetermined"
      @unknown default: return "unavailable"
      }
    }
    return "unavailable"
  }

  /// The snapshot before the radio exists: reading state must not prompt,
  /// so power is unknown and the reason says the radio is created on
  /// request. Availability stays `available`: every Apple host ships
  /// Bluetooth hardware, and the allocated central corrects `unsupported`
  /// (simulator, Mac without Bluetooth) on first use.
  static func prePermissionSnapshot(authorization: String) -> NSDictionary {
    [
      "availability": "available",
      "authorization": authorization,
      "power": "unknown",
      "safeReason": "CoreBluetooth state is unknown until an explicit radio request or authorized native restoration creates the central"
    ] as NSDictionary
  }
}

/// Queue-confined ownership of one activation and its retained session. Joined
/// queries share the original deadline; late failures cannot complete them twice.
final class AppleAccessoryStartupAuthorization {
  private enum Phase { case pending, active, retired }
  private var phase = Phase.pending
  private var completions: [(Result<Bool, NSError>) -> Void]
  private var sessionFailures: [(NSError) -> Void]
  private let activated: () -> Void
  private let retire: () -> Void
  var isActivated: Bool { phase == .active }

  @discardableResult
  func join(completion: @escaping (Result<Bool, NSError>) -> Void,
            sessionFailure: @escaping (NSError) -> Void) -> Bool {
    guard phase == .pending, completions.count < 64 else { return false }
    completions.append(completion)
    sessionFailures.append(sessionFailure)
    return true
  }

  init(completion: @escaping (Result<Bool, NSError>) -> Void,
       sessionFailure: @escaping (NSError) -> Void, activated: @escaping () -> Void,
       retire: @escaping () -> Void) {
    self.completions = [completion]
    self.sessionFailures = [sessionFailure]
    self.activated = activated
    self.retire = retire
  }

  @discardableResult
  func activate(authorized: Bool) -> Bool {
    guard phase == .pending else { return false }
    phase = .active
    let answers = completions
    completions.removeAll()
    activated()
    for completion in answers { completion(.success(authorized)) }
    return true
  }

  @discardableResult
  func fail(_ error: NSError) -> Bool {
    guard phase != .retired else { return false }
    let wasPending = phase == .pending
    phase = .retired
    let answers = completions
    let failures = sessionFailures
    completions.removeAll()
    sessionFailures.removeAll()
    retire() // Retire identity before invalidate's synchronous callback.
    if wasPending { for completion in answers { completion(.failure(error)) } }
    else { for failure in failures { failure(error) } }
    return true
  }
}
