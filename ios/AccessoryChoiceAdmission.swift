import CoreBluetooth
import Foundation
import CoreFoundation

/// CoreBluetooth UUID equality is semantic: 180D and its full SIG base UUID
/// have different uuidString values but name the same service. Validate first
/// because CBUUID's string constructor must never receive arbitrary input.
enum AccessoryChoiceAdmission {
  /// Serializes only actual ASK-authorized Bluetooth identifiers. Display
  /// labels remain association metadata, not radio-observed peer names.
  static func authorizedListJson(
    _ items: [(bluetoothIdentifier: UUID?, name: String?, authorized: Bool)]
  ) throws -> String {
    guard items.count <= 256 else { throw CocoaError(.coderInvalidValue) }
    var seen = Set<String>()
    var records = [[String: Any]]()
    for item in items where item.authorized {
      guard let identifier = item.bluetoothIdentifier else { continue }
      let uuid = identifier.uuidString
      guard !seen.contains(uuid), item.name.map({ $0.utf8.count <= 1024 }) != false else {
        throw CocoaError(.coderInvalidValue)
      }
      seen.insert(uuid)
      records.append(["bluetoothIdentifier": uuid, "name": item.name ?? NSNull()])
    }
    let data = try JSONSerialization.data(withJSONObject: [
      "revision": "ubm-accessory-authorized/1",
      "accessories": records
    ], options: [.sortedKeys])
    guard data.count <= 131072, let text = String(data: data, encoding: .utf8) else {
      throw CocoaError(.coderInvalidValue)
    }
    return text
  }
  static func integer(_ value: Any) -> Int? {
    guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID(),
          number.doubleValue.isFinite, number.doubleValue == Double(number.intValue) else { return nil }
    return number.intValue
  }

  static func validFilter(_ filter: [String: Any]) -> Bool {
    guard Set(filter.keys).isSubset(of: ["serviceUuid", "namePrefix", "companyIdentifier", "manufacturerPrefix"]) else { return false }
    // A Bluetooth service or company is ASK's required selector. Optional
    // name/manufacturer-data constraints cannot replace it.
    guard filter["serviceUuid"] != nil || filter["companyIdentifier"] != nil else { return false }
    if let service = filter["serviceUuid"] { guard let text = service as? String, validUuid(text) else { return false } }
    if let name = filter["namePrefix"] { guard let text = name as? String, !text.isEmpty, text.utf8.count <= 1024 else { return false } }
    if let company = filter["companyIdentifier"] { guard let value = integer(company), (0...65535).contains(value) else { return false } }
    if let prefix = filter["manufacturerPrefix"] {
      guard filter["companyIdentifier"] != nil, let values = prefix as? [Any], values.count <= 512,
            values.allSatisfy({ value in integer(value).map { (0...255).contains($0) } == true }) else { return false }
    }
    return true
  }
  static func validUuid(_ value: String) -> Bool {
    let compact = value.replacingOccurrences(of: "-", with: "")
    return [4, 8, 32].contains(compact.count) && compact.allSatisfy({ $0.isHexDigit }) &&
      (value.count == compact.count || UUID(uuidString: value) != nil)
  }

  static func serviceDeclared(_ value: String, allowed: [String]) -> Bool {
    return serviceUuidForDescriptor(value, allowed: allowed) != nil
  }

  static func serviceUuidForDescriptor(_ value: String, allowed: [String]) -> CBUUID? {
    guard validUuid(value), allowed.allSatisfy(validUuid) else { return nil }
    let requested = CBUUID(string: value)
    // ASK compares the descriptor's UUID representation with Info.plist, not
    // only BLE semantic equality. Select the first equivalent declaration and
    // construct from that exact representation, never from the public UUID.
    guard let declared = allowed.first(where: { CBUUID(string: $0) == requested }) else { return nil }
    return CBUUID(string: declared)
  }
}
