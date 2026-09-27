import Foundation
public final class UnifiedBleRustCoreSessions: @unchecked Sendable {
  public static let shared = UnifiedBleRustCoreSessions()
  public static let envelope = "{\"ok\":false,\"error\":{\"code\":\"platform.failure\",\"platform\":{\"code\":\"native-marker\"}},\"retryability\":\"caller-decides\",\"commit\":null}"
  private let lock = NSLock()
  private var calls: [String] = []
  private var pending: [(String) -> Void] = []
  private var holding = false
  private var duplicateNext = false
  public func duplicateNextCallback() { lock.lock(); duplicateNext = true; lock.unlock() }
  public func setHolding(_ value: Bool) { lock.lock(); holding = value; lock.unlock() }
  public func callCount() -> Int { lock.lock(); defer { lock.unlock() }; return calls.count }
  public func release() {
    lock.lock(); let callbacks = pending; pending = []; holding = false; lock.unlock()
    callbacks.forEach { $0(Self.envelope) }
  }
  private func answer(_ name: String, _ completion: @escaping (String) -> Void) {
    precondition(!Thread.isMainThread, "native work ran on main")
    lock.lock(); calls.append(name); let held = holding
    let duplicate = duplicateNext; duplicateNext = false
    if held { pending.append(completion) }
    lock.unlock()
    if !held {
      completion(Self.envelope)
      if duplicate { completion(Self.envelope) }
    }
  }
  public func executeNativeContinuation(_ peer: String, declarationJson: String, completion: @escaping (String) -> Void) { answer("execute", completion) }
  public func describeNativeContinuation(completion: @escaping (String) -> Void) { answer("status", completion) }
  public func prepareNativeContinuationClaim(maxItems: Double, maxBytes: Double, completion: @escaping (String) -> Void) { answer("prepare", completion) }
  public func acknowledgeNativeContinuationClaim(_ token: String, completion: @escaping (String) -> Void) { answer("acknowledge", completion) }
}
