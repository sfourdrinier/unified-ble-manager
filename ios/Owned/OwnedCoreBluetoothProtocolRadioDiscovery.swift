// Discovery remains confined to the owning radio queue.
import CoreBluetooth
import Foundation

extension OwnedCoreBluetoothProtocolRadio {
  public func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
    let identifier = peripheral.identifier.uuidString
    guard var pending = pendingDiscovery[identifier], pending.awaitingServices else { return }
    pending.awaitingServices = false
    if let error { failDiscovery(identifier, pending: pending, error: error); return }
    if pending.cancelled || pending.completionDelivered {
      pendingDiscovery[identifier] = pending
      finishDiscoveryIfReady(identifier)
      return
    }
    let services = peripheral.services ?? []
    guard services.count <= 4096, Set(services.map(ObjectIdentifier.init)).count == services.count else {
      failDiscovery(identifier, pending: pending, error: self.error(code: 1009, message: "The service graph exceeds its bound or contains duplicate objects"))
      return
    }
    servicesByPeer[identifier] = services
    pending.characteristicCallbacks = Set(services.map(ObjectIdentifier.init))
    pending.includeCallbacks = Set(services.map(ObjectIdentifier.init))
    pendingDiscovery[identifier] = pending
    for service in services {
      peripheral.discoverCharacteristics(nil, for: service)
      peripheral.discoverIncludedServices(nil, for: service)
    }
    finishDiscoveryIfReady(identifier)
  }

  public func peripheral(_ peripheral: CBPeripheral, didDiscoverIncludedServicesFor service: CBService, error: Error?) {
    let identifier = peripheral.identifier.uuidString
    guard var pending = pendingDiscovery[identifier], pending.consumeIncludes(service) else { return }
    if let error { failDiscovery(identifier, pending: pending, error: error); return }
    if pending.cancelled || pending.completionDelivered {
      pendingDiscovery[identifier] = pending
      finishDiscoveryIfReady(identifier)
      return
    }
    var services = servicesByPeer[identifier] ?? []
    var added = [CBService]()
    for target in service.includedServices ?? [] where !services.contains(where: { $0 === target }) {
      services.append(target)
      added.append(target)
    }
    guard services.count <= 4096 else {
      failDiscovery(identifier, pending: pending, error: self.error(code: 1009, message: "The included service graph exceeds its bound"))
      return
    }
    servicesByPeer[identifier] = services
    for target in added {
      pending.includeCallbacks.insert(ObjectIdentifier(target))
      pending.characteristicCallbacks.insert(ObjectIdentifier(target))
    }
    pendingDiscovery[identifier] = pending
    for target in added {
      peripheral.discoverCharacteristics(nil, for: target)
      peripheral.discoverIncludedServices(nil, for: target)
    }
    finishDiscoveryIfReady(identifier)
  }

  public func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
    let identifier = peripheral.identifier.uuidString
    guard var pending = pendingDiscovery[identifier], pending.consumeCharacteristics(service) else { return }
    if let error { failDiscovery(identifier, pending: pending, error: error); return }
    if pending.cancelled || pending.completionDelivered {
      pendingDiscovery[identifier] = pending
      finishDiscoveryIfReady(identifier)
      return
    }
    let characteristics = service.characteristics ?? []
    guard pending.descriptorCallbacks.count + characteristics.count <= 65536 else {
      failDiscovery(identifier, pending: pending, error: self.error(code: 1009, message: "The characteristic graph exceeds its bound"))
      return
    }
    for characteristic in characteristics {
      pending.descriptorCallbacks[ObjectIdentifier(characteristic)] = ObjectIdentifier(service)
    }
    pendingDiscovery[identifier] = pending
    for characteristic in characteristics { peripheral.discoverDescriptors(for: characteristic) }
    finishDiscoveryIfReady(identifier)
  }

  public func peripheral(_ peripheral: CBPeripheral, didDiscoverDescriptorsFor characteristic: CBCharacteristic, error: Error?) {
    let identifier = peripheral.identifier.uuidString
    guard var pending = pendingDiscovery[identifier], pending.consumeDescriptors(characteristic) else { return }
    if let error { failDiscovery(identifier, pending: pending, error: error); return }
    pendingDiscovery[identifier] = pending
    finishDiscoveryIfReady(identifier)
  }

  func invalidateDiscovery(_ identifier: String, services: [CBService]) {
    guard var pending = pendingDiscovery[identifier] else { return }
    pending.retireInvalidatedServices(services)
    failDiscovery(identifier, pending: pending, error: self.error(code: 1026, message: "CoreBluetooth services changed during discovery"))
  }

  func failDiscovery(_ identifier: String, pending source: PendingDiscovery, error: Error) {
    var pending = source
    let deliver = !pending.cancelled && !pending.completionDelivered
    pending.completionDelivered = true
    pendingDiscovery[identifier] = pending
    if deliver { pending.completion(nil, error as NSError) }
    finishDiscoveryIfReady(identifier)
  }

  func finishDiscoveryIfReady(_ peerIdentifier: String) {
    guard let pending = pendingDiscovery[peerIdentifier], pending.isDrained else { return }
    pendingDiscovery.removeValue(forKey: peerIdentifier)
    clearDiscoveryCancellationCleanup(forPeerIdentifier: peerIdentifier)
    guard !pending.cancelled && !pending.completionDelivered else { return }
    do {
      pending.completion(try OwnedCoreBluetoothProtocolRadioSupport.discoverySnapshot(servicesByPeer[peerIdentifier] ?? []), nil)
    } catch { pending.completion(nil, error as NSError) }
  }
}
