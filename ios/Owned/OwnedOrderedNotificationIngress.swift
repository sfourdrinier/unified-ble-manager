// ios/Owned/OwnedOrderedNotificationIngress.swift
import Foundation

/// Full instance identity within a connection/database generation.
struct OrderedNotificationRoute: Hashable {
  let peer: String
  let serviceUUID: String
  let serviceOccurrence: Int
  let characteristicUUID: String
  let characteristicOccurrence: Int
}

/// A source-issued logical claim; never infer it from a UUID or a queued callback.
struct OrderedNotificationClaim: Hashable {
  let generation: String
  let route: OrderedNotificationRoute
  let token: UInt64
}

/// Queue-confined prototype. Its caller must serialize every operation with CoreBluetooth callbacks.
/// This does not own CCCD operations or install a production route.
final class OwnedOrderedNotificationIngress {
  enum Provenance: Equatable { case notification, readOrNotification }
  enum PlatformTail: Equatable { case uncertain }
  enum End: Equatable { case closed, invalidated, overflow, ordinalExhausted }
  enum Event: Equatable {
    case value(member: OrderedNotificationRoute, epoch: UInt64, ordinal: UInt64, bytes: Data, provenance: Provenance)
    case fence(member: OrderedNotificationRoute, epoch: UInt64, platformTail: PlatformTail)
    case loss(member: OrderedNotificationRoute?, count: UInt64?)
    case terminal(End, platformTail: PlatformTail)
  }
  enum Failure: Error, Equatable { case invalidMembers, invalidCapacity, inactive, unknownMember, alreadyRegistered, notRegistered, epochExhausted, controlOverflow }

  let generation: String
  fileprivate let queue: DispatchQueue
  let members: Set<OrderedNotificationRoute>
  private let capacity: Int
  private let byteCapacity: Int
  private var queuedBytes = 0
  private var events: [Event] = []
  private var terminal: Event?
  private var epochs: [OrderedNotificationRoute: UInt64] = [:]
  private var registered: Set<OrderedNotificationRoute> = []
  private(set) var cleanupPending: Set<OrderedNotificationClaim> = []
  private var claims: [OrderedNotificationRoute: OrderedNotificationClaim] = [:]

  init(queue: DispatchQueue, generation: String, members: [OrderedNotificationRoute], capacity: Int, byteCapacity: Int) throws {
    guard !members.isEmpty, Set(members).count == members.count else { throw Failure.invalidMembers }
    guard capacity > 0, byteCapacity > 0 else { throw Failure.invalidCapacity }
    self.generation = generation
    self.queue = queue
    self.members = Set(members)
    self.capacity = capacity
    self.byteCapacity = byteCapacity
  }

