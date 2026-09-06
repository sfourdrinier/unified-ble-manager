// native/protocol/tests/AppleCoreBluetoothReadNotifyProvenanceHarness.swift

import Foundation

@main
enum AppleCoreBluetoothReadNotifyProvenanceHarness {
  static func main() {
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false
      ),
      false,
      "an idle characteristic still admits an independent read"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: true,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false
      ),
      true,
      "isNotifying makes independent read ambiguous"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: false,
        hasInstalledSubscription: true,
        pendingNotifyEnable: false
      ),
      true,
      "multi-consumer notify ownership makes independent read ambiguous"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: true
      ),
      true,
      "a pending notify enable makes independent read ambiguous"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.independentReadIsAmbiguous(
        isNotifying: true,
        hasInstalledSubscription: true,
        pendingNotifyEnable: false
      ),
      true,
      "repeated independent reads stay ambiguous while notifying"
    )

    let notificationFirst = OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
      hasPendingRead: true,
      isNotifying: true,
      hasInstalledSubscription: true,
      pendingNotifyEnable: false,
      hasError: false,
      hasValue: true
    )
    expect(
      notificationFirst,
      .rejectPendingReadAndDeliverNotification,
      "notification arrives before the read response"
    )
    expect(
      notificationFirst != .completePendingRead,
      true,
      "the notification value must not complete the pending read"
    )

    let lateReadResponse = OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
      hasPendingRead: false,
      isNotifying: true,
      hasInstalledSubscription: true,
      pendingNotifyEnable: false,
      hasError: false,
      hasValue: true
    )
    expect(
      lateReadResponse,
      .deliverNotification,
      "a later fused callback stays on the notify path after the pending read is rejected"
    )
    expect(
      lateReadResponse != .completePendingRead,
      true,
      "the read-response value must not be attributed as a read after notify is active"
    )

    expect(
      OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
        hasPendingRead: true,
        isNotifying: false,
        hasInstalledSubscription: false,
        pendingNotifyEnable: false,
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
        hasError: false,
        hasValue: true
      ),
      .deliverNotification,
      "notifications continue to their installed subscription"
    )
    expect(
      OwnedCoreBluetoothReadNotifyProvenance.routeValueUpdate(
        hasPendingRead: false,
        isNotifying: true,
        hasInstalledSubscription: true,
        pendingNotifyEnable: false,
        hasError: false,
        hasValue: true
      ),
      .deliverNotification,
      "cancelled pending read cannot be completed by a later fused callback"
    )
  }

  static func expect<T: Equatable>(_ actual: T, _ expected: T, _ message: String) {
    guard actual == expected else {
      fputs("\(message): expected \(expected), got \(actual)\n", stderr)
      exit(1)
    }
  }
}
