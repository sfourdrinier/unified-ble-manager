// ios/__tests__/AppleOrderedNotificationIngressHarness.swift
import Foundation

@main enum AppleOrderedNotificationIngressHarness {
  static func main() throws {
    admissionTransactionChecks()
    let queue = DispatchQueue(label: "test.owner")
    try queue.sync {
    let a = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let b = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 1)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "g")
    guard let a1 = source.makeClaim(route: a), let b1 = source.makeClaim(route: b) else { fatalError("claims") }
    let first = try OwnedOrderedNotificationIngress(queue: queue, generation: "g", members: [a, b], capacity: 4, byteCapacity: 2)
    let second = try OwnedOrderedNotificationIngress(queue: queue, generation: "g", members: [a, b], capacity: 8, byteCapacity: 20)
    assert(source.add(first) && source.add(second))
    // A callback with no group currently interested still consumes a source ordinal.
    source.admit(route: b, bytes: Data([0]), provenance: .notification)
    try first.register(a1); try second.register(a1); try second.register(b1)
    source.admit(route: a, bytes: Data([1, 2]), provenance: .readOrNotification)
    source.admit(route: b, bytes: Data([3]), provenance: .notification)
    source.admit(route: a, bytes: Data([4]), provenance: .notification)
    assert(first.drain() == [.value(member: a, epoch: 1, ordinal: 1, bytes: Data([1, 2]), provenance: .readOrNotification), .terminal(.overflow, platformTail: .uncertain, rejected: .value(member: a, epoch: 1, bytes: 1))])
    assert(second.drain() == [.value(member: a, epoch: 1, ordinal: 1, bytes: Data([1, 2]), provenance: .readOrNotification), .value(member: b, epoch: 1, ordinal: 2, bytes: Data([3]), provenance: .notification), .value(member: a, epoch: 1, ordinal: 3, bytes: Data([4]), provenance: .notification)])
    let third = try OwnedOrderedNotificationIngress(queue: queue, generation: "g", members: [a], capacity: 2, byteCapacity: 4)
    assert(source.add(third)); try third.register(a1)
    second.retainCleanup(a1)
    source.admit(route: a, bytes: Data([5]), provenance: .notification)
    try second.retire(a)
    guard let a2 = source.makeClaim(route: a) else { fatalError("second claim") }
    try second.register(a2)
    second.retainCleanup(a2)
    second.cleanupFinished(a1)
    assert(second.cleanupPending == [a2])
    // The platform may have queued this callback before the fence; its physical tail is unknown.
    source.admit(route: a, bytes: Data([6]), provenance: .notification)
    assert(second.drain() == [.value(member: a, epoch: 1, ordinal: 4, bytes: Data([5]), provenance: .notification), .fence(member: a, epoch: 1, platformTail: .uncertain), .value(member: a, epoch: 2, ordinal: 5, bytes: Data([6]), provenance: .notification)])
    assert(third.drain() == [.value(member: a, epoch: 1, ordinal: 4, bytes: Data([5]), provenance: .notification), .value(member: a, epoch: 1, ordinal: 5, bytes: Data([6]), provenance: .notification)])
    source.invalidate(.serviceChanged)
    source.admit(route: a, bytes: Data([7]), provenance: .notification)
    assert(second.drain() == [.terminal(.serviceChanged, platformTail: .uncertain, rejected: nil)])
    assert(!source.remove(second))
    second.cleanupFinished(a2)
    try second.seal()
    assert(source.remove(second))
    do { try second.register(a2); fatalError("ended") } catch OwnedOrderedNotificationIngress.Failure.inactive {}
    let small = try OwnedOrderedNotificationIngress(queue: queue, generation: "h", members: [a], capacity: 1, byteCapacity: 10)
    let other = OwnedOrderedNotificationSource(queue: queue, generation: "h")
    guard let claim = other.makeClaim(route: a) else { fatalError("claim") }
    assert(other.add(small)); try small.register(claim)
    other.admit(route: a, bytes: Data([8]), provenance: .notification)
    try small.retire(a)
    // A full data FIFO retains the retirement control; after the value leaves
    // it, the fence is published without exposing a manual retry to callers.
    assert(small.drain() == [.value(member: a, epoch: 1, ordinal: 0, bytes: Data([8]), provenance: .notification)])
    assert(small.drain() == [.fence(member: a, epoch: 1, platformTail: .uncertain)])
    // A terminal does not end failure observation until source seal. Preserve
    // one late error and explicitly report further observations lost to bounds.
    let failureSource = OwnedOrderedNotificationSource(queue: queue, generation: "failure")
    let failureGroup = try OwnedOrderedNotificationIngress(queue: queue, generation: "failure", members: [a], capacity: 1, byteCapacity: 4)
    guard let failureClaim = failureSource.makeClaim(route: a) else { fatalError("failure claim") }
    assert(failureSource.add(failureGroup)); try failureGroup.register(failureClaim)
    let underlying = NSError(domain: "cause", code: 9, userInfo: [NSLocalizedDescriptionKey: "radio disconnected"])
    let originalA = NSError(domain: "CB", code: 42, userInfo: [NSLocalizedDescriptionKey: "first description", NSUnderlyingErrorKey: underlying])
    let originalB = NSError(domain: "CB", code: 42, userInfo: [NSLocalizedDescriptionKey: "second description"])
    let nativeA = OwnedOrderedNotificationIngress.NativeFailure(error: originalA)
    let nativeB = OwnedOrderedNotificationIngress.NativeFailure(error: originalB)
    assert(nativeA != nativeB && nativeA.detail.contains("first description") && nativeA.detail.contains("cause:9") && nativeA.detail.contains("radio disconnected"))
    assert(nativeA.attributed(to: failureClaim, epoch: 1).detail == nativeA.detail)
    let firstFailure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 1)
    let lateFailure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 2)
    failureSource.fail(route: a, failure: firstFailure)
    failureSource.fail(route: a, failure: lateFailure)
    failureSource.fail(route: a, failure: firstFailure)
    assert(failureGroup.drain() == [.terminal(.sourceFailed(firstFailure, member: a, sourceMember: failureClaim.token, epoch: 1), platformTail: .uncertain, rejected: nil), .subsequentSourceFailure(lateFailure.attributed(to: failureClaim, epoch: 1)), .failureObservationsOmitted])
    failureSource.fail(route: a, failure: firstFailure)
    assert(!failureSource.remove(failureGroup), "undrained late evidence cannot be removed")
    try failureGroup.seal()
    failureSource.fail(route: a, failure: lateFailure)
    assert(failureGroup.drain() == [.subsequentSourceFailure(firstFailure.attributed(to: failureClaim, epoch: 1))])
    assert(failureSource.remove(failureGroup))
    // Reusing a source claim in a later epoch must not let an old receipt
    // terminate that replacement registration.
    let epochSource = OwnedOrderedNotificationSource(queue: queue, generation: "epoch")
    let epochGroup = try OwnedOrderedNotificationIngress(queue: queue, generation: "epoch", members: [a], capacity: 4, byteCapacity: 4)
    let epochClaim = epochSource.makeClaim(route: a)!
    assert(epochSource.add(epochGroup)); try epochGroup.register(epochClaim)
    let oldReceipt = epochSource.captureDeferredFailure(claim: epochClaim, failure: firstFailure)!
    try epochGroup.retire(a); try epochGroup.register(epochClaim)
    oldReceipt(true)
    epochSource.admit(route: a, bytes: Data([7]), provenance: .notification)
    try epochGroup.barrier(91)
    assert(epochGroup.drain() == [.fence(member: a, epoch: 1, platformTail: .uncertain),
      .subsequentSourceFailure(firstFailure.attributed(to: epochClaim, epoch: 1)),
      .value(member: a, epoch: 2, ordinal: 0, bytes: Data([7]), provenance: .notification),
      .barrier(id: 91, platformTail: .uncertain)])
    let overflowSource = OwnedOrderedNotificationSource(queue: queue, generation: "late-overflow")
    let overflowGroup = try OwnedOrderedNotificationIngress(queue: queue, generation: "late-overflow", members: [a], capacity: 1, byteCapacity: 4)
    let overflowOld = overflowSource.makeClaim(route: a)!
    assert(overflowSource.add(overflowGroup)); try overflowGroup.register(overflowOld)
    let overflowReceipt1 = overflowSource.captureDeferredFailure(claim: overflowOld, failure: firstFailure)!
    try overflowGroup.retire(a); _ = overflowGroup.drain()
    let overflowNew = overflowSource.makeClaim(route: a)!
    try overflowGroup.register(overflowNew)
    let overflowReceipt2 = overflowSource.captureDeferredFailure(claim: overflowNew, failure: lateFailure)!
    try overflowGroup.retire(a); _ = overflowGroup.drain()
    overflowReceipt1(true); overflowReceipt2(true)
    // The second retirement was accepted logically while the first fence held
    // the one-record FIFO. Draining that prefix must autonomously publish the
    // retained second fence; callers never retry the retirement themselves.
    assert(overflowGroup.drain() == [.fence(member: a, epoch: 1, platformTail: .uncertain)])
    overflowGroup.close(); try overflowGroup.seal()
    assert(!overflowSource.remove(overflowGroup), "late record and omission must block cleanup")
    assert(overflowGroup.drain() == [.subsequentSourceFailure(firstFailure.attributed(to: overflowOld, epoch: 1)), .failureObservationsOmitted,
      .fence(member: a, epoch: 2, platformTail: .uncertain), .terminal(.closed, platformTail: .uncertain, rejected: nil)])
    assert(overflowSource.remove(overflowGroup))
    let capturedSource = OwnedOrderedNotificationSource(queue: queue, generation: "captured")
    let capturedGroup = try OwnedOrderedNotificationIngress(queue: queue, generation: "captured", members: [a], capacity: 4, byteCapacity: 4)
    let capturedClaim = capturedSource.makeClaim(route: a)!
    assert(capturedSource.add(capturedGroup)); try capturedGroup.register(capturedClaim)
    let capturedReport = capturedSource.captureFailure(route: a, failure: firstFailure)
    try capturedGroup.retire(a)
    let capturedReplacement = capturedSource.makeClaim(route: a)!
    try capturedGroup.register(capturedReplacement)
    capturedReport(); capturedReport()
    assert(capturedGroup.drain() == [.fence(member: a, epoch: 1, platformTail: .uncertain),
      .subsequentSourceFailure(firstFailure.attributed(to: capturedClaim, epoch: 1))])
    capturedSource.admit(route: a, bytes: Data([8]), provenance: .notification)
    assert(capturedGroup.drain() == [.value(member: a, epoch: 2, ordinal: 0, bytes: Data([8]), provenance: .notification)])
    var writeCalls = 0
    var callbackCalled = false
    let dispatchOrdinal = try OwnedOrderedWriteDispatch.start(queue: queue, admit: { 8 }, write: { finish in
      writeCalls += 1
      finish(nil)
      assert(!callbackCalled)
    }, completion: { _ in callbackCalled = true })
    assert(dispatchOrdinal == 8 && writeCalls == 1 && !callbackCalled)
    do {
      _ = try OwnedOrderedWriteDispatch.start(queue: queue, admit: { throw OwnedOrderedNotificationIngress.Failure.controlOverflow }, write: { _ in writeCalls += 1 }, completion: { _ in })
      fatalError("refused marker dispatched a write")
    } catch OwnedOrderedNotificationIngress.Failure.controlOverflow {}
    assert(writeCalls == 1)
    var permitEvents: [String] = []
    let permit = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 2, maxBytes: 4, overflow: { permitEvents.append("overflow") })
    let permitKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
    let staleKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "old", snapshot: permitKey.snapshot, id: 0)
    assert(permit.acquire(staleKey, valid: false) == .stale)
    assert(permit.acquire(permitKey, valid: true) == .acquired)
    let independent = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 2, maxBytes: 4, overflow: {})
    let independentKey = OwnedOrderedGraphPermitGate.Key(peer: "q", connection: "other", snapshot: permitKey.snapshot, id: 2)
    assert(independent.acquire(independentKey, valid: true) == .acquired)
    assert(independent.release(permitKey) == nil)
    assert(independent.commit(independentKey) == .committed)
    assert(independent.release(independentKey) == .released)
    // A route owns its entire control budget until every accepted callback has
    // left the FIFO, including while its callback runs reentrantly.
    var reservedOrder: [String] = []
    let reservedGate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 1, maxBytes: 1,
      maxReservedControls: 3, overflow: { reservedOrder.append("overflow") })
    let owner = UUID(), replacement = UUID(), otherOwner = UUID()
    assert(!reservedGate.reserveNotificationControls(route: owner, count: 0))
    assert(!reservedGate.reserveNotificationControls(route: owner, count: -1))
    assert(reservedGate.reserveNotificationControls(route: owner, count: 2))
    assert(!reservedGate.reserveNotificationControls(route: owner, count: 1))
    assert(!reservedGate.reserveNotificationControls(route: otherOwner, count: 2))
    assert(reservedGate.reserveNotificationControls(route: otherOwner, count: 1))
    // An admission cancelled before publication returns its whole budget.
    assert(reservedGate.releaseNotificationControls(route: otherOwner))
    assert(reservedGate.reserveNotificationControls(route: otherOwner, count: 1))
    assert(!reservedGate.canDiscard)
    let reservedKey = OwnedOrderedGraphPermitGate.Key(peer: "reserved", connection: "c", snapshot: permitKey.snapshot, id: 70)
    assert(reservedGate.acquire(reservedKey, valid: true) == .acquired)
    assert(reservedGate.enqueue { reservedOrder.append("prefix") })
    assert(reservedGate.enqueueReservedNotificationControl(route: owner) {
      reservedOrder.append("42")
      assert(!reservedGate.releaseNotificationControls(route: owner))
      assert(!reservedGate.enqueueReservedNotificationControl(route: owner) {})
      assert(reservedGate.enqueueReservedNotificationControl(route: otherOwner) { reservedOrder.append("44") })
      assert(!reservedGate.releaseNotificationControls(route: otherOwner))
    })
    var resumeReserved: (() -> Void)?
    assert(reservedGate.enqueueBarrier(invalidating: false) { reservedOrder.append("barrier"); resumeReserved = $0 })
    assert(reservedGate.enqueueReservedNotificationControl(route: owner) { reservedOrder.append("43") })
    assert(!reservedGate.enqueueReservedNotificationControl(route: owner) {})
    assert(!reservedGate.releaseNotificationControls(route: owner))
    assert(!reservedGate.enqueueReservedNotificationControl(route: replacement) {})
    assert(reservedGate.release(reservedKey) == .released)
    assert(reservedOrder == ["prefix", "42", "barrier"])
    assert(!reservedGate.releaseNotificationControls(route: owner))
    resumeReserved?(); resumeReserved?()
    assert(reservedOrder == ["prefix", "42", "barrier", "43", "44"])
    assert(reservedGate.releaseNotificationControls(route: owner))
    assert(!reservedGate.releaseNotificationControls(route: owner))
    assert(reservedGate.reserveNotificationControls(route: replacement, count: 2))
    assert(!reservedGate.enqueueReservedNotificationControl(route: owner) {})
    assert(reservedGate.releaseNotificationControls(route: replacement))
    assert(reservedGate.releaseNotificationControls(route: otherOwner))
    assert(reservedGate.forget(reservedKey) && reservedGate.canDiscard)
    // A route's reservation is a concurrent budget, not a lifetime quota.
    let reusable = UUID()
    assert(reservedGate.reserveNotificationControls(route: reusable, count: 2))
    var delivered = 0
    for _ in 0..<20 {
      assert(reservedGate.enqueueReservedNotificationControl(route: reusable) { delivered += 1 })
    }
    assert(delivered == 20)
    assert(reservedGate.releaseNotificationControls(route: reusable))
    permit.invalidateAndEnqueue { permitEvents.append("reset") }
    permit.enqueue(bytes: 1) { permitEvents.append("value") }
    assert(permit.commit(permitKey) == .invalidated && permitEvents.isEmpty)
    assert(permit.release(permitKey) == .released && permitEvents == ["reset", "value"])
    assert(permit.release(permitKey) == .released)
    let reentrantGate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 4, maxBytes: 4, overflow: {})
    let reentrantKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "c", snapshot: permitKey.snapshot, id: 90)
    var reentrantOrder: [String] = []
    assert(reentrantGate.acquire(reentrantKey, valid: true) == .acquired)
    reentrantGate.enqueue {
      reentrantOrder.append("first")
      reentrantGate.enqueue { reentrantOrder.append("nested") }
    }
    reentrantGate.enqueue { reentrantOrder.append("second") }
    assert(reentrantGate.release(reentrantKey) == .released)
    precondition(reentrantOrder == ["first", "second", "nested"],
      "reentrant gate work must follow the already queued prefix: \(reentrantOrder)")
    let resumedGate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 4, maxBytes: 4, overflow: {})
    let resumedKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "c", snapshot: permitKey.snapshot, id: 91)
    var resumedOrder: [String] = []
    var resumeReplay: (() -> Void)?
    assert(resumedGate.acquire(resumedKey, valid: true) == .acquired)
    assert(resumedGate.enqueueBarrier(invalidating: false) { resumeReplay = $0 })
    resumedGate.enqueue {
      resumedOrder.append("first")
      resumedGate.enqueue { resumedOrder.append("nested") }
    }
    resumedGate.enqueue { resumedOrder.append("second") }
    assert(resumedGate.release(resumedKey) == .released && resumedOrder.isEmpty)
    resumeReplay?()
    precondition(resumedOrder == ["first", "second", "nested"],
      "resumed gate work must retain FIFO under reentrancy: \(resumedOrder)")
    let secondKey = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "c", snapshot: permitKey.snapshot, id: 2)
    assert(permit.acquire(secondKey, valid: true) == .acquired)
    assert(permit.commit(secondKey) == .committed)
    permit.enqueue(bytes: 4) { permitEvents.append("first") }
    permit.enqueue(bytes: 1) { permitEvents.append("second") }
    assert(permitEvents == ["reset", "value"])
    assert(permit.commit(secondKey) == .committed && permit.release(secondKey) == .released)
    assert(permitEvents == ["reset", "value", "first", "overflow"])
    var brokenEvents: [String] = []
    let brokenGate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 1, maxBytes: 4, overflow: { brokenEvents.append("overflow") })
    let brokenKey = OwnedOrderedGraphPermitGate.Key(peer: "broken", connection: "c", snapshot: permitKey.snapshot, id: 1)
    assert(brokenGate.acquire(brokenKey, valid: true) == .acquired)
    assert(brokenGate.enqueue { brokenEvents.append("accepted") })
    brokenGate.breakForOverflow()
    assert(brokenEvents.isEmpty, "overflow must not overtake the accepted prefix")
    assert(brokenGate.commit(brokenKey) == .invalidated)
    assert(brokenGate.release(brokenKey) == .released)
    assert(brokenEvents == ["accepted", "overflow"], "overflow must follow the accepted prefix")
    let pGate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 4, maxBytes: 4, overflow: {})
    let qGate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 4, maxBytes: 4, overflow: {})
    let pk = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "c", snapshot: permitKey.snapshot, id: 3)
    let qk = OwnedOrderedGraphPermitGate.Key(peer: "q", connection: "c", snapshot: permitKey.snapshot, id: 4)
    assert(pGate.acquire(pk, valid: true) == .acquired && qGate.acquire(qk, valid: true) == .acquired)
    var staggered: [String] = []
    pGate.invalidateAndEnqueue { staggered.append("p-reset") }
    qGate.invalidateAndEnqueue { staggered.append("q-reset") }
    pGate.enqueue { staggered.append("p-value") }
    qGate.enqueue { staggered.append("q-value") }
    assert(pGate.release(pk) == .released && staggered == ["p-reset", "p-value"])
    assert(qGate.release(qk) == .released && staggered == ["p-reset", "p-value", "q-reset", "q-value"])
    let cancelGate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 2, maxBytes: 4, overflow: {})
    let cancelledKey = OwnedOrderedGraphPermitGate.Key(peer: "cancel", connection: "c", snapshot: permitKey.snapshot, id: 5)
    assert(cancelGate.cancelBeforeAcquire(cancelledKey) == .notAcquired)
    assert(cancelGate.acquire(cancelledKey, valid: true) == .notAcquired)
    let followingKey = OwnedOrderedGraphPermitGate.Key(peer: "cancel", connection: "c", snapshot: permitKey.snapshot, id: 6)
    assert(cancelGate.acquire(followingKey, valid: true) == .acquired)
    assert(cancelGate.release(followingKey) == .released)
    let registry = OwnedOrderedGraphPermitRegistry()
    let zeroKey = OwnedOrderedGraphPermitGate.Key(peer: "zero", connection: "c", snapshot: permitKey.snapshot, id: 0)
    assert(!registry.alreadyForgotten(zeroKey) && !registry.forget(zeroKey))
    let reg1 = OwnedOrderedGraphPermitGate.Key(peer: "one", connection: "c", snapshot: permitKey.snapshot, id: 1)
    let reg2 = OwnedOrderedGraphPermitGate.Key(peer: "two", connection: "c", snapshot: permitKey.snapshot, id: 2)
    let foreign = OwnedOrderedGraphPermitGate.Key(peer: "foreign", connection: "c", snapshot: permitKey.snapshot, id: 2)
    let request1 = OwnedOrderedGraphPermitRegistry.Request(authority: 0, request: 10, peer: reg1.peer, connection: reg1.connection, snapshot: reg1.snapshot)
    let request2 = OwnedOrderedGraphPermitRegistry.Request(authority: 0, request: 20, peer: reg2.peer, connection: reg2.connection, snapshot: reg2.snapshot)
    assert(registry.issue(request: request1) == 1)
    let resultLock = NSLock()
    var replayedIDs: [UInt64] = []
    DispatchQueue.concurrentPerform(iterations: 16) { _ in
      if let id = registry.issue(request: request1) { resultLock.lock(); replayedIDs.append(id); resultLock.unlock() }
    }
    assert(replayedIDs.count == 16 && Set(replayedIDs) == [1])
    let conflictingRequest = OwnedOrderedGraphPermitRegistry.Request(authority: 0, request: 10, peer: "foreign", connection: reg1.connection, snapshot: reg1.snapshot)
    assert(registry.issue(request: conflictingRequest) == nil && registry.requestConflicts(conflictingRequest))
    assert(!registry.forget(reg2))
    if case .stale = registry.reserve(reg2) {} else { fatalError("future permit admitted") }
    assert(registry.issue(request: request2) == 2)
    if case .accepted = registry.reserve(reg2) {} else { fatalError("out of order reserve") }
    if case .accepted = registry.reserve(reg1) {} else { fatalError("first reserve") }
    if case .foreign = registry.reserve(foreign) {} else { fatalError("foreign identity") }
    assert(!registry.forget(foreign) && registry.forget(reg2) && registry.alreadyForgotten(reg2))
    assert(registry.forget(reg1) && registry.alreadyForgotten(reg1))
    assert(registry.forgetRequest(request1) && registry.forgetRequest(request2))
    for id in UInt64(3)...100 {
      let key = OwnedOrderedGraphPermitGate.Key(peer: "peer-\(id)", connection: "c", snapshot: permitKey.snapshot, id: id)
      let request = OwnedOrderedGraphPermitRegistry.Request(authority: 0, request: id * 10, peer: key.peer, connection: key.connection, snapshot: key.snapshot)
      assert(registry.issue(request: request) == id)
      if case .accepted = registry.reserve(key) {} else { fatalError("sequential capacity") }
      assert(registry.forget(key) && registry.forgetRequest(request))
    }
    if case .stale = registry.reserve(reg1) {} else { fatalError("retired id") }
    let authorities = OwnedOrderedGraphAuthorities()
    assert(authorities.reserve(request: 0) == nil && !authorities.forget(0) && !authorities.forgetReceipt(0))
    assert(!authorities.close(999) && !authorities.forget(999) && !authorities.forgetReceipt(999))
    guard let receipt1 = authorities.reserve(request: 100), let receipt2 = authorities.reserve(request: 200),
          let auth1 = authorities.open(receipt: receipt1), let auth2 = authorities.open(receipt: receipt2),
          let permits1 = authorities.permits(auth1), let permits2 = authorities.permits(auth2) else { fatalError("authority open") }
    assert(authorities.reserve(request: 100) == receipt1 && authorities.open(receipt: receipt1) == auth1 && auth1 != auth2)
    var receiptReplays: [UInt64] = []
    DispatchQueue.concurrentPerform(iterations: 16) { _ in
      if let id = authorities.reserve(request: 100) { resultLock.lock(); receiptReplays.append(id); resultLock.unlock() }
    }
    assert(receiptReplays.count == 16 && Set(receiptReplays) == [receipt1])
    let authKey1 = OwnedOrderedGraphPermitGate.Key(authority: auth1, peer: "one", connection: "c", snapshot: permitKey.snapshot, id: 1)
    let authKey2 = OwnedOrderedGraphPermitGate.Key(authority: auth2, peer: "two", connection: "c", snapshot: permitKey.snapshot, id: 1)
    let authRequest1 = OwnedOrderedGraphPermitRegistry.Request(authority: auth1, request: 1000, peer: authKey1.peer, connection: authKey1.connection, snapshot: authKey1.snapshot)
    let authRequest2 = OwnedOrderedGraphPermitRegistry.Request(authority: auth2, request: 2000, peer: authKey2.peer, connection: authKey2.connection, snapshot: authKey2.snapshot)
    assert(permits1.issue(request: authRequest1) == 1 && permits2.issue(request: authRequest2) == 1)
    if case .accepted = permits1.reserve(authKey1) {} else { fatalError("authority permit") }
    if case .accepted = permits2.reserve(authKey2) {} else { fatalError("independent authority") }
    assert(!authorities.close(auth1) && permits1.forget(authKey1) && permits1.forgetRequest(authRequest1))
    let unseen = OwnedOrderedGraphPermitGate.Key(authority: auth1, peer: "one", connection: "c", snapshot: permitKey.snapshot, id: 2)
    let unseenRequest = OwnedOrderedGraphPermitRegistry.Request(authority: auth1, request: 1001, peer: unseen.peer, connection: unseen.connection, snapshot: unseen.snapshot)
    assert(permits1.issue(request: unseenRequest) == 2)
    assert(permits1.forget(unseen) && permits1.alreadyForgotten(unseen) && permits1.forgetRequest(unseenRequest))
    if case .stale = permits1.reserve(unseen) {} else { fatalError("forgotten unseen id reopened") }
    assert(authorities.close(auth1) && authorities.close(auth1) && authorities.forget(auth1) && authorities.forget(auth1))
    assert(authorities.status(receipt: receipt1) == .closed(auth1) && authorities.forgetReceipt(receipt1) && authorities.forgetReceipt(receipt1))
    assert(authorities.permits(auth1) == nil && permits2.owns(authKey2))
    assert(permits2.forget(authKey2) && permits2.forgetRequest(authRequest2) && authorities.close(auth2) && authorities.forget(auth2) && authorities.forgetReceipt(receipt2))
    guard let cancelledReceipt = authorities.reserve(request: 999) else { fatalError("cancel receipt") }
    assert(authorities.status(receipt: cancelledReceipt) == .pending)
    assert(authorities.cancelOpen(receipt: cancelledReceipt) == .notCreated)
    assert(authorities.open(receipt: cancelledReceipt) == nil && authorities.reserve(request: 999) == cancelledReceipt)
    assert(authorities.forgetReceipt(cancelledReceipt) && authorities.forgetReceipt(cancelledReceipt))
    let markerSource = OwnedOrderedNotificationSource(queue: queue, generation: "marker")
    let markerGroup = try OwnedOrderedNotificationIngress(queue: queue, generation: "marker", members: [a], capacity: 1, byteCapacity: 10)
    assert(markerSource.add(markerGroup))
    try markerSource.admitMarker(group: markerGroup, id: 41)
    do { try markerSource.admitMarker(group: markerGroup, id: 42); fatalError("marker capacity") }
    catch OwnedOrderedNotificationIngress.Failure.controlOverflow {}
    assert(markerGroup.drain() == [.marker(id: 41, ordinal: 0), .terminal(.overflow, platformTail: .uncertain, rejected: .marker(id: 42))])
    let lossSource = OwnedOrderedNotificationSource(queue: queue, generation: "loss")
    let lossGroup = try OwnedOrderedNotificationIngress(queue: queue, generation: "loss", members: [a], capacity: 1, byteCapacity: 10)
    guard let lossClaim = lossSource.makeClaim(route: a) else { fatalError("loss claim") }
    assert(lossSource.add(lossGroup)); try lossGroup.register(lossClaim)
    lossSource.admit(route: a, bytes: Data([1, 2, 3]), provenance: .notification)
    lossGroup.reportLoss(member: a, count: UInt64.max)
    assert(lossGroup.drain() == [.value(member: a, epoch: 1, ordinal: 0, bytes: Data([1, 2, 3]), provenance: .notification),
      .terminal(.overflow, platformTail: .uncertain, rejected: .sourceLoss(member: a, skippedRecords: UInt64.max))])
    for cause in [OwnedOrderedNotificationIngress.End.connectionLost, .serviceChanged, .sourceReplaced, .shutdown, .adapterLost] {
      let lifecycleSource = OwnedOrderedNotificationSource(queue: queue, generation: "life")
      let lifecycleGroup = try OwnedOrderedNotificationIngress(queue: queue, generation: "life", members: [a], capacity: 2, byteCapacity: 10)
      guard let lifecycleClaim = lifecycleSource.makeClaim(route: a) else { fatalError("lifecycle claim") }
      assert(lifecycleSource.add(lifecycleGroup)); try lifecycleGroup.register(lifecycleClaim)
      lifecycleSource.admit(route: a, bytes: Data([9]), provenance: .notification)
      lifecycleSource.invalidate(cause)
      assert(lifecycleGroup.drain() == [.value(member: a, epoch: 1, ordinal: 0, bytes: Data([9]), provenance: .notification),
        .terminal(cause, platformTail: .uncertain, rejected: nil)])
    }
    do { _ = try OwnedOrderedNotificationIngress(queue: queue, generation: "g", members: [a, a], capacity: 1, byteCapacity: 1); fatalError("duplicate") } catch OwnedOrderedNotificationIngress.Failure.invalidMembers {}
    var wakes = 0
    let wakingSource = OwnedOrderedNotificationSource(queue: queue, generation: "wake")
    let waking = try OwnedOrderedNotificationIngress(queue: queue, generation: "wake", members: [a], capacity: 2, byteCapacity: 4, wake: { wakes += 1 })
    guard let wakingClaim = wakingSource.makeClaim(route: a) else { fatalError("wake claim") }
    assert(wakingSource.add(waking)); try waking.register(wakingClaim)
    wakingSource.admit(route: a, bytes: Data([1]), provenance: .notification)
    try waking.retire(a)
    assert(wakes == 1, "one wake covers value and fence")
    _ = waking.drain()
    waking.close()
    assert(wakes == 2, "terminal after drain wakes the consumer")
    _ = waking.drain()
    waking.close()
    assert(wakes == 2, "repeated closure cannot wake after terminal")
    var closeWakes = 0
    let closingSource = OwnedOrderedNotificationSource(queue: queue, generation: "close")
    let closing = try OwnedOrderedNotificationIngress(queue: queue, generation: "close", members: [a], capacity: 2, byteCapacity: 4, wake: { closeWakes += 1 })
    guard let closingClaim = closingSource.makeClaim(route: a) else { fatalError("close claim") }
    assert(closingSource.add(closing)); try closing.register(closingClaim)
    closingSource.admit(route: a, bytes: Data([2]), provenance: .notification)
    closing.close()
    closingSource.admit(route: a, bytes: Data([3]), provenance: .notification)
    assert(closeWakes == 1, "close shares the pending wake")
    assert(closing.drain() == [.value(member: a, epoch: 1, ordinal: 0, bytes: Data([2]), provenance: .notification), .terminal(.closed, platformTail: .uncertain, rejected: nil)])
    print("Ordered ingress owner: deterministic tests passed")
    }
  }
}

