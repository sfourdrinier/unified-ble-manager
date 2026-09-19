// ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift

import CoreBluetooth
import Foundation

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

  static func discoverySnapshot(_ discoveredServices: [CBService]) -> NSDictionary {
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
    if central == nil,
       OwnedCoreBluetoothProtocolRadioSupport.currentAuthorizationWord() == "notDetermined" {
      return OwnedCoreBluetoothProtocolRadioSupport.prePermissionSnapshot(authorization: "notDetermined")
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
  var awaitingCharacteristics: Int
  var awaitingDescriptors: Int
}

struct PendingCancellationCleanup {
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

/// Finding 179: one Apple Bluetooth permission request over injected seams,
/// so the harness drives it without allocating a `CBCentralManager` (which
/// would present the system prompt). The radio owns one and reuses it; the
/// exchange inside stays single-flight across requests.
final class ApplePermissionPrompter {
  private let currentAuthorization: () -> String
  private let ensureCentral: () -> Void
  private let makeError: (Int, String) -> NSError
  private let exchange: ApplePermissionExchange
  private var completion: ((NSDictionary?, NSError?) -> Void)?

  init(
    currentAuthorization: @escaping () -> String,
    ensureCentral: @escaping () -> Void,
    schedule: @escaping (UInt64, @escaping () -> Void) -> () -> Void,
    makeError: @escaping (Int, String) -> NSError
  ) {
    self.currentAuthorization = currentAuthorization
    self.ensureCentral = ensureCentral
    self.makeError = makeError
    self.exchange = ApplePermissionExchange(schedule: schedule)
  }

  /// Starts the request; `false` when one is already pending (Android's
  /// prompt refuses a concurrent one the same way).
  @discardableResult
  func start(timeoutMs: UInt64, completion: @escaping (NSDictionary?, NSError?) -> Void) -> Bool {
    guard exchange.begin(timeoutMs: timeoutMs, onTimeout: { [weak self] in self?.completeTimeout() }) else {
      return false
    }
    self.completion = completion
    let word = currentAuthorization()
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
    guard exchange.finish(authorization: word) != nil else { return }
    let completion = self.completion
    self.completion = nil
    switch AppleBluetoothPermissionRequest.decision(authorization: word) {
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
      "safeReason": "The Bluetooth permission prompt has not been requested; the radio is created on request"
    ] as NSDictionary
  }
}
