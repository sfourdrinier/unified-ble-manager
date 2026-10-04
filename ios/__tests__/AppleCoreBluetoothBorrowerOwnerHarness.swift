// ios/__tests__/AppleCoreBluetoothBorrowerOwnerHarness.swift
//
// Finding 179 sections cover the Apple permission request: the pure
// authorization-word decision and the injectable prompter (decision,
// single-flight wait, timeout, teardown). Both run without allocating a
// CBCentralManager (which would present the system prompt and needs a
// Bluetooth-authorized host), so they are deterministic here; the prompt
// itself is hardware-verified.

import Foundation
import CoreBluetooth

@main
enum AppleCoreBluetoothBorrowerOwnerHarness {
  static func main() {
    checkAccessoryStartupAdmission()
    checkPermissionDecision()
    checkAuthorizationWaiters()
    checkRadioPreparation()
    checkKnownPeerRetrieval()
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

  static func checkAccessoryStartupAdmission() {
    let bluetoothIdentifier = UUID()
    let bluetooth = OwnedCoreBluetoothProtocolRadioSupport.authorizedBluetoothAccessory
    precondition(!bluetooth(true, nil))
    precondition(!bluetooth(false, bluetoothIdentifier))
    precondition(bluetooth(true, bluetoothIdentifier))
    precondition([(true, nil), (true, bluetoothIdentifier)].contains { bluetooth($0.0, $0.1) })
    let policy = OwnedCoreBluetoothProtocolRadioSupport.shouldCreateStartupCentral
    precondition(policy("restore-1", false, [])) // Legacy timing unchanged.
    precondition(!policy(nil, false, ["restore-1"]))
    precondition(!policy("restore-1", true, []))
    precondition(!policy("restore-1", true, ["different-restore"]))
    precondition(policy("restore-1", true, ["restore-1"]))
    var installed = false
    var allocated = 0
    var failures = [NSError]()
    let queryFailure = NSError(domain: "ASErrorDomain", code: 550,
      userInfo: [NSLocalizedDescriptionKey: "CBManagers active with global permissions"])
    for answer: Result<Bool, NSError> in [.success(false), .success(true), .failure(queryFailure)] {
      // Production installs/binds the host before beginning a query, including
      // an injected synchronously answered query. No central is allocated yet.
      installed = true
      OwnedCoreBluetoothProtocolRadioSupport.resumeAuthorizedAccessoryStartup(
        query: { completion in completion(answer) },
        createCentral: { precondition(installed); allocated += 1 },
        failure: { failures.append($0) }
      )
    }
    precondition(allocated == 1)
    precondition(failures.count == 1 && failures[0] === queryFailure)
    precondition(OwnedCoreBluetoothProtocolRadioSupport.prePermissionSnapshot(authorization: "granted")["power"] as? String == "unknown")
    for lateReason in [queryFailure, NSError(domain: "UnifiedBleAccessoryStartup", code: 2)] {
      var answers = 0
      var lateFailures = [NSError]()
      var retired = 0
      let query = AppleAccessoryStartupAuthorization(
        completion: { _ in answers += 1 },
        sessionFailure: { lateFailures.append($0) },
        activated: {}, retire: { retired += 1 }
      )
      precondition(query.activate(authorized: true))
      precondition(!query.activate(authorized: false))
      precondition(query.fail(lateReason))
      precondition(!query.fail(queryFailure))
      precondition(answers == 1 && lateFailures.count == 1 && lateFailures[0] === lateReason)
      precondition(retired == 1 && !query.isActivated)
    }
    var pendingAnswers = [Result<Bool, NSError>]()
    var pendingRetirements = 0
    let pending = AppleAccessoryStartupAuthorization(
      completion: { pendingAnswers.append($0) },
      sessionFailure: { _ in preconditionFailure("timed-out query produced a late session answer") },
      activated: { preconditionFailure("late activation created a central") },
      retire: { pendingRetirements += 1 }
    )
    precondition(pending.fail(queryFailure))
    precondition(!pending.activate(authorized: true))
    precondition(!pending.fail(queryFailure))
    precondition(pendingAnswers.count == 1 && pendingRetirements == 1)
  }

  /// The exact helper used by production connect. Script OS lookup only;
  /// no central, scan, restoration event, or native connection is created.
  static func checkKnownPeerRetrieval() {
    final class Peripheral {
      let id: UUID
      init(_ id: UUID) { self.id = id }
    }
    let id = UUID(uuidString: "00112233-4455-6677-8899-AABBCCDDEEFF")!
    precondition(OwnedCoreBluetoothKnownPeerLookup.identifier(id.uuidString) == id)
    for invalid in [id.uuidString.lowercased(), "not-a-peer", id.uuidString + "\n", " " + id.uuidString, "00112233445566778899AABBCCDDEEFF"] {
      precondition(OwnedCoreBluetoothKnownPeerLookup.identifier(invalid) == nil,
        "only canonical UUID syntax is an identifier; do not normalize arbitrary strings")
    }
    let known = Peripheral(id)
    let other = Peripheral(UUID(uuidString: "00112233-4455-6677-8899-AABBCCDDEE00")!)
    var queried = [UUID]()
    var returned = [other, known]
    func resolve(_ cached: Peripheral?) -> Peripheral? {
      OwnedCoreBluetoothKnownPeerLookup.resolve(
        identifier: id, cached: cached, retrieve: { queried.append($0); return returned },
        identifierOf: { $0.id }
      )
    }
    precondition(resolve(nil) === known, "registry miss must retrieve the exact OS-known identifier")
    precondition(queried == [id], "retrieval asks for only the supplied identifier")
    precondition(resolve(known) === known && queried.count == 1,
      "an existing callback owner must not be replaced or retrieved again")
    returned = [other]
    precondition(resolve(nil) == nil && queried.count == 2, "a foreign returned identifier is not this peer")
    returned = []
    precondition(resolve(nil) == nil && queried.count == 3, "unknown identifier remains unknown")
    for cached in [false, true] {
      for connected in [false, true] {
        precondition(OwnedCoreBluetoothKnownPeerLookup.requiresConnection(
          hasCachedPeripheral: cached, isConnected: connected
        ) == (!cached || !connected),
          "an OS-retrieved object must acquire the local central connection even if already physically connected")
      }
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
    var accessorySetup = false
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
        },
        accessorySetupConfigured: { accessorySetup }
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
    prompter.authorizationChanged()
    precondition(answered.isEmpty, "an undecided delegate update must keep permission pending")
    precondition(!start(prompter), "an undecided update must preserve single-flight ownership")
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
    prompter.authorizationChanged()
    precondition(answered.isEmpty, "an undecided update must not cancel the original deadline")
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
    accessorySetup = true
    prompter = make()
    answered.removeAll()
    word = "notDetermined"
    let allocated = ensured
    let deadlines = scheduled.count
    precondition(start(prompter))
    precondition(answered.count == 1 && answered[0].1?.code == 1040, "ASK has no global permission prompt")
    precondition(answered[0].0 == nil, "ASK scope must not fabricate global permission results")
    precondition(ensured == allocated && scheduled.count == deadlines, "unsupported ASK prompt must not allocate or arm a five-minute waiter")
    for decided in ["granted", "denied", "restricted", "unavailable"] {
      answered.removeAll()
      word = decided
      precondition(start(prompter))
      precondition(answered.count == 1, "decided authorization semantics remain available on ASK hosts")
      precondition(answered[0].1?.code != 1040, "ASK refusal applies only to undecided global prompts")
    }
  }

  static func checkRadioPreparation() {
    let unauthorized = OwnedCoreBluetoothProtocolRadioSupport.operationReadinessFailure(state: .unauthorized)
    precondition(unauthorized?.domain == "CoreBluetooth.CBManagerState")
    precondition(unauthorized?.code == CBManagerState.unauthorized.rawValue)
    precondition(OwnedCoreBluetoothProtocolRadioSupport.operationReadinessFailure(state: .unknown) == nil)
    precondition(OwnedCoreBluetoothProtocolRadioSupport.operationReadinessFailure(state: .resetting) == nil)
    let preparation = AppleRadioPreparation()
    let pending: NSDictionary = ["availability": "available", "authorization": "notDetermined", "power": "unknown"]
    let ready: NSDictionary = ["availability": "available", "authorization": "notDetermined", "power": "on"]
    var answers = [String]()
    preparation.start("connect", snapshot: pending, waitForInitialState: true) { snapshot, error in
      precondition(error == nil)
      precondition(snapshot?["authorization"] as? String == "notDetermined")
      answers.append("connect")
    }
    precondition(answers.isEmpty, "fresh ASK central must not connect before its state callback")
    preparation.update(pending)
    precondition(answers.isEmpty)
    preparation.update(ready)
    precondition(answers == ["connect"])
    preparation.update(ready)
    precondition(answers.count == 1)
    let cancellation = NSError(domain: "test", code: 1020)
    preparation.start("cancelled", snapshot: pending, waitForInitialState: true) { _, error in
      precondition(error === cancellation)
      answers.append("cancelled")
    }
    preparation.cancel("cancelled", error: cancellation)
    preparation.update(ready)
    precondition(answers == ["connect", "cancelled"], "late poweredOn must not dispatch a cancelled operation")
    preparation.start("destroyed", snapshot: pending, waitForInitialState: true) { _, error in
      precondition(error === cancellation)
      answers.append("destroyed")
    }
    preparation.failAll(cancellation)
    preparation.update(ready)
    precondition(answers.count == 3)
    for authorization in ["denied", "restricted", "unavailable"] {
      preparation.start(authorization, snapshot: ["authorization": authorization, "power": "unknown"], waitForInitialState: true) { snapshot, error in
        precondition(error == nil && snapshot?["authorization"] as? String == authorization)
        answers.append(authorization)
      }
    }
    precondition(answers.count == 6, "negative authorization must not wait for power")
    preparation.start("legacy", snapshot: pending, waitForInitialState: false) { _, _ in answers.append("legacy") }
    precondition(answers.last == "legacy", "legacy permission admission must not be silently changed")
    preparation.start("scoped-refused", snapshot: pending, waitForInitialState: true) { snapshot, error in
      precondition(snapshot == nil && error === unauthorized)
      answers.append("scoped-refused")
    }
    preparation.failAll(unauthorized!)
    preparation.update(ready)
    precondition(answers.last == "scoped-refused")
    precondition(pending["authorization"] as? String == "notDetermined", "scoped refusal never rewrites global authorization")
  }
}
