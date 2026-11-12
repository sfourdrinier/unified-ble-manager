// ios/__tests__/AppleRustOrderedGroupBridgeHarness.swift
import Foundation

@main enum AppleRustOrderedGroupBridgeHarness {
  static func testLateFailureBudgetHeldUntilBatchAcknowledgement() throws {
    let queue = DispatchQueue(label: "ordered.bridge.failure.budget")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
      characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "budget")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let descriptor = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 1, byteCapacity: 32)
      let claim = OrderedNotificationClaim(generation: source.generation, route: route, token: descriptor.members[0].sourceMember)
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      source.fail(route: route, failure: failure)
      source.fail(route: route, failure: failure)
      let first = try bridge.drain(groupID: 1)!
      source.fail(route: route, failure: failure)
      let retry = try bridge.drain(groupID: 1)
      precondition(retry == first)
      do { try bridge.acknowledge(groupID: 1, batchID: first.id + 1); fatalError("wrong batch released failure capacity") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      try bridge.acknowledge(groupID: 1, batchID: first.id)
      let omitted = try bridge.drain(groupID: 1)!
      precondition(omitted.records == [.failureObservationsOmitted], "unacknowledged batch must continue charging failure capacity")
      try bridge.acknowledge(groupID: 1, batchID: omitted.id)
      source.fail(route: route, failure: failure)
      let reusable = try bridge.drain(groupID: 1)!
      precondition(reusable.records == [.subsequentSourceFailure(failure.attributed(to: claim, epoch: descriptor.members[0].epoch))])
      try bridge.seal(groupID: 1)
      do { try bridge.remove(groupID: 1); fatalError("unacknowledged failure was removed") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      try bridge.acknowledge(groupID: 1, batchID: reusable.id)
      try bridge.remove(groupID: 1)
    }
  }
  static func testWakeRouteIsCapturedAtOpen() throws {
    let queue = DispatchQueue(label: "ordered.bridge.wake.routes")
    let firstWake = DispatchSemaphore(value: 0)
    let secondWake = DispatchSemaphore(value: 0)
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
      characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "shared")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress))
      return (source, ingress)
    }, wake: { _, _ in firstWake.signal() })
    try queue.sync {
      _ = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 2, byteCapacity: 8)
      bridge.setWake { _, _ in secondWake.signal() }
      source.admit(route: route, bytes: Data([1]), provenance: .notification)
    }
    precondition(firstWake.wait(timeout: .now() + 2) == .success, "wake must reach the route that opened the group")
    precondition(secondWake.wait(timeout: .now() + 0.1) == .timedOut, "wake reached a replacement route")
  }
  static func testDisconnectOnlyRetiresOwningBridge() throws {
    let queue = DispatchQueue(label: "ordered.bridge.disconnect.owners")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
      characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "shared")
    func makeBridge() -> UnifiedBleRustOrderedGroupBridge {
      UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
        let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation,
          members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
        precondition(source.add(ingress)); return (source, ingress)
      }, wake: { _, _ in })
    }
    let first = makeBridge(), second = makeBridge()
    try queue.sync {
      _ = try first.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 8, byteCapacity: 32)
      _ = try second.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 8, byteCapacity: 32)
      first.disconnectGroups(peer: "p")
      source.admit(route: route, bytes: Data([9]), provenance: .notification)
      let retired = try first.drain(groupID: 1)?.records
      let surviving = try second.drain(groupID: 1)?.records
      precondition(retired == [.terminal(.connectionLost, platformTail: .uncertain, rejected: nil)],
        "departing owner sees a terminal without later values")
      precondition(surviving?.count == 1, "the other owner retains the shared source")
      if case .value? = surviving?.first {} else { fatalError("surviving owner lost notification") }
    }
  }
  static func testBatchProtocolOwnership() throws {
    let queue = DispatchQueue(label: "ordered.bridge.protocol.owner")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "owner")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      _ = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 8, byteCapacity: 32)
      source.admit(route: route, bytes: Data([1]), provenance: .notification)
      let first = try bridge.drain(groupID: 1)!
      do { try bridge.acknowledgeLegacyBatch(groupID: 1, batchID: first.id); fatalError("undelivered batch acknowledged") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      try bridge.markVersionedBatch(groupID: 1, batchID: first.id)
      do { try bridge.acknowledgeLegacyBatch(groupID: 1, batchID: first.id); fatalError("legacy consumed V2 batch") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      do { try bridge.markLegacyBatch(groupID: 1, batchID: first.id); fatalError("legacy stole V2 delivery") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      let replay = try bridge.drain(groupID: 1)
      precondition(replay == first)
      try bridge.acknowledgeVersionedBatch(groupID: 1, batchID: first.id)
      try bridge.acknowledgeVersionedBatch(groupID: 1, batchID: first.id)
      source.admit(route: route, bytes: Data([2]), provenance: .notification)
      let second = try bridge.drain(groupID: 1)!
      try bridge.markLegacyBatch(groupID: 1, batchID: second.id)
      do { try bridge.markVersionedBatch(groupID: 1, batchID: second.id); fatalError("V2 stole legacy delivery") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      do { try bridge.acknowledgeVersionedBatch(groupID: 1, batchID: second.id); fatalError("V2 consumed legacy batch") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      try bridge.acknowledgeLegacyBatch(groupID: 1, batchID: second.id)
      try bridge.acknowledgeLegacyBatch(groupID: 1, batchID: second.id)
    }
  }
  static func testLateFailureAcknowledgement() throws {
    let queue = DispatchQueue(label: "ordered.bridge.late.failures")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
      characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "late-generation")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let descriptor = try bridge.open(groupID: 1, members: [.init(id: 8, route: route)], capacity: 4, byteCapacity: 16)
      let old = descriptor.members[0]
      let oldClaim = OrderedNotificationClaim(generation: source.generation, route: route, token: old.sourceMember)
      let oldCleanup = try bridge.retainPhysicalClaim(groupID: 1, memberID: 8)
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      let oldReceipt = source.captureDeferredFailure(claim: oldClaim, failure: failure)!
      try bridge.retire(groupID: 1, expected: old)
      let replacement = try bridge.register(groupID: 1, expected: old)
      let newClaim = OrderedNotificationClaim(generation: source.generation, route: route, token: replacement.sourceMember)
      let newCleanup = try bridge.retainPhysicalClaim(groupID: 1, memberID: 8)
      let cancelled = source.captureDeferredFailure(claim: newClaim, failure: failure)!
      cancelled(false); cancelled(true)
      oldReceipt(true); oldReceipt(true)
      source.admit(route: route, bytes: Data([7]), provenance: .notification)
      try bridge.barrier(groupID: 1, id: 99)
      guard let first = try bridge.drain(groupID: 1) else { fatalError("missing original failure") }
      precondition(first.records == [.fence(memberID: 8, sourceMember: old.sourceMember, epoch: old.epoch, platformTail: .uncertain),
        .subsequentSourceFailure(failure.attributed(to: oldClaim, epoch: old.epoch)),
        .value(memberID: 8, sourceMember: replacement.sourceMember, epoch: replacement.epoch, ordinal: 0, value: Data([7]), provenance: .notification),
        .barrier(id: 99, platformTail: .uncertain)])
      let retry = try bridge.drain(groupID: 1)
      precondition(retry == first, "retry must replay the exact original claim")
      let secondReceipt = source.captureDeferredFailure(claim: newClaim, failure: failure)!
      try bridge.retire(groupID: 1, expected: replacement)
      secondReceipt(true)
      try bridge.close(groupID: 1); try bridge.seal(groupID: 1)
      do { try bridge.remove(groupID: 1); fatalError("unacknowledged failure removed") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      do { try bridge.acknowledge(groupID: 1, batchID: first.id + 1); fatalError("wrong batch acknowledged") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      let afterWrongAck = try bridge.drain(groupID: 1)
      precondition(afterWrongAck == first)
      try bridge.acknowledge(groupID: 1, batchID: first.id)
      try bridge.acknowledge(groupID: 1, batchID: first.id)
      guard let second = try bridge.drain(groupID: 1) else { fatalError("missing replacement failure") }
      precondition(second.records == [.fence(memberID: 8, sourceMember: replacement.sourceMember, epoch: replacement.epoch, platformTail: .uncertain),
        .subsequentSourceFailure(failure.attributed(to: newClaim, epoch: replacement.epoch)),
        .terminal(.closed, platformTail: .uncertain, rejected: nil)])
      let secondRetry = try bridge.drain(groupID: 1)
      precondition(secondRetry == second && first != second)
      try bridge.confirmPhysicalCleanup(oldCleanup)
      try bridge.confirmPhysicalCleanup(newCleanup)
      do { try bridge.remove(groupID: 1); fatalError("second failure not acknowledged") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      try bridge.acknowledge(groupID: 1, batchID: second.id)
      try bridge.remove(groupID: 1)
    }
  }
  static func testAdmissionSlots() throws {
    func require(_ value: Bool) { precondition(value) }
    typealias Bridge = UnifiedBleRustOrderedGroupBridge
    typealias Holder = Bridge.AdmissionSlots
    let holder = Holder()
    let other = Holder()
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
      characteristicUUID: "c", characteristicOccurrence: 0)
    let spec = Holder.Spec(connectionGeneration: "connection", databaseGeneration: "database",
      snapshot: .init(owner: "snapshot", attachment: 1, revision: 2),
      members: [.init(id: 1, route: route)], itemCapacity: 3, byteCapacity: 4)
    let specDefault = spec
    func identity(_ generation: UInt64, _ request: UInt64 = 10, _ group: UInt64 = 20,
                  owner: UUID? = nil, slot: UInt32 = 0, spec: Holder.Spec? = nil) -> Holder.Identity {
      .init(version: 2, owner: owner ?? holder.owner, slot: slot, generation: generation,
            requestID: request, groupID: group, peer: "p", spec: spec ?? specDefault)
    }
    let first = identity(1)
    let failed = Holder.Outcome.failed(.init(domain: "test", code: 7, detail: "failure"), .notCreated)
    let fenceFailure = Holder.FailureCause(domain: "fence", code: 1, detail: "fenced")
    try require(holder.fence(first, failure: fenceFailure) == .failed(fenceFailure, .notCreated))
    try require(holder.reserve(first) == .terminal(.failed(fenceFailure, .notCreated)))
    precondition(!holder.begin(first))
    let retired = try holder.retire(first)
    precondition(retired == .init(owner: holder.owner, slot: 0, generation: 1))
    try require(holder.retire(first) == retired)
    let second = identity(2, 11, 21)
    try require(holder.reserve(second) == .reserved)
    try require(holder.reserve(second) == .reserved)
    precondition(holder.begin(second))
    precondition(!holder.begin(second))
    try require(holder.fence(second, failure: fenceFailure) == .pending)
    do { try holder.retire(second); fatalError("executing slot retired") } catch Holder.Error.notTerminal {}
    precondition(!holder.complete(first, outcome: failed))
    try holder.retire(first)
    let malformed = Bridge.Descriptor(groupID: 999, opaqueSourceGeneration: "source", members: [])
    precondition(!holder.complete(second, outcome: .opened(malformed)))
    let installed = Bridge.Descriptor(groupID: second.groupID, opaqueSourceGeneration: "source", members: [])
    let partial = Holder.Outcome.failed(.init(domain: "install", code: 9, detail: "partial"), .created(installed))
    precondition(holder.complete(second, outcome: partial))
    try require(holder.fence(second, failure: fenceFailure) == partial)
    try holder.retire(second)
    do { _ = try holder.fence(first, failure: fenceFailure); fatalError("stale fence") } catch Holder.Error.stale {}
    do { _ = try holder.reserve(identity(4)); fatalError("future generation") } catch Holder.Error.generation {}
    do { _ = try holder.reserve(identity(3, owner: other.owner)); fatalError("wrong owner") } catch Holder.Error.owner {}
    do { _ = try holder.reserve(identity(3, slot: 64)); fatalError("invalid slot") } catch Holder.Error.slot {}
    let foreignPeerRoute = OrderedNotificationRoute(peer: "other", serviceUUID: "s", serviceOccurrence: 0,
      characteristicUUID: "c", characteristicOccurrence: 0)
    let badSpec = Holder.Spec(connectionGeneration: "connection", databaseGeneration: "database", snapshot: spec.snapshot,
      members: [.init(id: 1, route: foreignPeerRoute)], itemCapacity: 3, byteCapacity: 4)
    // Old retirement is identified solely by slot, even after reuse.
    try require(holder.retire(identity(1, spec: badSpec)) == retired)
    do { _ = try holder.reserve(identity(3, spec: badSpec)); fatalError("peer mismatch") } catch Holder.Error.identity {}
    do { _ = try holder.reserve(.init(version: 1, owner: holder.owner, slot: 0, generation: 3,
      requestID: 1, groupID: 1, peer: "p", spec: spec)); fatalError("wrong version") } catch Holder.Error.version {}
    precondition(Holder.nextGeneration(after: UInt64.max) == nil)
    let third = identity(3)
    try require(holder.reserve(third) == .reserved)
    let changed = Holder.Spec(connectionGeneration: "changed", databaseGeneration: "database", snapshot: spec.snapshot,
      members: spec.members, itemCapacity: spec.itemCapacity, byteCapacity: spec.byteCapacity)
    do { _ = try holder.fence(identity(3, spec: changed), failure: fenceFailure); fatalError("wrong fingerprint") }
    catch Holder.Error.conflict {}
    try require(holder.fence(third, failure: fenceFailure) == .failed(fenceFailure, .notCreated))
    try holder.retire(third)
    for generation in UInt64(4)...UInt64(300) {
      let request = identity(generation, generation, generation)
      try require(holder.reserve(request) == .reserved)
      precondition(holder.begin(request))
      precondition(holder.complete(request, outcome: failed))
      try holder.retire(request)
    }
    // The holder has 64 independent bounded records; fence each without an open.
    for slot in UInt32(1)..<UInt32(64) {
      let request = identity(1, UInt64(slot), UInt64(slot), slot: slot)
      try require(holder.fence(request, failure: fenceFailure) == .failed(fenceFailure, .notCreated))
      try holder.retire(request)
    }
    // A blocked installation does not hold the admission lock or delay recovery.
    let concurrent = identity(1, 400, 400, slot: 1)
    _ = try holder.reserve(identity(2, 401, 401, slot: 1))
    let active = identity(2, 401, 401, slot: 1)
    let started = DispatchSemaphore(value: 0)
    let finish = DispatchSemaphore(value: 0)
    let finished = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
      precondition(holder.begin(active))
      started.signal()
      finish.wait()
      precondition(holder.complete(active, outcome: failed))
      finished.signal()
    }
    precondition(started.wait(timeout: .now() + 5) == .success)
    try require(holder.fence(active, failure: fenceFailure) == .pending)
    try holder.retire(concurrent) // stale acknowledgement does not touch active
    finish.signal()
    precondition(finished.wait(timeout: .now() + 5) == .success)
    try require(holder.fence(active, failure: fenceFailure) == failed)
    try holder.retire(active)
    let unresolved = identity(301)
    _ = try holder.reserve(unresolved)
    precondition(holder.begin(unresolved))
    precondition(holder.complete(unresolved, outcome: .indeterminate(fenceFailure)))
    do { try holder.retire(unresolved); fatalError("indeterminate slot retired") } catch Holder.Error.notTerminal {}
    precondition(holder.complete(unresolved, outcome: failed))
    try holder.retire(unresolved)
  }

  static func main() throws {
    try testLateFailureBudgetHeldUntilBatchAcknowledgement()
    try testBatchProtocolOwnership()
    try testLateFailureAcknowledgement()
    try testDisconnectOnlyRetiresOwningBridge()
    try testWakeRouteIsCapturedAtOpen()
    try testAdmissionSlots()
    let queue = DispatchQueue(label: "ordered.bridge.harness")
    let wakes = DispatchSemaphore(value: 0)
    let a = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
                                     characteristicUUID: "c", characteristicOccurrence: 0)
    let b = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0,
                                     characteristicUUID: "c", characteristicOccurrence: 1)
    // Recovery must overtake an open held behind a peer graph permit, and a
    // late deferred closure must remain fenced even after an acknowledgement.
    let admissionBridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { _, _, _, _ in
      fatalError("fenced open reached the factory")
    }, wake: { _, _ in })
    let open = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: 101, groupID: 201, peer: "p", fingerprint: "exact")
    let token = try admissionBridge.reserveOpen(open)
    do { _ = try admissionBridge.reserveOpen(open); fatalError("duplicate admission") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionConflict {}
    let status = try admissionBridge.openStatus(open, token: token)
    precondition(status == .pending, "observation must not fence an intended open")
    let otherHost = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { _, _, _, _ in fatalError("unused") }, wake: { _, _ in })
    precondition(admissionBridge.admissionOwner(version: 1).isEmpty)
    precondition(admissionBridge.admissionOwner(version: 2).isEmpty)
    let slotOwner = admissionBridge.admissionOwner(version: 3)
    precondition(UUID(uuidString: slotOwner) != nil, "v3 must negotiate the actual slot owner")
    precondition(slotOwner == admissionBridge.admissionOwner(version: 3))
    precondition(slotOwner != otherHost.admissionOwner(version: 3))

    do { try otherHost.acknowledgeOpen(open, token: token); fatalError("cross-host acknowledgement") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.unknownOpenReceipt {}
    let changed = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: 101, groupID: 201, peer: "other", fingerprint: "exact")
    do { _ = try admissionBridge.fenceOpen(changed, token: token); fatalError("wrong peer fenced the open") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.unknownOpenReceipt {}
    let deferred = OwnedGraphDeferredQueue(maxItems: 4, maxBytes: 4)
    var resumed: (() -> Void)?
    precondition(deferred.enqueueBarrier { resumed = $0 })
    _ = deferred.enqueueRequest(bytes: 0, run: {
      precondition(!admissionBridge.beginOpen(open, token: token), "late open crossed a recovery fence")
    }, onReject: { fatalError("unexpected rejection") })
    deferred.drain()
    let firstFence = try admissionBridge.fenceOpen(groupID: 201)
    precondition(firstFence == .notCreated)
    let replayFence = try admissionBridge.fenceOpen(groupID: 201)
    precondition(replayFence == .notCreated, "lost recovery response must replay")
    try admissionBridge.acknowledgeOpen(open, token: token)
    try admissionBridge.acknowledgeOpen(open, token: token)
    resumed?()
    let cancelled = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: 102, groupID: 202, peer: "p", fingerprint: "cancel")
    let cancelledToken = try admissionBridge.reserveOpen(cancelled)
    admissionBridge.cancelOpen(cancelled, token: cancelledToken)
    let cancelledFence = try admissionBridge.fenceOpen(groupID: 202)
    precondition(cancelledFence == .notCreated)
    precondition(!admissionBridge.beginOpen(cancelled, token: cancelledToken))
    try admissionBridge.acknowledgeOpen(cancelled, token: cancelledToken)
    do { _ = try admissionBridge.reserveOpen(cancelled); fatalError("acknowledged fence admitted a delayed submission") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionConflict {}
    let executing = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: 103, groupID: 203, peer: "p", fingerprint: "partial")
    let executingToken = try admissionBridge.reserveOpen(executing)
    precondition(admissionBridge.beginOpen(executing, token: executingToken))
    let pendingFence = try admissionBridge.fenceOpen(groupID: 203)
    precondition(pendingFence == .pending)
    let partial = UnifiedBleRustOrderedGroupBridge.Descriptor(groupID: 203, opaqueSourceGeneration: "partial", members: [])
    admissionBridge.completeOpen(executing, token: executingToken, outcome: .created(partial))
    let partialFence = try admissionBridge.fenceOpen(groupID: 203)
    precondition(partialFence == .created(partial))
    try admissionBridge.acknowledgeOpen(executing, token: executingToken)
    // A lost ack after slot reuse is a no-op for the replacement.
    let replacementOpen = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: 104, groupID: 203, peer: "p", fingerprint: "replacement")
    let replacementToken = try admissionBridge.reserveOpen(replacementOpen)
    try admissionBridge.acknowledgeOpen(executing, token: executingToken)
    precondition(admissionBridge.beginOpen(replacementOpen, token: replacementToken))
    admissionBridge.completeOpen(replacementOpen, token: replacementToken, outcome: .notCreated)
    try admissionBridge.acknowledgeOpen(replacementOpen, token: replacementToken)
    var capacityTickets: [(UnifiedBleRustOrderedGroupBridge.OpenRequest, UnifiedBleRustOrderedGroupBridge.OpenToken)] = []
    for id in UInt64(1000)..<UInt64(1064) {
      let request = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: id, groupID: id, peer: "p", fingerprint: id)
      capacityTickets.append((request, try admissionBridge.reserveOpen(request)))
    }
    let overflowRequest = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: 2000, groupID: 2000, peer: "p", fingerprint: "overflow")
    do { _ = try admissionBridge.reserveOpen(overflowRequest); fatalError("receipt capacity must be bounded") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionUnavailable {}
    guard let refusal = admissionBridge.openReceiptIdentity(groupID: overflowRequest.groupID) else {
      fatalError("lost capacity refusal must have a recoverable receipt")
    }
    let refusalOutcome = try admissionBridge.fenceOpen(refusal.request, token: refusal.token)
    precondition(refusalOutcome == .notCreated)
    let (capacityRequest, capacityToken) = capacityTickets.removeFirst()
    admissionBridge.cancelOpen(capacityRequest, token: capacityToken)
    try admissionBridge.acknowledgeOpen(capacityRequest, token: capacityToken)
    do { _ = try admissionBridge.reserveOpen(overflowRequest); fatalError("retry admitted a refused submission") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionConflict {}
    try admissionBridge.acknowledgeOpen(refusal.request, token: refusal.token)
    try admissionBridge.acknowledgeOpen(refusal.request, token: refusal.token)
    do { _ = try admissionBridge.reserveOpen(overflowRequest); fatalError("late refused submission was admitted after acknowledgement") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionConflict {}
    for (request, token) in capacityTickets {
      admissionBridge.cancelOpen(request, token: token)
      try admissionBridge.acknowledgeOpen(request, token: token)
    }
    let boundedBridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { _, _, _, _ in fatalError("unused") }, wake: { _, _ in })
    for id in UInt64(1)...UInt64(64) {
      _ = try boundedBridge.reserveOpen(.init(requestID: id, groupID: id, peer: "p", fingerprint: id))
    }
    for id in UInt64(65)...UInt64(128) {
      let request = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: id, groupID: id, peer: "p", fingerprint: id)
      do { _ = try boundedBridge.reserveOpen(request); fatalError("full admission table") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionUnavailable {}
      guard let receipt = boundedBridge.openReceiptIdentity(groupID: id) else { fatalError("refusal receipt missing") }
      try boundedBridge.acknowledgeOpen(request, token: receipt.token)
    }
    let untracked = UnifiedBleRustOrderedGroupBridge.OpenRequest(requestID: 129, groupID: 129, peer: "p", fingerprint: 129)
    do { _ = try boundedBridge.reserveOpen(untracked); fatalError("refusal ledger must be bounded") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.openAdmissionUntracked {}
    do { _ = try boundedBridge.fenceOpen(groupID: 129); fatalError("missing receipt is not evidence of absence") }
    catch UnifiedBleRustOrderedGroupBridge.Failure.unknownOpenReceipt {}
    let knownError = NSError(domain: "pre-install", code: 42)
    let refusedBridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { _, _, _, _ in
      throw UnifiedBleRustOrderedGroupBridge.FactoryNoEffect(underlying: knownError)
    }, wake: { _, _ in })
    queue.sync {
      do {
        _ = try refusedBridge.open(groupID: 89, members: [.init(id: 1, route: a)], capacity: 2, byteCapacity: 8)
        fatalError("factory refusal was lost")
      } catch let failure as UnifiedBleRustOrderedGroupBridge.FactoryNoEffect {
        let actual = failure.underlying as NSError
        precondition(actual.domain == "pre-install" && actual.code == 42, "known refusal must preserve its cause")
      } catch { fatalError("known no-effect classification was lost: \(error)") }
    }
    let uncertainSource = OwnedOrderedNotificationSource(queue: queue, generation: "uncertain-source")
    let uncertainBridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: queue, generation: uncertainSource.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard uncertainSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      throw NSError(domain: "post-install", code: 1)
    }, wake: { _, _ in })
    queue.sync {
      do {
        _ = try uncertainBridge.open(groupID: 90, members: [.init(id: 1, route: a)], capacity: 2, byteCapacity: 8)
        fatalError("factory did not fail")
      } catch {
        precondition(String(describing: error).contains("factoryIndeterminate"), "factory failure must retain uncertain ownership")
      }
    }
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "native-opaque-uuid")
    var lastIngress: OwnedOrderedNotificationIngress?
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard source.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      lastIngress = group
      return (source, group)
    }, wake: { _, _ in wakes.signal() })
    func take(_ groupID: UInt64) throws -> [UnifiedBleRustOrderedGroupBridge.Record] {
      guard let batch = try bridge.drain(groupID: groupID) else { return [] }
      try bridge.acknowledge(groupID: groupID, batchID: batch.id)
      return batch.records
    }

    try queue.sync {
      let descriptor = try bridge.open(groupID: 7, members: [.init(id: 11, route: a), .init(id: 12, route: b)], capacity: 8, byteCapacity: 32)
      precondition(descriptor.opaqueSourceGeneration == "native-opaque-uuid")
      try bridge.validateSource(groupID: 7, opaqueSourceGeneration: descriptor.opaqueSourceGeneration)
      do { try bridge.validateSource(groupID: 7, opaqueSourceGeneration: "old"); fatalError("stale source") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.staleGeneration {}
      let input = UnifiedBleRustOrderedGroupBridge.MarkerInput(connection: "connection", snapshot: .init(owner: "o", attachment: 1, revision: 1), route: a, value: Data([9]), withResponse: true)
      let firstMarker = try bridge.beginMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      precondition(firstMarker == nil)
      let pending = try bridge.beginMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      precondition(pending?.admission == .pending && pending?.write == .initiating)
      do { try bridge.forgetMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0); fatalError("unsettled receipt") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.markerNotSettled {}
      let initialA = descriptor.members[0]
      let initialB = descriptor.members[1]
      let aClaim = try bridge.retainPhysicalClaim(groupID: 7, memberID: 11, sourceMember: initialA.sourceMember, epoch: initialA.epoch)
      let retriedClaim = try bridge.retainPhysicalClaim(groupID: 7, memberID: 11, sourceMember: initialA.sourceMember, epoch: initialA.epoch)
      precondition(retriedClaim == aClaim)
      let bClaim = try bridge.retainPhysicalClaim(groupID: 7, memberID: 12)
      source.admit(route: a, bytes: Data([1]), provenance: .notification)
      try bridge.admitMarker(groupID: 7, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0)
      try bridge.markerAdmitted(groupID: 7, id: 0, ordinal: 1)
      let initiating = try bridge.markerStatus(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0)
      precondition(initiating.admission == .admitted(1) && initiating.write == .initiating)
      _ = try bridge.markerStarted(groupID: 7, id: 0)
      // A lost waiter retries the same identity without a second dispatch.
      let retriedStarted = try bridge.beginMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      precondition(retriedStarted?.write == .started(nil))
      let changedInput = UnifiedBleRustOrderedGroupBridge.MarkerInput(connection: "other", snapshot: input.snapshot, route: a, value: input.value, withResponse: true)
      do { _ = try bridge.beginMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0, input: changedInput); fatalError("changed retry") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.markerMismatch {}
      try bridge.markerSettled(groupID: 7, id: 0, failure: nil)
      let settledRetry = try bridge.beginMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      precondition(settledRetry?.write == .settled(nil))
      source.admit(route: b, bytes: Data([2]), provenance: .readOrNotification)
      try bridge.retire(groupID: 7, expected: initialA)
      try bridge.retire(groupID: 7, expected: initialA)
      let replacementMember = try bridge.register(groupID: 7, expected: initialA)
      let registerRetry = try bridge.register(groupID: 7, expected: initialA)
      precondition(registerRetry == replacementMember)
      do { try bridge.retire(groupID: 7, expected: initialA); fatalError("stale retire changed replacement") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.inactiveMember {}
      do {
        _ = try bridge.retainPhysicalClaim(groupID: 7, memberID: 11, sourceMember: initialA.sourceMember, epoch: initialA.epoch)
        fatalError("stale retain must not claim replacement")
      } catch UnifiedBleRustOrderedGroupBridge.Failure.inactiveMember {}
      let replacement = try bridge.retainPhysicalClaim(groupID: 7, memberID: 11)
      precondition(aClaim != replacement)
      source.admit(route: a, bytes: Data([3]), provenance: .notification)
      guard let firstBatch = try bridge.drain(groupID: 7) else { fatalError("missing first batch") }
      let replay = try bridge.drain(groupID: 7)
      precondition(replay == firstBatch, "unacknowledged drain must replay exactly")
      do { try bridge.acknowledge(groupID: 7, batchID: firstBatch.id + 1); fatalError("wrong ack") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      try bridge.acknowledge(groupID: 7, batchID: firstBatch.id)
      try bridge.forgetMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0)
      try bridge.forgetMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0)
      do { _ = try bridge.beginMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 0, input: input); fatalError("retired marker redispatched") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.markerStale {}
      _ = try bridge.beginMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 1, input: input)
      let refusal = UnifiedBleRustOrderedGroupBridge.MarkerFailure(kind: "busy", gattStatus: nil, domain: "owned", code: 1014, detail: "busy", dispatched: false)
      let refused = try bridge.markerRejected(groupID: 7, id: 1, failure: refusal)
      precondition(refused.admission == .notAdmitted && refused.write == .notStarted(refusal))
      let refusalRetry = try bridge.beginMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 1, input: input)
      precondition(refusalRetry == refused)
      try bridge.forgetMarker(groupID: 7, generation: descriptor.opaqueSourceGeneration, id: 1)
      try bridge.acknowledge(groupID: 7, batchID: firstBatch.id)
      let records = firstBatch.records
      precondition(records == [.value(memberID: 11, sourceMember: initialA.sourceMember, epoch: initialA.epoch, ordinal: 0, value: Data([1]), provenance: .notification),
        .marker(id: 0, ordinal: 1),
        .value(memberID: 12, sourceMember: initialB.sourceMember, epoch: initialB.epoch, ordinal: 2, value: Data([2]), provenance: .readOrNotification),
        .fence(memberID: 11, sourceMember: initialA.sourceMember, epoch: initialA.epoch, platformTail: .uncertain),
        .value(memberID: 11, sourceMember: replacementMember.sourceMember, epoch: replacementMember.epoch, ordinal: 3, value: Data([3]), provenance: .notification)])
      do { try bridge.seal(groupID: 7); fatalError("live source must not seal") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.source(.inactive) {}
      try bridge.close(groupID: 7)
      try bridge.seal(groupID: 7)
      try bridge.seal(groupID: 7)
      try bridge.failWake(groupID: 7, opaqueSourceGeneration: descriptor.opaqueSourceGeneration)
      let retainedTerminal = try bridge.drain(groupID: 7)
      let replayTerminal = try bridge.drain(groupID: 7)
      precondition(retainedTerminal == replayTerminal, "seal must retain unacknowledged terminal")
      guard let terminalBatch = retainedTerminal else { fatalError("missing sealed terminal") }
      try bridge.acknowledge(groupID: 7, batchID: terminalBatch.id)
      let afterSeal = try bridge.drain(groupID: 7)
      precondition(afterSeal == nil, "sealed source admitted a later control")
      let closed = terminalBatch.records
      precondition(closed == [.terminal(.closed, platformTail: .uncertain, rejected: nil)])
      do { try bridge.remove(groupID: 7); fatalError("must retain physical claims") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      do {
        try bridge.confirmPhysicalCleanup(groupID: 7, memberID: 11, opaqueSourceGeneration: "stale-generation", sourceMember: initialA.sourceMember, epoch: initialA.epoch)
        fatalError("wrong source generation must not confirm cleanup")
      } catch UnifiedBleRustOrderedGroupBridge.Failure.unknownPhysicalClaim {}
      do {
        try bridge.confirmPhysicalCleanup(groupID: 7, memberID: 11, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, sourceMember: initialA.sourceMember, epoch: replacementMember.epoch)
        fatalError("wrong activation epoch must not confirm cleanup")
      } catch UnifiedBleRustOrderedGroupBridge.Failure.unknownPhysicalClaim {}
      try bridge.confirmPhysicalCleanup(groupID: 7, memberID: 11, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, sourceMember: initialA.sourceMember, epoch: initialA.epoch)
      try bridge.confirmPhysicalCleanup(groupID: 7, memberID: 11, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, sourceMember: initialA.sourceMember, epoch: initialA.epoch)
      try bridge.confirmPhysicalCleanup(bClaim)
      do { try bridge.remove(groupID: 7); fatalError("replacement still owns cleanup") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      try bridge.confirmPhysicalCleanup(replacement)
      try bridge.remove(groupID: 7)
      try bridge.remove(groupID: 7, opaqueSourceGeneration: descriptor.opaqueSourceGeneration)
      do { try bridge.remove(groupID: 7, opaqueSourceGeneration: "wrong"); fatalError("wrong removal generation") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.staleGeneration {}
      do { _ = try bridge.open(groupID: 7, members: [.init(id: 11, route: a)], capacity: 8, byteCapacity: 32); fatalError("unacknowledged removal must reserve identity") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.duplicateGroup {}
      do { try bridge.acknowledgeRemoval(groupID: 7, opaqueSourceGeneration: "wrong"); fatalError("wrong receipt generation") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.staleGeneration {}
      try bridge.acknowledgeRemoval(groupID: 7, opaqueSourceGeneration: descriptor.opaqueSourceGeneration)
      try bridge.acknowledgeRemoval(groupID: 7, opaqueSourceGeneration: descriptor.opaqueSourceGeneration)

      let descA = try bridge.open(groupID: 8, members: [.init(id: 21, route: a)], capacity: 1, byteCapacity: 8)
      let descB = try bridge.open(groupID: 9, members: [.init(id: 22, route: a)], capacity: 2, byteCapacity: 8)
      let sharedA = try bridge.retainPhysicalClaim(groupID: 8, memberID: 21)
      let sharedB = try bridge.retainPhysicalClaim(groupID: 9, memberID: 22)
      source.admit(route: a, bytes: Data([4]), provenance: .notification)
      source.admit(route: a, bytes: Data([5]), provenance: .readOrNotification)
      let overflow = try take(8)
      precondition(overflow == [.value(memberID: 21, sourceMember: descA.members[0].sourceMember, epoch: descA.members[0].epoch, ordinal: 4, value: Data([4]), provenance: .notification),
        .terminal(.overflow, platformTail: .uncertain, rejected: .value(memberID: 21, sourceMember: descA.members[0].sourceMember, epoch: descA.members[0].epoch, bytes: 1))])
      do { try bridge.remove(groupID: 8); fatalError("overflow cleanup retained") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      try bridge.confirmPhysicalCleanup(sharedA)
      try bridge.seal(groupID: 8)
      try bridge.remove(groupID: 8)
      source.admit(route: a, bytes: Data([6]), provenance: .notification)
      let other = try take(9)
      precondition(other == [.value(memberID: 22, sourceMember: descB.members[0].sourceMember, epoch: descB.members[0].epoch, ordinal: 4, value: Data([4]), provenance: .notification),
        .value(memberID: 22, sourceMember: descB.members[0].sourceMember, epoch: descB.members[0].epoch, ordinal: 5, value: Data([5]), provenance: .readOrNotification),
        .terminal(.overflow, platformTail: .uncertain, rejected: .value(memberID: 22, sourceMember: descB.members[0].sourceMember, epoch: descB.members[0].epoch, bytes: 1))])
      try bridge.confirmPhysicalCleanup(sharedB)
      try bridge.seal(groupID: 9)
      try bridge.remove(groupID: 9)

      let lossDescriptor = try bridge.open(groupID: 10, members: [.init(id: 23, route: a)], capacity: 1, byteCapacity: 8)
      source.admit(route: a, bytes: Data([7, 8]), provenance: .notification)
      lastIngress?.reportLoss(member: a, count: UInt64.max)
      let loss = try take(10)
      guard case let .value(member, sourceMember, epoch, _, value, .notification) = loss[0] else { fatalError("value") }
      precondition(member == 23 && sourceMember == lossDescriptor.members[0].sourceMember && epoch == lossDescriptor.members[0].epoch && value == Data([7, 8]))
      precondition(loss[1] == .terminal(.overflow, platformTail: .uncertain,
        rejected: .sourceLoss(memberID: 23, skippedRecords: UInt64.max)))
      try bridge.seal(groupID: 10)
      try bridge.remove(groupID: 10)

      let failureDescriptor = try bridge.open(groupID: 12, members: [.init(id: 25, route: a)], capacity: 2, byteCapacity: 8)
      _ = try bridge.open(groupID: 13, members: [.init(id: 26, route: b)], capacity: 2, byteCapacity: 8)
      source.fail(route: a, failure: .init(domain: "CBFixture", code: 42))
      let failed = try take(12)
      precondition(failed == [.terminal(.sourceFailed(.init(domain: "CBFixture", code: 42), memberID: 25, sourceMember: failureDescriptor.members[0].sourceMember, epoch: failureDescriptor.members[0].epoch), platformTail: .uncertain, rejected: nil)])
      try bridge.seal(groupID: 12)
      try bridge.remove(groupID: 12)
      source.admit(route: b, bytes: Data([10]), provenance: .notification)
      let unaffected = try take(13)
      guard case .value(memberID: 26, sourceMember: _, epoch: _, ordinal: _, value: let unaffectedValue, provenance: .notification) = unaffected.first else { fatalError("unrelated group") }
      precondition(unaffectedValue == Data([10]))
      try bridge.close(groupID: 13)
      _ = try take(13)
      try bridge.seal(groupID: 13)
      try bridge.remove(groupID: 13)

      let barrierGroup = try bridge.open(groupID: 14, members: [.init(id: 27, route: a)], capacity: 4, byteCapacity: 8)
      let exact = barrierGroup.members[0]
      let claim = try bridge.retainPhysicalClaim(groupID: 14, memberID: 27, sourceMember: exact.sourceMember, epoch: exact.epoch)
      try bridge.confirmPhysicalCleanup(claim)
      do { _ = try bridge.retainPhysicalClaim(groupID: 14, memberID: 27, sourceMember: exact.sourceMember, epoch: exact.epoch); fatalError("confirmed epoch cannot be retained again") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.alreadyRetained {}
      try bridge.confirmPhysicalCleanup(claim)
      source.admit(route: a, bytes: Data([1]), provenance: .notification)
      try bridge.barrier(groupID: 14, id: 99)
      try bridge.barrier(groupID: 14, id: 99)
      source.admit(route: a, bytes: Data([2]), provenance: .notification)
      let barrierBatch = try bridge.drain(groupID: 14)!
      precondition(barrierBatch.records.count == 3)
      guard case .value(_, _, _, _, let before, _) = barrierBatch.records[0], case .barrier(99, .uncertain) = barrierBatch.records[1], case .value(_, _, _, _, let after, _) = barrierBatch.records[2] else { fatalError("barrier order") }
      precondition(before == Data([1]) && after == Data([2]))
      try bridge.acknowledge(groupID: 14, batchID: barrierBatch.id)
      try bridge.barrier(groupID: 14, id: 99)
      let duplicateBarrier = try bridge.drain(groupID: 14)
      precondition(duplicateBarrier == nil)
      try bridge.retire(groupID: 14, expected: exact)
      _ = try take(14)
      let next = try bridge.register(groupID: 14, expected: exact)
      try bridge.retire(groupID: 14, expected: next)
      _ = try take(14)
      let newest = try bridge.register(groupID: 14, expected: next)
      do { _ = try bridge.register(groupID: 14, expected: exact); fatalError("stale register returned newer binding") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.inactiveMember {}
      let nextClaim = try bridge.retainPhysicalClaim(groupID: 14, memberID: 27, sourceMember: newest.sourceMember, epoch: newest.epoch)
      try bridge.confirmPhysicalCleanup(claim)
      try bridge.close(groupID: 14)
      _ = try take(14)
      do { try bridge.remove(groupID: 14); fatalError("stale confirmation released replacement") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      try bridge.confirmPhysicalCleanup(nextClaim)
      try bridge.seal(groupID: 14)
      try bridge.remove(groupID: 14)

      _ = try bridge.open(groupID: 15, members: [.init(id: 28, route: a)], capacity: 1, byteCapacity: 8)
      source.admit(route: a, bytes: Data([1]), provenance: .notification)
      do { try bridge.barrier(groupID: 15, id: 100); fatalError("barrier overflow") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.source(.controlOverflow) {}
      let overflowBarrier = try take(15)
      precondition(overflowBarrier.last == .terminal(.overflow, platformTail: .uncertain, rejected: .barrier(id: 100)))
      try bridge.seal(groupID: 15)
      try bridge.remove(groupID: 15)

      let unretained = try bridge.open(groupID: 16, members: [.init(id: 29, route: a)], capacity: 2, byteCapacity: 8).members[0]
      try bridge.retire(groupID: 16, memberID: 29)
      _ = try take(16)
      try bridge.confirmPhysicalCleanup(groupID: 16, memberID: 29, opaqueSourceGeneration: "native-opaque-uuid", sourceMember: unretained.sourceMember, epoch: unretained.epoch)
      try bridge.confirmPhysicalCleanup(groupID: 16, memberID: 29, opaqueSourceGeneration: "native-opaque-uuid", sourceMember: unretained.sourceMember, epoch: unretained.epoch)
      try bridge.close(groupID: 16)
      try bridge.close(groupID: 16)
      _ = try take(16)
      try bridge.seal(groupID: 16)
      try bridge.remove(groupID: 16)

      _ = try bridge.open(groupID: 11, members: [.init(id: 24, route: a)], capacity: 2, byteCapacity: 8)
      source.admit(route: a, bytes: Data([9]), provenance: .notification)
      source.invalidate(.connectionLost)
      let lifecycle = try take(11)
      precondition(lifecycle.count == 2)
      precondition(lifecycle[1] == .terminal(.connectionLost, platformTail: .uncertain, rejected: nil))
      try bridge.seal(groupID: 11)
      try bridge.remove(groupID: 11)
    }
    queue.sync {}
    precondition(wakes.wait(timeout: .now()) == .timedOut, "removed groups must not emit stale wakes")
    let wakeSource = OwnedOrderedNotificationSource(queue: queue, generation: "wake-generation")
    let identities = OwnedGattSnapshotTracker()
    let identityBridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: queue, generation: wakeSource.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard wakeSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      return (wakeSource, group)
    }, validateSnapshot: { peer, identity in identities.matches(peer, identity) }, wake: { _, _ in })
    try queue.sync {
      precondition(identities.attach(a.peer))
      let first = identities.publish(a.peer)!
      identities.invalidate(a.peer)
      do { _ = try identityBridge.open(groupID: 50, members: [.init(id: 50, route: a)], capacity: 2, byteCapacity: 8, snapshot: first); fatalError("service change must fence stale graph") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.staleGeneration {}
      let second = identities.publish(a.peer)!
      precondition(first != second)
      _ = try identityBridge.open(groupID: 50, members: [.init(id: 50, route: a)], capacity: 2, byteCapacity: 8, snapshot: second)
      try identityBridge.close(groupID: 50)
      guard let batch = try identityBridge.drain(groupID: 50) else { fatalError("missing terminal") }
      try identityBridge.acknowledge(groupID: 50, batchID: batch.id)
      try identityBridge.seal(groupID: 50)
      try identityBridge.remove(groupID: 50)
      identities.disconnect(a.peer)
      precondition(identities.attach(a.peer))
      let replacement = identities.publish(a.peer)!
      precondition(replacement.attachment != second.attachment)
    }
    let immediateRollbackSource = OwnedOrderedNotificationSource(queue: queue, generation: "immediate-rollback")
    let immediateRollback = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: queue, generation: immediateRollbackSource.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard immediateRollbackSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      immediateRollbackSource.invalidate(.connectionLost)
      return (immediateRollbackSource, group)
    }, wake: { _, _ in })
    try queue.sync {
      do { _ = try immediateRollback.open(groupID: 39, members: [.init(id: 39, route: a)], capacity: 2, byteCapacity: 8); fatalError("failed registration") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.sourceClaimExhausted { }
    }
    let failedSource = OwnedOrderedNotificationSource(queue: queue, generation: "rollback-generation")
    var refuseRemoval = true
    let rollbackBridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: queue, generation: failedSource.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard failedSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      failedSource.invalidate(.connectionLost)
      return (failedSource, group)
    }, removeSourceGroup: { source, group in
      if refuseRemoval { return false }
      return source.remove(group)
    }, wake: { _, _ in })
    try queue.sync {
      do { _ = try rollbackBridge.open(groupID: 40, members: [.init(id: 40, route: a)], capacity: 2, byteCapacity: 8); fatalError("failed rollback") }
      catch let UnifiedBleRustOrderedGroupBridge.Failure.openIndeterminate(generation, groupID, members, cause) {
        precondition(generation == "rollback-generation" && groupID == 40 && members.isEmpty)
        precondition(cause.detail.contains("sourceClaimExhausted"), "partial rollback must retain the original failure")
      }
      refuseRemoval = false
      guard let batch = try rollbackBridge.drain(groupID: 40) else { fatalError("rollback must retain terminal") }
      precondition(batch.records == [.terminal(.connectionLost, platformTail: .uncertain, rejected: nil)])
      try rollbackBridge.acknowledge(groupID: 40, batchID: batch.id)
      try rollbackBridge.seal(groupID: 40)
      try rollbackBridge.remove(groupID: 40, opaqueSourceGeneration: "rollback-generation")
    }
    let wakeBridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let group = try OwnedOrderedNotificationIngress(queue: queue, generation: wakeSource.generation,
        members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      guard wakeSource.add(group) else { throw OwnedOrderedNotificationIngress.Failure.inactive }
      return (wakeSource, group)
    }, wake: { id, generation in
      precondition(id == 30 && generation == "wake-generation")
      wakes.signal()
    })
    try queue.sync {
      _ = try wakeBridge.open(groupID: 30, members: [.init(id: 30, route: a)], capacity: 2, byteCapacity: 8)
      wakeSource.admit(route: a, bytes: Data([1]), provenance: .notification)
    }
    precondition(wakes.wait(timeout: .now() + 2) == .success, "live admission must schedule a generation-scoped wake")
    let firstWakeBatch = try queue.sync { try wakeBridge.drain(groupID: 30)! }
    queue.sync { wakeSource.admit(route: a, bytes: Data([2]), provenance: .notification) }
    precondition(wakes.wait(timeout: .now() + 2) == .success, "second value wakes while first batch is retained")
    try queue.sync {
      let replay = try wakeBridge.drain(groupID: 30)
      precondition(replay == firstWakeBatch, "host can consume second wake while first batch is still pending")
      try wakeBridge.acknowledge(groupID: 30, batchID: firstWakeBatch.id)
    }
    precondition(wakes.wait(timeout: .now() + 2) == .success, "ack must re-wake for the still-buffered second value")
    try queue.sync { try wakeBridge.acknowledge(groupID: 30, batchID: firstWakeBatch.id) }
    precondition(wakes.wait(timeout: .now() + 2) == .success, "an uncertain ack retry must re-wake still-buffered work")
    try queue.sync {
      guard let secondBatch = try wakeBridge.drain(groupID: 30) else { fatalError("missing second batch") }
      guard case .value(memberID: 30, sourceMember: _, epoch: _, ordinal: 1, value: let second, provenance: .notification) = secondBatch.records.first else { fatalError("second value") }
      precondition(second == Data([2]))
      try wakeBridge.acknowledge(groupID: 30, batchID: secondBatch.id)
    }
    print("Ordered Rust group bridge: source FIFO, provenance, fences, overflow and exact cleanup passed")
  }
}
