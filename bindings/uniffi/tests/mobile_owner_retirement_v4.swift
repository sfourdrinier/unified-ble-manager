import Foundation

private let owner = "12345678-1234-4abc-8def-123456789abc"

private final class Radio: MobilePlatformRadio, @unchecked Sendable {
    private let condition = NSCondition()
    private var queue: [MobileRadioRequest] = []
    private(set) var versions: [UInt32] = []

    func orderedAdmissionOwner(version: UInt32) -> String {
        versions.append(version)
        return version == 3 || version == 4 ? owner : ""
    }
    func submit(request: MobileRadioRequest) {
        condition.lock()
        queue.append(request)
        condition.signal()
        condition.unlock()
    }
    func cancel(requestId: UInt64) { fatalError("unexpected cancellation: \(requestId)") }
    func next() -> MobileRadioRequest {
        condition.lock()
        defer { condition.unlock() }
        while queue.isEmpty {
            precondition(condition.wait(until: Date().addingTimeInterval(8)), "native callback did not arrive")
        }
        return queue.removeFirst()
    }
}

private final class Wake: MobileWakeSink {
    func wake(sessionId: UInt64) { fatalError("unexpected wake") }
}

private final class ShutdownResult: @unchecked Sendable {
    let done = DispatchSemaphore(value: 0)
    private var value: String?
    func finish(_ receipt: String) { value = receipt; done.signal() }
    func wait() -> String {
        precondition(done.wait(timeout: .now() + 8) == .success, "shutdown did not finish")
        return value!
    }
}

@main
private struct MobileOwnerRetirement {
    static func main() throws {
        let radio = Radio()
        let host = try mobileHostInstall(radio: radio, wake: Wake(), platform: "apple", owner: "v4-swift", adapterLabel: "synthetic")
        precondition(radio.versions == [3, 4], "retirement must be negotiated separately")
        let result = ShutdownResult()
        DispatchQueue.global().async { result.finish(host.shutdown()) }
        while true {
            switch radio.next() {
            case let .close(id):
                precondition(host.complete(requestId: id, completion: .closed(failures: [])) == "delivered")
            case let .ownerFence(id, ownerIncarnation, retirementId):
                precondition(ownerIncarnation == owner)
                let completion = MobileRadioCompletion.ownerRetirement(ownerIncarnation: ownerIncarnation, retirementId: retirementId, state: .quiescent)
                precondition(host.complete(requestId: id, completion: completion) == "delivered")
                precondition(result.wait().contains("\"state\":\"released\""))
                print("generated Swift UniFFI host independently negotiated and completed owner retirement: OK")
                return
            default:
                fatalError("unexpected native request")
            }
        }
    }
}
