// ios/Owned/OwnedOrderedNotificationIngress.swift
import Foundation

struct OwnedGattSnapshotIdentity: Equatable {
  let owner: String
  let attachment: UInt64
  let revision: UInt64
}

/// Queue-confined identity for native graph publication and invalidation.
final class OwnedGattSnapshotTracker {
  private let owner = UUID().uuidString
  private var nextAttachment: UInt64 = 0
  private var nextRevision: UInt64 = 0
  private var attachmentAvailable = true
  private var revisionAvailable = true
  private var attachments: [String: UInt64] = [:]
  private var published: [String: OwnedGattSnapshotIdentity] = [:]

  func attach(_ peer: String) -> Bool {
    guard attachmentAvailable else { return false }
    let id = nextAttachment
    if id == UInt64.max { attachmentAvailable = false } else { nextAttachment += 1 }
    attachments[peer] = id
    published.removeValue(forKey: peer)
    return true
  }
  func ensureAttachment(_ peer: String) -> Bool { attachments[peer] != nil || attach(peer) }
  func invalidate(_ peer: String) { published.removeValue(forKey: peer) }
  func disconnect(_ peer: String) { published.removeValue(forKey: peer); attachments.removeValue(forKey: peer) }
  func reset() { published.removeAll(); attachments.removeAll() }
  func peers() -> Set<String> { Set(attachments.keys).union(published.keys) }
  func publish(_ peer: String) -> OwnedGattSnapshotIdentity? {
    guard let attachment = attachments[peer], revisionAvailable else { return nil }
    let revision = nextRevision
    if revision == UInt64.max { revisionAvailable = false } else { nextRevision += 1 }
    let identity = OwnedGattSnapshotIdentity(owner: owner, attachment: attachment, revision: revision)
    published[peer] = identity
    return identity
  }
  func current(_ peer: String) -> OwnedGattSnapshotIdentity? { published[peer] }
  func matches(_ peer: String, _ identity: OwnedGattSnapshotIdentity) -> Bool { published[peer] == identity }
}


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
  /// Native details remain intact; late observations carry their source identity
  /// with those details until the owning batch is acknowledged.
  struct NativeFailure: Equatable {
    struct Origin: Equatable { let claim: OrderedNotificationClaim; let epoch: UInt64 }
    let domain: String
    let code: Int
    /// Localized description and the NSError underlying-error chain, in order.
    /// Other arbitrary userInfo values are not part of this wire contract.
    let detail: String
    let origin: Origin?

    init(domain: String, code: Int, detail: String = "CoreBluetooth notification source failure", origin: Origin? = nil) {
      self.domain = domain; self.code = code; self.detail = detail; self.origin = origin
    }

    init(error: NSError) {
      var lines = [error.localizedDescription]
      var seen: Set<ObjectIdentifier> = [ObjectIdentifier(error)]
      var current = error
      while let underlying = current.userInfo[NSUnderlyingErrorKey] as? NSError {
        guard seen.insert(ObjectIdentifier(underlying)).inserted else {
          lines.append("Underlying error cycle")
          break
        }
        lines.append("Underlying [\(underlying.domain):\(underlying.code)]: \(underlying.localizedDescription)")
        current = underlying
      }
      self.init(domain: error.domain, code: error.code, detail: lines.joined(separator: "\n"))
    }

    func attributed(to claim: OrderedNotificationClaim, epoch: UInt64) -> NativeFailure {
      NativeFailure(domain: domain, code: code, detail: detail, origin: .init(claim: claim, epoch: epoch))
    }
  }
  enum End: Equatable {
    case closed, connectionLost, serviceChanged, sourceReplaced, shutdown, adapterLost, overflow, ordinalExhausted, wakeFailed
    case sourceFailed(NativeFailure, member: OrderedNotificationRoute, sourceMember: UInt64, epoch: UInt64)
  }
  /// Rejected source evidence is separate from loss already reported by the source.
  enum Rejected: Equatable {
    case value(member: OrderedNotificationRoute, epoch: UInt64, bytes: UInt64)
    case sourceLoss(member: OrderedNotificationRoute?, skippedRecords: UInt64?)
    case fence(member: OrderedNotificationRoute, epoch: UInt64)
    case barrier(id: UInt64)
    case marker(id: UInt64)
  }
  enum Event: Equatable {
    case value(member: OrderedNotificationRoute, epoch: UInt64, ordinal: UInt64, bytes: Data, provenance: Provenance)
    case fence(member: OrderedNotificationRoute, epoch: UInt64, platformTail: PlatformTail)
    case barrier(id: UInt64, platformTail: PlatformTail)
    case marker(id: UInt64, ordinal: UInt64)
    case loss(member: OrderedNotificationRoute?, count: UInt64?)
    case terminal(End, platformTail: PlatformTail, rejected: Rejected?)
    case subsequentSourceFailure(NativeFailure)
    case failureObservationsOmitted
  }
  enum Failure: Error, Equatable { case invalidMembers, invalidCapacity, inactive, unknownMember, alreadyRegistered, notRegistered, epochExhausted, controlOverflow, markerOrdinalExhausted }

  let generation: String
  fileprivate let queue: DispatchQueue
  let members: Set<OrderedNotificationRoute>
  private let capacity: Int
  private let byteCapacity: Int
  private let wake: () -> Void
  private var queuedBytes = 0
  private var events: [Event] = []
  // Indexes refer to events still owned here, never to an acknowledged batch.
  // Each reservation consumes the ordinary item and byte budgets immediately.
  private var pendingValues: [UUID: Int] = [:]
  // A capture is an observed position even before its failure has been
  // classified.  Keep its insertion index in this same FIFO so later close,
  // lifecycle and value records cannot overtake it.
  private var externalCaptureBarriers: [UUID: Int] = [:]
  private var ordinaryCount = 0
  /// A retirement is a source-owned control, not disposable data. When a
  /// prior callback temporarily consumes the ordinary FIFO, retain the
  /// logical retirement and publish its fence as soon as a credit opens.
  /// There is at most one outstanding retirement per member, so this ledger
  /// is bounded by the group's immutable member set rather than by a second,
  /// unrelated queue limit.
  private struct PendingRetirement {
    let member: OrderedNotificationRoute
    let epoch: UInt64
  }
  private var pendingRetirements: [PendingRetirement] = []
  private var lateFailureCount = 0
  private var ownedLateFailures = 0
  private var terminal: Event?
  private var lateFailuresOmitted = false
  private var sealed = false
  private var deferredFailures = 0
  /// True until every source callback captured for deferred graph-gate replay
  /// has reported or explicitly cancelled its observation.
  var hasDeferredFailureCaptures: Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    return deferredFailures != 0
  }
  var hasPendingValues: Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    return !pendingValues.isEmpty
  }
  /// Whether an admitted observation precedes a later capture boundary.  This
  /// is intentionally queue-confined: callers use it only while recording
  /// that boundary on the same native callback queue.
  var hasQueuedObservations: Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    return !events.isEmpty
  }
  func beginExternalCaptureBarrier() -> UUID {
    dispatchPrecondition(condition: .onQueue(queue))
    let ticket = UUID()
    externalCaptureBarriers[ticket] = events.count
    return ticket
  }
  func endExternalCaptureBarrier(_ ticket: UUID, notify: Bool = true) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard externalCaptureBarriers.removeValue(forKey: ticket) != nil else { return }
    if notify { wake() }
  }
  func hasObservationAfterExternalCaptureBarrier(_ ticket: UUID) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let index = externalCaptureBarriers[ticket] else { return false }
    return index < events.count
  }
  func resolveExternalCaptureBarrier(_ ticket: UUID, with event: Event?, notify: Bool = true) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let index = externalCaptureBarriers.removeValue(forKey: ticket) else { return }
    if let event { events.insert(event, at: index) }
    // `events` did not contain a placeholder for the barrier.  A resolution
    // therefore shifts only later deferred reservations, not ordinary budget.
    let delta = event == nil ? 0 : 1
    if delta != 0 {
      for (valueTicket, valueIndex) in pendingValues where valueIndex >= index { pendingValues[valueTicket] = valueIndex + delta }
      for (barrierTicket, barrierIndex) in externalCaptureBarriers where barrierIndex >= index { externalCaptureBarriers[barrierTicket] = barrierIndex + delta }
    }
    if notify { wake() }
  }
  /// Resolve an externally-owned position and, when an opaque coalesced run
  /// still has an unresolved suffix, leave a new barrier immediately after
  /// the delivered record.  This preserves FIFO without retaining one UUID
  /// per omitted callback.
  func replaceExternalCaptureBarrier(_ ticket: UUID, with event: Event, notify: Bool = true) -> UUID? {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let index = externalCaptureBarriers.removeValue(forKey: ticket) else { return nil }
    events.insert(event, at: index)
    for (valueTicket, valueIndex) in pendingValues where valueIndex >= index { pendingValues[valueTicket] = valueIndex + 1 }
    for (barrierTicket, barrierIndex) in externalCaptureBarriers where barrierIndex >= index { externalCaptureBarriers[barrierTicket] = barrierIndex + 1 }
    let replacement = UUID()
    externalCaptureBarriers[replacement] = index + 1
    if notify { wake() }
    return replacement
  }

  /// Classify a replayed captured failure at its original ingress position.
  /// This is deliberately the positional counterpart of
  /// `admitOwnedFailureBeforeQueued`; inserting at FIFO index zero would
  /// reverse two ready captures and lose interleaving with values.
  func resolveOwnedFailureCaptureBarrier(_ ticket: UUID, failure: NativeFailure, notify: Bool = true) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let origin = failure.origin else { preconditionFailure("capture missing source identity") }
    let claim = origin.claim
    let event: Event
    let captureIndex = externalCaptureBarriers[ticket]
    let terminalIndex = events.firstIndex { if case .terminal = $0 { return true }; return false }
    let supersedesLaterTerminal = captureIndex != nil && terminalIndex != nil && captureIndex! <= terminalIndex!
    if !sealed, !ended, (terminal == nil || supersedesLaterTerminal), claims[claim.route] == claim, epochs[claim.route] == origin.epoch {
      let native = NativeFailure(domain: failure.domain, code: failure.code, detail: failure.detail)
      registered.removeAll()
      if supersedesLaterTerminal, let removed = terminalIndex {
        events.remove(at: removed)
        for (valueTicket, valueIndex) in pendingValues where valueIndex > removed { pendingValues[valueTicket] = valueIndex - 1 }
        for (barrierTicket, barrierIndex) in externalCaptureBarriers where barrierIndex > removed { externalCaptureBarriers[barrierTicket] = barrierIndex - 1 }
      }
      event = .terminal(.sourceFailed(native, member: claim.route, sourceMember: claim.token, epoch: origin.epoch), platformTail: .uncertain, rejected: nil)
      terminal = event
    } else {
      event = .subsequentSourceFailure(failure)
      lateFailureCount += 1
      ownedLateFailures += 1
    }
    resolveExternalCaptureBarrier(ticket, with: event, notify: notify)
  }
  private var epochs: [OrderedNotificationRoute: UInt64] = [:]
  private var registered: Set<OrderedNotificationRoute> = []
  private(set) var cleanupPending: Set<OrderedNotificationClaim> = []
  private var claims: [OrderedNotificationRoute: OrderedNotificationClaim] = [:]

  /// `wake` runs on the owner queue and must only schedule a drain, never reenter it.
  init(queue: DispatchQueue, generation: String, members: [OrderedNotificationRoute], capacity: Int, byteCapacity: Int, wake: @escaping () -> Void = {}) throws {
    guard !members.isEmpty, Set(members).count == members.count else { throw Failure.invalidMembers }
    guard capacity > 0, byteCapacity > 0 else { throw Failure.invalidCapacity }
    self.generation = generation
    self.queue = queue
    self.members = Set(members)
    self.capacity = capacity
    self.byteCapacity = byteCapacity
    self.wake = wake
  }

  @discardableResult
  func register(_ claim: OrderedNotificationClaim) throws -> UInt64 {
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
    return old + 1
  }

  /// A successful retirement places the fence after prior values and before any re-registration.
  func retire(_ member: OrderedNotificationRoute) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    guard terminal == nil, !ended else { throw Failure.inactive }
    guard members.contains(member) else { throw Failure.unknownMember }
    guard registered.contains(member), let epoch = epochs[member] else { throw Failure.notRegistered }
    if ordinaryCount < capacity {
      precondition(append(.fence(member: member, epoch: epoch, platformTail: .uncertain)))
    } else {
      // The source may already have captured a callback whose position is
      // before this retirement. Do not end the group or make the caller
      // rediscover/retry a physical state transition: retire it logically and
      // publish this exact fence automatically after the accepted prefix.
      precondition(!pendingRetirements.contains { $0.member == member })
      pendingRetirements.append(PendingRetirement(member: member, epoch: epoch))
    }
    registered.remove(member)
    claims.removeValue(forKey: member)
  }

  func barrier(_ id: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    guard terminal == nil, !ended else { throw Failure.inactive }
    guard append(.barrier(id: id, platformTail: .uncertain)) else { throw Failure.controlOverflow }
  }

  fileprivate func admitMarker(id: UInt64, ordinal: UInt64) throws {
    guard terminal == nil, !ended else { throw Failure.inactive }
    guard append(.marker(id: id, ordinal: ordinal)) else { throw Failure.controlOverflow }
  }

  fileprivate var markerActive: Bool { terminal == nil && !ended && !sealed }
  fileprivate var canRemove: Bool {
    sealed && ended && deferredFailures == 0 && pendingValues.isEmpty && cleanupPending.isEmpty && pendingRetirements.isEmpty && events.isEmpty && terminal == nil
  }

  fileprivate func currentClaim(_ route: OrderedNotificationRoute) -> OrderedNotificationClaim? {
    guard terminal == nil, !ended else { return nil }
    return claims[route]
  }

  /// Queue-confined exact ownership check used by the bridge when a logical
  /// retirement was held behind an earlier captured callback.  Do not expose
  /// the mutable claim itself: callers only need to decide whether submitting
  /// its source fence remains valid.
  func ownsCurrentClaim(_ claim: OrderedNotificationClaim) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    return terminal == nil && !ended && claims[claim.route] == claim
  }

  fileprivate func failureClaim(_ route: OrderedNotificationRoute) -> OrderedNotificationClaim? {
    guard !sealed else { return nil }
    return claims[route]
  }

  fileprivate func accepts(_ claim: OrderedNotificationClaim) -> Bool {
    terminal == nil && !ended && claims[claim.route] == claim
  }

  fileprivate func admit(claim: OrderedNotificationClaim, ordinal: UInt64, bytes: Data, provenance: Provenance) {
    _ = reserveValue(claim: claim, ordinal: ordinal, bytes: bytes, provenance: provenance, deferred: false)
  }

  fileprivate func reserveValue(claim: OrderedNotificationClaim, ordinal: UInt64, bytes: Data, provenance: Provenance, deferred: Bool) -> UUID? {
    let member = claim.route
    guard accepts(claim), let epoch = epochs[member] else { return nil }
    guard ordinaryCount < capacity, bytes.count <= byteCapacity - queuedBytes else {
      end(.overflow, platformTail: .uncertain, rejected: .value(member: member, epoch: epoch, bytes: UInt64(bytes.count)))
      return nil
    }
    queuedBytes += bytes.count
    let needsWake = events.isEmpty
    let ticket = deferred ? UUID() : nil
    if let ticket { pendingValues[ticket] = events.count }
    events.append(.value(member: member, epoch: epoch, ordinal: ordinal, bytes: Data(bytes), provenance: provenance))
    ordinaryCount += 1
    if needsWake { wake() }
    return ticket
  }

  fileprivate func settleValue(_ ticket: UUID, deliver: Bool) {
    guard let index = pendingValues.removeValue(forKey: ticket) else { return }
    guard case let .value(member, _, _, bytes, _) = events[index] else { preconditionFailure("missing reserved value") }
    if !deliver {
      queuedBytes -= bytes.count
      // The native value was observed, but the gate could not deliver it.
      // Keep its position and report one omitted source observation.
      events[index] = .loss(member: member, count: 1)
    }
    wake()
  }

  fileprivate func failSource(_ failure: NativeFailure, claim: OrderedNotificationClaim) {
    guard !sealed, claims[claim.route] == claim, let epoch = epochs[claim.route] else { return }
    if terminal == nil && !ended {
      end(.sourceFailed(failure, member: claim.route, sourceMember: claim.token, epoch: epoch), platformTail: .uncertain)
    } else {
      appendLateFailure(failure.attributed(to: claim, epoch: epoch))
    }
  }

  /// Admit a capture whose payload and acknowledgement remain owned by the
  /// group capture owner. Its original claim determines whether it can end
  /// this ingress; a retired claim must never terminate a replacement.
  func admitOwnedFailure(_ failure: NativeFailure) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let origin = failure.origin else { preconditionFailure("capture missing source identity") }
    let claim = origin.claim
    if !sealed, terminal == nil, !ended, claims[claim.route] == claim, epochs[claim.route] == origin.epoch {
      let native = NativeFailure(domain: failure.domain, code: failure.code, detail: failure.detail)
      end(.sourceFailed(native, member: claim.route, sourceMember: claim.token, epoch: origin.epoch), platformTail: .uncertain, notify: false)
    } else {
      events.append(.subsequentSourceFailure(failure))
      ownedLateFailures += 1
    }
  }

  /// A captured callback already owns its observation position.  When that
  /// callback is replayed after a graph handoff, its classification must be
  /// inserted before observations admitted after the capture, rather than
  /// appended behind them.  The bridge only calls this while no foreign batch
  /// is outstanding, so index zero is the next unacknowledged position.
  func admitOwnedFailureBeforeQueued(_ failure: NativeFailure) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let origin = failure.origin else { preconditionFailure("capture missing source identity") }
    let claim = origin.claim
    let event: Event
    if !sealed, !ended, claims[claim.route] == claim, epochs[claim.route] == origin.epoch {
      let native = NativeFailure(domain: failure.domain, code: failure.code, detail: failure.detail)
      registered.removeAll()
      // A close/invalidation admitted after the retained callback has not
      // crossed that callback.  It cannot classify the earlier observation;
      // discard that superseded terminal and retain the callback's terminal.
      if terminal != nil {
        events.removeAll { event in
          if case .terminal = event { return true }
          return false
        }
      }
      event = .terminal(.sourceFailed(native, member: claim.route, sourceMember: claim.token, epoch: origin.epoch), platformTail: .uncertain, rejected: nil)
      terminal = event
    } else {
      event = .subsequentSourceFailure(failure)
      lateFailureCount += 1
      ownedLateFailures += 1
    }
    insertOwnedCapture(event)
  }

  /// Omission ownership and its bounded storage remain with the capture owner.
  func admitOwnedFailureOmission() {
    dispatchPrecondition(condition: .onQueue(queue))
    events.append(.failureObservationsOmitted)
  }

  func admitOwnedFailureOmissionBeforeQueued() {
    dispatchPrecondition(condition: .onQueue(queue))
    insertOwnedCapture(.failureObservationsOmitted)
  }

  /// Retain a callback observed before a graph barrier until it is replayed.
  fileprivate func retainDeferredFailure(_ claim: OrderedNotificationClaim) -> UInt64? {
    guard !sealed, claims[claim.route] == claim, let epoch = epochs[claim.route] else { return nil }
    deferredFailures += 1
    return epoch
  }

  fileprivate func settleDeferredFailure(_ failure: NativeFailure?, claim: OrderedNotificationClaim, epoch: UInt64) {
    precondition(deferredFailures > 0)
    deferredFailures -= 1
    guard let failure else { return }
    if claims[claim.route] == claim && epochs[claim.route] == epoch {
      failSource(failure, claim: claim)
    } else {
      appendLateFailure(failure.attributed(to: claim, epoch: epoch))
    }
  }

  fileprivate func exhaustMarker(id: UInt64) {
    end(.ordinalExhausted, platformTail: .uncertain, rejected: .marker(id: id))
  }

  fileprivate func exhaustOrdinals(claim: OrderedNotificationClaim?, byteCount: Int) {
    let rejected: Rejected?
    if let claim, let epoch = epochs[claim.route], accepts(claim) {
      rejected = .value(member: claim.route, epoch: epoch, bytes: UInt64(byteCount))
    } else {
      rejected = nil
    }
    end(.ordinalExhausted, platformTail: .uncertain, rejected: rejected)
  }

  /// Unknown source loss remains unknown; a lost control ends the collection.
  func reportLoss(member: OrderedNotificationRoute?, count: UInt64?) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard terminal == nil, !ended else { return }
    _ = append(.loss(member: member, count: count))
  }

  func retainCleanup(_ claim: OrderedNotificationClaim) { dispatchPrecondition(condition: .onQueue(queue)); if claim.generation == generation && members.contains(claim.route) { cleanupPending.insert(claim) } }
  func cleanupFinished(_ claim: OrderedNotificationClaim) { dispatchPrecondition(condition: .onQueue(queue)); cleanupPending.remove(claim) }
  func invalidate(_ cause: End) { dispatchPrecondition(condition: .onQueue(queue)); end(cause, platformTail: .uncertain) }
  func close() { dispatchPrecondition(condition: .onQueue(queue)); end(.closed, platformTail: .uncertain) }
  /// Seal only an ended source; queued records and the reserved terminal remain drainable.
  func seal() throws {
    dispatchPrecondition(condition: .onQueue(queue))
    guard (terminal != nil || ended || sealed), deferredFailures == 0, pendingValues.isEmpty,
          pendingRetirements.isEmpty else { throw Failure.inactive }
    sealed = true
    claims.removeAll()
  }
  func failWake() { dispatchPrecondition(condition: .onQueue(queue)); end(.wakeFailed, platformTail: .uncertain) }

  /// After a retained foreign batch is acknowledged, re-signal work that may
  /// have arrived while the host was still replaying that prior batch.
  func wakeIfPending() {
    dispatchPrecondition(condition: .onQueue(queue))
    if !sealed && !events.isEmpty { wake() }
  }

  /// Terminal is reserved outside the value/control FIFO and cannot be crowded out.
  func drain() -> [Event] {
    let (result, failureCredits) = drainForAcknowledgement()
    acknowledgeFailureCredits(failureCredits)
    return result
  }

  /// The bridge owns these credits while the returned records are in its
  /// unacknowledged batch, even though the ingress FIFO has been drained.
  func drainForAcknowledgement() -> ([Event], Int) {
    dispatchPrecondition(condition: .onQueue(queue))
    // A platform callback captured before a graph handoff has an observation
    // position even though its eventual classification is not known yet.  Do
    // not let later values or controls escape while that position is pending.
    guard deferredFailures == 0 else { return ([], 0) }
    admitPendingRetirements()
    let pendingValuePrefix = pendingValues.values.min() ?? events.count
    let capturePrefix = externalCaptureBarriers.values.min() ?? events.count
    let prefix = min(pendingValuePrefix, capturePrefix)
    // One full ordinary FIFO can contain a deferred control that must precede
    // a later terminal. Drain only through the first released credit, publish
    // that retained control in-place, and let the normal wake/drain cycle
    // deliver the rest. This preserves source order without requiring callers
    // to retry a retirement manually.
    let controlPrefix: Int
    if !pendingRetirements.isEmpty, ordinaryCount >= capacity,
       let firstOrdinary = events.firstIndex(where: { event in
         switch event {
         case .value, .fence, .barrier, .marker, .loss: return true
         case .terminal, .subsequentSourceFailure, .failureObservationsOmitted: return false
         }
       }) {
      controlPrefix = min(prefix, firstOrdinary + 1)
    } else {
      controlPrefix = prefix
    }
    let result = Array(events.prefix(controlPrefix))
    let failureCredits = result.reduce(into: 0) { count, event in
      if case .subsequentSourceFailure = event { count += 1 }
    }
    // Owned late failures occur only after a terminal, which prevents new
    // value reservations; they therefore cannot straddle a pending value.
    let ownedDrained = controlPrefix == events.count ? ownedLateFailures : 0
    let creditedFailures = failureCredits - ownedDrained
    ownedLateFailures -= ownedDrained
    events.removeFirst(controlPrefix)
    for (ticket, index) in pendingValues { pendingValues[ticket] = index - controlPrefix }
    for (ticket, index) in externalCaptureBarriers { externalCaptureBarriers[ticket] = index - controlPrefix }
    for event in result {
      switch event {
      case let .value(_, _, _, bytes, _): queuedBytes -= bytes.count; ordinaryCount -= 1
      case .fence, .barrier, .marker, .loss: ordinaryCount -= 1
      case .failureObservationsOmitted: lateFailuresOmitted = false
      case .terminal: terminal = nil; ended = true
      case .subsequentSourceFailure: break
      }
    }
    admitPendingRetirements()
    return (result, creditedFailures)
  }

  func acknowledgeFailureCredits(_ count: Int) {
    dispatchPrecondition(condition: .onQueue(queue))
    precondition(count >= 0 && count <= lateFailureCount)
    lateFailureCount -= count
  }

  private var ended = false
  private func insertOwnedCapture(_ event: Event) {
    // No payload bytes are introduced by a captured failure/omission.  Shift
    // only deferred value indexes; their existing byte reservation remains
    // owned until their own settlement and exact batch acknowledgement.
    events.insert(event, at: 0)
    for (ticket, index) in pendingValues { pendingValues[ticket] = index + 1 }
    wake()
  }
  private func appendLateFailure(_ failure: NativeFailure) {
    let needsWake = events.isEmpty
    if lateFailureCount < capacity {
      events.append(.subsequentSourceFailure(failure))
      lateFailureCount += 1
    } else if !lateFailuresOmitted {
      events.append(.failureObservationsOmitted)
      lateFailuresOmitted = true
    }
    if needsWake && !events.isEmpty { wake() }
  }
  private func append(_ event: Event) -> Bool {
    guard ordinaryCount < capacity else {
      let rejected: Rejected?
      switch event {
      case let .loss(member, count): rejected = .sourceLoss(member: member, skippedRecords: count)
      case let .fence(member, epoch, _): rejected = .fence(member: member, epoch: epoch)
      case let .barrier(id, _): rejected = .barrier(id: id)
      case let .marker(id, _): rejected = .marker(id: id)
      case let .value(member, epoch, _, bytes, _): rejected = .value(member: member, epoch: epoch, bytes: UInt64(bytes.count))
      case .terminal, .subsequentSourceFailure, .failureObservationsOmitted: rejected = nil
      }
      end(.overflow, platformTail: .uncertain, rejected: rejected)
      return false
    }
    let needsWake = events.isEmpty
    events.append(event)
    ordinaryCount += 1
    if needsWake { wake() }
    return true
  }
  /// Publish retained retirements in original request order. A terminal is a
  /// reserved end marker, so controls observed before it are inserted just
  /// before it rather than being silently stranded after shutdown.
  private func admitPendingRetirements() {
    guard deferredFailures == 0 else { return }
    var didAdmit = false
    while ordinaryCount < capacity, !pendingRetirements.isEmpty {
      let pending = pendingRetirements.removeFirst()
      let event = Event.fence(member: pending.member, epoch: pending.epoch, platformTail: .uncertain)
      let index = events.firstIndex { if case .terminal = $0 { return true }; return false } ?? events.endIndex
      events.insert(event, at: index)
      for (ticket, valueIndex) in pendingValues where valueIndex >= index { pendingValues[ticket] = valueIndex + 1 }
      for (ticket, barrierIndex) in externalCaptureBarriers where barrierIndex >= index { externalCaptureBarriers[ticket] = barrierIndex + 1 }
      ordinaryCount += 1
      didAdmit = true
    }
    if didAdmit { wake() }
  }
  private func end(_ reason: End, platformTail: PlatformTail, rejected: Rejected? = nil, notify: Bool = true) {
    guard !sealed, terminal == nil, !ended else { return }
    registered.removeAll()
    let needsWake = events.isEmpty
    let event = Event.terminal(reason, platformTail: platformTail, rejected: rejected)
    terminal = event
    events.append(event)
    if needsWake && notify { wake() }
  }
}

