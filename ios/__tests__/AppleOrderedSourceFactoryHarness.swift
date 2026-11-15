// Production owner factory tests using inert CoreBluetooth objects; no radio is started.
import Foundation
import CoreBluetooth
import ObjectiveC.runtime

private final class FactoryPeripheral: CBPeripheral {
  override var state: CBPeripheralState { .connected }
}

@main enum AppleOrderedSourceFactoryHarness {
  static func checkNoEffect(_ error: Error, _ expected: OwnedOrderedNotificationIngress.Failure) {
    guard let failure = error as? OwnedOrderedSourceFactoryNoEffect,
          let underlying = failure.underlying as? OwnedOrderedNotificationIngress.Failure else {
      preconditionFailure("expected typed no-install failure with original error: \(error)")
    }
    precondition(underlying == expected)
  }

  static func main() throws {
    guard let peripheral = class_createInstance(FactoryPeripheral.self, 0) as? FactoryPeripheral else {
      fatalError("fixture allocation")
    }
    // CoreBluetooth's private deinitializer expects initialization we intentionally bypass.
    _ = Unmanaged.passRetained(peripheral)
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let peer = "factory-peer"
    let characteristic = CBMutableCharacteristic(type: CBUUID(string: "2A37"), properties: [.notify], value: nil, permissions: [.readable])
    let service = CBMutableService(type: CBUUID(string: "180D"), primary: true)
    service.characteristics = [characteristic]
    let address = CharacteristicAddress(peerIdentifier: peer,
      serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("180D"), serviceOccurrence: 0,
      characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID("2A37"), characteristicOccurrence: 0)
    try radio.queue.sync {
      do {
        _ = try radio.createOrderedSourceGroup(addresses: [address], capacity: 1, byteCapacity: 1)
        preconditionFailure("unresolved characteristic accepted")
      } catch { checkNoEffect(error, .invalidMembers); precondition(radio.orderedSources[peer] == nil) }
      radio.peripheralByIdentifier[peer] = peripheral
      radio.servicesByPeer[peer] = [service]
      radio.retiredNotificationOwners[ObjectIdentifier(characteristic)] = RetiredNotificationOwner(
        generation: "old", address: address, peripheral: peripheral, characteristic: characteristic, phase: .stalled)
      do {
        _ = try radio.createOrderedSourceGroup(addresses: [address], capacity: 1, byteCapacity: 1)
        preconditionFailure("unavailable characteristic accepted")
      } catch { checkNoEffect(error, .invalidMembers); precondition(radio.orderedSources[peer] == nil) }
      radio.retiredNotificationOwners.removeValue(forKey: ObjectIdentifier(characteristic))
      do {
        _ = try radio.createOrderedSourceGroup(addresses: [address], capacity: 0, byteCapacity: 1)
        preconditionFailure("invalid capacity accepted")
      } catch {
        checkNoEffect(error, .invalidCapacity)
        precondition(radio.orderedSources[peer] == nil, "failed ingress construction published a source")
      }
      do {
        _ = try radio.createOrderedSourceGroup(addresses: [address, address], capacity: 1, byteCapacity: 1)
        preconditionFailure("duplicate routes accepted")
      } catch { checkNoEffect(error, .invalidMembers); precondition(radio.orderedSources[peer] == nil) }
      let (source, first) = try radio.createOrderedSourceGroup(addresses: [address], capacity: 2, byteCapacity: 8)
      precondition(radio.orderedSources[peer] === source)
      do {
        _ = try radio.createOrderedSourceGroup(addresses: [address], capacity: 0, byteCapacity: 8)
        preconditionFailure("invalid capacity accepted for existing source")
      } catch { checkNoEffect(error, .invalidCapacity); precondition(radio.orderedSources[peer] === source) }
      let (sameSource, second) = try radio.createOrderedSourceGroup(addresses: [address], capacity: 2, byteCapacity: 8)
      precondition(sameSource === source)
      first.close(); second.close()
      try first.seal(); try second.seal()
      _ = first.drain(); _ = second.drain()
      precondition(source.remove(first) && source.remove(second), "both groups retain their ownership")
      source.invalidate(.serviceChanged)
      do {
        _ = try radio.createOrderedSourceGroup(addresses: [address], capacity: 2, byteCapacity: 8)
        preconditionFailure("inactive source accepted")
      } catch { checkNoEffect(error, .inactive); precondition(radio.orderedSources[peer] === source) }
    }
    print("Apple ordered source factory: deterministic tests passed")
  }
}
