// ios/__tests__/AppleCoreBluetoothBorrowerOwnerHarness.swift
//
// Finding 179 sections cover the Apple permission request: the pure
// authorization-word decision and the injectable prompter (decision,
// single-flight wait, timeout, teardown). Both run without allocating a
// CBCentralManager (which would present the system prompt and needs a
// Bluetooth-authorized host), so they are deterministic here; the prompt
// itself is hardware-verified.

import Foundation

@main
enum AppleCoreBluetoothBorrowerOwnerHarness {
  static func main() {
    checkPermissionDecision()
    checkAuthorizationWaiters()
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

  /// Finding 179: the authorization word decides the permission request
  /// without touching CoreBluetooth. A decided word answers at once, a
  /// restriction is genuinely unpromptable, and only `notDetermined`
  /// allocates the central that presents the system prompt.
  static func checkPermissionDecision() {
    guard case .answerGranted = AppleBluetoothPermissionRequest.decision(authorization: "granted") else {
      preconditionFailure("granted must answer at once")
    }
    guard case .answerDenied = AppleBluetoothPermissionRequest.decision(authorization: "denied") else {
      preconditionFailure("denied must answer at once")
    }
    guard case .refuseRestricted = AppleBluetoothPermissionRequest.decision(authorization: "restricted") else {
      preconditionFailure("restricted must refuse without prompting")
    }
    guard case .refuseUnavailable = AppleBluetoothPermissionRequest.decision(authorization: "unavailable") else {
      preconditionFailure("unavailable must refuse without prompting")
    }
    guard case .promptThenWait = AppleBluetoothPermissionRequest.decision(authorization: "notDetermined") else {
      preconditionFailure("notDetermined must prompt, never answer")
    }
    guard case .refuseUnavailable = AppleBluetoothPermissionRequest.decision(authorization: "something-else") else {
      preconditionFailure("an unknown word must refuse, never prompt")
    }
  }

  /// Finding 179: the prompter answers a decided word at once without
  /// allocating a central, prompts (allocates) only while undecided, stays
  /// single-flight like Android's prompt, times out an unanswered prompt,
  /// and drops late answers. No `CBCentralManager` is allocated here.
  static func checkAuthorizationWaiters() {
    var word = "notDetermined"
    var ensured = 0
    var scheduled = [(delayMs: UInt64, work: () -> Void)]()
    func make() -> ApplePermissionPrompter {
      ApplePermissionPrompter(
        currentAuthorization: { word },
        ensureCentral: { ensured += 1 },
        schedule: { delayMs, work in
          scheduled.append((delayMs, work))
          return {}
        },
        makeError: { code, message in
          NSError(domain: "test", code: code, userInfo: [NSLocalizedDescriptionKey: message])
        }
      )
    }

    var answered = [(NSDictionary?, NSError?)]()
    func start(_ prompter: ApplePermissionPrompter) -> Bool {
      prompter.start(timeoutMs: 300_000) { answered.append(($0, $1)) }
    }

    var prompter = make()
    word = "granted"
    precondition(start(prompter), "a decided prompt starts")
    precondition(answered.count == 1, "a decided word answers at once")
    precondition((answered[0].0?["granted"] as? [String]) == ["bluetooth"], "a grant reports granted")
    precondition(answered[0].0?["recommendedSettingsTarget"] is NSNull, "a grant needs no settings")
    precondition(answered[0].1 == nil, "a grant carries no error")
    precondition(ensured == 0, "an answered prompt never allocates")

    answered.removeAll()
    word = "denied"
    precondition(start(prompter), "the exchange is reusable")
    precondition((answered[0].0?["denied"] as? [String]) == ["bluetooth"], "a denial reports denied")
    precondition((answered[0].0?["recommendedSettingsTarget"] as? String) == "app", "a denial points at settings")

    answered.removeAll()
    word = "restricted"
    precondition(start(prompter), "a restriction starts")
    precondition(answered[0].0 == nil && answered[0].1?.code == 1035, "a restriction refuses with its reason")
    word = "unavailable"
    answered.removeAll()
    precondition(start(prompter), "an unknown platform starts")
    precondition(answered[0].0 == nil && answered[0].1?.code == 1036, "an unknown platform refuses")
    precondition(ensured == 0, "a refusal never allocates")

    prompter = make()
    answered.removeAll()
    word = "notDetermined"
    precondition(start(prompter), "an undecided prompt starts")
    precondition(answered.isEmpty, "an undecided prompt waits")
    precondition(ensured == 1, "waiting allocates the central that prompts")
    precondition(!start(prompter), "a concurrent prompt is refused like Android's")
    word = "denied"
    prompter.authorizationChanged()
    precondition(answered.count == 1, "the decision settles the waiter")
    precondition((answered[0].0?["denied"] as? [String]) == ["bluetooth"], "the decision reports denied")
    prompter.authorizationChanged()
    precondition(answered.count == 1, "a second decision must not refire")

    prompter = make()
    answered.removeAll()
    word = "notDetermined"
    precondition(start(prompter), "the prompter is reusable after a decision")
    scheduled.last!.work()
    precondition(answered.count == 1, "an unanswered prompt times out")
    precondition(answered[0].0 == nil && answered[0].1?.code == 1038, "the timeout reports what happened")
    word = "granted"
    prompter.authorizationChanged()
    precondition(answered.count == 1, "a decision after the timeout stays dropped")

    prompter = make()
    answered.removeAll()
    word = "notDetermined"
    precondition(start(prompter), "the prompter is reusable after a timeout")
    prompter.abandon()
    precondition(answered.count == 1, "teardown answers the waiter")
    precondition(answered[0].0 == nil && answered[0].1?.code == 1021, "teardown reports destruction")
  }
}
