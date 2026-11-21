// ios/Owned/OwnedCoreBluetoothCentralDelegate.swift

import CoreBluetooth
import Foundation

/**
 * Forwards central callbacks without advertising restoration support when the
 * consumer did not configure a CoreBluetooth restoration identifier.
 */
class OwnedCoreBluetoothCentralDelegate: NSObject, CBCentralManagerDelegate {
  fileprivate weak var radio: OwnedCoreBluetoothProtocolRadio?

  init(radio: OwnedCoreBluetoothProtocolRadio) {
    self.radio = radio
    super.init()
  }

  fileprivate func currentRadio(_ central: CBCentralManager) -> OwnedCoreBluetoothProtocolRadio? {
    guard let radio, radio.central === central, radio.powerOffRetirementSnapshot == nil else { return nil }
    return radio
  }

  func centralManagerDidUpdateState(_ central: CBCentralManager) {
    currentRadio(central)?.centralManagerDidUpdateState(central)
  }

  func centralManager(
    _ central: CBCentralManager,
    didDiscover peripheral: CBPeripheral,
    advertisementData: [String: Any],
    rssi RSSI: NSNumber
  ) {
    currentRadio(central)?.centralManager(
      central,
      didDiscover: peripheral,
      advertisementData: advertisementData,
      rssi: RSSI
    )
  }

  func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
    currentRadio(central)?.centralManager(central, didConnect: peripheral)
  }

  func centralManager(_ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?) {
    currentRadio(central)?.centralManager(central, didFailToConnect: peripheral, error: error)
  }

  func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
    currentRadio(central)?.centralManager(central, didDisconnectPeripheral: peripheral, error: error)
  }
}

#if os(iOS)
/** Adds the optional restoration callback only when the central has a restoration identifier. */
final class OwnedCoreBluetoothRestoringCentralDelegate: OwnedCoreBluetoothCentralDelegate {
  func centralManager(_ central: CBCentralManager, willRestoreState dictionary: [String: Any]) {
    currentRadio(central)?.centralManager(central, willRestoreState: dictionary)
  }
}
#endif
