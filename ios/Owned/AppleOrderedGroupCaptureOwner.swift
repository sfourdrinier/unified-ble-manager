import Foundation

/// Queue-confined storage for pre-admitted ordered-group failure captures.
/// Admission reserves the complete slot and payload-byte budgets. The caller
/// must fence physical callbacks before retiring a reservation; this owner does
/// not install a radio route or a graph-gate reservation.
final class AppleOrderedGroupCaptureOwner {
  struct Reservation: Hashable {
    fileprivate let owner: UUID
    fileprivate let id: UUID
    let groupID: UInt64
    let sourceGeneration: String
  }
  struct Ticket: Hashable { fileprivate let id: UUID; let sequence: UInt64 }
  /// Identifies one observed omission in one coalesced run and reservation.
  struct OmissionTicket: Hashable { fileprivate let reservation: UUID; let run: UUID; let sequence: UInt64 }
  enum Capture: Equatable { case captured(Ticket), omitted(OmissionTicket) }
  enum Record: Equatable {
    case failure(OwnedOrderedNotificationIngress.NativeFailure)
    case omitted(count: UInt64, countSaturated: Bool)
  }
  struct OmissionDelivery: Equatable { let run: UUID; let continues: Bool }
  struct Batch: Equatable {
    let id: UInt64
    let records: [Record]
    /// The one coalesced omission record in `records`, if one was drained.
    /// `continues` means a later, not-yet-ready suffix remains at its original
    /// observation position.
    let omission: OmissionDelivery?
  }
  enum Failure: Error, Equatable { case invalidBudget, capacity, unknownGroup, invalidClaim, retired, unknownTicket, unknownBatch, busy, sequenceExhausted }

  private struct Payload {
    // Own compact UTF-8 rather than retaining a possibly larger String backing.
    let domain: Data
    let detail: Data
    let code: Int
    let claim: OrderedNotificationClaim
    let epoch: UInt64
    let bytes: Int
    var failure: OwnedOrderedNotificationIngress.NativeFailure {
      OwnedOrderedNotificationIngress.NativeFailure(domain: String(decoding: domain, as: UTF8.self), code: code,
        detail: String(decoding: detail, as: UTF8.self)).attributed(to: claim, epoch: epoch)
    }
  }
  private enum State { case captured, ready, inBatch }
  private struct Slot { let ticket: Ticket; let sequence: UInt64; let payload: Payload; var state: State }
  private struct Omission { let run: UUID; var first: UInt64; var last: UInt64; var readyThrough: UInt64? }
  private final class Group {
    var reservation: Reservation
    let slotBudget: Int
    let byteBudget: Int
    var slots: [Slot?]
    var usedBytes = 0
    var omitted: Omission?
    var batch: Batch?
    var batchSlots: [Int] = []
    var nextSequence: UInt64 = 0
    var sequenceAvailable = true
    var nextBatch: UInt64 = 0
    var batchAvailable = true
    var lastAck: UInt64?
    var retired = false
    init(_ reservation: Reservation, slots: Int, bytes: Int) {
      self.reservation = reservation; slotBudget = slots; byteBudget = bytes
      self.slots = Array(repeating: nil, count: slots)
    }
  }
  private let queue: DispatchQueue
  private let identity = UUID()
  private let maxGroups: Int
  private let maxSlots: Int
  private let maxBytes: Int
  private var groups: [UUID: Group] = [:]
  private var reservedSlots = 0
  private var reservedBytes = 0

