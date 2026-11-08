// ios/__tests__/AppleOrderedNotificationIngressHarness.swift
import Foundation

@main enum AppleOrderedNotificationIngressHarness {
  static func main() throws {
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
    assert(first.drain() == [.value(member: a, epoch: 1, ordinal: 1, bytes: Data([1, 2]), provenance: .readOrNotification), .loss(member: nil, count: nil), .terminal(.overflow, platformTail: .uncertain)])
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
    source.invalidate()
    source.admit(route: a, bytes: Data([7]), provenance: .notification)
    assert(second.drain() == [.terminal(.invalidated, platformTail: .uncertain)])
    assert(!source.remove(second))
    second.cleanupFinished(a2)
    assert(source.remove(second))
    do { try second.register(a2); fatalError("ended") } catch OwnedOrderedNotificationIngress.Failure.inactive {}
    let small = try OwnedOrderedNotificationIngress(queue: queue, generation: "h", members: [a], capacity: 1, byteCapacity: 10)
    let other = OwnedOrderedNotificationSource(queue: queue, generation: "h")
    guard let claim = other.makeClaim(route: a) else { fatalError("claim") }
    assert(other.add(small)); try small.register(claim)
    other.admit(route: a, bytes: Data([8]), provenance: .notification)
    do { try small.retire(a); fatalError("lost fence") } catch OwnedOrderedNotificationIngress.Failure.controlOverflow {}
    assert(small.drain() == [.value(member: a, epoch: 1, ordinal: 0, bytes: Data([8]), provenance: .notification), .loss(member: nil, count: nil), .terminal(.overflow, platformTail: .uncertain)])
    do { _ = try OwnedOrderedNotificationIngress(queue: queue, generation: "g", members: [a, a], capacity: 1, byteCapacity: 1); fatalError("duplicate") } catch OwnedOrderedNotificationIngress.Failure.invalidMembers {}
    print("Ordered ingress owner: deterministic tests passed")
    }
  }
}
