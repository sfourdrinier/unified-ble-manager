// ios/__tests__/AppleCoreBluetoothReadNotifyProvenanceHarness.swift

import Foundation

/// CoreBluetooth reports a read response and a notification through one
/// `didUpdateValueFor` callback. A read on a notifying characteristic runs and
/// completes as `readOrNotification`; the value still reaches the subscription;
/// reads of one characteristic complete in request order, one `readValue` at a
/// time, and an abandoned read's update never completes a later read.
@main
enum AppleCoreBluetoothReadNotifyProvenanceHarness {
  typealias Provenance = OwnedCoreBluetoothReadNotifyProvenance

  static func main() {
    provenanceChecks()
    notificationDeliveryChecks()
    laneOrderChecks()
    notificationBeforeReadReplyRace()
    abandonedReadChecks()
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
      false,
      "a subscription whose cancellation is being undone receives nothing"
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
  /// subscriber; the reply that follows is a plain notification delivery.
  static func notificationBeforeReadReplyRace() {
    var lane = OwnedCoreBluetoothReadLane<String>()
    expect(lane.admit("read"), true, "the read is issued while notifying")
    let notificationFirst = lane.answer()
    expect(notificationFirst.completed, "read", "the notification that arrived first completes the read")
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
    let replyAfter = lane.answer()
    expect(replyAfter.completed, nil, "the late read reply completes no read")
    expect(
      Provenance.deliversNotification(
        hasInstalledSubscription: true, pendingNotifyEnable: false, pendingCancellationCleanup: false,
        hasError: false, hasValue: true
      ),
      true,
      "the late read reply is delivered to the subscriber, never dropped"
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