/// Defers even a synchronous platform completion until the owner has recorded
/// the admitted transaction and left its current queue turn.
enum OwnedOrderedWriteDispatch {
  static func start(queue: DispatchQueue, admit: () throws -> UInt64,
                    write: (@escaping (NSError?) -> Void) -> Void,
                    completion: @escaping (NSError?) -> Void) throws -> UInt64 {
    dispatchPrecondition(condition: .onQueue(queue))
    let ordinal = try admit()
    write { error in queue.async { completion(error) } }
    return ordinal
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
      for group in groups {
        let claim = targets.first(where: { $0.0 === group })?.1
        group.exhaustOrdinals(claim: claim, byteCount: bytes.count)
      }
      return
    }
    let ordinal = nextOrdinal
    if ordinal == UInt64.max { ordinalAvailable = false } else { nextOrdinal += 1 }
    let owned = Data(bytes)
    for (group, claim) in targets { group.admit(claim: claim, ordinal: ordinal, bytes: owned, provenance: provenance) }
  }

  /// Reserve the value and its ordinal before a graph gate can defer it. The
  /// one-shot settlement owns only bounded ingress tickets, not another copy
  /// of the payload. A refusal is an explicit loss at the reserved position.
  func reserveValue(route: OrderedNotificationRoute, bytes: Data,
                    provenance: OwnedOrderedNotificationIngress.Provenance) -> (Bool) -> Void {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !invalidated else { return { _ in } }
    let targets = groups.compactMap { group in group.currentClaim(route).map { (group, $0) } }
    guard ordinalAvailable else {
      for group in groups {
        group.exhaustOrdinals(claim: targets.first(where: { $0.0 === group })?.1, byteCount: bytes.count)
      }
      return { _ in }
    }
    let ordinal = nextOrdinal
    if ordinal == UInt64.max { ordinalAvailable = false } else { nextOrdinal += 1 }
    let tickets = targets.compactMap { group, claim -> (OwnedOrderedNotificationIngress, UUID)? in
      group.reserveValue(claim: claim, ordinal: ordinal, bytes: bytes, provenance: provenance, deferred: true).map { (group, $0) }
    }
    var settled = false
    return { deliver in
      dispatchPrecondition(condition: .onQueue(self.queue))
      guard !settled else { return }
      settled = true
      for (group, ticket) in tickets { group.settleValue(ticket, deliver: deliver) }
    }
  }

  /// Reserve the marker in this exact group on the delegate queue. The ordinal
  /// shares the callback source sequence; it does not establish radio causation.
  @discardableResult
  func admitMarker(group: OwnedOrderedNotificationIngress, id: UInt64) throws -> UInt64 {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !invalidated, groups.contains(where: { $0 === group }), group.markerActive else { throw OwnedOrderedNotificationIngress.Failure.inactive }
    guard ordinalAvailable else {
      group.exhaustMarker(id: id)
      throw OwnedOrderedNotificationIngress.Failure.markerOrdinalExhausted
    }
    let ordinal = nextOrdinal
    try group.admitMarker(id: id, ordinal: ordinal)
    if ordinal == UInt64.max { ordinalAvailable = false } else { nextOrdinal += 1 }
    return ordinal
  }

  /// An unsolicited CoreBluetooth failure has an exact route but no value.
  /// Terminate only groups that currently own that route.
  func fail(route: OrderedNotificationRoute, failure: OwnedOrderedNotificationIngress.NativeFailure) {
    dispatchPrecondition(condition: .onQueue(queue))
    captureFailure(route: route, failure: failure)()
  }

  /// Capture membership at observation, before a graph gate can defer delivery.
  /// The exact captured claim and epoch stay owned through replay, including
  /// retirement or replacement before delivery. The returned closure is one-shot.
  func captureFailure(route: OrderedNotificationRoute, failure: OwnedOrderedNotificationIngress.NativeFailure,
                      claim expected: OrderedNotificationClaim? = nil) -> () -> Void {
    dispatchPrecondition(condition: .onQueue(queue))
    let targets: [(OwnedOrderedNotificationIngress, OrderedNotificationClaim, UInt64)] = invalidated ? [] : groups.compactMap { group in
      guard let claim = group.failureClaim(route), expected == nil || expected == claim,
            let epoch = group.retainDeferredFailure(claim) else { return nil }
      return (group, claim, epoch)
    }
    var settled = false
    return {
      guard !settled else { return }
      settled = true
      for (group, claim, epoch) in targets { group.settleDeferredFailure(failure, claim: claim, epoch: epoch) }
    }
  }

  /// The returned settlement is one-shot and retains only the exact member
  /// which owned the callback at observation, even if it later retires.
  func captureDeferredFailure(claim: OrderedNotificationClaim, failure: OwnedOrderedNotificationIngress.NativeFailure) -> ((Bool) -> Void)? {
    dispatchPrecondition(condition: .onQueue(queue))
    let targets: [(OwnedOrderedNotificationIngress, UInt64)] = invalidated ? [] : groups.compactMap { group in
      group.retainDeferredFailure(claim).map { (group, $0) }
    }
    guard !targets.isEmpty else { return nil }
    var settled = false
    return { report in
      guard !settled else { return }
      settled = true
      for (group, epoch) in targets { group.settleDeferredFailure(report ? failure : nil, claim: claim, epoch: epoch) }
    }
  }

  /// Snapshot every group owning the physical route, including groups whose
  /// activation is independent of an ordinary subscription's route record.
  func captureDeferredFailure(route: OrderedNotificationRoute, failure: OwnedOrderedNotificationIngress.NativeFailure) -> ((Bool) -> Void)? {
    dispatchPrecondition(condition: .onQueue(queue))
    let targets: [(OwnedOrderedNotificationIngress, OrderedNotificationClaim, UInt64)] = invalidated ? [] : groups.compactMap { group in
      guard let claim = group.failureClaim(route), let epoch = group.retainDeferredFailure(claim) else { return nil }
      return (group, claim, epoch)
    }
    guard !targets.isEmpty else { return nil }
    var settled = false
    return { report in
      guard !settled else { return }
      settled = true
      for (group, claim, epoch) in targets { group.settleDeferredFailure(report ? failure : nil, claim: claim, epoch: epoch) }
    }
  }

  func invalidate(_ cause: OwnedOrderedNotificationIngress.End) {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !invalidated else { return }
    invalidated = true
    for group in groups { group.invalidate(cause) }
  }
}

