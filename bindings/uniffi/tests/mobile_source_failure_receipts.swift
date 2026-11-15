import Foundation

private let instance = MobileInstance(peerId: "old", serviceUuid: "180d", serviceOccurrence: 1,
    characteristicUuid: "2a37", characteristicOccurrence: 2)
private let failure = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: "CB",
    nativeCode: 42, detail: "original", dispatched: true)

private final class Radio: MobilePlatformRadio {
    func orderedAdmissionOwner(version: UInt32) -> String { version == 3 ? "12345678-1234-4abc-8def-123456789abc" : "" }
    func submit(request: MobileRadioRequest) { fatalError("unexpected request") }
    func cancel(requestId: UInt64) { fatalError("unexpected cancellation") }
}
private final class Wake: MobileWakeSink {
    func wake(sessionId: UInt64) {}
}
private final class Binding: MobileHostBinding {
    func bind(host: MobileCoreHost) -> Bool {
        precondition(host.notificationFailureIngressVersion(offered: 1) == 0)
        precondition(host.notificationFailureIngressVersion(offered: 2) == 0)
        precondition(host.ingestNotificationFailureV1(sequence: 1, instance: instance, epoch: 7, failure: failure) == "closed")
        return true
    }
}

@main
private struct SourceFailureReceipts {
    static func main() throws {
        let host = try mobileHostInstallBound(radio: Radio(), wake: Wake(), binding: Binding(),
            platform: "apple", owner: "receipts", adapterLabel: "synthetic")
        precondition(host.ingestNotificationFailureV1(sequence: 1, instance: instance, epoch: 7, failure: failure) == "closed")
        precondition(host.ingestNotificationFailureV1(sequence: 1, instance: instance, epoch: 7, failure: failure) == "closed")
        let changed = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: "CB",
            nativeCode: 43, detail: "changed", dispatched: true)
        precondition(host.ingestNotificationFailureV1(sequence: 1, instance: instance, epoch: 7, failure: changed) == "invalid")
        print("generated Swift UniFFI unadvertised and closed admission: OK (synthetic host)")
    }
}
