import Foundation

private final class Radio: MobilePlatformRadio {
    func orderedAdmissionOwner(version: UInt32) -> String {
        version == 3 ? "12345678-1234-4abc-8def-123456789abc" : ""
    }
    func submit(request: MobileRadioRequest) { fatalError("unexpected radio request: \(request)") }
    func cancel(requestId: UInt64) { fatalError("unexpected cancellation") }
}

private final class Wake: MobileWakeSink { func wake(sessionId: UInt64) {} }

private func object(_ response: String) -> [String: Any] {
    try! JSONSerialization.jsonObject(with: Data(response.utf8)) as! [String: Any]
}

private func value(_ response: String) -> [String: Any] {
    let outer = object(response)
    precondition(outer["ok"] as? Bool == true, "expected success: \(response)")
    return outer["value"] as! [String: Any]
}

private func rejected(_ response: String) {
    let outer = object(response)
    precondition(outer["ok"] as? Bool == false, "expected rejection: \(response)")
    precondition((outer["error"] as? [String: Any])?["code"] as? String == "lifecycle.invalid-state", "wrong rejection: \(response)")
}

@main
private struct TerminalEvidenceRecovery {
    static func main() throws {
        let directory = CommandLine.arguments[1]
        _ = value(mobileRecordingConfigureDirectory(path: directory))
        try mobileConfigureTerminalEvidenceMaximumBytes(maximumBytes: 65_536)
        let host = try mobileHostInstall(radio: Radio(), wake: Wake(), platform: "apple", owner: "swift-terminal-recovery", adapterLabel: "synthetic")

        let firstLease = value(host.terminalEvidenceRecoveryOpenSelected())["leaseId"] as! String
        rejected(host.terminalEvidenceRecoveryList(leaseId: "forged-lease", maxItems: 1, maxBytes: 65_536))
        let first = value(host.terminalEvidenceRecoveryList(leaseId: firstLease, maxItems: 1, maxBytes: 65_536))
        let firstItems = first["items"] as! [[String: Any]]
        precondition(firstItems.count == 1 && first["more"] as? Bool == true, "max pagination must report more: \(first)")

        let secondLease = value(host.terminalEvidenceRecoveryOpenSelected())["leaseId"] as! String
        rejected(host.terminalEvidenceRecoveryList(leaseId: firstLease, maxItems: 1, maxBytes: 65_536))
        let listed = value(host.terminalEvidenceRecoveryList(leaseId: secondLease, maxItems: 8, maxBytes: 65_536))
        let items = listed["items"] as! [[String: Any]]
        precondition(items.count == 2 && listed["more"] as? Bool == false, "both retained rows must list: \(listed)")
        precondition(Set(items.compactMap { $0["state"] as? String }) == Set(["staged", "committed"]), "staged and committed response schemas must survive Swift: \(items)")
        let committed = items.first { $0["state"] as? String == "committed" }!
        precondition(committed["reference"] is String && committed["byteLength"] is String && committed["digest"] is String && committed["formatVersion"] as? Int == 1, "committed schema lost fields: \(committed)")
        let staged = items.first { $0["state"] as? String == "staged" }!
        precondition(staged["reference"] == nil && staged["byteLength"] == nil && staged["digest"] == nil && staged["formatVersion"] == nil, "staged schema must not invent commit fields: \(staged)")
        let status = value(host.terminalEvidenceRecoveryListWithStatus(leaseId: secondLease, maxItems: 8, maxBytes: 65_536))
        precondition((status["busyProducers"] as? [String]) == [], "status listing must report no competing owner: \(status)")

        let handle = committed["handle"] as! String
        let page = value(host.terminalEvidenceRecoveryRead(handle: handle, offset: "0", maxBytes: 4))
        precondition(page["more"] as? Bool == true && page["bytesBase64"] as? String == "Y29tbQ==", "bounded read must paginate exact bytes: \(page)")
        precondition(value(host.terminalEvidenceRecoveryAcknowledge(handle: handle)).isEmpty)
        precondition(value(host.terminalEvidenceRecoveryAcknowledge(handle: handle)).isEmpty, "lost ACK reply must retry idempotently")
        let remaining = value(host.terminalEvidenceRecoveryList(leaseId: secondLease, maxItems: 8, maxBytes: 65_536))["items"] as! [[String: Any]]
        precondition(remaining.count == 1 && remaining[0]["state"] as? String == "staged", "ACK must retire only the acknowledged opaque handle")
        _ = value(host.terminalEvidenceRecoveryOpenSelected())
        rejected(host.terminalEvidenceRecoveryAcknowledge(handle: handle))
        print("generated Swift UniFFI terminal-evidence recovery: staged+committed, pagination, forged/stale lease, lost ACK retry: OK")
    }
}