/// One peer's owner-queue gate for a short native graph handoff. Deferred work captures
/// immutable callback data at ingress; overflow breaks an uncommitted permit.
final class OwnedOrderedGraphPermitGate {
  struct Key: Equatable {
    let authority: UInt64
    let peer: String
    let connection: String
    let snapshot: OwnedGattSnapshotIdentity
    let id: UInt64
    init(authority: UInt64 = 0, peer: String, connection: String, snapshot: OwnedGattSnapshotIdentity, id: UInt64) {
      self.authority = authority; self.peer = peer; self.connection = connection; self.snapshot = snapshot; self.id = id
    }
  }
  enum State: Equatable { case acquired, committed, invalidated, notAcquired, stale, busy, released }
  enum Acquire: Equatable { case acquired, notAcquired, stale, busy }
  private struct Receipt {
    let key: Key
    var state: State
  }
  private let queue: DispatchQueue
  private let maxItems: Int
  private let maxBytes: Int
  private let maxReservedControls: Int
  private let overflow: () -> Void
  private let barrierOverflow: () -> Void
  private var receipt: Receipt?
  private var outcomes: [(key: Key, result: State)] = []
  private var lastForgotten: Key?
  private var replaying = false
  private var drainingReplay = false
  private var barrierMutation = false
  private var notificationControls: [UUID: [OwnedGraphDeferredQueue.Token]] = [:]
  private lazy var deferred = OwnedGraphDeferredQueue(maxItems: maxItems, maxBytes: maxBytes, maxReservedControls: maxReservedControls, onOverflow: { [weak self] in
    guard let self, var current = self.receipt, current.state == .acquired else { return }
    current.state = .invalidated
    self.receipt = current
  }, onBarrierOverflow: { [weak self] in self?.barrierOverflow() })
  var isHolding: Bool { (receipt != nil || deferred.hasBarrier) && !replaying }
  /// Notification-state replay calls its private handler directly, so a new
  /// delegate callback during replay is ingress and must join the FIFO.
  var isDeferringNotificationIngress: Bool { isHolding || (deferred.isDraining && !barrierMutation) }
  var hasOutstandingBarrier: Bool { deferred.hasBarrier }
  var canDiscard: Bool { receipt == nil && outcomes.isEmpty && deferred.isEmpty && notificationControls.isEmpty }
  fileprivate func isOwned(by queue: DispatchQueue) -> Bool { self.queue === queue }

