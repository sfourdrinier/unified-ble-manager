import Foundation

final class Wake: MobileWakeSink {
    func wake(sessionId: UInt64) { fatalError("unexpected wake") }
}

final class Radio: MobilePlatformRadio {
    func orderedAdmissionOwner(version: UInt32) -> String {
        precondition(version == 3 || version == 4)
        return version == 3 ? "12345678-1234-4abc-8def-123456789abc" : ""
    }
    func submit(request: MobileRadioRequest) { fatalError("unexpected request") }
    func cancel(requestId: UInt64) { fatalError("unexpected cancellation") }
}

final class RefusingBinding: MobileHostBinding, @unchecked Sendable {
    var host: MobileCoreHost?
    func bind(host: MobileCoreHost) -> Bool {
        precondition(mobileHostCurrent() == nil, "unbound host was published")
        precondition(host.shutdown().contains("binding in progress"))
        do {
            _ = try host.openSession(owner: "early", expectedWireRevision: mobileWireRevision())
            fatalError("unpublished host admitted a session")
        } catch MobileCoreError.Failed(let code, _, _, _) {
            precondition(code == "lifecycle.invalid-state")
        } catch {
            fatalError("unexpected admission failure: \(error)")
        }
        self.host = host
        return false
    }
}

@main
struct MobileBoundInstall {
    static func main() throws {
        let binding = RefusingBinding()
        do {
            _ = try mobileHostInstallBound(radio: Radio(), wake: Wake(), binding: binding,
                platform: "apple", owner: "test", adapterLabel: "test")
            fatalError("refused binding was accepted")
        } catch MobileCoreError.Failed(let code, _, _, _) {
            precondition(code == "lifecycle.invalid-state")
        }
        precondition(binding.host != nil && mobileHostCurrent() == nil)
        for _ in 0..<2 {
            precondition(mobileHostRetryPendingCleanup().contains("\"state\":\"release-failed\""))
            precondition(mobileHostHasPendingCleanup() && mobileHostCurrent() == nil)
            precondition(binding.host!.complete(requestId: 987654321, completion: .unit) == "late")
        }
        do {
            _ = try mobileHostInstallBound(radio: Radio(), wake: Wake(), binding: RefusingBinding(),
                platform: "apple", owner: "replacement", adapterLabel: "test")
            fatalError("replacement accepted during pending cleanup")
        } catch MobileCoreError.Failed(let code, _, _, _) {
            precondition(code == "lifecycle.invalid-state")
        }
        print("generated Swift UniFFI refused binding and cleanup retention: OK")
    }
}
