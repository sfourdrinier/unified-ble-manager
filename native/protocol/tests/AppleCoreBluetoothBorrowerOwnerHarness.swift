// native/protocol/tests/AppleCoreBluetoothBorrowerOwnerHarness.swift

import Foundation

@main
enum AppleCoreBluetoothBorrowerOwnerHarness {
  static func main() {
    let coordinator = OwnedCoreBluetoothBorrowerReleaseCoordinator()
    let first = NSObject()
    let second = NSObject()
    var firstResults = [NSError?]()
    var joinedResults = [NSError?]()

    precondition(coordinator.attach(first))
    precondition(!coordinator.attach(first))
    precondition(!coordinator.attach(second))
    guard case .start = coordinator.beginRelease(first, completion: { firstResults.append($0) }) else {
      preconditionFailure("The first borrower release did not start")
    }
    guard case .joined = coordinator.beginRelease(first, completion: { joinedResults.append($0) }) else {
      preconditionFailure("A concurrent release did not join")
    }
    precondition(coordinator.releaseInProgress)
    precondition(!coordinator.attach(second))

    let timeout = NSError(domain: "test", code: 1)
    for completion in coordinator.finish(timeout) { completion(timeout) }
    precondition(firstResults.first! === timeout)
    precondition(joinedResults.first! === timeout)
    precondition(!coordinator.releaseInProgress)
    precondition(!coordinator.attach(second))

    guard case .start = coordinator.beginRelease(first, completion: { firstResults.append($0) }) else {
      preconditionFailure("Failed cleanup did not remain retryable by the same borrower")
    }
    for completion in coordinator.finish(nil) { completion(nil) }
    precondition(firstResults.count == 2 && firstResults[1] == nil)
    precondition(coordinator.attach(second))
    guard case .noBorrower = coordinator.beginRelease(first, completion: { _ in }) else {
      preconditionFailure("A stale borrower was allowed to release the replacement")
    }
  }
}