  init(queue: DispatchQueue, maxItems: Int, maxBytes: Int, maxReservedControls: Int = 0,
       barrierOverflow: (() -> Void)? = nil, overflow: @escaping () -> Void) {
    precondition(maxItems > 0 && maxBytes > 0 && maxReservedControls >= 0)
    self.queue = queue; self.maxItems = maxItems; self.maxBytes = maxBytes
    self.maxReservedControls = maxReservedControls; self.overflow = overflow
    self.barrierOverflow = barrierOverflow ?? overflow
  }

  /// Reserve the complete gate budget for one route lifetime. This is only a
  /// gate credit: callers must separately reserve every downstream recipient
  /// before admitting a source that can produce notification failures.
  func reserveNotificationControls(route: UUID, count: Int) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard notificationControls[route] == nil, let tokens = deferred.reserveControls(count: count) else { return false }
    notificationControls[route] = tokens
    return true
  }

  /// Refusal leaves the entire reservation intact, including during delivery.
  /// The owner must retry retirement after all queued callbacks have settled.
  func releaseNotificationControls(route: UUID) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let tokens = notificationControls[route], deferred.releaseControls(tokens) else { return false }
    notificationControls.removeValue(forKey: route)
    return true
  }

  /// A full reservation returns false without running the callback or making
  /// an overflow claim. The owner must retain and handle that refusal.
  @discardableResult
  func enqueueReservedNotificationControl(route: UUID, _ operation: @escaping () -> Void) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let tokens = notificationControls[route] else { return false }
    for token in tokens where deferred.enqueueReservedControl(token, run: operation) {
      if receipt == nil { drain() }
      return true
    }
    return false
  }

  private func recorded(_ key: Key) -> State? { outcomes.first(where: { $0.key == key })?.result }

  func acquire(_ key: Key, valid: Bool) -> Acquire {
    dispatchPrecondition(condition: .onQueue(queue))
    if let state = recorded(key) {
      switch state { case .notAcquired: return .notAcquired; case .busy: return .busy; default: return .stale }
    }
    if let receipt, receipt.key == key { return .acquired }
    guard outcomes.count < 64, key.id > 0 else { return .stale }
    if receipt != nil { outcomes.append((key, .busy)); return .busy }
    guard valid else { outcomes.append((key, .stale)); return .stale }
    receipt = Receipt(key: key, state: .acquired)
    return .acquired
  }

  /// Cancellation before queue admission is retained without holding a permit.
  func cancelBeforeAcquire(_ key: Key) -> Acquire {
    dispatchPrecondition(condition: .onQueue(queue))
    if let state = recorded(key) {
      switch state { case .notAcquired: return .notAcquired; case .busy: return .busy; default: return .stale }
    }
    if let receipt, receipt.key == key { return .acquired }
    guard outcomes.count < 64, key.id > 0 else { return .stale }
    if receipt != nil { outcomes.append((key, .busy)); return .busy }
    outcomes.append((key, .notAcquired))
    return .notAcquired
  }

  func commit(_ key: Key) -> State? {
    dispatchPrecondition(condition: .onQueue(queue))
    guard var current = receipt, current.key == key else { return nil }
    if current.state == .acquired { current.state = .committed; receipt = current }
    return current.state
  }

  func release(_ key: Key) -> State? {
    dispatchPrecondition(condition: .onQueue(queue))
    if recorded(key) == .released { return .released }
    guard let current = receipt, current.key == key else { return nil }
    receipt = nil
    outcomes.append((key, .released))
    drain()
    return .released
  }

  func forget(_ key: Key) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    if lastForgotten == key { return true }
    guard let index = outcomes.firstIndex(where: { $0.key == key }) else { return false }
    outcomes.remove(at: index)
    lastForgotten = key
    return true
  }

  @discardableResult
  func invalidateAndEnqueue(bytes: Int = 0, _ operation: @escaping () -> Void) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    if var current = receipt, current.state == .acquired {
      current.state = .invalidated
      receipt = current
    }
    return enqueue(bytes: bytes, operation)
  }

  func breakForOverflow() {
    dispatchPrecondition(condition: .onQueue(queue))
    if var current = receipt, current.state == .acquired { current.state = .invalidated; receipt = current }
    // Keep the already accepted observations ahead of the fail-stop. The
    // queue reserves a loss marker when its ordinary capacity is exhausted.
    _ = deferred.enqueueValue(bytes: 0, run: overflow, onLoss: overflow)
    if receipt == nil { drain() }
  }

  @discardableResult
  func enqueueBarrier(invalidating: Bool = true, _ arrived: @escaping (@escaping () -> Void) -> Void) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    if invalidating, var current = receipt, current.state == .acquired { current.state = .invalidated; receipt = current }
    let accepted = deferred.enqueueBarrier(arrived: arrived)
    if receipt == nil { drain() }
    return accepted
  }

  func beginBarrierMutation() { dispatchPrecondition(condition: .onQueue(queue)); barrierMutation = true; replaying = true }
  func endBarrierMutation() { dispatchPrecondition(condition: .onQueue(queue)); barrierMutation = false; replaying = drainingReplay }

  @discardableResult
  func enqueue(bytes: Int = 0, reject: (() -> Void)? = nil, _ operation: @escaping () -> Void) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard (deferred.isDraining && !barrierMutation) || (!replaying && (receipt != nil || deferred.hasBarrier)) else { operation(); return true }
    if let reject { return deferred.enqueueRequest(bytes: bytes, run: operation, onReject: reject) != nil }
    return deferred.enqueueValue(bytes: bytes, run: operation, onLoss: overflow) != nil
  }

  private func drain() {
    guard !drainingReplay else { return }
    drainingReplay = true
    replaying = true
    deferred.drain()
    replaying = false
    drainingReplay = false
  }

}

