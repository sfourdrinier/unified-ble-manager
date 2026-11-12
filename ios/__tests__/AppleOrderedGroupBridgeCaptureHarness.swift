import Foundation

@main enum AppleOrderedGroupBridgeCaptureHarness {
  static func main() throws {
    if CommandLine.arguments.contains("--reserved-value-before-capture") {
      try testReservedValueBeforeCapture()
      return
    }
    if CommandLine.arguments.contains("--capture-finality") {
      try testPendingCaptureFinality()
      return
    }
    if CommandLine.arguments.contains("--first-capture") {
      try testFirstCapturedFailure()
      return
    }
    if CommandLine.arguments.contains("--bind-recovery") {
      try testBindRecovery()
      return
    }
    if CommandLine.arguments.contains("--capture-order") {
      try testCaptureOrder()
      return
    }
    if CommandLine.arguments.contains("--capture-cancel-order") {
      try testCaptureCancellationOrder()
      return
    }
    if CommandLine.arguments.contains("--capture-retire-order") {
      try testCaptureRetirementOrder()
      return
    }
    if CommandLine.arguments.contains("--capture-source-order") {
      try testCaptureSourceOrder()
      return
    }
    if CommandLine.arguments.contains("--capture-source-cancel-ack") {
      try testCaptureSourceCancellationAcknowledgement()
      return
    }
    if CommandLine.arguments.contains("--capture-invalidation-order") {
      try testCaptureInvalidationOrder()
      return
    }
    if CommandLine.arguments.contains("--capture-omission-interleave") {
      try testCaptureOmissionInterleave()
      return
    }
    if CommandLine.arguments.contains("--deferred-callback-order") {
      try testDeferredCallbackOrder()
      return
    }
    if CommandLine.arguments.contains("--deferred-callback-fifo") {
      try testDeferredCallbackFIFO()
      return
    }
    if CommandLine.arguments.contains("--deferred-callback-capture-interleave") {
      try testDeferredCallbackCaptureInterleave()
      return
    }
    if CommandLine.arguments.contains("--capture-barrier-pending") {
      try testCaptureBarrierPending()
      return
    }
    if CommandLine.arguments.contains("--capture-marker-dispatch") {
      try testCaptureMarkerDispatch()
      return
    }
    if CommandLine.arguments.contains("--capture-marker-cancel") {
      try testCaptureMarkerCancel()
      return
    }
    if CommandLine.arguments.contains("--capture-marker-failure") {
      try testCaptureMarkerFailure()
      return
    }
    let queue = DispatchQueue(label: "bridge.capture.integration")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "capture-generation")
    var factoryCalls = 0
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      factoryCalls += 1
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress))
      return (source, ingress)
    }, captureBudget: .init(maxGroups: 1, maxSlots: 1, maxBytes: 1024, slotsPerGroup: 1, bytesPerGroup: 1024), wake: { _, _ in })
    try queue.sync {
      let descriptor = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32)
      let original = descriptor.members[0]
      do {
        _ = try bridge.open(groupID: 2, members: [.init(id: 2, route: route)], capacity: 4, byteCapacity: 32)
        fatalError("admission did not reserve the actual owner")
      } catch UnifiedBleRustOrderedGroupBridge.Failure.captureCapacity {}
      precondition(factoryCalls == 1, "exhaustion must precede factory effects")
      let firstFailure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42, detail: "first")
      let first = try bridge.captureFailure(groupID: 1, expected: original, failure: firstFailure)
      guard case let .captured(firstTicket) = first else { fatalError("expected first capture") }
      try bridge.cancelCapturedFailure(groupID: 1, ticket: firstTicket)
      try bridge.retire(groupID: 1, expected: original)
      let replacement = try bridge.register(groupID: 1, expected: original)
      let fence = try bridge.drain(groupID: 1)!
      precondition(fence.records.count == 1)
      try bridge.acknowledge(groupID: 1, batchID: fence.id)
      let replacementFirst = try bridge.captureFailure(groupID: 1, expected: replacement, failure: firstFailure)
      try bridge.replayCapturedFailure(groupID: 1, capture: replacementFirst)
      let batch = try bridge.drain(groupID: 1)!
      precondition(batch.records == [.terminal(.sourceFailed(firstFailure, memberID: replacement.id, sourceMember: replacement.sourceMember, epoch: replacement.epoch), platformTail: .uncertain, rejected: nil)])
      try bridge.markVersionedBatch(groupID: 1, batchID: batch.id)
      let second = try bridge.captureFailure(groupID: 1, expected: replacement, failure: .init(domain: "CB", code: 43))
      guard case .omitted = second else { fatalError("blocked ACK must retain slot") }
      do { try bridge.acknowledgeVersionedBatch(groupID: 1, batchID: batch.id + 1); fatalError("wrong ACK") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      do { try bridge.acknowledge(groupID: 1, batchID: batch.id); fatalError("ambiguous ACK") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      let stillBlocked = try bridge.captureFailure(groupID: 1, expected: replacement, failure: .init(domain: "CB", code: 45))
      guard case .omitted = stillBlocked else { fatalError("wrong ACK must not free a capture slot") }
      try bridge.replayCapturedFailure(groupID: 1, capture: stillBlocked)
      let retry = try bridge.drain(groupID: 1)
      precondition(retry == batch)
      try bridge.acknowledgeVersionedBatch(groupID: 1, batchID: batch.id)
      try bridge.acknowledgeVersionedBatch(groupID: 1, batchID: batch.id)
      let omission = try bridge.drain(groupID: 1)!
      precondition(omission.records == [.failureObservationsOmitted])
      try bridge.acknowledge(groupID: 1, batchID: omission.id)
      let replacementFailure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 44, detail: "replacement")
      let reused = try bridge.captureFailure(groupID: 1, expected: replacement, failure: replacementFailure)
      guard case .captured = reused else { fatalError("matching ACK did not free the owner slot") }
      try bridge.replayCapturedFailure(groupID: 1, capture: reused)
      let replacementBatch = try bridge.drain(groupID: 1)!
      let claim = OrderedNotificationClaim(generation: source.generation, route: route, token: replacement.sourceMember)
      precondition(replacementBatch.records == [.subsequentSourceFailure(replacementFailure.attributed(to: claim, epoch: replacement.epoch))])
      try bridge.close(groupID: 1)
      do { try bridge.remove(groupID: 1); fatalError("retirement released captures") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      let replacementRetry = try bridge.drain(groupID: 1)
      precondition(replacementRetry == replacementBatch)
      try bridge.acknowledge(groupID: 1, batchID: replacementBatch.id)
      try bridge.seal(groupID: 1)
      let empty = try bridge.drain(groupID: 1)
      precondition(empty == nil, "capture must be delivered exactly once")
      try bridge.remove(groupID: 1)
      _ = try bridge.open(groupID: 2, members: [.init(id: 2, route: route)], capacity: 4, byteCapacity: 32)
      precondition(factoryCalls == 2, "retired empty reservation should be reusable")
    }
    try testFirstCapturedFailure()
    try testFactoryReservations()
    try testCancellationWake()
    try testReservedValueBeforeCapture()
    try testCaptureOrder()
    try testCaptureCancellationOrder()
    try testCaptureRetirementOrder()
    try testCaptureSourceOrder()
    try testCaptureSourceCancellationAcknowledgement()
    try testCaptureInvalidationOrder()
    try testCaptureOmissionInterleave()
    try testDeferredCallbackOrder()
    try testDeferredCallbackFIFO()
    try testDeferredCallbackCaptureInterleave()
    try testCaptureBarrierPending()
    try testCaptureMarkerDispatch()
    try testCaptureMarkerCancel()
    try testCaptureMarkerFailure()
    print("AppleOrderedGroupBridgeCaptureHarness passed")
  }

  static func testReservedValueBeforeCapture() throws {
    let queue = DispatchQueue(label: "bridge.reserved-value-before-capture")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "reserved-value")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      for (groupID, deliver) in [(UInt64(1), true), (UInt64(2), false)] {
        let member = try bridge.open(groupID: groupID, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
        let settle = source.reserveValue(route: route, bytes: Data([7]), provenance: .notification)
        let capture = try bridge.captureFailure(groupID: groupID, expected: member, failure: failure)
        try bridge.replayCapturedFailure(groupID: groupID, capture: capture)
        let blocked = try bridge.drain(groupID: groupID)
        precondition(blocked == nil, "later failure crossed the reserved value")
        settle(deliver); settle(!deliver)
        let first = try bridge.drain(groupID: groupID)!
        if deliver {
          precondition(first.records == [.value(memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch, ordinal: groupID - 1, value: Data([7]), provenance: .notification)])
        } else {
          precondition(first.records == [.loss(memberID: member.id, sourceRecords: 1)], "cancelled value must have explicit omission")
        }
        do { try bridge.acknowledge(groupID: groupID, batchID: first.id + 1); fatalError("wrong ACK accepted") }
        catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
        let retry = try bridge.drain(groupID: groupID)
        precondition(retry == first, "wrong ACK released reserved value")
        try bridge.acknowledge(groupID: groupID, batchID: first.id)
        let second = try bridge.drain(groupID: groupID)!
        precondition(second.records == [.terminal(.sourceFailed(failure, memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch), platformTail: .uncertain, rejected: nil)])
        try bridge.acknowledge(groupID: groupID, batchID: second.id)
      }
      let member = try bridge.open(groupID: 3, members: [.init(id: 1, route: route)], capacity: 1, byteCapacity: 32).members[0]
      let firstValue = source.reserveValue(route: route, bytes: Data([8]), provenance: .notification)
      let rejectedValue = source.reserveValue(route: route, bytes: Data([9]), provenance: .notification)
      let capture = try bridge.captureFailure(groupID: 3, expected: member, failure: failure)
      try bridge.replayCapturedFailure(groupID: 3, capture: capture)
      let blocked = try bridge.drain(groupID: 3)
      precondition(blocked == nil, "overflow and later failure must wait for the earlier value")
      rejectedValue(false); firstValue(true)
      let batch = try bridge.drain(groupID: 3)!
      precondition(batch.records == [
        .value(memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch, ordinal: 2, value: Data([8]), provenance: .notification),
        .terminal(.overflow, platformTail: .uncertain, rejected: .value(memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch, bytes: 1))
      ])
      try bridge.acknowledge(groupID: 3, batchID: batch.id)
      let late = try bridge.drain(groupID: 3)!
      precondition(late.records == [.subsequentSourceFailure(failure.attributed(to: .init(generation: source.generation, route: route, token: member.sourceMember), epoch: member.epoch))], "overflow must not erase or reclassify a later failure")
      try bridge.acknowledge(groupID: 3, batchID: late.id)

      let gatedMember = try bridge.open(groupID: 4, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let gate = OwnedOrderedGraphPermitGate(queue: queue, maxItems: 1, maxBytes: 32, overflow: {})
      let key = OwnedOrderedGraphPermitGate.Key(peer: "p", connection: "c", snapshot: .init(owner: "o", attachment: 1, revision: 1), id: 1)
      precondition(gate.acquire(key, valid: true) == .acquired)
      let accepted = source.reserveValue(route: route, bytes: Data([10]), provenance: .notification)
      precondition(gate.enqueue(bytes: 1, reject: { accepted(false) }, { accepted(true) }))
      let rejected = source.reserveValue(route: route, bytes: Data([11]), provenance: .notification)
      precondition(!gate.enqueue(bytes: 1, reject: { rejected(false) }, { rejected(true) }))
      let report = source.captureDeferredFailure(route: route, failure: failure)!
      report(true)
      let beforeRelease = try bridge.drain(groupID: 4)
      precondition(beforeRelease == nil)
      precondition(gate.release(key) == .released)
      let gated = try bridge.drain(groupID: 4)!
      precondition(gated.records == [
        .value(memberID: gatedMember.id, sourceMember: gatedMember.sourceMember, epoch: gatedMember.epoch, ordinal: 4, value: Data([10]), provenance: .notification),
        .loss(memberID: gatedMember.id, sourceRecords: 1),
        .terminal(.sourceFailed(failure, memberID: gatedMember.id, sourceMember: gatedMember.sourceMember, epoch: gatedMember.epoch), platformTail: .uncertain, rejected: nil)
      ])
      do { try bridge.acknowledge(groupID: 4, batchID: gated.id + 1); fatalError("wrong gate batch ACK accepted") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      let gatedRetry = try bridge.drain(groupID: 4)
      precondition(gatedRetry == gated)
      try bridge.acknowledge(groupID: 4, batchID: gated.id)
    }
  }

  static func testBindRecovery() throws {
    let queue = DispatchQueue(label: "bridge.capture.bind-recovery")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "oversized")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, captureBudget: .init(maxGroups: 1, maxSlots: 1, maxBytes: 4, slotsPerGroup: 1, bytesPerGroup: 4), wake: { _, _ in })
    let emptySource = OwnedOrderedNotificationSource(queue: queue, generation: "")
    let emptyBridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: emptySource.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(emptySource.add(ingress)); return (emptySource, ingress)
    }, captureBudget: .init(maxGroups: 1, maxSlots: 1, maxBytes: 4, slotsPerGroup: 1, bytesPerGroup: 4), wake: { _, _ in })
    try queue.sync {
      do {
        _ = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32)
        fatalError("oversized generation admitted")
      } catch let UnifiedBleRustOrderedGroupBridge.Failure.openIndeterminate(generation, groupID, members, _) {
        precondition(generation == source.generation && groupID == 1 && members.isEmpty)
      }
      let recovered = try bridge.recoverOpen(groupID: 1)
      precondition(recovered.opaqueSourceGeneration == source.generation && recovered.members.isEmpty)
      try bridge.close(groupID: 1)
      let terminal = try bridge.drain(groupID: 1)!
      precondition(terminal.records == [.terminal(.closed, platformTail: .uncertain, rejected: nil)])
      try bridge.acknowledge(groupID: 1, batchID: terminal.id)
      try bridge.seal(groupID: 1)
      try bridge.remove(groupID: 1)
      do {
        _ = try bridge.open(groupID: 2, members: [.init(id: 2, route: route)], capacity: 4, byteCapacity: 32)
        fatalError("oversized generation admitted")
      } catch UnifiedBleRustOrderedGroupBridge.Failure.openIndeterminate {}
      do {
        _ = try emptyBridge.open(groupID: 3, members: [.init(id: 3, route: route)], capacity: 4, byteCapacity: 32)
        fatalError("empty generation admitted")
      } catch UnifiedBleRustOrderedGroupBridge.Failure.factoryIndeterminate {}
    }
  }

  static func testCaptureOrder() throws {
    let queue = DispatchQueue(label: "bridge.capture.order")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "order")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      try bridge.barrier(groupID: 1, id: 7)
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: failure)
      try bridge.close(groupID: 1)
      let prefix = try bridge.drain(groupID: 1)!
      precondition(prefix.records == [.barrier(id: 7, platformTail: .uncertain)], "terminal must not overtake a captured failure")
      try bridge.acknowledge(groupID: 1, batchID: prefix.id)
      let blocked = try bridge.drain(groupID: 1)
      precondition(blocked == nil, "unreplayed capture must fence later records")
      try bridge.replayCapturedFailure(groupID: 1, capture: capture)
      let terminal = try bridge.drain(groupID: 1)!
      guard case .terminal(.sourceFailed, _, _) = terminal.records.first else { fatalError("captured first failure must terminate") }
    }
  }

  // This is the same source entry point used by the CoreBluetooth notification
  // state callback while its graph gate defers replay.
  static func testDeferredCallbackOrder() throws {
    let queue = DispatchQueue(label: "bridge.deferred-callback.order")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "deferred-callback-order")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      let cancel = source.captureDeferredFailure(route: route, failure: failure)!
      do { try bridge.barrier(groupID: 1, id: 7); fatalError("barrier admitted before callback settlement") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      do { try bridge.checkMarkerCaptureOrder(groupID: 1, opaqueSourceGeneration: source.generation); fatalError("marker admitted before callback settlement") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      let beforeReplay = try bridge.drain(groupID: 1)
      precondition(beforeReplay == nil, "a barrier must not cross an earlier deferred callback")
      cancel(false)
      try bridge.checkMarkerCaptureOrder(groupID: 1, opaqueSourceGeneration: source.generation)
      try bridge.barrier(groupID: 1, id: 7)
      let barrier = try bridge.drain(groupID: 1)!
      precondition(barrier.records == [.barrier(id: 7, platformTail: .uncertain)])
      try bridge.acknowledge(groupID: 1, batchID: barrier.id)
      let settle = source.captureDeferredFailure(route: route, failure: failure)!
      try bridge.barrier(groupID: 1, id: 7) // Retry of an already acknowledged barrier.
      do { try bridge.barrier(groupID: 1, id: 8); fatalError("barrier admitted before callback settlement") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      settle(true)
      let first = try bridge.drain(groupID: 1)!
      precondition(first.records.first == .terminal(.sourceFailed(failure, memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch), platformTail: .uncertain, rejected: nil))
      do { try bridge.barrier(groupID: 1, id: 8); fatalError("barrier admitted after source failure") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.source(.inactive) {}
    }
  }

  // The notification-state delegate uses this source capture entrypoint.
  // Later values and close must wait for the capture's actual settlement;
  // the returned batch must remain owned until its exact acknowledgement.
  static func testDeferredCallbackFIFO() throws {
    let queue = DispatchQueue(label: "bridge.deferred-callback.fifo")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "deferred-callback-fifo")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let settle = source.captureDeferredFailure(route: route, failure: .init(domain: "CB", code: 42))!
      source.admit(route: route, bytes: Data([7]), provenance: .notification)
      try bridge.close(groupID: 1)
      let blocked = try bridge.drain(groupID: 1)
      precondition(blocked == nil, "production deferred capture must fence later value and close")
      settle(false)
      let batch = try bridge.drain(groupID: 1)!
      precondition(batch.records == [
        .value(memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch, ordinal: 0, value: Data([7]), provenance: .notification),
        .terminal(.closed, platformTail: .uncertain, rejected: nil)
      ])
      do { try bridge.acknowledge(groupID: 1, batchID: batch.id + 1); fatalError("wrong ACK released deferred records") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      let retry = try bridge.drain(groupID: 1)
      precondition(retry == batch, "wrong ACK must replay the exact batch")
      try bridge.acknowledge(groupID: 1, batchID: batch.id)
      try bridge.seal(groupID: 1)
      try bridge.remove(groupID: 1)
    }
  }

  static func testDeferredCallbackCaptureInterleave() throws {
    let queue = DispatchQueue(label: "bridge.deferred-callback.capture-interleave")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "deferred-callback-capture-interleave")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let earlier = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 1))
      guard case let .captured(ticket) = earlier else { fatalError("expected reserved capture") }
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 2)
      let settle = source.captureDeferredFailure(route: route, failure: failure)!
      settle(true)
      let blocked = try bridge.drain(groupID: 1)
      precondition(blocked == nil, "production capture must not overtake an earlier group capture")
      try bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
      let batch = try bridge.drain(groupID: 1)!
      precondition(batch.records == [.terminal(.sourceFailed(failure, memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch), platformTail: .uncertain, rejected: nil)])
      try bridge.acknowledge(groupID: 1, batchID: batch.id)
    }
  }

  static func testCaptureBarrierPending() throws {
    let queue = DispatchQueue(label: "bridge.capture.barrier-pending")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "capture-barrier-pending")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 42))
      guard case let .captured(ticket) = capture else { fatalError("capture unexpectedly omitted") }
      do { try bridge.barrier(groupID: 1, id: 7); fatalError("barrier admitted before capture settlement") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      let blocked = try bridge.drain(groupID: 1)
      precondition(blocked == nil)
      try bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
      try bridge.barrier(groupID: 1, id: 7)
      let batch = try bridge.drain(groupID: 1)!
      precondition(batch.records == [.barrier(id: 7, platformTail: .uncertain)])
      try bridge.acknowledge(groupID: 1, batchID: batch.id)
    }
  }

  static func testCaptureCancellationOrder() throws {
    let queue = DispatchQueue(label: "bridge.capture.cancel-order")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "cancel-order")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 42))
      guard case let .captured(ticket) = capture else { fatalError("capture unexpectedly omitted") }
      try bridge.close(groupID: 1)
      let beforeCancellation = try bridge.drain(groupID: 1)
      precondition(beforeCancellation == nil, "close must wait for the earlier capture to replay or cancel")
      try bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
      let terminal = try bridge.drain(groupID: 1)!
      precondition(terminal.records == [.terminal(.closed, platformTail: .uncertain, rejected: nil)])
      try bridge.acknowledge(groupID: 1, batchID: terminal.id)
      try bridge.seal(groupID: 1)
      try bridge.remove(groupID: 1)
    }
  }

  static func testCaptureRetirementOrder() throws {
    let queue = DispatchQueue(label: "bridge.capture.retire-order")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "retire-order")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: failure)
      // Retirement is accepted once, but its source mutation stays behind the
      // captured callback.  The callback must therefore retain its original
      // source-failure classification without requiring the caller to retry.
      try bridge.retire(groupID: 1, expected: member)
      try bridge.replayCapturedFailure(groupID: 1, capture: capture)
      let first = try bridge.drain(groupID: 1)!
      precondition(first.records.first == .terminal(.sourceFailed(failure, memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch), platformTail: .uncertain, rejected: nil), "capture must be classified before the later retirement and replacement")
      try bridge.acknowledge(groupID: 1, batchID: first.id)
      // An exact retry observes the retained retirement receipt; it does not
      // submit a second fence after source failure.
      try bridge.retire(groupID: 1, expected: member)

      let otherRoute = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "d", characteristicOccurrence: 0)
      let members = try bridge.open(groupID: 2, members: [.init(id: 1, route: route), .init(id: 2, route: otherRoute)], capacity: 4, byteCapacity: 32).members
      try bridge.retire(groupID: 2, expected: members[0])
      let settle = source.captureDeferredFailure(route: otherRoute, failure: failure)!
      try bridge.retire(groupID: 2, expected: members[1])
      let replacement = try bridge.register(groupID: 2, expected: members[0])
      precondition(replacement.epoch == members[0].epoch + 1)
      settle(false)
      let secondRetirement = try bridge.drain(groupID: 2)!
      precondition(secondRetirement.records.contains { record in
        if case let .fence(memberID, sourceMember, epoch, _) = record {
          return memberID == members[1].id && sourceMember == members[1].sourceMember && epoch == members[1].epoch
        }
        return false
      }, "cancelled capture must automatically publish the retained retirement")
      try bridge.acknowledge(groupID: 2, batchID: secondRetirement.id)
      try bridge.retire(groupID: 2, expected: members[1])

      // A cancelled capture must also complete an accepted retirement without
      // a second caller request.  The replacement is admitted only after the
      // retained fence is delivered and acknowledged.
      let cancelledMember = try bridge.open(groupID: 3, members: [.init(id: 3, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let cancelledCapture = try bridge.captureFailure(groupID: 3, expected: cancelledMember, failure: failure)
      guard case let .captured(cancelledTicket) = cancelledCapture else { fatalError("expected retained cancellation capture") }
      try bridge.retire(groupID: 3, expected: cancelledMember)
      do { _ = try bridge.register(groupID: 3, expected: cancelledMember); fatalError("replacement crossed retained capture") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      try bridge.cancelCapturedFailure(groupID: 3, ticket: cancelledTicket)
      let cancelledFence = try bridge.drain(groupID: 3)!
      precondition(cancelledFence.records == [.fence(memberID: cancelledMember.id, sourceMember: cancelledMember.sourceMember, epoch: cancelledMember.epoch, platformTail: .uncertain)])
      try bridge.acknowledge(groupID: 3, batchID: cancelledFence.id)
      let cancelledReplacement = try bridge.register(groupID: 3, expected: cancelledMember)
      precondition(cancelledReplacement.epoch == cancelledMember.epoch + 1)
    }
  }

  static func testCaptureSourceOrder() throws {
    let queue = DispatchQueue(label: "bridge.capture.source-order")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "source-order")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: failure)
      source.admit(route: route, bytes: Data([1]), provenance: .notification)
      let blocked = try bridge.drain(groupID: 1)
      precondition(blocked == nil, "a source value must not cross an earlier unresolved capture")
      try bridge.replayCapturedFailure(groupID: 1, capture: capture)
      let first = try bridge.drain(groupID: 1)!
      precondition(first.records.first == .terminal(.sourceFailed(failure, memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch), platformTail: .uncertain, rejected: nil))
    }
  }

  // A cancelled earlier failure must release the later value without
  // releasing the value's foreign batch on an unrelated acknowledgement.
  static func testCaptureSourceCancellationAcknowledgement() throws {
    let queue = DispatchQueue(label: "bridge.capture.source-cancel-ack")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "source-cancel-ack")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 42))
      guard case let .captured(ticket) = capture else { fatalError("expected reserved capture") }
      source.admit(route: route, bytes: Data([7]), provenance: .notification)
      let blocked = try bridge.drain(groupID: 1)
      precondition(blocked == nil, "a value must wait for cancellation of an earlier capture")
      try bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
      let batch = try bridge.drain(groupID: 1)!
      precondition(batch.records == [.value(memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch, ordinal: 0, value: Data([7]), provenance: .notification)])
      do { try bridge.acknowledge(groupID: 1, batchID: batch.id + 1); fatalError("wrong ACK") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      let retry = try bridge.drain(groupID: 1)
      precondition(retry == batch, "wrong ACK must retain the exact value batch")
      try bridge.acknowledge(groupID: 1, batchID: batch.id)
      let empty = try bridge.drain(groupID: 1)
      precondition(empty == nil)
    }
  }

  static func testCaptureInvalidationOrder() throws {
    let queue = DispatchQueue(label: "bridge.capture.invalidation-order")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "invalidation-order")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: failure)
      source.invalidate(.connectionLost)
      let blocked = try bridge.drain(groupID: 1)
      precondition(blocked == nil, "source invalidation must not cross an earlier unresolved capture")
      try bridge.replayCapturedFailure(groupID: 1, capture: capture)
      let first = try bridge.drain(groupID: 1)!
      precondition(first.records.first == .terminal(.sourceFailed(failure, memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch), platformTail: .uncertain, rejected: nil))
    }
  }

  static func testCaptureOmissionInterleave() throws {
    let queue = DispatchQueue(label: "bridge.capture.omission-interleave")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "omission-interleave")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, captureBudget: .init(maxGroups: 1, maxSlots: 1, maxBytes: 1024, slotsPerGroup: 1, bytesPerGroup: 1024), wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let held = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 1))
      guard case let .captured(ticket) = held else { fatalError("missing reserved capture") }
      let first = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 2))
      guard case .omitted = first else { fatalError("missing first omission") }
      source.admit(route: route, bytes: Data([7]), provenance: .notification)
      let second = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 3))
      guard case .omitted = second else { fatalError("missing second omission") }
      try bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
      try bridge.replayCapturedFailure(groupID: 1, capture: first)
      try bridge.replayCapturedFailure(groupID: 1, capture: second)
      var records: [UnifiedBleRustOrderedGroupBridge.Record] = []
      while let batch = try bridge.drain(groupID: 1) {
        records.append(contentsOf: batch.records)
        try bridge.acknowledge(groupID: 1, batchID: batch.id)
      }
      precondition(records == [
        .failureObservationsOmitted,
        .value(memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch, ordinal: 0, value: Data([7]), provenance: .notification),
        .failureObservationsOmitted
      ], "omissions on opposite sides of a source value must retain separate positions")
    }
  }

  static func testCaptureMarkerDispatch() throws {
    let queue = DispatchQueue(label: "bridge.capture.marker-dispatch")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "marker-dispatch")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let descriptor = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32)
      let capture = try bridge.captureFailure(groupID: 1, expected: descriptor.members[0], failure: .init(domain: "CB", code: 42))
      guard case let .captured(ticket) = capture else { fatalError("capture unexpectedly omitted") }
      let input = UnifiedBleRustOrderedGroupBridge.MarkerInput(connection: "connection", snapshot: .init(owner: "owner", attachment: 1, revision: 1), route: route, value: Data([9]), withResponse: true)
      let initial = try bridge.beginMarker(groupID: 1, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      precondition(initial == nil)
      var dispatched = false
      do {
        _ = try OwnedOrderedWriteDispatch.start(queue: queue, admit: {
          try bridge.admitMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0)
        }, write: { _ in dispatched = true }, completion: { _ in })
        fatalError("admission must report the unresolved capture")
      } catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      precondition(!dispatched, "marker write must not dispatch before an earlier capture is settled")
      let waitingRetry = try bridge.retryWaitingMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0)
      precondition(waitingRetry == .waiting,
        "a retained control must remain waiting while its capture is unresolved")
      let pending = try bridge.markerStatus(groupID: 1, generation: descriptor.opaqueSourceGeneration, id: 0)
      precondition(pending.admission == .pending && pending.write == .initiating)
      let retry = try bridge.beginMarker(groupID: 1, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      precondition(retry == nil, "the same pending request must be retryable")
      try bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
      let readyRetry = try bridge.retryWaitingMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0)
      precondition(readyRetry == .ready,
        "the exact pending receipt must become retryable after its capture settles")
      let ordinal = try OwnedOrderedWriteDispatch.start(queue: queue, admit: {
        try bridge.admitMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0)
      }, write: { _ in dispatched = true }, completion: { _ in })
      precondition(ordinal == 0 && dispatched, "cancellation must allow the same marker to be admitted and dispatched")
      try bridge.markerAdmitted(groupID: 1, id: 0, ordinal: ordinal)
      _ = try bridge.markerStarted(groupID: 1, id: 0)
      let duplicate = try bridge.beginMarker(groupID: 1, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      precondition(duplicate?.write == .started(nil), "an admitted retry must not dispatch twice")
      let batch = try bridge.drain(groupID: 1)!
      precondition(batch.records == [.marker(id: 0, ordinal: 0)], "only the admitted retry may produce a marker")
    }
  }

  static func testCaptureMarkerCancel() throws {
    let queue = DispatchQueue(label: "bridge.capture.marker-cancel")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "marker-cancel")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let descriptor = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32)
      let capture = try bridge.captureFailure(groupID: 1, expected: descriptor.members[0], failure: .init(domain: "CB", code: 42))
      guard case let .captured(ticket) = capture else { fatalError("capture unexpectedly omitted") }
      let input = UnifiedBleRustOrderedGroupBridge.MarkerInput(connection: "connection", snapshot: .init(owner: "owner", attachment: 1, revision: 1), route: route, value: Data([9]), withResponse: true)
      _ = try bridge.beginMarker(groupID: 1, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      do { _ = try bridge.admitMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0); fatalError("marker admitted") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      let cancelled = try bridge.cancelWaitingMarker(groupID: 1, generation: descriptor.opaqueSourceGeneration, id: 0)
      guard case let .notStarted(failure)? = cancelled?.write else { fatalError("pending cancellation did not settle") }
      precondition(cancelled?.admission == .notAdmitted && failure?.kind == "cancelled" && failure?.dispatched == false)
      try bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
      let settledRetry = try bridge.retryWaitingMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0)
      precondition(settledRetry == .settled,
        "a cancelled pending receipt must not be re-admitted by a late retry")
      let retry = try bridge.beginMarker(groupID: 1, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      precondition(retry == cancelled, "cancelled marker must not become dispatchable after capture settles")
      let empty = try bridge.drain(groupID: 1)
      precondition(empty == nil)
    }
  }

  static func testCaptureMarkerFailure() throws {
    let queue = DispatchQueue(label: "bridge.capture.marker-failure")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "marker-failure")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let descriptor = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32)
      let capture = try bridge.captureFailure(groupID: 1, expected: descriptor.members[0], failure: .init(domain: "CB", code: 42))
      let input = UnifiedBleRustOrderedGroupBridge.MarkerInput(connection: "connection", snapshot: .init(owner: "owner", attachment: 1, revision: 1), route: route, value: Data([9]), withResponse: true)
      _ = try bridge.beginMarker(groupID: 1, generation: descriptor.opaqueSourceGeneration, id: 0, input: input)
      do { _ = try bridge.admitMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0); fatalError("marker admitted") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      try bridge.replayCapturedFailure(groupID: 1, capture: capture)
      do { _ = try bridge.admitMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0); fatalError("unacknowledged capture bypassed") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.capturePending {}
      let terminal = try bridge.drain(groupID: 1)!
      guard case .terminal(.sourceFailed, _, _) = terminal.records.first else { fatalError("capture did not terminate") }
      try bridge.acknowledge(groupID: 1, batchID: terminal.id)
      do { _ = try bridge.admitMarker(groupID: 1, opaqueSourceGeneration: descriptor.opaqueSourceGeneration, id: 0); fatalError("marker admitted after terminal") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.source(.inactive) {}
    }
  }

  static func testPendingCaptureFinality() throws {
    let queue = DispatchQueue(label: "bridge.capture.finality")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "finality")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 42))
      // The accepted retirement remains a bridge receipt while close and the
      // captured callback race.  Close must not turn the earlier callback
      // into late evidence or strand that receipt.
      try bridge.retire(groupID: 1, expected: member)
      try bridge.close(groupID: 1)
      do { try bridge.seal(groupID: 1); fatalError("sealed while capture is pending") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      // The close was observed after the capture.  It must remain behind the
      // unresolved position instead of escaping as a terminal before the
      // source failure can be classified.
      let beforeReplay = try bridge.drain(groupID: 1)
      precondition(beforeReplay == nil)
      try bridge.replayCapturedFailure(groupID: 1, capture: capture)
      let failure = try bridge.drain(groupID: 1)!
      guard case .terminal(.sourceFailed, _, _) = failure.records.first else {
        fatalError("captured failure did not supersede the later close")
      }
      do { try bridge.seal(groupID: 1); fatalError("sealed before capture acknowledgement") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      let retry = try bridge.drain(groupID: 1)
      precondition(retry == failure)
      do { try bridge.acknowledge(groupID: 1, batchID: failure.id + 1); fatalError("wrong acknowledgement") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      do { try bridge.seal(groupID: 1); fatalError("wrong acknowledgement released capture") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.cleanupPending {}
      try bridge.acknowledge(groupID: 1, batchID: failure.id)
      try bridge.seal(groupID: 1)
      let empty = try bridge.drain(groupID: 1)
      precondition(empty == nil)
    }
  }

  static func testFirstCapturedFailure() throws {
    let queue = DispatchQueue(label: "bridge.capture.first")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "first")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 4, byteCapacity: 32).members[0]
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42)
      let capture = try bridge.captureFailure(groupID: 1, expected: member, failure: failure)
      let laterFailure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 43)
      let later = try bridge.captureFailure(groupID: 1, expected: member, failure: laterFailure)
      try bridge.replayCapturedFailure(groupID: 1, capture: capture)
      try bridge.replayCapturedFailure(groupID: 1, capture: later)
      let batch = try bridge.drain(groupID: 1)!
      guard case let .terminal(.sourceFailed(actual, memberID, sourceMember, epoch), _, _) = batch.records.first else {
        fatalError("first current capture must end the group")
      }
      precondition(actual == failure && memberID == member.id && sourceMember == member.sourceMember && epoch == member.epoch)
      let claim = OrderedNotificationClaim(generation: source.generation, route: route, token: member.sourceMember)
      precondition(batch.records == [
        .terminal(.sourceFailed(failure, memberID: member.id, sourceMember: member.sourceMember, epoch: member.epoch), platformTail: .uncertain, rejected: nil),
        .subsequentSourceFailure(laterFailure.attributed(to: claim, epoch: member.epoch))
      ])
      try bridge.markVersionedBatch(groupID: 1, batchID: batch.id)
      do { try bridge.acknowledgeVersionedBatch(groupID: 1, batchID: batch.id + 1); fatalError("wrong ACK") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.unknownBatch {}
      let retry = try bridge.drain(groupID: 1)
      precondition(retry == batch, "retry must retain both exact records")
      try bridge.acknowledgeVersionedBatch(groupID: 1, batchID: batch.id)
      try bridge.seal(groupID: 1)
      let empty = try bridge.drain(groupID: 1)
      precondition(empty == nil)
      try bridge.remove(groupID: 1)
    }
  }

  static func testCancellationWake() throws {
    let queue = DispatchQueue(label: "bridge.capture.cancel")
    let wakes = DispatchSemaphore(value: 0)
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "cancel")
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, captureBudget: .init(maxGroups: 1, maxSlots: 2, maxBytes: 1024, slotsPerGroup: 2, bytesPerGroup: 1024), wake: { _, _ in wakes.signal() })
    try queue.sync {
      let member = try bridge.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 2, byteCapacity: 32).members[0]
      let blocked = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 1))
      let later = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 2))
      guard case let .captured(ticket) = blocked else { fatalError("missing first capture") }
      try bridge.replayCapturedFailure(groupID: 1, capture: later)
      for _ in 0..<100 { try bridge.replayCapturedFailure(groupID: 1, capture: later) }
      let before = try bridge.drain(groupID: 1)
      precondition(before == nil)
      try bridge.cancelCapturedFailure(groupID: 1, ticket: ticket)
      let batch = try bridge.drain(groupID: 1)!
      precondition(batch.records.count == 1)
      guard case let .terminal(.sourceFailed(failure, memberID, sourceMember, epoch), _, _) = batch.records[0] else { fatalError("wrong record") }
      precondition(failure.code == 2 && memberID == member.id && sourceMember == member.sourceMember && epoch == member.epoch)
      try bridge.acknowledge(groupID: 1, batchID: batch.id)
    }
    precondition(wakes.wait(timeout: .now() + 3) == .success)
    queue.sync {}
    precondition(wakes.wait(timeout: .now() + 0.1) == .timedOut, "duplicate replay and cancellation must share one scheduled wake")
    try queue.sync {
      let member = try bridge.recoverOpen(groupID: 1).members[0]
      let next = try bridge.captureFailure(groupID: 1, expected: member, failure: .init(domain: "CB", code: 3))
      try bridge.replayCapturedFailure(groupID: 1, capture: next)
    }
    precondition(wakes.wait(timeout: .now() + 3) == .success, "wake must rearm after execution and ACK")
  }

  static func testFactoryReservations() throws {
    let queue = DispatchQueue(label: "bridge.capture.factory")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let budget = UnifiedBleRustOrderedGroupBridge.CaptureBudget(maxGroups: 1, maxSlots: 1, maxBytes: 1024, slotsPerGroup: 1, bytesPerGroup: 1024)
    var calls = 0
    let noEffect = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { _, _, _, _ in
      calls += 1
      throw UnifiedBleRustOrderedGroupBridge.FactoryNoEffect(underlying: NSError(domain: "fixture", code: 1))
    }, captureBudget: budget, wake: { _, _ in })
    let uncertain = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { _, _, _, _ in
      throw NSError(domain: "fixture", code: 2)
    }, captureBudget: budget, wake: { _, _ in })
    let oversizedSource = OwnedOrderedNotificationSource(queue: queue, generation: "ééx")
    let smallBudget = UnifiedBleRustOrderedGroupBridge.CaptureBudget(maxGroups: 1, maxSlots: 1, maxBytes: 4, slotsPerGroup: 1, bytesPerGroup: 4)
    let oversized = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: oversizedSource.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(oversizedSource.add(ingress))
      return (oversizedSource, ingress)
    }, captureBudget: smallBudget, wake: { _, _ in })
    try queue.sync {
      for id: UInt64 in [1, 2] {
        do { _ = try noEffect.open(groupID: id, members: [.init(id: id, route: route)], capacity: 1, byteCapacity: 1); fatalError("expected factory refusal") }
        catch is UnifiedBleRustOrderedGroupBridge.FactoryNoEffect {}
      }
      precondition(calls == 2, "known rollback must release the reservation")
      do { _ = try uncertain.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 1, byteCapacity: 1); fatalError("expected uncertain factory") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.factoryIndeterminate {}
      do { _ = try uncertain.open(groupID: 2, members: [.init(id: 2, route: route)], capacity: 1, byteCapacity: 1); fatalError("uncertain factory lost reservation") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.captureCapacity {}
      do { _ = try oversized.open(groupID: 1, members: [.init(id: 1, route: route)], capacity: 1, byteCapacity: 1); fatalError("factory generation exceeded UTF-8 budget") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.openIndeterminate {}
      do { _ = try oversized.open(groupID: 2, members: [.init(id: 2, route: route)], capacity: 1, byteCapacity: 1); fatalError("bind failure lost uncertain reservation") }
      catch UnifiedBleRustOrderedGroupBridge.Failure.captureCapacity {}
    }
  }
}
