// ios/Owned/OwnedCoreBluetoothProtocolRadioSupport.swift

import CoreBluetooth
import Foundation

/**
 * Pure CoreBluetooth projections and value validation for the Native Protocol radio.
 *
 * This owns no radio state and never calls CoreBluetooth asynchronously; the radio remains the
 * sole owner of delegates, pending operations, and queue-confined mutable state.
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
    let authorization: String
    if #available(iOS 13.1, tvOS 13.1, *) {
      switch CBManager.authorization {
      case .allowedAlways: authorization = "granted"
      case .denied: authorization = "denied"
      case .restricted: authorization = "restricted"
      case .notDetermined: authorization = "notDetermined"
      @unknown default: authorization = "unavailable"
      }
    } else {
      authorization = "unavailable"
    }
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