/// Queue-confined receipt capacity. A reservation keeps capacity charged until
/// every receipt is acknowledged and its owner explicitly retires it.
final class OwnedNotificationReceiptBudget {
  final class Reservation {
    fileprivate let budget: OwnedNotificationReceiptBudget
    fileprivate let count: Int
    fileprivate var outstanding = 0
    fileprivate var released = false

    fileprivate init(budget: OwnedNotificationReceiptBudget, count: Int) {
      self.budget = budget; self.count = count
    }
    fileprivate func claim() -> Bool {
      dispatchPrecondition(condition: .onQueue(budget.queue))
      guard !released, outstanding < count else { return false }
      outstanding += 1
      return true
    }
    fileprivate func acknowledge() {
      dispatchPrecondition(condition: .onQueue(budget.queue))
      precondition(!released && outstanding > 0)
      outstanding -= 1
    }
    func release() {
      dispatchPrecondition(condition: .onQueue(budget.queue))
      precondition(!released && outstanding == 0)
      released = true
      budget.reserved -= count
    }
  }

  fileprivate let queue: DispatchQueue
  private let capacity: Int
  private var reserved = 0
  var available: Int {
    dispatchPrecondition(condition: .onQueue(queue))
    return capacity - reserved
  }
  init(queue: DispatchQueue, capacity: Int) {
    precondition(capacity >= 0)
    self.queue = queue; self.capacity = capacity
  }
  func reserve(_ count: Int) -> Reservation? {
    dispatchPrecondition(condition: .onQueue(queue))
    guard count > 0, count <= capacity - reserved else { return nil }
    reserved += count
    return Reservation(budget: self, count: count)
  }
}