  init(queue: DispatchQueue, maxGroups: Int, maxSlots: Int, maxBytes: Int) throws {
    guard maxGroups > 0, maxSlots > 0, maxBytes > 0 else { throw Failure.invalidBudget }
    self.queue = queue; self.maxGroups = maxGroups; self.maxSlots = maxSlots; self.maxBytes = maxBytes
  }
  /// Call before physical enable or join; refusal creates no reservation.
  func admit(groupID: UInt64, sourceGeneration: String, slots: Int, bytes: Int) throws -> Reservation {
    dispatchPrecondition(condition: .onQueue(queue))
    guard slots > 0, bytes > 0, sourceGeneration.utf8.count <= bytes else { throw Failure.invalidBudget }
    guard groups.count < maxGroups, slots <= maxSlots - reservedSlots, bytes <= maxBytes - reservedBytes,
          !groups.values.contains(where: { $0.reservation.groupID == groupID }) else { throw Failure.capacity }
    let reservation = Reservation(owner: identity, id: UUID(), groupID: groupID, sourceGeneration: copy(sourceGeneration))
    groups[reservation.id] = Group(reservation, slots: slots, bytes: bytes)
    reservedSlots += slots; reservedBytes += bytes
    return reservation
  }
  /// Reserve before a factory can publish a source; bind its generation only
  /// after the factory returns the authoritative identity.
  func reserve(groupID: UInt64, slots: Int, bytes: Int) throws -> Reservation {
    try admit(groupID: groupID, sourceGeneration: "", slots: slots, bytes: bytes)
  }
  func bind(_ reservation: Reservation, sourceGeneration: String) throws -> Reservation {
    let group = try lookup(reservation)
    guard reservation.sourceGeneration.isEmpty, !sourceGeneration.isEmpty,
          sourceGeneration.utf8.count <= group.byteBudget else { throw Failure.invalidBudget }
    let bound = Reservation(owner: identity, id: reservation.id, groupID: reservation.groupID, sourceGeneration: copy(sourceGeneration))
    group.reservation = bound
    return bound
  }
  /// A returned ticket contains no error payload. Overflow is coalesced in a
  /// separately reserved omission record; later failures cannot overtake it.
  func capture(_ reservation: Reservation, claim: OrderedNotificationClaim, epoch: UInt64,
               failure: OwnedOrderedNotificationIngress.NativeFailure) throws -> Capture {
    let group = try lookup(reservation)
    guard !group.retired else { throw Failure.retired }
    guard claim.generation == reservation.sourceGeneration else { throw Failure.invalidClaim }
    if group.omitted != nil { return .omitted(try omit(group)) }
    let components = [failure.domain, failure.detail, claim.generation, claim.route.peer,
                      claim.route.serviceUUID, claim.route.characteristicUUID]
    var size = 0
    for value in components {
      let count = value.utf8.count
      guard count <= group.byteBudget - size else { return .omitted(try startOmission(group)) }
      size += count
    }
    guard size <= group.byteBudget - group.usedBytes, let index = group.slots.firstIndex(where: { $0 == nil }) else {
      return .omitted(try startOmission(group))
    }
    let sequence = try nextSequence(group)
    let ticket = Ticket(id: UUID(), sequence: sequence)
    let route = claim.route
    let ownedClaim = OrderedNotificationClaim(generation: copy(claim.generation),
      route: OrderedNotificationRoute(peer: copy(route.peer), serviceUUID: copy(route.serviceUUID),
        serviceOccurrence: route.serviceOccurrence, characteristicUUID: copy(route.characteristicUUID),
        characteristicOccurrence: route.characteristicOccurrence), token: claim.token)
    let payload = Payload(domain: Data(failure.domain.utf8), detail: Data(failure.detail.utf8), code: failure.code,
                          claim: ownedClaim, epoch: epoch, bytes: size)
    group.slots[index] = Slot(ticket: ticket, sequence: sequence, payload: payload, state: .captured)
    group.usedBytes += size
    return .captured(ticket)
  }
  func replay(_ reservation: Reservation, ticket: Ticket) throws {
    let group = try lookup(reservation)
    guard let index = group.slots.firstIndex(where: { $0?.ticket == ticket }), var slot = group.slots[index] else { throw Failure.unknownTicket }
    if case .captured = slot.state { slot.state = .ready; group.slots[index] = slot }
  }
  /// A gate receipt advances only the prefix ending at this exact observation.
  func replayOmissions(_ reservation: Reservation, through ticket: OmissionTicket) throws {
    let group = try lookup(reservation)
    guard ticket.reservation == reservation.id, var omission = group.omitted,
          ticket.run == omission.run, ticket.sequence <= omission.last else { throw Failure.unknownTicket }
    if ticket.sequence >= omission.first {
      omission.readyThrough = max(omission.readyThrough ?? ticket.sequence, ticket.sequence)
      group.omitted = omission
    }
  }
  /// Explicit cancellation means this observation is not to be reported.
  func cancel(_ reservation: Reservation, ticket: Ticket) throws {
    let group = try lookup(reservation)
    guard let index = group.slots.firstIndex(where: { $0?.ticket == ticket }), let slot = group.slots[index],
          case .captured = slot.state else { throw Failure.unknownTicket }
    group.usedBytes -= slot.payload.bytes; group.slots[index] = nil
  }
  func drain(_ reservation: Reservation) throws -> Batch? {
    let group = try lookup(reservation)
    if let batch = group.batch { return batch }
    var records: [Record] = []
    var indices: [Int] = []
    let sorted = group.slots.indices.compactMap { index -> (Int, Slot)? in group.slots[index].map { (index, $0) } }.sorted { $0.1.sequence < $1.1.sequence }
    for (index, slot) in sorted {
      if let omitted = group.omitted, omitted.first < slot.sequence { break }
      guard case .ready = slot.state else { break }
      records.append(.failure(slot.payload.failure)); indices.append(index)
    }
    let nextUnselected = sorted.dropFirst(indices.count).first?.1.sequence
    let includeOmission = group.omitted?.readyThrough != nil && (nextUnselected == nil || group.omitted!.first < nextUnselected!)
    if includeOmission, let omission = group.omitted, let through = omission.readyThrough {
      let distance = through - omission.first
      records.append(.omitted(count: distance == UInt64.max ? UInt64.max : distance + 1, countSaturated: distance == UInt64.max))
    }
    guard !records.isEmpty else { return nil }
    guard group.batchAvailable else { throw Failure.sequenceExhausted }
    let id = group.nextBatch
    if id == UInt64.max { group.batchAvailable = false } else { group.nextBatch += 1 }
    var omissionDelivery: OmissionDelivery?
    if includeOmission, var omission = group.omitted, let through = omission.readyThrough {
      if through == omission.last {
        omissionDelivery = OmissionDelivery(run: omission.run, continues: false)
        group.omitted = nil
      } else {
        omissionDelivery = OmissionDelivery(run: omission.run, continues: true)
        omission.first = through + 1; omission.readyThrough = nil; group.omitted = omission
      }
    }
    for index in indices { group.slots[index]!.state = .inBatch }
    let batch = Batch(id: id, records: records, omission: omissionDelivery)
    group.batchSlots = indices; group.batch = batch
    return batch
  }
  func acknowledge(_ reservation: Reservation, batchID: UInt64) throws {
    let group = try lookup(reservation)
    if group.lastAck == batchID { return }
    guard group.batch?.id == batchID else { throw Failure.unknownBatch }
    for index in group.batchSlots {
      guard let slot = group.slots[index] else { preconditionFailure("missing owned slot") }
      group.usedBytes -= slot.payload.bytes; group.slots[index] = nil
    }
    group.batchSlots.removeAll(); group.batch = nil; group.lastAck = batchID
  }
  /// Only retire after the caller has fenced new physical callbacks.
  func retire(_ reservation: Reservation) throws { try lookup(reservation).retired = true }
  func hasPending(_ reservation: Reservation) throws -> Bool {
    let group = try lookup(reservation)
    return group.batch != nil || group.omitted != nil || group.slots.contains(where: { $0 != nil })
  }
  func canRelease(_ reservation: Reservation) throws -> Bool {
    let group = try lookup(reservation)
    return group.retired && group.batch == nil && group.omitted == nil && group.slots.allSatisfy({ $0 == nil })
  }
  func release(_ reservation: Reservation) throws {
    let group = try lookup(reservation)
    guard try canRelease(reservation) else { throw Failure.busy }
    groups.removeValue(forKey: reservation.id)
    reservedSlots -= group.slotBudget; reservedBytes -= group.byteBudget
  }
  private func copy(_ value: String) -> String { String(decoding: Data(value.utf8), as: UTF8.self) }
  private func lookup(_ reservation: Reservation) throws -> Group {
    dispatchPrecondition(condition: .onQueue(queue))
    guard reservation.owner == identity, let group = groups[reservation.id], group.reservation == reservation else { throw Failure.unknownGroup }
    return group
  }
  private func nextSequence(_ group: Group) throws -> UInt64 {
    guard group.sequenceAvailable else { throw Failure.sequenceExhausted }
    let value = group.nextSequence
    if value == UInt64.max { group.sequenceAvailable = false } else { group.nextSequence += 1 }
    return value
  }
  private func startOmission(_ group: Group) throws -> OmissionTicket {
    let sequence = try nextSequence(group)
    let run = UUID()
    group.omitted = Omission(run: run, first: sequence, last: sequence, readyThrough: nil)
    return OmissionTicket(reservation: group.reservation.id, run: run, sequence: sequence)
  }
  private func omit(_ group: Group) throws -> OmissionTicket {
    let sequence = try nextSequence(group)
    group.omitted!.last = sequence
    return OmissionTicket(reservation: group.reservation.id, run: group.omitted!.run, sequence: sequence)
  }
}
