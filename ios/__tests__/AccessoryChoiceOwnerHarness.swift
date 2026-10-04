import Foundation

@main struct AccessoryChoiceOwnerHarness {
  static func main() {
    precondition(AccessoryChoiceAdmission.validFilter(["serviceUuid": "180D", "namePrefix": "SIM", "companyIdentifier": 107, "manufacturerPrefix": [0, 255]]))
    for filter: [String: Any] in [["serviceUuid": false], ["companyIdentifier": true], ["companyIdentifier": 1.5], ["namePrefix": "SIM", "companyIdentifier": 107, "manufacturerPrefix": [true]], ["manufacturerPrefix": [1]], ["unknown": 1]] {
      precondition(!AccessoryChoiceAdmission.validFilter(filter))
    }
    precondition(AccessoryChoiceAdmission.serviceDeclared("0000180d-0000-1000-8000-00805f9b34fb", allowed: ["180D"]))
    precondition(AccessoryChoiceAdmission.serviceDeclared("180D", allowed: ["0000180d-0000-1000-8000-00805f9b34fb"]))
    precondition(!AccessoryChoiceAdmission.serviceDeclared("180F", allowed: ["180D"]))
    precondition(!AccessoryChoiceAdmission.serviceDeclared("not-a-uuid", allowed: ["180D"]))
    precondition(!AccessoryChoiceAdmission.serviceDeclared("180D", allowed: ["not-a-uuid"]))
    // Execute the same UUID builder assigned to ASDiscoveryDescriptor. Semantic
    // equality alone is insufficient: ASK checks the declared representation.
    let full = "0000180D-0000-1000-8000-00805F9B34FB"
    precondition(AccessoryChoiceAdmission.serviceUuidForDescriptor(full, allowed: ["180D"])?.uuidString == "180D")
    precondition(AccessoryChoiceAdmission.serviceUuidForDescriptor("180D", allowed: [full])?.uuidString == full)
    precondition(AccessoryChoiceAdmission.serviceUuidForDescriptor(full, allowed: ["180F", "180D", full])?.uuidString == "180D")
    precondition(AccessoryChoiceAdmission.serviceUuidForDescriptor("180D", allowed: [full, "180D"])?.uuidString == full)
    precondition(AccessoryChoiceAdmission.serviceUuidForDescriptor("180F", allowed: ["180D"]) == nil)
    precondition(AccessoryChoiceAdmission.serviceUuidForDescriptor("180D", allowed: ["invalid"]) == nil)
    let owner = AccessoryChoiceOwner()
    var results: [String] = []
    precondition(owner.begin("one") { result, failure in results.append(result ?? failure ?? "empty") } == nil)
    precondition(owner.begin("two") { _, _ in preconditionFailure("busy request acquired ownership") } == "chooser.busy")
    precondition(!owner.finish("two", result: "foreign", failure: nil))
    precondition(results.isEmpty)
    precondition(owner.finish("one", result: nil, failure: "operation.timed-out"))
    precondition(!owner.finish("one", result: "late-authorization", failure: nil))
    precondition(results == ["operation.timed-out"])
    for _ in 0..<100 { precondition(owner.cancelBeforeAdmission("one")) }
    precondition(owner.cancelBeforeAdmission("before"))
    precondition(owner.begin("before") { _, _ in preconditionFailure("cancelled request started") } == "operation.aborted")
    precondition(owner.begin("three") { result, _ in results.append(result ?? "missing") } == nil)
    precondition(owner.finish("three", result: "authorized", failure: nil))
    precondition(results == ["operation.timed-out", "authorized"])
    for i in 0..<64 { precondition(owner.cancelBeforeAdmission("pending-\(i)")) }
    precondition(!owner.cancelBeforeAdmission("over-capacity"))
    print("AccessoryChoiceOwner: busy, identity, timeout/late, cancellation-before-dispatch, retired cancellation, bounded tombstone checks passed")
  }
}