/// Atomic reservation of the real graph gate and all recipient receipt budgets.
/// Callers still own bounded payload storage, physical cleanup, and delivery.
final class OwnedNotificationAdmissionTransaction {
  final class Observation {
    fileprivate let owner: OwnedNotificationAdmissionTransaction
    fileprivate var acknowledged: [Bool]
    fileprivate var enqueued = false
    fileprivate var replayStarted = false
    fileprivate var replayFinished = false
    fileprivate let overflow: Bool

    fileprivate init(owner: OwnedNotificationAdmissionTransaction, overflow: Bool) {
      self.owner = owner; self.overflow = overflow
      acknowledged = Array(repeating: false, count: owner.recipients.count)
    }
    func acknowledge(recipient: Int) {
      dispatchPrecondition(condition: .onQueue(owner.queue))
      precondition(acknowledged.indices.contains(recipient))
      precondition(replayStarted, "a receipt cannot be acknowledged before delivery")
      guard !acknowledged[recipient] else { return }
      acknowledged[recipient] = true
      owner.recipients[recipient].acknowledge()
      owner.finishIfSettled(self)
    }
  }

  private let queue: DispatchQueue
  private let gate: OwnedOrderedGraphPermitGate
  private let identity: UUID
  private let recipients: [OwnedNotificationReceiptBudget.Reservation]
  private let concurrentObservations: Int
  private var ordinaryOutstanding = 0
  private var overflowOutstanding = false
  private var retiring = false
  private var released = false

