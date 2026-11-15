// ios/__tests__/AppleCoreBluetoothReadNotifyProvenanceHarness.swift

import Foundation

/// CoreBluetooth reports a read response and a notification through one
/// `didUpdateValueFor` callback. A read on a notifying characteristic runs and
/// completes as `readOrNotification`; the value still reaches the subscription;
/// reads of one characteristic are serialized, and an ambiguous or abandoned
/// read's physical obligation never completes a later read.
@main
enum AppleCoreBluetoothReadNotifyProvenanceHarness {
  typealias Provenance = OwnedCoreBluetoothReadNotifyProvenance

  static func main() {
    provenanceChecks()
    notificationDeliveryChecks()
    laneOrderChecks()
    notificationBeforeReadReplyRace()
    abandonedReadChecks()
    notificationStateAnswerChecks()
    cancelledNotificationTransitionChecks()
    invalidatedNotificationCleanupChecks()
    serviceChangePendingEnableChecks()
    serviceChangeActiveSubscriptionChecks()
  }

  /// Runs the production delegate's state-answer handler, without allocating
  /// a central or touching a radio. A nil NSError is not an enabled CCCD.
  static func notificationStateAnswerChecks() {
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let address = CharacteristicAddress(
      peerIdentifier: "test-peer", serviceUUID: "180d", serviceOccurrence: 0,
      characteristicUUID: "2a37", characteristicOccurrence: 0
    )
    radio.queue.sync {
      var answers = [NSError?]()
      func install(_ enabled: Bool) {
        radio.pendingNotify[address] = PendingNotify(
          operationIdentifier: "notification-state-test", subscriptionIdentifier: "owned-consumer",
          enabled: enabled, completion: { answers.append($0) }
        )
      }
      install(true)
      radio.handleNotificationStateUpdate(address: address, isNotifying: false, error: nil)
      precondition(answers.count == 1 && answers[0] != nil, "nil error with notifications off must refuse enable")
      precondition(radio.subscriptions[address] == nil && radio.pendingNotify[address] == nil)
      install(true)
      radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nil)
      precondition(answers.count == 2 && answers[1] == nil)
      precondition(radio.subscriptions[address] == "owned-consumer", "confirmed enable installs its consumer")
      install(false)
      radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nil)
      precondition(answers.count == 3 && answers[2] != nil, "nil error with notifications on must refuse disable")
      precondition(radio.subscriptions[address] == "owned-consumer", "failed disable retains exact cleanup ownership")
      let nativeFailure = NSError(domain: "test-native", code: 77)
      install(false)
      radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nativeFailure)
      precondition(answers.count == 4 && answers[3] === nativeFailure, "the original platform refusal survives")
      precondition(radio.subscriptions[address] == "owned-consumer")
      install(false)
      radio.handleNotificationStateUpdate(address: address, isNotifying: false, error: nativeFailure)
      precondition(answers.count == 5 && answers[4] === nativeFailure)
      precondition(radio.subscriptions[address] == "owned-consumer",
        "an errored disable must retain its cleanup target even if notifications are off")
      install(false)
      radio.handleNotificationStateUpdate(address: address, isNotifying: false, error: nil)
      precondition(answers.count == 6 && answers[5] == nil)
      precondition(radio.subscriptions[address] == nil, "confirmed disable releases its consumer")
      install(true)
      radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nativeFailure)
      precondition(answers.count == 7 && answers[6] === nativeFailure)
      precondition(radio.subscriptions[address] == nil, "a platform enable failure is not ready")
      radio.pendingCancellationCleanup["cancelled-enable"] = PendingCancellationCleanup(
        notificationDesiredStates: [address: false], notificationAwaitingCallbacks: [address]
      )
      install(true)
      radio.handleNotificationStateUpdate(address: address, isNotifying: false, error: nil)
      precondition(answers.count == 8 && answers[7] != nil)
      precondition(radio.pendingCancellationCleanup["cancelled-enable"] == nil,
        "the same callback can refuse enable while authoritatively settling its cancelled-enable cleanup")
      precondition(radio.central == nil, "the fixture must not allocate a Bluetooth manager")
    }
  }

  static func cancelledNotificationTransitionChecks() {
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let address = CharacteristicAddress(
      peerIdentifier: "test-peer", serviceUUID: "180d", serviceOccurrence: 0,
      characteristicUUID: "2a37", characteristicOccurrence: 0
    )
    radio.queue.sync {
      var writes = [Bool]()
      radio.notificationStateWriter = { _, enabled in writes.append(enabled); return true }
      for initialEnable in [true, false] {
        let operation = initialEnable ? "cancel-enable" : "cancel-disable"
        if !initialEnable { radio.subscriptions[address] = "original" }
        radio.pendingNotify[address] = PendingNotify(
          operationIdentifier: operation, subscriptionIdentifier: "original", enabled: initialEnable,
          completion: { _ in preconditionFailure("cancelled operation must not complete") }
        )
        radio.cancelPendingOperation(operation)
        precondition(writes.isEmpty, "cancellation must wait for the original callback")
        precondition(!radio.pendingCancellationCleanupRetryScheduled.contains(operation),
          "waiting for a callback must not create a recurring retry timer")
        precondition(!radio.notificationTransitionAvailable(for: address), "cleanup blocks a replacement claim")
        precondition(radio.pendingCancellationCleanup[operation]?.notificationAwaitingCallbacks.contains(address) == true)
        radio.handleNotificationStateUpdate(address: address, isNotifying: initialEnable, error: nil)
        precondition(writes == [!initialEnable], "compensation follows the original callback")
        precondition(radio.pendingCancellationCleanup[operation]?.notificationAwaitingCallbacks.contains(address) == true)
        // An errored compensation cannot release the owner, even when the state appears correct.
        radio.handleNotificationStateUpdate(address: address, isNotifying: !initialEnable,
          error: NSError(domain: "test", code: 1))
        precondition(writes == [!initialEnable, !initialEnable])
        precondition(!radio.notificationTransitionAvailable(for: address))
        radio.handleNotificationStateUpdate(address: address, isNotifying: !initialEnable, error: nil)
        precondition(radio.pendingCancellationCleanup[operation] == nil)
        precondition(radio.notificationTransitionAvailable(for: address))
        if !initialEnable { precondition(radio.subscriptions[address] == "original") }
        writes.removeAll()
      }
      radio.notificationStateWriter = nil
    }
  }

  static func invalidatedNotificationCleanupChecks() {
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let address = CharacteristicAddress(
      peerIdentifier: "peer", serviceUUID: "180d", serviceOccurrence: 0,
      characteristicUUID: "2a37", characteristicOccurrence: 0
    )
    radio.queue.sync {
      var writes = [Bool]()
      radio.notificationStateWriter = { _, enabled in writes.append(enabled); return true }
      radio.pendingNotify[address] = PendingNotify(
        operationIdentifier: "cancelled", subscriptionIdentifier: "owner", enabled: true,
        completion: { _ in preconditionFailure("cancelled operation must not complete") }
      )
      radio.cancelPendingOperation("cancelled")
      radio.clearNotificationOwnership(forPeerIdentifier: "peer")
      precondition(writes.isEmpty, "service change must not race the original transition")
      precondition(!radio.notificationTransitionAvailable(for: address))
      let rediscoveredAddress = CharacteristicAddress(
        peerIdentifier: "peer", serviceUUID: "180d", serviceOccurrence: 1,
        characteristicUUID: "2a37", characteristicOccurrence: 0
      )
      precondition(!radio.notificationTransitionAvailable(for: rediscoveredAddress),
        "rediscovery may assign the same characteristic a different occurrence")
      let result = radio.retryCancellationCleanupOnQueue("cancelled")
      precondition(result["state"] as? String == "retryable")
      precondition(writes.isEmpty, "an invalidated path must not target a rediscovered characteristic")
      radio.handleNotificationStateUpdate(address: address, isNotifying: false, error: nil)
      precondition(radio.pendingCancellationCleanup["cancelled"] != nil,
        "a callback resolved through a replacement path cannot settle the old owner")
      radio.pendingCancellationCleanup["cancelled"]?.discoveryPeers.insert("peer")
      radio.clearCancellationCleanup(forPeerIdentifier: "peer")
      precondition(radio.pendingCancellationCleanup["cancelled"]?.discoveryPeers.contains("peer") == true,
        "disconnect must not discard outstanding discovery cleanup")
      radio.clearDiscoveryCancellationCleanup(forPeerIdentifier: "peer")
      precondition(radio.pendingCancellationCleanup["cancelled"] == nil)
      precondition(radio.notificationTransitionAvailable(for: address))
      radio.notificationStateWriter = nil
    }
  }

  static func serviceChangePendingEnableChecks() {
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let address = CharacteristicAddress(
      peerIdentifier: "peer", serviceUUID: "180d", serviceOccurrence: 0,
      characteristicUUID: "2a37", characteristicOccurrence: 0
    )
    radio.queue.sync {
      var writes = [Bool]()
      var answers = [NSError?]()
      radio.notificationStateWriter = { _, enabled in writes.append(enabled); return true }
      radio.pendingNotify[address] = PendingNotify(
        operationIdentifier: "pending-enable", subscriptionIdentifier: "owner", enabled: true,
        completion: { answers.append($0) }
      )
      radio.clearNotificationOwnership(forPeerIdentifier: "peer")
      precondition(answers.count == 1 && answers[0]?.code == 1027)
      precondition(writes.isEmpty, "service change must not overlap the pending enable")
      precondition(radio.pendingNotify[address] == nil)
      // Cleanup is keyed by the physical transition identity, not the
      // caller-provided operation ID.  The latter can be reused while a late
      // CoreBluetooth callback still belongs to this retired transition.
      precondition(radio.pendingCancellationCleanup.values.contains {
        $0.notificationAwaitingCallbacks.contains(address)
      })
      let replacement = CharacteristicAddress(
        peerIdentifier: "peer", serviceUUID: "180d", serviceOccurrence: 1,
        characteristicUUID: "2a37", characteristicOccurrence: 0
      )
      precondition(!radio.notificationTransitionAvailable(for: replacement))
      radio.handleNotificationStateUpdate(address: address, isNotifying: true, error: nil)
      let cleanupID = radio.pendingCancellationCleanup.first(where: {
        $0.value.notificationAwaitingCallbacks.contains(address) || $0.value.notificationDesiredStates[address] != nil
      })?.key
      precondition(cleanupID != nil && writes.isEmpty,
        "a late enable must not release invalidated ownership")
      precondition(radio.retryCancellationCleanupOnQueue(cleanupID!)["state"] as? String == "retryable")
      radio.clearCancellationCleanup(forPeerIdentifier: "peer")
      precondition(radio.notificationTransitionAvailable(for: replacement))
      radio.notificationStateWriter = nil
    }
  }

  static func serviceChangeActiveSubscriptionChecks() {
    let radio = OwnedCoreBluetoothProtocolRadio(restoreIdentifierKey: nil)
    let address = CharacteristicAddress(
      peerIdentifier: "peer", serviceUUID: "180d", serviceOccurrence: 0,
      characteristicUUID: "2a37", characteristicOccurrence: 0
    )
    let replacement = CharacteristicAddress(
      peerIdentifier: "peer", serviceUUID: "180d", serviceOccurrence: 1,
      characteristicUUID: "2a37", characteristicOccurrence: 0
    )
    radio.queue.sync {
      var writes = [Bool]()
      radio.notificationStateWriter = { _, enabled in writes.append(enabled); return true }
      radio.subscriptions[address] = "owner"
      radio.clearNotificationOwnership(forPeerIdentifier: "peer")
      precondition(writes.isEmpty, "invalidated service must not receive a stale disable")
      precondition(radio.subscriptions[address] == nil)
      precondition(radio.hasPendingCancellationCleanup(for: address))
      precondition(!radio.notificationTransitionAvailable(for: replacement))
      radio.handleNotificationStateUpdate(address: replacement, isNotifying: false, error: nil)
      precondition(!radio.notificationTransitionAvailable(for: replacement),
        "replacement callback cannot settle the old physical owner")
      radio.clearCancellationCleanup(forPeerIdentifier: "peer")
      precondition(radio.notificationTransitionAvailable(for: replacement))
      radio.notificationStateWriter = nil
    }
  }

  static func provenanceChecks() {
    expect(
      Provenance.readProvenance(
        isNotifying: false, hasInstalledSubscription: false, pendingNotifyChange: false, pendingCancellationCleanup: false
      ),
      .readResponse,
      "a characteristic that cannot notify answers with the read response"
    )
    for (isNotifying, subscription, change, cleanup, reason) in [
      (true, false, false, false, "isNotifying"),
      (false, true, false, false, "an installed subscription"),
      (false, false, true, false, "a notification state change in flight"),
      (false, false, false, true, "a cancelled notification change being undone"),
      (true, true, false, false, "the Polar PMD control point: subscribed, then read"),
    ] {
      expect(
        Provenance.readProvenance(
          isNotifying: isNotifying,
          hasInstalledSubscription: subscription,
          pendingNotifyChange: change,
          pendingCancellationCleanup: cleanup
        ),
        .readOrNotification,
        "\(reason) makes a read value possibly a notification"
      )
    }
    expect(OwnedCoreBluetoothReadProvenance.readResponse.wire, "read-response", "shared vocabulary word")
    expect(OwnedCoreBluetoothReadProvenance.readOrNotification.wire, "read-or-notification", "shared vocabulary word")
  }

  static func notificationDeliveryChecks() {
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: true, pendingNotifyEnable: false, pendingCancellationCleanup: false,
        hasError: false, hasValue: true
      ),
      true,
      "a value that completes a read on a subscribed characteristic still reaches the subscriber"
    )
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: false, pendingNotifyEnable: true, pendingCancellationCleanup: false,
        hasError: false, hasValue: true
      ),
      true,
      "a value that lands while the enable is in flight reaches the subscriber being installed"
    )
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: false, pendingNotifyEnable: false, pendingCancellationCleanup: false,
        hasError: false, hasValue: true
      ),
      false,
      "a read response on an unsubscribed characteristic is not a notification"
    )
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: true, pendingNotifyEnable: false, pendingCancellationCleanup: false,
        hasError: true, hasValue: true
      ),
      false,
      "an error answers the read, never the stream"
    )
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: true, pendingNotifyEnable: false, pendingCancellationCleanup: true,
        hasError: false, hasValue: true
      ),
      true,
      "an installed subscription keeps receiving values during compensation"
    )
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: false, pendingNotifyEnable: true, pendingCancellationCleanup: true,
        hasError: false, hasValue: true
      ),
      false,
      "a cancelled enable without an installed owner must not deliver"
    )
  }

  static func laneOrderChecks() {
    var lane = OwnedCoreBluetoothReadLane<String>()
    expect(lane.admit("a"), true, "the first read issues readValue")
    expect(lane.admit("b"), false, "a second read waits for the first one's update")
    expect(lane.admit("c"), false, "a third read waits too")
    var answer = lane.answer()
    expect(answer.completed, "a", "the first update completes the first read")
    expect(answer.issueNext, true, "and issues the next readValue")
    answer = lane.answer()
    expect(answer.completed, "b", "reads complete in request order")
    answer = lane.answer()
    expect(answer.completed, "c", "reads complete in request order")
    expect(answer.issueNext, false, "nothing left to issue")
    expect(lane.isIdle, true, "the lane drains")
    answer = lane.answer()
    expect(answer.completed, nil, "an update with no read owed completes nothing")
  }

  /// The Polar H10 ECG race: the PMD control point is subscribed, a read is
  /// issued, and a notification lands just before the read reply. The first
  /// update completes the read as `readOrNotification` and still reaches the
  /// subscriber; the physical read remains owed because the next value could
  /// still be its reply.
  static func notificationBeforeReadReplyRace() {
    var lane = OwnedCoreBluetoothReadLane<String>()
    expect(lane.admit("read"), true, "the read is issued while notifying")
    let notificationFirst = lane.answerAmbiguously()
    expect(notificationFirst, "read", "the notification that arrived first completes the read")
    expect(lane.updateOwed, true, "an ambiguous value does not discharge the physical read")
    expect(lane.admit("later"), false, "a later physical read cannot be issued")
    expect(
      Provenance.readProvenance(
        isNotifying: true, hasInstalledSubscription: true, pendingNotifyChange: false, pendingCancellationCleanup: false
      ),
      .readOrNotification,
      "and the read reports that the value may be a notification"
    )
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: true, pendingNotifyEnable: false, pendingCancellationCleanup: false,
        hasError: false, hasValue: true
      ),
      true,
      "the notification value is not withheld from the subscriber"
    )
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: true, pendingNotifyEnable: false, pendingCancellationCleanup: false,
        hasError: false, hasValue: true
      ),
      true,
      "a subsequent value is delivered to the subscriber, never dropped"
    )
  }

  static func abandonedReadChecks() {
    var lane = OwnedCoreBluetoothReadLane<String>()
    _ = lane.admit("timed-out")
    _ = lane.admit("next")
    _ = lane.admit("cancelled-while-queued")
    lane.cancel { $0 == "timed-out" || $0 == "cancelled-while-queued" }
    expect(lane.isIdle, false, "the abandoned read's update is still owed")
    let abandoned = lane.answer()
    expect(abandoned.completed, nil, "the abandoned read's update completes no later read")
    expect(abandoned.issueNext, true, "the next read is issued only after it")
    let next = lane.answer()
    expect(next.completed, "next", "the next read receives its own update")
    expect(next.issueNext, false, "the cancelled queued read was never issued")
    expect(lane.isIdle, true, "the lane drains")

    var failing = OwnedCoreBluetoothReadLane<String>()
    _ = failing.admit("first")
    _ = failing.admit("second")
    expect(failing.waiting, ["first", "second"], "a disconnect answers every waiting read in order")
  }

  static func expect<T: Equatable>(_ actual: T, _ expected: T, _ message: String) {
    guard actual == expected else {
      fputs("\(message): expected \(expected), got \(actual)\n", stderr)
      exit(1)
    }
  }
}
