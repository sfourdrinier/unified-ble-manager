import Foundation

/// Single-flight UI ownership, separate from persistent OS accessory authorization.
/// Confined to the main queue by the platform adapter. Pure Foundation so the
/// production admission/late-callback behavior is executable without a picker.
final class AccessoryChoiceOwner {
  private(set) var requestId: String?
  private var cancelled = Set<String>()
  private var retired: [String] = []
  private var completion: ((String?, String?) -> Void)?

  func begin(_ id: String, completion: @escaping (String?, String?) -> Void) -> String? {
    if cancelled.remove(id) != nil { return "operation.aborted" }
    if requestId != nil { return "chooser.busy" }
    requestId = id
    self.completion = completion
    return nil
  }

  @discardableResult
  func finish(_ id: String, result: String?, failure: String?) -> Bool {
    guard requestId == id else { return false }
    let answer = completion
    requestId = nil
    completion = nil
    retired.append(id)
    if retired.count > 64 { retired.removeFirst() }
    answer?(result, failure)
    return true
  }

  /// Cancellation may precede native dispatch while identity verification is
  /// pending. Keep a bounded tombstone rather than allowing a late picker.
  func cancelBeforeAdmission(_ id: String) -> Bool {
    if retired.contains(id) { return true }
    if cancelled.contains(id) { return true }
    guard cancelled.count < 64 else { return false }
    cancelled.insert(id)
    return true
  }
}