  private init(queue: DispatchQueue, gate: OwnedOrderedGraphPermitGate, identity: UUID,
               recipients: [OwnedNotificationReceiptBudget.Reservation], concurrentObservations: Int) {
    self.queue = queue; self.gate = gate; self.identity = identity
    self.recipients = recipients; self.concurrentObservations = concurrentObservations
  }

  static func prepare(queue: DispatchQueue, gate: OwnedOrderedGraphPermitGate, identity: UUID,
                      recipients: [OwnedNotificationReceiptBudget], concurrentObservations: Int) -> OwnedNotificationAdmissionTransaction? {
    dispatchPrecondition(condition: .onQueue(queue))
    guard concurrentObservations > 0, concurrentObservations < Int.max,
          gate.isOwned(by: queue), !recipients.isEmpty, Set(recipients.map(ObjectIdentifier.init)).count == recipients.count,
          recipients.allSatisfy({ $0.queue === queue }) else { return nil }
    let count = concurrentObservations + 1 // independent first-overflow receipt
    var reservations: [OwnedNotificationReceiptBudget.Reservation] = []
    for recipient in recipients {
      guard let reservation = recipient.reserve(count) else {
        for reservation in reservations.reversed() { reservation.release() }
        return nil
      }
      reservations.append(reservation)
    }
    guard gate.reserveNotificationControls(route: identity, count: count) else {
      for reservation in reservations.reversed() { reservation.release() }
      return nil
    }
    return OwnedNotificationAdmissionTransaction(queue: queue, gate: gate, identity: identity,
      recipients: reservations, concurrentObservations: concurrentObservations)
  }

  func claim() -> Observation? { claim(overflow: false) }
  func claimOverflow() -> Observation? { claim(overflow: true) }

  private func claim(overflow: Bool) -> Observation? {
    dispatchPrecondition(condition: .onQueue(queue))
    guard !retiring, !released,
          overflow ? !overflowOutstanding : ordinaryOutstanding < concurrentObservations else { return nil }
    var claimed: [OwnedNotificationReceiptBudget.Reservation] = []
    for recipient in recipients {
      guard recipient.claim() else {
        for reservation in claimed.reversed() { reservation.acknowledge() }
        return nil
      }
      claimed.append(recipient)
    }
    if overflow { overflowOutstanding = true } else { ordinaryOutstanding += 1 }
    return Observation(owner: self, overflow: overflow)
  }

  @discardableResult
  func enqueue(_ observation: Observation, _ operation: @escaping () -> Void) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard observation.owner === self, !observation.enqueued, !released else { return false }
    observation.enqueued = true
    let accepted = gate.enqueueReservedNotificationControl(route: identity) {
      observation.replayStarted = true
      operation()
      observation.replayFinished = true
      self.finishIfSettled(observation)
    }
    if !accepted { observation.enqueued = false }
    return accepted
  }

  /// A claim that was never published into the gate can be rolled back.
  @discardableResult
  func cancelUnqueued(_ observation: Observation) -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    guard observation.owner === self, !observation.enqueued, !observation.replayFinished,
          observation.acknowledged.allSatisfy({ !$0 }) else { return false }
    observation.enqueued = true // fence repeated cancellation and publication
    for index in observation.acknowledged.indices {
      observation.acknowledged[index] = true
      recipients[index].acknowledge()
    }
    if observation.overflow { overflowOutstanding = false } else { ordinaryOutstanding -= 1 }
    return true
  }

  private func finishIfSettled(_ observation: Observation) {
    guard observation.replayFinished, observation.acknowledged.allSatisfy({ $0 }) else { return }
    observation.replayFinished = false // exact-once settlement
    if observation.overflow { overflowOutstanding = false } else { ordinaryOutstanding -= 1 }
  }

  /// A false result retains every reservation; retry after replay and receipts settle.
  func releaseWhenIdle() -> Bool {
    dispatchPrecondition(condition: .onQueue(queue))
    if released { return true }
    retiring = true
    guard ordinaryOutstanding == 0, !overflowOutstanding,
          gate.releaseNotificationControls(route: identity) else { return false }
    for reservation in recipients.reversed() { reservation.release() }
    released = true
    return true
  }
}

/// Bounded host-global issued-ID window; exact live keys remain owned until forget.
final class OwnedOrderedGraphPermitRegistry {
  typealias Key = OwnedOrderedGraphPermitGate.Key
  struct Request: Equatable {
    let authority: UInt64
    let request: UInt64
    let peer: String
    let connection: String
    let snapshot: OwnedGattSnapshotIdentity
  }
  enum Reservation { case accepted, stale, foreign, full }
  private let lock = NSRecursiveLock()
  private var frontier: UInt64 = 0
  private var live: [UInt64: Key] = [:]
  private var retired: [UInt64: Key] = [:]
  private var refused: Set<UInt64> = []
  private var requests: [UInt64: (Request, UInt64)] = [:]
  private var issued: [UInt64: Request] = [:]
  private var nextPermit: UInt64 = 1
  private var available = true
  private let window: UInt64 = 256

  func issue(request: Request) -> UInt64? { lock.lock(); defer { lock.unlock() }
    if let existing = requests[request.request] { return existing.0 == request ? existing.1 : nil }
    guard request.request > 0, available, issued.count < 64, nextPermit - frontier <= window else { return nil }
    let id = nextPermit
    if id == UInt64.max { available = false } else { nextPermit += 1 }
    requests[request.request] = (request, id); issued[id] = request
    return id
  }
  func issuedPermit(request: Request) -> UInt64? { lock.lock(); defer { lock.unlock() }
    guard let existing = requests[request.request], existing.0 == request else { return nil }
    return existing.1
  }
  func requestConflicts(_ request: Request) -> Bool { lock.lock(); defer { lock.unlock() }
    guard let existing = requests[request.request] else { return false }
    return existing.0 != request
  }
  func forgetRequest(_ request: Request) -> Bool { lock.lock(); defer { lock.unlock() }
    guard let existing = requests[request.request], existing.0 == request,
          existing.1 <= frontier || retired[existing.1] != nil else { return false }
    requests.removeValue(forKey: request.request); issued.removeValue(forKey: existing.1)
    return true
  }

