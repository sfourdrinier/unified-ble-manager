// ios/Owned/OwnedCoreBluetoothProtocolRadioDescriptors.swift

import CoreBluetooth
import Foundation

struct OwnedCoreBluetoothDescriptorAddress: Hashable {
  let characteristic: CharacteristicAddress
  let descriptorUUID: String
  let descriptorOccurrence: Int

  var peerIdentifier: String {
    characteristic.peerIdentifier
  }
}

final class OwnedCoreBluetoothDescriptorOperations {
  let pendingReads = OwnedCoreBluetoothCallbackLedger<OwnedCoreBluetoothDescriptorAddress, (NSData?, NSError?)>()
  let pendingWrites = OwnedCoreBluetoothCallbackLedger<OwnedCoreBluetoothDescriptorAddress, NSError?>()

  @discardableResult func cancel(_ operationIdentifier: String) -> Bool {
    let read = pendingReads.cancel(operationIdentifier)
    let write = pendingWrites.cancel(operationIdentifier)
    return read || write
  }

  func fail(_ peerIdentifier: String, error: NSError) {
    pendingReads.retire(peer: peerIdentifier, failure: (nil, error))
    pendingWrites.retire(peer: peerIdentifier, failure: error)
  }

  func failAll(_ error: NSError) {
    pendingReads.retireAll(failure: (nil, error))
    pendingWrites.retireAll(failure: error)
  }
}

extension OwnedCoreBluetoothProtocolRadio {
  @objc public func readDescriptor(peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int,
    characteristicUUID: String, characteristicOccurrence: Int, descriptorUUID: String, descriptorOccurrence: Int,
    operationIdentifier: String, completion: @escaping (NSData?, NSError?) -> Void) {
    readDescriptorOwned(owner: nil, peerIdentifier: peerIdentifier, serviceUUID: serviceUUID, serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID, characteristicOccurrence: characteristicOccurrence, descriptorUUID: descriptorUUID,
      descriptorOccurrence: descriptorOccurrence, operationIdentifier: operationIdentifier, completion: completion)
  }

  func readDescriptor(borrower: OwnedCoreBluetoothProtocolRadioDelegate, peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int,
    characteristicUUID: String, characteristicOccurrence: Int, descriptorUUID: String, descriptorOccurrence: Int,
    operationIdentifier: String, completion: @escaping (NSData?, NSError?) -> Void) {
    readDescriptorOwned(owner: borrower, peerIdentifier: peerIdentifier, serviceUUID: serviceUUID, serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID, characteristicOccurrence: characteristicOccurrence, descriptorUUID: descriptorUUID,
      descriptorOccurrence: descriptorOccurrence, operationIdentifier: operationIdentifier, completion: completion)
  }

