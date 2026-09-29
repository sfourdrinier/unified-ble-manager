import Foundation
import React
import BlePlx

/// App-only warm process controls. Native ownership outlives the React context.
/// This transport does not report or simulate an OS restoration event.
@objc(UBMReferenceContinuation)
final class ReferenceContinuationModule: NSObject {
  private static let worker = DispatchQueue(label: "com.ubm.reference.continuation")
  private static let admission = DispatchSemaphore(value: 16)

  private final class Settlement: @unchecked Sendable {
    private let lock = NSLock()
    private var completed = false
    func claim() -> Bool {
      lock.lock()
      defer { lock.unlock() }
      guard !completed else { return false }
      completed = true
      return true
    }
  }

  @objc static func requiresMainQueueSetup() -> Bool { false }

  @objc(invoke:peer:declarationJson:token:maxItems:maxBytes:resolve:reject:)
  func invoke(_ operation: String, peer: String, declarationJson: String, token: String,
              maxItems: Double, maxBytes: Double,
              resolve: @escaping RCTPromiseResolveBlock, reject: @escaping RCTPromiseRejectBlock) {
    guard Self.admission.wait(timeout: .now()) == .success else {
      return Self.failure("lifecycle.invalid-state", operation: operation, detail: "Reference continuation queue is full", resolve: resolve, reject: reject)
    }
    Self.worker.async {
      let settlement = Settlement()
      let finish: (String) -> Void = { envelope in
        guard settlement.claim() else {
          NSLog("[UBMReferenceContinuation] duplicate native completion ignored; promise and admission slot already settled")
          return
        }
        Self.admission.signal()
        resolve(envelope)
      }
      func invalid(_ detail: String) {
        Self.admission.signal()
        Self.failure("argument.invalid", operation: operation, detail: detail, resolve: resolve, reject: reject)
      }
      let host = UnifiedBleRustCoreSessions.shared
      switch operation {
      case "execute":
        guard !peer.isEmpty, peer.utf8.count <= 128,
              !declarationJson.isEmpty, declarationJson.utf8.count <= 65536,
              let declaration = try? JSONSerialization.jsonObject(with: Data(declarationJson.utf8)) as? [String: Any],
              declaration["onAppearance"] as? String == "native",
              declaration["peerId"] as? String == peer else {
          return invalid("Exact native declaration peer required")
        }
        host.executeNativeContinuation(peer, declarationJson: declarationJson, completion: finish)
      case "status":
        host.describeNativeContinuation(completion: finish)
      case "prepare":
        guard maxItems.isFinite, maxItems > 0, maxItems <= 2048, maxItems.rounded(.towardZero) == maxItems,
              maxBytes.isFinite, maxBytes > 0, maxBytes <= 4194304, maxBytes.rounded(.towardZero) == maxBytes else {
          return invalid("Invalid claim bounds")
        }
        host.prepareNativeContinuationClaim(maxItems: maxItems, maxBytes: maxBytes, completion: finish)
      case "acknowledge":
        guard !token.isEmpty, token.utf8.count <= 256 else { return invalid("Invalid claim token") }
        host.acknowledgeNativeContinuationClaim(token, completion: finish)
      default:
        invalid("Unknown continuation control")
      }
    }
  }

  private static func failure(_ code: String, operation: String, detail: String,
                              resolve: RCTPromiseResolveBlock, reject: RCTPromiseRejectBlock) {
    let context: String
    switch operation {
    case "execute", "status", "prepare", "acknowledge": context = "continuation.\(operation)"
    default: context = "continuation.invoke"
    }
    // JSONSerialization is the same serializer as the native session boundary;
    // native-owned results never enter this wrapper-owned validation path.
    let envelope: [String: Any] = ["ok": false, "commit": NSNull(), "retryability": "never",
      "error": ["code": code, "domain": "restoration", "operation": context, "detail": detail]]
    do {
      resolve(String(decoding: try JSONSerialization.data(withJSONObject: envelope, options: [.sortedKeys]), as: UTF8.self))
    } catch {
      reject("UBMReferenceContinuation", "Could not serialize native control failure", error)
    }
  }
}
