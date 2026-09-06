// native/protocol/tests/AppleCoreBluetoothReadNotifyProvenanceHarness.swift

import Foundation

@main
enum AppleCoreBluetoothReadNotifyProvenanceHarness {
  static func main() {
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: false
      ),
      false,
      "an idle characteristic still admits an independent read"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: true,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: false
      ),
      true,
      "isNotifying makes independent read ambiguous"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: false,
        hasInstalledSubscription: true,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: false
      ),
      true,
      "multi-consumer notify ownership makes independent read ambiguous"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: true,
        pendingCancellationCleanup: false
      ),
      true,
      "a pending notify enable makes independent read ambiguous"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: true
      ),
      true,
      "pendingCancellationCleanup makes independent read ambiguous"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: true,
        hasInstalledSubscription: true,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: false
      ),
      true,
      "repeated independent reads stay ambiguous while notifying"
    )

    expect(
      OwnedCoreBluetoothReadNotifyProvenance.admitSubscribe(hasPendingRead: true, hasPendingNotify: false),
      .rejectPendingRead,
      "read-then-subscribe is serialized behind the in-flight read"
    )

    let readThenSubscribeRace = OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
      hasPendingRead: true,
      isNotifying: false,
      hasInstalledSubscription: false,
      pendingNotifyEnable: true,
      pendingCancellationCleanup: false,
      hasError: false,
      hasValue: true
    )
    expect(
      readThenSubscribeRace,
      .completePendingRead,
      "read-then-subscribe: in-flight read response must complete the read, not become a notification"
    )

    let leftoverWhileNotifying = OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
      hasPendingRead: true,
      isNotifying: true,
      hasInstalledSubscription: true,
      pendingNotifyEnable: false,
      pendingCancellationCleanup: false,
      hasError: false,
      hasValue: true
    )
    expect(
      leftoverWhileNotifying,
      .rejectPendingRead,
      "notification arrives before the read response"
    )
    expect(
      leftoverWhileNotifying != .completePendingRead,
      true,
      "the notification value must not complete the pending read"
    )
    expect(
      leftoverWhileNotifying != .deliverNotification,
      true,
      "do not deliver a pending-read callback as a notification"
    )

    let lateReadResponse = OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
      hasPendingRead: false,
      isNotifying: false,
      hasInstalledSubscription: false,
      pendingNotifyEnable: false,
      pendingCancellationCleanup: true,
      hasError: false,
      hasValue: true
    )
    expect(
      lateReadResponse,
      .ignore,
      "cancel-then-read drops fused notify-path callbacks that cannot be attributed"
    )
    expect(
      lateReadResponse != .completePendingRead,
      true,
      "the read-response value must not be attributed as a read after notify is cancelled"
    )

    expect(
      OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
        hasPendingRead: true,
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: false,
        hasError: false,
        hasValue: true
      ),
      .completePendingRead,
      "an independent read still completes when the characteristic is not notifying"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
        hasPendingRead: false,
        isNotifying: true,
        hasInstalledSubscription: true,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: false,
        hasError: false,
        hasValue: true
      ),
      .deliverNotification,
      "notifications continue to their installed subscription"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
        hasPendingRead: true,
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
        pendingCancellationCleanup: true,
        hasError: false,
        hasValue: true
      ),
      .rejectPendingRead,
      "cancelled pending read cannot be completed by a later fused callback"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.occurrenceValueUpdateShouldReturn(
        occurrenceAmbiguous: true,
        occurrenceStatePresent: false
      ),
      true,
      "occurrence-ambiguous updates never fall through to UUID maps"
    )
  }

  static func expect<T: Equatable>(_ actual: T, _ expected: T, _ message: String) {
    guard actual == expected else {
      fputs("\(message): expected \(expected), got \(actual)\n", stderr)
      exit(1)
    }
  }
}
