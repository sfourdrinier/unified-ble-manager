import Foundation

@main enum AppleOrderedGroupCaptureOwnerHarness {
  static func main() throws {
    if CommandLine.arguments.contains("--omission-prefix") {
      try testOmissionPrefix()
      return
    }
    let queue = DispatchQueue(label: "capture-owner")
    try queue.sync {
      let owner = try AppleOrderedGroupCaptureOwner(queue: queue, maxGroups: 2, maxSlots: 3, maxBytes: 256)
      let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
      let old = OrderedNotificationClaim(generation: "g", route: route, token: 1)
      let replacement = OrderedNotificationClaim(generation: "g", route: route, token: 2)
      let group = try owner.admit(groupID: 7, sourceGeneration: "g", slots: 2, bytes: 128)
      do { _ = try owner.admit(groupID: 8, sourceGeneration: "g", slots: 2, bytes: 128); fatalError("overcommitted slots") }
      catch AppleOrderedGroupCaptureOwner.Failure.capacity {}
      let original = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 42, detail: "original")
      guard case let .captured(first) = try owner.capture(group, claim: old, epoch: 1, failure: original) else { fatalError("missing capture") }
      try owner.replay(group, ticket: first)
      let batch = try owner.drain(group)!
      precondition(batch.records == [.failure(original.attributed(to: old, epoch: 1))])
      guard case let .captured(second) = try owner.capture(group, claim: replacement, epoch: 2, failure: .init(domain: "CB", code: 43)) else { fatalError("missing replacement capture") }
      let omitted = try owner.capture(group, claim: replacement, epoch: 2, failure: original)
      guard case let .omitted(omittedTicket) = omitted else { fatalError("missing omission") }
      try owner.replay(group, ticket: second)
      try owner.replayOmissions(group, through: omittedTicket)
      let retry = try owner.drain(group)
      precondition(retry == batch, "blocked ACK must replay identical payload")
      do { try owner.acknowledge(group, batchID: batch.id + 1); fatalError("wrong ACK") } catch AppleOrderedGroupCaptureOwner.Failure.unknownBatch {}
      try owner.retire(group)
      do { try owner.release(group); fatalError("released before ACK") } catch AppleOrderedGroupCaptureOwner.Failure.busy {}
      try owner.acknowledge(group, batchID: batch.id)
      let later = try owner.drain(group)!
      precondition(later.records == [.failure(OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 43).attributed(to: replacement, epoch: 2)), .omitted(count: 1, countSaturated: false)])
      try owner.acknowledge(group, batchID: later.id)
      try owner.release(group)
      let next = try owner.admit(groupID: 7, sourceGeneration: "replacement", slots: 3, bytes: 256)
      let nextClaim = OrderedNotificationClaim(generation: "replacement", route: route, token: 3)
      let oversized = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 99, detail: String(repeating: "x", count: 257))
      let tooLarge = try owner.capture(next, claim: nextClaim, epoch: 3, failure: oversized)
      let coalesced = try owner.capture(next, claim: nextClaim, epoch: 3, failure: original)
      guard case .omitted = tooLarge, case let .omitted(coalescedTicket) = coalesced else { fatalError("missing omission") }
      let beforeReplay = try owner.drain(next)
      precondition(beforeReplay == nil, "omission must wait for replay")
      try owner.replayOmissions(next, through: coalescedTicket)
      let overflow = try owner.drain(next)!
      precondition(overflow.records == [.omitted(count: 2, countSaturated: false)])
      let whileBlocked = try owner.capture(next, claim: nextClaim, epoch: 3, failure: oversized)
      guard case let .omitted(blockedTicket) = whileBlocked else { fatalError("missing omission") }
      try owner.replayOmissions(next, through: blockedTicket)
      let overflowRetry = try owner.drain(next)
      precondition(overflowRetry == overflow)
      try owner.acknowledge(next, batchID: overflow.id)
      let following = try owner.drain(next)!
      precondition(following.records == [.omitted(count: 1, countSaturated: false)])
      try owner.acknowledge(next, batchID: following.id)
      guard case let .captured(waiting) = try owner.capture(next, claim: nextClaim, epoch: 4, failure: original) else { fatalError("expected slot") }
      let cannotPass = try owner.drain(next)
      precondition(cannotPass == nil, "unreplayed capture is a FIFO barrier")
      try owner.cancel(next, ticket: waiting)
      let cancelled = try owner.drain(next)
      precondition(cancelled == nil)
      guard case let .captured(a) = try owner.capture(next, claim: nextClaim, epoch: 4, failure: original),
            case let .captured(b) = try owner.capture(next, claim: nextClaim, epoch: 4, failure: original),
            case let .captured(c) = try owner.capture(next, claim: nextClaim, epoch: 4, failure: original) else { fatalError("slots unavailable") }
      let saturated = try owner.capture(next, claim: nextClaim, epoch: 4, failure: original)
      guard case let .omitted(saturatedTicket) = saturated else { fatalError("missing omission") }
      try owner.replay(next, ticket: b)
      try owner.replay(next, ticket: c)
      try owner.replayOmissions(next, through: saturatedTicket)
      let stalled = try owner.drain(next)
      precondition(stalled == nil, "later captures and omission cannot pass an unreplayed capture")
      try owner.replay(next, ticket: a)
      let ordered = try owner.drain(next)!
      precondition(ordered.records.count == 4)
      precondition(ordered.records.last == .omitted(count: 1, countSaturated: false))
      try owner.acknowledge(next, batchID: ordered.id)
      try owner.retire(next)
      try owner.release(next)
      do { _ = try owner.capture(group, claim: old, epoch: 1, failure: original); fatalError("stale group") } catch AppleOrderedGroupCaptureOwner.Failure.unknownGroup {}
    }
    try testBridgeIdentities()
    try testBoundGenerationBudget()
    print("AppleOrderedGroupCaptureOwnerHarness passed")
  }
  static func testOmissionPrefix() throws {
    let queue = DispatchQueue(label: "capture-owner.omission-prefix")
    try queue.sync {
      let owner = try AppleOrderedGroupCaptureOwner(queue: queue, maxGroups: 1, maxSlots: 1, maxBytes: 64)
      let group = try owner.admit(groupID: 1, sourceGeneration: "g", slots: 1, bytes: 64)
      let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
      let claim = OrderedNotificationClaim(generation: "g", route: route, token: 1)
      let oversized = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 1, detail: String(repeating: "x", count: 65))
      guard case let .omitted(a) = try owner.capture(group, claim: claim, epoch: 1, failure: oversized) else { fatalError("missing A") } // A schedules a fence
      guard case let .omitted(b) = try owner.capture(group, claim: claim, epoch: 1, failure: oversized) else { fatalError("missing B") } // B has not crossed it
      try owner.replayOmissions(group, through: a) // A's delayed fence arrives
      let prefix = try owner.drain(group)!
      precondition(prefix.records == [.omitted(count: 1, countSaturated: false)], "a fence for A must not release B")
      try owner.acknowledge(group, batchID: prefix.id)
      let beforeB = try owner.drain(group)
      precondition(beforeB == nil, "B must wait for its own fence")
      try owner.replayOmissions(group, through: b)
      let suffix = try owner.drain(group)!
      precondition(suffix.records == [.omitted(count: 1, countSaturated: false)])
      try owner.acknowledge(group, batchID: suffix.id)
      try owner.retire(group)
      try owner.release(group)
      let replacement = try owner.admit(groupID: 1, sourceGeneration: "g", slots: 1, bytes: 64)
      guard case let .omitted(newTicket) = try owner.capture(replacement, claim: claim, epoch: 1, failure: oversized) else { fatalError("missing replacement") }
      do { try owner.replayOmissions(replacement, through: b); fatalError("stale fence crossed incarnation") }
      catch AppleOrderedGroupCaptureOwner.Failure.unknownTicket {}
      let beforeNewFence = try owner.drain(replacement)
      precondition(beforeNewFence == nil)
      try owner.replayOmissions(replacement, through: newTicket)
      let newBatch = try owner.drain(replacement)!
      try owner.acknowledge(replacement, batchID: newBatch.id)
      try owner.retire(replacement)
      try owner.release(replacement)
    }
  }
  static func testBoundGenerationBudget() throws {
    let queue = DispatchQueue(label: "capture-owner.generation-budget")
    try queue.sync {
      let owner = try AppleOrderedGroupCaptureOwner(queue: queue, maxGroups: 1, maxSlots: 1, maxBytes: 4)
      let reserved = try owner.reserve(groupID: 1, slots: 1, bytes: 4)
      do {
        _ = try owner.bind(reserved, sourceGeneration: "ééx")
        fatalError("oversized UTF-8 generation was retained")
      } catch AppleOrderedGroupCaptureOwner.Failure.invalidBudget {}
      let bound = try owner.bind(reserved, sourceGeneration: "éé")
      try owner.retire(bound)
      try owner.release(bound)
    }
  }
  /// Uses the real bridge to obtain and replace claims. The owner and bridge
  /// batches are deliberately separate until the production path adopts them.
  static func testBridgeIdentities() throws {
    let queue = DispatchQueue(label: "capture-owner.bridge")
    let source = OwnedOrderedNotificationSource(queue: queue, generation: "bridge-generation")
    let route = OrderedNotificationRoute(peer: "p", serviceUUID: "s", serviceOccurrence: 0, characteristicUUID: "c", characteristicOccurrence: 0)
    let bridge = UnifiedBleRustOrderedGroupBridge(queue: queue, factory: { routes, capacity, bytes, wake in
      let ingress = try OwnedOrderedNotificationIngress(queue: queue, generation: source.generation, members: routes, capacity: capacity, byteCapacity: bytes, wake: wake)
      precondition(source.add(ingress)); return (source, ingress)
    }, wake: { _, _ in })
    try queue.sync {
      let owner = try AppleOrderedGroupCaptureOwner(queue: queue, maxGroups: 1, maxSlots: 2, maxBytes: 256)
      let reservation = try owner.admit(groupID: 1, sourceGeneration: source.generation, slots: 2, bytes: 256)
      let descriptor = try bridge.open(groupID: 1, members: [.init(id: 9, route: route)], capacity: 4, byteCapacity: 32)
      let original = descriptor.members[0]
      let claim = OrderedNotificationClaim(generation: descriptor.opaqueSourceGeneration, route: route, token: original.sourceMember)
      let failure = OwnedOrderedNotificationIngress.NativeFailure(domain: "CB", code: 77, detail: "original")
      guard case let .captured(ticket) = try owner.capture(reservation, claim: claim, epoch: original.epoch, failure: failure) else { fatalError("bridge capture refused") }
      try bridge.retire(groupID: 1, expected: original)
      let replacement = try bridge.register(groupID: 1, expected: original)
      precondition(replacement.epoch != original.epoch && replacement.sourceMember != original.sourceMember)
      try owner.replay(reservation, ticket: ticket)
      let batch = try owner.drain(reservation)!
      precondition(batch.records == [.failure(failure.attributed(to: claim, epoch: original.epoch))])
      let retried = try owner.drain(reservation)
      precondition(retried == batch)
      try owner.acknowledge(reservation, batchID: batch.id)
      try owner.retire(reservation)
      try owner.release(reservation)
    }
  }
}
