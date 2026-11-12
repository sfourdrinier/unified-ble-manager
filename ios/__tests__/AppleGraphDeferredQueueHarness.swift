// ios/__tests__/AppleGraphDeferredQueueHarness.swift
import Foundation

@main
struct AppleGraphDeferredQueueHarness {
  static func main() {
    var events: [String] = []
    let q = OwnedGraphDeferredQueue(maxItems: 2, maxBytes: 4, maxBarriers: 1, onBarrierOverflow: { events.append("barrier overflow") })
    let read = q.enqueueRequest(bytes: 2, run: { events.append("read") }, onReject: { events.append("read rejected") })!
    _ = q.enqueueRequest(bytes: 2, run: { events.append("write") }, onReject: { events.append("write rejected") })
    precondition(q.enqueueRequest(bytes: 1, run: { events.append("discovery") }, onReject: { events.append("discovery rejected") }) == nil)
    precondition(q.enqueueValue(bytes: 1, run: {}, onLoss: { events.append("value loss") }) == nil)
    precondition(q.enqueueControl(bytes: 1, run: {}, onLoss: { events.append("control loss") }) == nil)
    q.cancel(read)
    q.cancel(read)
    precondition(events == ["discovery rejected", "read rejected"])
    var resumeA: (() -> Void)?
    precondition(q.enqueueBarrier { resume in events.append("barrier A"); resumeA = resume })
    precondition(!q.enqueueBarrier { _ in preconditionFailure("duplicate barrier admitted") })
    q.drain()
    precondition(q.isPaused && events.suffix(3) == ["write", "value loss", "barrier A"])
    _ = q.enqueueRequest(bytes: 2, run: { events.append("after reset") }, onReject: { events.append("after rejected") })
    let other = OwnedGraphDeferredQueue(maxItems: 1, maxBytes: 1)
    var resumeB: (() -> Void)?
    precondition(other.enqueueBarrier { resume in events.append("barrier B"); resumeB = resume })
    other.drain()
    precondition(other.isPaused && q.isPaused)
    resumeA?()
    resumeA?()
    precondition(events.suffix(2) == ["barrier overflow", "after reset"] && !q.isPaused && other.isPaused)
    resumeB?()
    precondition(q.isEmpty && other.isEmpty)
    let first = OwnedGraphDeferredQueue(maxItems: 4, maxBytes: 4, maxBarriers: 2)
    let second = OwnedGraphDeferredQueue(maxItems: 4, maxBytes: 4, maxBarriers: 2)
    var order: [String] = []
    var a1: (() -> Void)?; var a2: (() -> Void)?
    var b1: (() -> Void)?; var b2: (() -> Void)?
    precondition(first.enqueueBarrier { a1 = $0; order.append("a reset 1") })
    _ = first.enqueueValue(bytes: 1, run: { order.append("a value") }, onLoss: {})
    precondition(first.enqueueBarrier { a2 = $0; order.append("a reset 2") })
    precondition(second.enqueueBarrier { b1 = $0; order.append("b reset 1") })
    _ = second.enqueueValue(bytes: 1, run: { order.append("b value") }, onLoss: {})
    precondition(second.enqueueBarrier { b2 = $0; order.append("b reset 2") })
    first.drain(); second.drain()
    a1?(); b1?(); b2?(); a2?()
    precondition(order == ["a reset 1", "b reset 1", "a value", "a reset 2", "b value", "b reset 2"])
    precondition(first.isEmpty && second.isEmpty)
    var rejected = 0
    _ = q.enqueueRequest(bytes: 0, run: { preconditionFailure("cancelled request ran") }, onReject: { rejected += 1 })
    q.rejectPending()
    q.rejectPending()
    q.drain()
    precondition(rejected == 1)
    var nested: [Int] = []
    _ = q.enqueueRequest(bytes: 0, run: {
      nested.append(1)
      _ = q.enqueueRequest(bytes: 0, run: { nested.append(3) }, onReject: {})
      q.drain()
    }, onReject: {})
    _ = q.enqueueRequest(bytes: 0, run: { nested.append(2) }, onReject: {})
    q.drain()
    precondition(nested == [1, 2, 3])
    // Reserved controls share the FIFO with ordinary work and barriers, even
    // when the ordinary budget is full. Later observations keep their place
    // after an already scheduled fail-stop.
    var ordered: [String] = []
    let reserved = OwnedGraphDeferredQueue(maxItems: 1, maxBytes: 1, maxBarriers: 1, maxReservedControls: 3)
    let firstControl = reserved.reserveControl()!
    let secondControl = reserved.reserveControl()!
    let nestedControl = reserved.reserveControl()!
    precondition(reserved.reserveControl() == nil)
    _ = reserved.enqueueValue(bytes: 1, run: { ordered.append("prefix") }, onLoss: {})
    precondition(reserved.enqueueReservedControl(firstControl) {
      ordered.append("failure 42")
      precondition(!reserved.releaseControl(firstControl), "an executing control still owns its reservation")
      precondition(!reserved.enqueueReservedControl(firstControl) {}, "reentrancy must not reuse an executing reservation")
      precondition(reserved.enqueueReservedControl(nestedControl) { ordered.append("failure 44") })
      reserved.drain()
    })
    reserved.scheduleFailStop { ordered.append("fail-stop") }
    reserved.scheduleFailStop { preconditionFailure("duplicate fail-stop") }
    var resume: (() -> Void)?
    precondition(reserved.enqueueBarrier { ordered.append("barrier"); resume = $0 })
    precondition(reserved.enqueueReservedControl(secondControl) { ordered.append("failure 43") })
    reserved.drain()
    precondition(ordered == ["prefix", "failure 42", "barrier"] && reserved.isPaused)
    reserved.rejectPending()
    resume?(); resume?()
    precondition(ordered == ["prefix", "failure 42", "barrier", "fail-stop", "failure 43", "failure 44"])
    precondition(reserved.isEmpty)
    precondition(reserved.releaseControl(firstControl))
    precondition(!reserved.releaseControl(firstControl))
    let reused = reserved.reserveControl()!
    precondition(reserved.enqueueReservedControl(reused) { ordered.append("retained through cancellation") })
    reserved.cancel(reused)
    reserved.rejectPending()
    reserved.drain()
    precondition(ordered.last == "retained through cancellation")
    precondition(reserved.releaseControl(reused))
    // A physical owner's entire control budget must be admitted atomically.
    // A failed reservation must leave every slot available to another owner.
    let admission = OwnedGraphDeferredQueue(maxItems: 1, maxBytes: 1, maxReservedControls: 3)
    precondition(admission.reserveControls(count: 0) == nil)
    precondition(admission.reserveControls(count: -1) == nil)
    let owner = admission.reserveControls(count: 2)!
    precondition(owner.count == 2 && Set(owner).count == 2)
    precondition(admission.reserveControls(count: 2) == nil)
    let remaining = admission.reserveControls(count: 1)!
    precondition(admission.reserveControls(count: 1) == nil)
    precondition(admission.enqueueReservedControl(owner[0]) {})
    precondition(!admission.releaseControls(owner), "a busy control must prevent partial owner release")
    precondition(!admission.releaseControls([owner[1], owner[1]]), "duplicate credits must not be released")
    precondition(admission.reserveControls(count: 1) == nil)
    admission.drain()
    precondition(admission.releaseControls(owner))
    precondition(!admission.releaseControls(owner), "an owner reservation cannot be released twice")
    precondition(admission.releaseControls(remaining))
    precondition(admission.reserveControls(count: 3)?.count == 3)
    // A callback observed after fail-stop was scheduled must not jump ahead
    // of that already observed boundary merely because it owns a credit.
    var boundaryOrder: [String] = []
    let boundary = OwnedGraphDeferredQueue(maxItems: 1, maxBytes: 1, maxReservedControls: 1)
    let credit = boundary.reserveControl()!
    boundary.scheduleFailStop { boundaryOrder.append("fail-stop") }
    precondition(boundary.enqueueReservedControl(credit) { boundaryOrder.append("later observation") })
    boundary.drain()
    precondition(boundaryOrder == ["fail-stop", "later observation"], "reserved observations must keep their actual FIFO position")
    print("AppleGraphDeferredQueueHarness passed")
  }
}
