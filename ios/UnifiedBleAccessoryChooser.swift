import Foundation

#if os(iOS) && !targetEnvironment(macCatalyst)
import AccessorySetupKit
import CoreBluetooth
import UIKit
#endif

/// OS setup adapter. It never starts a central, connects, or synthesizes a
/// restoration event. Its result names only an accessory the OS authorized.
@objc public final class UnifiedBleAccessoryChooser: NSObject {
  @objc public static let shared = UnifiedBleAccessoryChooser()
  private let owner = AccessoryChoiceOwner()
  private var timer: DispatchWorkItem?
#if os(iOS) && !targetEnvironment(macCatalyst)
  private var session: AnyObject?
#endif

  @objc public func choose(_ requestId: String, optionsJson: String, timeoutMs: Double,
                          completion: @escaping (String?, String?) -> Void) {
    DispatchQueue.main.async { [self] in
      guard requestId.count == 32, requestId.allSatisfy({ $0.isHexDigit }),
            timeoutMs.isFinite, timeoutMs.rounded() == timeoutMs,
            timeoutMs >= 1, timeoutMs <= 2_147_483_647 else {
        completion(nil, Self.failure("argument.invalid", "invalid request identity or timeout")); return
      }
#if os(iOS) && !targetEnvironment(macCatalyst)
      guard #available(iOS 18.0, *) else {
        completion(nil, Self.failure("capability.unsupported", "AccessorySetupKit requires iOS 18")); return
      }
      guard UIApplication.shared.applicationState == .active else {
        completion(nil, Self.failure("chooser.user-activation-required", "Accessory setup requires a foreground app")); return
      }
      do {
        // Validate app declarations BEFORE ASAccessorySession allocation. Apple
        // documents a process crash for undeclared picker identifiers.
        let items = try Self.items(optionsJson, info: Bundle.main.infoDictionary ?? [:])
        if let code = self.owner.begin(requestId, completion: completion) {
          completion(nil, Self.failure(code, "accessory choice admission refused")); return
        }
        let session = ASAccessorySession()
        self.session = session
        let timeout = DispatchWorkItem { [weak self] in
          self?.finish(requestId, result: nil, failure: Self.failure("operation.timed-out", "accessory setup deadline expired"))
        }
        self.timer = timeout
        DispatchQueue.main.asyncAfter(deadline: .now() + timeoutMs / 1000, execute: timeout)
        session.activate(on: .main) { [weak self, weak session] event in
          guard let self, let session, self.owner.requestId == requestId else { return }
          if let error = event.error {
            self.finish(requestId, result: nil, failure: Self.failure("platform.failure", "AccessorySetupKit refused setup", platform: error as NSError)); return
          }
          switch event.eventType {
          case .activated:
            session.showPicker(for: items) { [weak self] error in
              if let error { self?.finish(requestId, result: nil, failure: Self.failure("platform.failure", "AccessorySetupKit picker failed", platform: error as NSError)) }
            }
          case .accessoryAdded:
            guard let accessory = event.accessory, accessory.state == .authorized,
                  let identifier = accessory.bluetoothIdentifier else { return }
            do {
              let result = try Self.json(["revision": "ubm-accessory-chooser/1", "peripheralIdentifier": identifier.uuidString,
                                        "name": accessory.displayName])
              self.finish(requestId, result: result, failure: nil)
            } catch {
              self.finish(requestId, result: nil, failure: Self.failure("protocol.malformed", "Unable to encode authorized accessory"))
            }
          case .pickerDidDismiss:
            self.finish(requestId, result: nil, failure: Self.failure("chooser.cancelled", "Accessory picker dismissed without a selection"))
          case .invalidated:
            self.finish(requestId, result: nil, failure: Self.failure("chooser.closed", "Accessory session invalidated"))
          default: break
          }
        }
      } catch {
        completion(nil, Self.failure("capability.unsupported", "Accessory setup filter or app declaration is unsupported: \(error.localizedDescription)"))
      }
#else
      completion(nil, Self.failure("capability.unsupported", "AccessorySetupKit is unavailable on this platform"))
#endif
    }
  }

  @objc public func cancel(_ requestId: String, completion: @escaping (String?) -> Void) {
    DispatchQueue.main.async {
      guard requestId.count == 32, requestId.allSatisfy({ $0.isHexDigit }) else {
        completion(Self.failure("argument.invalid", "invalid cancellation identity")); return
      }
      if self.owner.requestId == requestId {
        self.finish(requestId, result: nil, failure: Self.failure("operation.aborted", "Accessory setup cancelled"))
        completion(nil)
      } else if self.owner.cancelBeforeAdmission(requestId) { completion(nil) }
      else { completion(Self.failure("platform.failure", "Accessory cancellation admission capacity reached")) }
    }
  }

  private func finish(_ id: String, result: String?, failure: String?) {
    guard owner.requestId == id else { return }
    timer?.cancel(); timer = nil
    // Retire callback identity before invalidate: AS can synchronously emit an
    // invalidated event, which must not replace an authorized winning result.
    owner.finish(id, result: result, failure: failure)
#if os(iOS) && !targetEnvironment(macCatalyst)
    if #available(iOS 18.0, *), let active = session as? ASAccessorySession {
      session = nil
      active.invalidate()
    }