extension AppleOrderedNotificationIngressHarness {
  static func admissionTransactionChecks() {
    let queue = DispatchQueue(label: "admission.transaction.test")
    queue.sync {
      let gate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 1, maxBytes: 32,
        maxReservedControls: 3, overflow: {})
      let first = OwnedNotificationReceiptBudget(queue: queue, capacity: 3)
      let second = OwnedNotificationReceiptBudget(queue: queue, capacity: 2)
      let identity = UUID()
      precondition(OwnedNotificationAdmissionTransaction.prepare(queue: queue, gate: gate,
        identity: identity, recipients: [first, second], concurrentObservations: 2) == nil)
      precondition(first.available == 3 && second.available == 2, "partial recipient admission must unwind")
      let full = OwnedNotificationReceiptBudget(queue: queue, capacity: 3)
      guard let transaction = OwnedNotificationAdmissionTransaction.prepare(queue: queue, gate: gate,
        identity: identity, recipients: [first, full], concurrentObservations: 2) else { fatalError("prepare") }
      precondition(first.available == 0 && full.available == 0)
      let other = OwnedNotificationReceiptBudget(queue: queue, capacity: 3)
      precondition(OwnedNotificationAdmissionTransaction.prepare(queue: queue, gate: gate,
        identity: UUID(), recipients: [other], concurrentObservations: 2) == nil)
      precondition(other.available == 3, "gate refusal must unwind the recipient reservation")
      guard let cancelled = transaction.claim() else { fatalError("claim for rollback") }
      precondition(transaction.cancelUnqueued(cancelled))
      precondition(!transaction.cancelUnqueued(cancelled), "cancellation is exact-once")
      guard let ordinaryA = transaction.claim(), let ordinaryB = transaction.claim(),
            let overflow = transaction.claimOverflow() else { fatalError("reserved concurrency") }
      precondition(transaction.claim() == nil && transaction.claimOverflow() == nil,
        "normal and overflow observations have separate bounded credits")
      precondition(transaction.cancelUnqueued(ordinaryA))
      precondition(transaction.cancelUnqueued(ordinaryB))
      precondition(transaction.cancelUnqueued(overflow))
      let key = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "c",
        snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
      precondition(gate.acquire(key, valid: true) == .acquired)
      var order: [String] = []
      precondition(gate.enqueue { order.append("prefix") })
      guard let observation = transaction.claim() else { fatalError("claim") }
      precondition(transaction.enqueue(observation) {
        order.append("failure")
        precondition(!transaction.releaseWhenIdle(), "executing gate credit must stay reserved")
      })
      precondition(!transaction.releaseWhenIdle(), "queued observation and receipts must stay reserved")
      precondition(gate.release(key) == .released)
      precondition(order == ["prefix", "failure"])
      precondition(!transaction.releaseWhenIdle(), "recipient acknowledgements are independent of gate replay")
      observation.acknowledge(recipient: 0)
      precondition(!transaction.releaseWhenIdle())
      observation.acknowledge(recipient: 1)
      precondition(transaction.releaseWhenIdle())
      precondition(first.available == 3 && full.available == 3)
      guard let reusable = OwnedNotificationAdmissionTransaction.prepare(queue: queue, gate: gate,
        identity: UUID(), recipients: [first, full], concurrentObservations: 2) else { fatalError("reuse reservation") }
      for _ in 0..<20 {
        guard let receipt = reusable.claim() else { fatalError("receipt credits were not reused") }
        precondition(reusable.enqueue(receipt) {
          receipt.acknowledge(recipient: 0)
          receipt.acknowledge(recipient: 0) // exact acknowledgement is idempotent
          receipt.acknowledge(recipient: 1)
        })
      }
      precondition(reusable.releaseWhenIdle() && first.available == 3 && full.available == 3)
      print("Atomic notification admission transaction passed")
    }
  }
}
