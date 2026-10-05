import AccessorySetupKit
import CoreBluetooth
import Foundation

@main struct AccessoryChoiceDescriptorHarness {
  static func main() throws {
    if #available(iOS 18.2, *) {
      try checkDescriptors()
      print("AccessoryChoiceDescriptor: service-only/company-only, optional constraints and declarations passed")
    } else {
      preconditionFailure("ASK descriptor harness requires iOS 18.2+")
    }
  }

  @available(iOS 18.2, *)
  static func checkDescriptors() throws {
    let info: [String: Any] = [
      "NSAccessorySetupKitSupports": ["Bluetooth"],
      "NSAccessorySetupBluetoothServices": ["180D"],
      "NSAccessorySetupBluetoothCompanyIdentifiers": ["006B"],
      "NSAccessorySetupBluetoothNames": ["SIM"]
    ]
    func items(_ filter: [String: Any], declarations: [String: Any]? = nil) throws -> [ASPickerDisplayItem] {
      let data = try JSONSerialization.data(withJSONObject: [
        "revision": "ubm-accessory-chooser/1", "filters": [filter]
      ])
      return try UnifiedBleAccessoryChooser.items(String(decoding: data, as: UTF8.self), info: declarations ?? info)
    }
    let service = try items(["serviceUuid": "0000180d-0000-1000-8000-00805f9b34fb"])
    precondition(service.count == 1 && service[0].descriptor.bluetoothServiceUUID?.uuidString == "180D")
    precondition(service[0].descriptor.bluetoothNameSubstring == nil)
    precondition(service[0].descriptor.bluetoothManufacturerDataBlob == nil)
    for filter: [String: Any] in [["companyIdentifier": 107], ["companyIdentifier": 107, "manufacturerPrefix": []]] {
      let company = try items(filter)
      precondition(company.count == 1 && company[0].descriptor.bluetoothCompanyIdentifier.rawValue == 107)
      precondition(company[0].descriptor.bluetoothServiceUUID == nil)
      precondition(company[0].descriptor.bluetoothNameSubstring == nil)
      precondition(company[0].descriptor.bluetoothManufacturerDataBlob == nil)
    }
    let constrained = try items([
      "serviceUuid": "180D", "companyIdentifier": 107,
      "namePrefix": "SIM", "manufacturerPrefix": [63, 21]
    ])[0].descriptor
    precondition(constrained.bluetoothServiceUUID?.uuidString == "180D")
    precondition(constrained.bluetoothCompanyIdentifier.rawValue == 107)
    precondition(constrained.bluetoothNameSubstring == "SIM")
    precondition(constrained.bluetoothNameSubstringCompareOptions == .anchored)
    precondition(constrained.bluetoothManufacturerDataBlob == Data([63, 21]))
    precondition(constrained.bluetoothManufacturerDataMask == Data([255, 255]))
    for filter: [String: Any] in [
      [:], ["namePrefix": "SIM"], ["serviceUuid": "180F"],
      ["companyIdentifier": 108], ["companyIdentifier": 65536],
      ["companyIdentifier": true], ["companyIdentifier": 107, "manufacturerPrefix": [true]],
      ["serviceUuid": "180D", "namePrefix": "undeclared"]
    ] {
      do {
        _ = try items(filter)
      } catch { continue }
      preconditionFailure("invalid or undeclared descriptor accepted: \(filter)")
    }
    do {
      _ = try items(["serviceUuid": "180D"], declarations: [:])
    } catch { return }
    preconditionFailure("missing ASK declaration accepted")
  }
}