#endif
  }

  private static func json(_ value: [String: Any]) throws -> String {
    let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    guard let text = String(data: data, encoding: .utf8) else { throw CocoaError(.fileReadInapplicableStringEncoding) }
    return text
  }

  private static func failure(_ code: String, _ detail: String, platform: NSError? = nil) -> String {
    var value: [String: Any] = ["code": code, "domain": "chooser", "operation": "accessory.choose", "detail": detail]
    if let platform {
      value["platform"] = ["domain": platform.domain, "code": String(platform.code), "message": platform.localizedDescription,
                           "metadata": [:] as [String: String]]
    }
    // All values above are JSON primitives. No user payload or nonfinite value
    // can make this serializer fail; a literal fail-closed envelope is retained.
    return (try? json(value)) ?? "{\"code\":\"protocol.malformed\",\"domain\":\"chooser\",\"operation\":\"accessory.choose\",\"detail\":\"failure encoding failed\"}"
  }

  @objc public func available() -> Bool {
#if os(iOS) && !targetEnvironment(macCatalyst)
    if #available(iOS 18.0, *) {
      return (Bundle.main.object(forInfoDictionaryKey: "NSAccessorySetupKitSupports") as? [String])?.contains("Bluetooth") == true
    }
#endif
    return false
  }

#if os(iOS) && !targetEnvironment(macCatalyst)
  @available(iOS 18.0, *)
  private static func items(_ text: String, info: [String: Any]) throws -> [ASPickerDisplayItem] {
    guard let data = text.data(using: .utf8),
          let value = try JSONSerialization.jsonObject(with: data) as? [String: Any],
          Set(value.keys) == ["revision", "filters"], value["revision"] as? String == "ubm-accessory-chooser/1",
          let filters = value["filters"] as? [[String: Any]], !filters.isEmpty, filters.count <= 16,
          let supports = info["NSAccessorySetupKitSupports"] as? [String], supports.contains("Bluetooth") else {
      throw CocoaError(.coderInvalidValue)
    }
    let names = info["NSAccessorySetupBluetoothNames"] as? [String] ?? []
    let declaredServices = info["NSAccessorySetupBluetoothServices"] as? [String] ?? []
    guard declaredServices.allSatisfy(AccessoryChoiceAdmission.validUuid) else { throw CocoaError(.coderInvalidValue) }
    let companies = info["NSAccessorySetupBluetoothCompanyIdentifiers"] as? [String] ?? []
    return try filters.map { filter in
      guard AccessoryChoiceAdmission.validFilter(filter) else {
        throw CocoaError(.coderInvalidValue)
      }
      let descriptor = ASDiscoveryDescriptor()
      var hasIdentity = false
      if let service = filter["serviceUuid"] as? String {
        guard let uuid = AccessoryChoiceAdmission.serviceUuidForDescriptor(service, allowed: declaredServices) else { throw CocoaError(.coderInvalidValue) }
        descriptor.bluetoothServiceUUID = uuid
      }
      if let name = filter["namePrefix"] as? String, !name.isEmpty, names.contains(name) {
        guard #available(iOS 18.2, *) else { throw CocoaError(.coderInvalidValue) }
        descriptor.bluetoothNameSubstring = name
        descriptor.bluetoothNameSubstringCompareOptions = .anchored
        hasIdentity = true
      } else if filter["namePrefix"] != nil { throw CocoaError(.coderInvalidValue) }
      if let inputCompany = filter["companyIdentifier"], let company = AccessoryChoiceAdmission.integer(inputCompany) {
        guard companies.contains(where: { UInt16($0.replacingOccurrences(of: "0x", with: ""), radix: 16) == UInt16(company) }) else {
          throw CocoaError(.coderInvalidValue)
        }
        descriptor.bluetoothCompanyIdentifier = ASBluetoothCompanyIdentifier(rawValue: UInt16(company))
        if let prefix = filter["manufacturerPrefix"] as? [Int], !prefix.isEmpty, prefix.count <= 512,
           prefix.allSatisfy({ $0 >= 0 && $0 <= 255 }) {
          descriptor.bluetoothManufacturerDataBlob = Data(prefix.map { UInt8($0) })
          descriptor.bluetoothManufacturerDataMask = Data(repeating: 255, count: prefix.count)
          hasIdentity = true
        } else if !hasIdentity { throw CocoaError(.coderInvalidValue) }
      } else if filter["companyIdentifier"] != nil { throw CocoaError(.coderInvalidValue) }
      guard hasIdentity, descriptor.bluetoothServiceUUID != nil || filter["companyIdentifier"] != nil,
            let image = UIImage(systemName: "sensor.tag.radiowaves.forward") else { throw CocoaError(.coderInvalidValue) }
      return ASPickerDisplayItem(name: Bundle.main.object(forInfoDictionaryKey: "CFBundleDisplayName") as? String ?? "Bluetooth accessory",
                                 productImage: image, descriptor: descriptor)
    }
  }

#endif
}