  func reserve(_ key: Key) -> Reservation { lock.lock(); defer { lock.unlock() };
    guard let request = issued[key.id] else { return .stale }
    guard request.authority == key.authority, request.peer == key.peer, request.connection == key.connection,
          request.snapshot == key.snapshot else { return .foreign }
    if key.id <= frontier { return .stale }
    if let old = live[key.id] { return old == key ? .accepted : .foreign }
    if let old = retired[key.id] { return old == key ? .stale : .foreign }
    guard key.id - frontier <= window else { return .full }
    live[key.id] = key
    return .accepted
  }
  var canClose: Bool { lock.lock(); defer { lock.unlock() }; return live.isEmpty && retired.isEmpty && issued.isEmpty }
  func owns(_ key: Key) -> Bool { lock.lock(); defer { lock.unlock() }; return live[key.id] == key }
  func markBusy(_ key: Key) { lock.lock(); defer { lock.unlock() }; if owns(key) { refused.insert(key.id) } }
  func isBusy(_ key: Key) -> Bool { lock.lock(); defer { lock.unlock() }; return owns(key) && refused.contains(key.id) }
  func alreadyForgotten(_ key: Key) -> Bool { lock.lock(); defer { lock.unlock() };
    guard key.id > 0, let request = issued[key.id], request.authority == key.authority,
          request.peer == key.peer, request.connection == key.connection, request.snapshot == key.snapshot else { return false }
    if key.id <= frontier { return true }
    return retired[key.id] == key
  }
  func forget(_ key: Key) -> Bool { lock.lock(); defer { lock.unlock() };
    if alreadyForgotten(key) { return true }
    if let existing = live[key.id], existing != key { return false }
    if let existing = retired[key.id], existing != key { return false }
    guard let request = issued[key.id], request.authority == key.authority, request.peer == key.peer,
          request.connection == key.connection, request.snapshot == key.snapshot,
          key.id > frontier, key.id - frontier <= window else { return false }
    live.removeValue(forKey: key.id)
    refused.remove(key.id)
    retired[key.id] = key
    while frontier < UInt64.max, retired.removeValue(forKey: frontier + 1) != nil { frontier += 1 }
    return true
  }
}

/// Native-issued authority-open receipts and independent permit namespaces.
/// The trusted host fences all reserve submissions for a token before forget.
final class OwnedOrderedGraphAuthorities {
  enum Status: Equatable { case pending, notCreated, opened(UInt64), closed(UInt64) }
  private struct OpenReceipt { let request: UInt64; var status: Status }
  private struct Entry { let receipt: UInt64; let permits: OwnedOrderedGraphPermitRegistry; var closed: Bool }
  private let lock = NSRecursiveLock()
  private var receipts: [UInt64: OpenReceipt] = [:]
  private var byRequest: [UInt64: UInt64] = [:]
  private var nextReceipt: UInt64 = 1
  private var receiptAvailable = true
  private var receiptFrontier: UInt64 = 0
  private var retiredReceipts: Set<UInt64> = []
  private var entries: [UInt64: Entry] = [:]
  private var nextAuthority: UInt64 = 1
  private var authorityAvailable = true
  private var authorityFrontier: UInt64 = 0
  private var retiredAuthorities: Set<UInt64> = []

  func reserve(request: UInt64) -> UInt64? { lock.lock(); defer { lock.unlock() };
    if let id = byRequest[request] { return id }
    guard request > 0, receiptAvailable, receipts.count < 64, nextReceipt - receiptFrontier <= 256 else { return nil }
    let id = nextReceipt
    if id == UInt64.max { receiptAvailable = false } else { nextReceipt += 1 }
    receipts[id] = OpenReceipt(request: request, status: .pending)
    byRequest[request] = id
    return id
  }
  func receipt(request: UInt64) -> UInt64? { lock.lock(); defer { lock.unlock() }; return byRequest[request] }
  func issuePermit(_ request: OwnedOrderedGraphPermitRegistry.Request) -> UInt64? {
    lock.lock(); defer { lock.unlock() }
    guard let entry = entries[request.authority], !entry.closed else { return nil }
    return entry.permits.issue(request: request)
  }
  func open(receipt id: UInt64) -> UInt64? { lock.lock(); defer { lock.unlock() };
    guard var receipt = receipts[id] else { return nil }
    switch receipt.status {
    case let .opened(authority), let .closed(authority): return authority
    case .notCreated: return nil
    case .pending: break
    }
    guard authorityAvailable, entries.count < 64, nextAuthority - authorityFrontier <= 256 else {
      receipt.status = .notCreated; receipts[id] = receipt; return nil
    }
    let authority = nextAuthority
    if authority == UInt64.max { authorityAvailable = false } else { nextAuthority += 1 }
    entries[authority] = Entry(receipt: id, permits: OwnedOrderedGraphPermitRegistry(), closed: false)
    receipt.status = .opened(authority); receipts[id] = receipt
    return authority
  }
  func cancelOpen(receipt id: UInt64) -> Status? { lock.lock(); defer { lock.unlock() };
    guard var receipt = receipts[id] else { return nil }
    if receipt.status == .pending { receipt.status = .notCreated; receipts[id] = receipt }
    return receipt.status
  }
  func status(receipt: UInt64) -> Status? { lock.lock(); defer { lock.unlock() }; return receipts[receipt]?.status }
  func permits(_ id: UInt64) -> OwnedOrderedGraphPermitRegistry? { lock.lock(); defer { lock.unlock() };
    guard let entry = entries[id], !entry.closed else { return nil }
    return entry.permits
  }
  func close(_ id: UInt64) -> Bool { lock.lock(); defer { lock.unlock() };
    guard var entry = entries[id], entry.permits.canClose, var receipt = receipts[entry.receipt] else { return false }
    entry.closed = true; entries[id] = entry
    receipt.status = .closed(id); receipts[entry.receipt] = receipt
    return true
  }
  func forget(_ id: UInt64) -> Bool { lock.lock(); defer { lock.unlock() };
    guard id > 0 else { return false }
    if id <= authorityFrontier || retiredAuthorities.contains(id) { return true }
    guard let entry = entries[id], entry.closed else { return false }
    entries.removeValue(forKey: id)
    retiredAuthorities.insert(id)
    while authorityFrontier < UInt64.max, retiredAuthorities.remove(authorityFrontier + 1) != nil { authorityFrontier += 1 }
    return true
  }
  func forgetReceipt(_ id: UInt64) -> Bool { lock.lock(); defer { lock.unlock() };
    guard id > 0 else { return false }
    if id <= receiptFrontier || retiredReceipts.contains(id) { return true }
    guard let receipt = receipts[id] else { return false }
    switch receipt.status {
    case .pending, .opened: return false
    case let .closed(authority): guard entries[authority] == nil else { return false }
    case .notCreated: break
    }
    receipts.removeValue(forKey: id); byRequest.removeValue(forKey: receipt.request)
    retiredReceipts.insert(id)
    while receiptFrontier < UInt64.max, retiredReceipts.remove(receiptFrontier + 1) != nil { receiptFrontier += 1 }
    return true
  }
}