  func register(_ claim: OrderedNotificationClaim) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let member = claim.route
    guard claim.generation == generation else { throw Failure.unknownMember }
    guard terminal == nil, !ended else { throw Failure.inactive }
    guard members.contains(member) else { throw Failure.unknownMember }
    guard !registered.contains(member) else { throw Failure.alreadyRegistered }
    let old = epochs[member] ?? 0
    guard old < UInt64.max else { throw Failure.epochExhausted }
    epochs[member] = old + 1
    registered.insert(member)
    claims[member] = claim
  }

  /// A successful retirement places the fence after prior values and before any re-registration.
  func retire(_ member: OrderedNotificationRoute) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    guard terminal == nil, !ended else { throw Failure.inactive }
    guard members.contains(member) else { throw Failure.unknownMember }
    guard registered.contains(member), let epoch = epochs[member] else { throw Failure.notRegistered }
    guard append(.fence(member: member, epoch: epoch, platformTail: .uncertain)) else { throw Failure.controlOverflow }
    registered.remove(member)
    claims.removeValue(forKey: member)
  }

  fileprivate var canRemove: Bool { ended && cleanupPending.isEmpty }

  fileprivate func currentClaim(_ route: OrderedNotificationRoute) -> OrderedNotificationClaim? {
    guard terminal == nil, !ended else { return nil }
    return claims[route]
  }

  fileprivate func accepts(_ claim: OrderedNotificationClaim) -> Bool {
    terminal == nil && !ended && claims[claim.route] == claim
  }

  fileprivate func admit(claim: OrderedNotificationClaim, ordinal: UInt64, bytes: Data, provenance: Provenance) {
    let member = claim.route
    guard accepts(claim), let epoch = epochs[member] else { return }
    guard events.count < capacity, bytes.count <= byteCapacity - queuedBytes else { end(.overflow, platformTail: .uncertain); return }
    queuedBytes += bytes.count
    events.append(.value(member: member, epoch: epoch, ordinal: ordinal, bytes: Data(bytes), provenance: provenance))
  }

  fileprivate func exhaustOrdinals() { end(.ordinalExhausted, platformTail: .uncertain) }

  /// Unknown source loss remains unknown; a lost control ends the collection.
  func reportLoss(member: OrderedNotificationRoute?, count: UInt64?) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard terminal == nil, !ended else { return }
    _ = append(.loss(member: member, count: count))
  }

  func retainCleanup(_ claim: OrderedNotificationClaim) { dispatchPrecondition(condition: .onQueue(queue)); if claim.generation == generation && members.contains(claim.route) { cleanupPending.insert(claim) } }
  func cleanupFinished(_ claim: OrderedNotificationClaim) { dispatchPrecondition(condition: .onQueue(queue)); cleanupPending.remove(claim) }
  func invalidate() { dispatchPrecondition(condition: .onQueue(queue)); end(.invalidated, platformTail: .uncertain) }
  func close() { dispatchPrecondition(condition: .onQueue(queue)); end(.closed, platformTail: .uncertain) }

  /// Terminal is reserved outside the value/control FIFO and cannot be crowded out.
  func drain() -> [Event] {
    dispatchPrecondition(condition: .onQueue(queue))
    var result = events
    events.removeAll(keepingCapacity: true)
    queuedBytes = 0
    if let terminal {
      if terminal == .terminal(.overflow, platformTail: .uncertain) { result.append(.loss(member: nil, count: nil)) }
      result.append(terminal); self.terminal = nil; ended = true
    }
    return result
  }

  private var ended = false
  private func append(_ event: Event) -> Bool {
    guard events.count < capacity else { end(.overflow, platformTail: .uncertain); return false }
    events.append(event)
    return true
  }
  private func end(_ reason: End, platformTail: PlatformTail) {
    guard terminal == nil, !ended else { return }
    registered.removeAll()
    claims.removeAll()
    terminal = .terminal(reason, platformTail: platformTail)
  }
}

/// One instance per connection generation, called on the same serial source queue.
/// `admit` must run directly on the serialized CoreBluetooth delegate queue, before
/// any asynchronous handoff or per-characteristic staging. It synchronously enqueues
/// each value (or ends that group with loss) before returning; merely capturing a
/// claim is not admission. Retirement must execute on that same queue. CoreBluetooth
/// callbacks still waiting in the platform queue at retirement have an uncertain tail:
/// their earlier physical arrival or logical owner cannot be recovered from the API.
final class OwnedOrderedNotificationSource {
  let generation: String
  private let queue: DispatchQueue
  private var groups: [OwnedOrderedNotificationIngress] = []
  private var nextOrdinal: UInt64 = 0
  private var ordinalAvailable = true
  private var invalidated = false
  private var nextClaim: UInt64 = 0
  private var claimsAvailable = true

  init(queue: DispatchQueue, generation: String) { self.queue = queue; self.generation = generation }

  func makeClaim(route: OrderedNotificationRoute) -> OrderedNotificationClaim? {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !invalidated, claimsAvailable else { return nil }
    let token = nextClaim
    if token == UInt64.max { claimsAvailable = false } else { nextClaim += 1 }
    return OrderedNotificationClaim(generation: generation, route: route, token: token)
  }

  func add(_ group: OwnedOrderedNotificationIngress) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !invalidated, group.generation == generation, group.queue === queue, !groups.contains(where: { $0 === group }) else { return false }
    groups.append(group)
    return true
  }

  func remove(_ group: OwnedOrderedNotificationIngress) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard group.canRemove, groups.contains(where: { $0 === group }) else { return false }
    groups.removeAll { $0 === group }
    return true
  }

  /// Runs at delegate ingress; no captured membership or value escapes before enqueue.
  func admit(route: OrderedNotificationRoute, bytes: Data, provenance: OwnedOrderedNotificationIngress.Provenance) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !invalidated else { return }
    let targets = groups.compactMap { group in group.currentClaim(route).map { (group, $0) } }
    guard ordinalAvailable else {
      for group in groups { group.exhaustOrdinals() }
      return
    }
    let ordinal = nextOrdinal
    if ordinal == UInt64.max { ordinalAvailable = false } else { nextOrdinal += 1 }
    let owned = Data(bytes)
    for (group, claim) in targets { group.admit(claim: claim, ordinal: ordinal, bytes: owned, provenance: provenance) }
  }

  func invalidate() {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !invalidated else { return }
    invalidated = true
    for group in groups { group.invalidate() }
  }
}