  private func readDescriptorOwned(owner: AnyObject?, peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int,
    characteristicUUID: String, characteristicOccurrence: Int, descriptorUUID: String, descriptorOccurrence: Int,
    operationIdentifier: String, completion: @escaping (NSData?, NSError?) -> Void) {
    queue.async {
      guard self.requireUsable({ completion(nil, $0) }) else { return }
      if let owner, !self.hasActiveGattClaim(owner: owner, peer: peerIdentifier) {
        completion(nil, self.error(code: 1007, message: "This borrower does not own an active connection claim")); return
      }
      let address = self.descriptorAddress(peerIdentifier: peerIdentifier, serviceUUID: serviceUUID, serviceOccurrence: serviceOccurrence,
        characteristicUUID: characteristicUUID, characteristicOccurrence: characteristicOccurrence, descriptorUUID: descriptorUUID, descriptorOccurrence: descriptorOccurrence)
      guard let resolved = self.resolveDescriptor(address) else {
        completion(nil, self.error(code: 1025, message: "The generation-bound descriptor path is stale")); return
      }
      let physical: String
      if let owner {
        guard let id = self.registerScopedGatt(owner: owner, peer: peerIdentifier, operation: operationIdentifier) else {
          completion(nil, self.error(code: 1014, message: "Operation identifier is already owned")); return
        }
        physical = id
      } else { physical = operationIdentifier }
      guard self.descriptorOperations.pendingReads.admit(address, peer: peerIdentifier,
        peripheral: resolved.peripheral, object: resolved.descriptor, operation: physical, completion: { result in
          if let owner {
            let current = self.scopedGattClaimIsCurrent(owner: owner, peer: peerIdentifier, operation: operationIdentifier, physical: physical)
            self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical)
            if !current && result.1 == nil {
              completion(nil, self.error(code: 1020, message: "Connection claim changed before descriptor read callback")); return
            }
          }
          completion(result.0, result.1)
        }) else {
        if let owner { self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical) }
        completion(nil, self.error(code: 1026, message: self.descriptorOperations.pendingReads.hasCapacity
          ? "A read callback is still outstanding for this descriptor" : "Descriptor read callback capacity exhausted")); return
      }
      resolved.peripheral.readValue(for: resolved.descriptor)
    }
  }

  @objc public func writeDescriptor(peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int,
    characteristicUUID: String, characteristicOccurrence: Int, descriptorUUID: String, descriptorOccurrence: Int,
    value: NSData, operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    writeDescriptorOwned(owner: nil, peerIdentifier: peerIdentifier, serviceUUID: serviceUUID, serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID, characteristicOccurrence: characteristicOccurrence, descriptorUUID: descriptorUUID,
      descriptorOccurrence: descriptorOccurrence, value: value, operationIdentifier: operationIdentifier, completion: completion)
  }

  func writeDescriptor(borrower: OwnedCoreBluetoothProtocolRadioDelegate, peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int,
    characteristicUUID: String, characteristicOccurrence: Int, descriptorUUID: String, descriptorOccurrence: Int,
    value: NSData, operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    writeDescriptorOwned(owner: borrower, peerIdentifier: peerIdentifier, serviceUUID: serviceUUID, serviceOccurrence: serviceOccurrence,
      characteristicUUID: characteristicUUID, characteristicOccurrence: characteristicOccurrence, descriptorUUID: descriptorUUID,
      descriptorOccurrence: descriptorOccurrence, value: value, operationIdentifier: operationIdentifier, completion: completion)
  }

  private func writeDescriptorOwned(owner: AnyObject?, peerIdentifier: String, serviceUUID: String, serviceOccurrence: Int,
    characteristicUUID: String, characteristicOccurrence: Int, descriptorUUID: String, descriptorOccurrence: Int,
    value: NSData, operationIdentifier: String, completion: @escaping (NSError?) -> Void) {
    queue.async {
      guard self.requireUsable(completion) else { return }
      if let owner, !self.hasActiveGattClaim(owner: owner, peer: peerIdentifier) {
        completion(self.error(code: 1007, message: "This borrower does not own an active connection claim")); return
      }
      guard value.length <= Self.maximumBinaryPayloadBytes else {
        completion(self.error(code: 1027, message: "The native binary payload exceeds the protocol limit")); return
      }
      let address = self.descriptorAddress(peerIdentifier: peerIdentifier, serviceUUID: serviceUUID, serviceOccurrence: serviceOccurrence,
        characteristicUUID: characteristicUUID, characteristicOccurrence: characteristicOccurrence, descriptorUUID: descriptorUUID, descriptorOccurrence: descriptorOccurrence)
      guard let resolved = self.resolveDescriptor(address) else {
        completion(self.error(code: 1028, message: "The generation-bound descriptor path is stale")); return
      }
      let physical: String
      if let owner {
        guard let id = self.registerScopedGatt(owner: owner, peer: peerIdentifier, operation: operationIdentifier) else {
          completion(self.error(code: 1014, message: "Operation identifier is already owned")); return
        }
        physical = id
      } else { physical = operationIdentifier }
      guard self.descriptorOperations.pendingWrites.admit(address, peer: peerIdentifier,
        peripheral: resolved.peripheral, object: resolved.descriptor, operation: physical, completion: { failure in
          if let owner {
            let current = self.scopedGattClaimIsCurrent(owner: owner, peer: peerIdentifier, operation: operationIdentifier, physical: physical)
            self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical)
            if !current && failure == nil {
              completion(self.error(code: 1020, message: "Connection claim changed before descriptor write callback")); return
            }
          }
          completion(failure)
        }) else {
        if let owner { self.forgetScopedGatt(owner: owner, operation: operationIdentifier, physical: physical) }
        completion(self.error(code: 1029, message: self.descriptorOperations.pendingWrites.hasCapacity
          ? "A write callback is still outstanding for this descriptor" : "Descriptor write callback capacity exhausted")); return
      }
      resolved.peripheral.writeValue(value as Data, for: resolved.descriptor)
    }
  }

  public func peripheral(_ peripheral: CBPeripheral, didUpdateValueFor descriptor: CBDescriptor, error: Error?) {
    let peer = peripheral.identifier.uuidString
    let value = (descriptor.value as? NSData).map { Data(bytes: $0.bytes, count: $0.length) }
    let failure = error as NSError?
    if let gate = orderedGraphPermitGates[peer], gate.isHolding {
      gate.enqueue(bytes: value?.count ?? 0) { self.completeDescriptorRead(peripheral, descriptor: descriptor, value: value, error: failure) }
      return
    }
    completeDescriptorRead(peripheral, descriptor: descriptor, value: value, error: failure)
  }

  private func completeDescriptorRead(_ peripheral: CBPeripheral, descriptor: CBDescriptor, value: Data?, error: NSError?) {
    let answer: (NSData?, NSError?) = error != nil ? (nil, error) :
      value.map { ($0 as NSData, nil) } ?? (nil, self.error(code: 1030, message: "CoreBluetooth returned a non-binary descriptor value"))
    if let operation = descriptorOperations.pendingReads.complete(peripheral: peripheral, object: descriptor, value: answer) {
      refreshCallbackCancellationCleanup(operation)
    }
  }

  public func peripheral(_ peripheral: CBPeripheral, didWriteValueFor descriptor: CBDescriptor, error: Error?) {
    let peer = peripheral.identifier.uuidString
    if let gate = orderedGraphPermitGates[peer], gate.isHolding {
      let failure = error as NSError?
      gate.enqueue { self.peripheral(peripheral, didWriteValueFor: descriptor, error: failure) }
      return
    }
    if let operation = descriptorOperations.pendingWrites.complete(peripheral: peripheral, object: descriptor,
      value: error as NSError?) { refreshCallbackCancellationCleanup(operation) }
  }

  private func descriptorAddress(
    peerIdentifier: String,
    serviceUUID: String,
    serviceOccurrence: Int,
    characteristicUUID: String,
    characteristicOccurrence: Int,
    descriptorUUID: String,
    descriptorOccurrence: Int
  ) -> OwnedCoreBluetoothDescriptorAddress {
    OwnedCoreBluetoothDescriptorAddress(
      characteristic: CharacteristicAddress(
        peerIdentifier: peerIdentifier,
        serviceUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(serviceUUID),
        serviceOccurrence: serviceOccurrence,
        characteristicUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(characteristicUUID),
        characteristicOccurrence: characteristicOccurrence
      ),
      descriptorUUID: OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(descriptorUUID),
      descriptorOccurrence: descriptorOccurrence
    )
  }

  private func resolveDescriptor(
    _ address: OwnedCoreBluetoothDescriptorAddress
  ) -> (peripheral: CBPeripheral, descriptor: CBDescriptor)? {
    guard let resolved = resolve(address.characteristic) else { return nil }
    let matches = (resolved.characteristic.descriptors ?? []).filter {
      OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID($0.uuid.uuidString) == address.descriptorUUID
    }
    guard address.descriptorOccurrence >= 0, address.descriptorOccurrence < matches.count else { return nil }
    return (resolved.peripheral, matches[address.descriptorOccurrence])
  }

  private func descriptorAddress(
    for descriptor: CBDescriptor,
    peerIdentifier: String
  ) -> OwnedCoreBluetoothDescriptorAddress? {
    guard let characteristic = descriptor.characteristic,
          let characteristicAddress = address(for: characteristic, peerIdentifier: peerIdentifier) else { return nil }
    let descriptorUUID = OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID(descriptor.uuid.uuidString)
    let matches = (characteristic.descriptors ?? []).filter {
      OwnedCoreBluetoothProtocolRadioSupport.normalizedUUID($0.uuid.uuidString) == descriptorUUID
    }
    guard let occurrence = matches.firstIndex(where: { $0 === descriptor }) else { return nil }
    return OwnedCoreBluetoothDescriptorAddress(
      characteristic: characteristicAddress,
      descriptorUUID: descriptorUUID,
      descriptorOccurrence: occurrence
    )
  }
}
