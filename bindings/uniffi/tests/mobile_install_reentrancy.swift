import Foundation

final class Wake: MobileWakeSink {
    func wake(sessionId: UInt64) { fatalError("unexpected wake") }
}

final class Radio: MobilePlatformRadio {
    let valid: Bool
    init(valid: Bool) { self.valid = valid }

    func orderedAdmissionOwner(version: UInt32) -> String {
        precondition(version == 3 || version == 4)
        precondition(mobileHostCurrent() == nil)
        do {
            _ = try mobileHostInstall(radio: self, wake: Wake(), platform: "apple", owner: "nested", adapterLabel: "nested")
            fatalError("nested installation was accepted")
        } catch MobileCoreError.Failed(let code, _, _, _) {
            precondition(code == "lifecycle.invalid-state")
        } catch {
            fatalError("unexpected nested installation error: \(error)")
        }
        return valid && version == 3 ? "12345678-1234-4abc-8def-123456789abc" : ""
    }

    func submit(request: MobileRadioRequest) { fatalError("unexpected request") }
    func cancel(requestId: UInt64) { fatalError("unexpected cancel") }
}

@main
struct MobileInstallReentrancy {
    static func main() throws {
        do {
            _ = try mobileHostInstall(radio: Radio(valid: false), wake: Wake(), platform: "apple", owner: "test", adapterLabel: "test")
            fatalError("unsupported owner was accepted")
        } catch MobileCoreError.Failed(let code, _, _, _) {
            precondition(code == "protocol.incompatible")
        }
        precondition(mobileHostCurrent() == nil)
        let host = try mobileHostInstall(radio: Radio(valid: true), wake: Wake(), platform: "apple", owner: "test", adapterLabel: "test")
        precondition(mobileHostCurrent() != nil)
        precondition(host.shutdown().contains("\"state\":\"release-failed\""))
        precondition(mobileHostCurrent() != nil)
        print("generated Swift UniFFI install reentrancy and retention: OK")
    }
}
