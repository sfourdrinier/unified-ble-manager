// ios/Owned/UnifiedBleRustOrderedGroupBridge.swift
import Foundation

/// Queue-confined native hooks. A later Rust binding can schedule these on the
/// radio queue; this type does not add a second platform stream or CCCD owner.
final class UnifiedBleRustOrderedGroupBridge {
  /// Slot admission is independent of source and cleanup ownership.
  final class AdmissionSlots {
    struct Spec: Equatable {
      let connectionGeneration: String
      let databaseGeneration: String
      let snapshot: OwnedGattSnapshotIdentity
      let members: [Member]
      let itemCapacity: UInt32
      let byteCapacity: UInt32
    }
    struct Identity: Equatable {
      let version: UInt32
      let owner: UUID
      let slot: UInt32
      let generation: UInt64
      let requestID: UInt64
      let groupID: UInt64
      let peer: String
      let spec: Spec
    }
    struct SlotIdentity: Equatable {
      let owner: UUID
      let slot: UInt32
      let generation: UInt64
    }
    struct FailureCause: Equatable {
      let kind: String
      let gattStatus: Int32?
      let domain: String?
      let code: Int64?
      let detail: String
      let dispatched: Bool

      init(kind: String = "platform", gattStatus: Int32? = nil, domain: String? = nil,
           code: Int64? = nil, detail: String, dispatched: Bool = false) {
        self.kind = kind
        self.gattStatus = gattStatus
        self.domain = domain
        self.code = code
        self.detail = detail
        self.dispatched = dispatched
      }
    }
    enum Ownership: Equatable { case notCreated, created(Descriptor) }
    enum Outcome: Equatable {
      case pending
      case opened(Descriptor)
      case failed(FailureCause, Ownership)
      case indeterminate(FailureCause)
    }
    enum Admission: Equatable { case reserved, pending, terminal(Outcome) }
    enum Error: Swift.Error, Equatable { case version, owner, slot, identity, stale, generation, conflict, notTerminal }
    private enum State { case retired, reserved(Identity), executing(Identity), terminal(Identity, Outcome) }
    private struct Slot { var generation: UInt64 = 0; var state: State = .retired }
    static let capacity = 64
    let owner = UUID()
    private let lock = NSLock()
    private var slots = Array(repeating: Slot(), count: capacity)

    static func nextGeneration(after generation: UInt64) -> UInt64? {
      let (next, overflow) = generation.addingReportingOverflow(1)
      return overflow ? nil : next
    }

    private func index(_ identity: Identity) throws -> Int {
      guard identity.version == 2 else { throw Error.version }
      guard identity.owner == owner else { throw Error.owner }
      guard identity.slot < UInt32(Self.capacity) else { throw Error.slot }
      guard identity.generation > 0 else { throw Error.identity }
      return Int(identity.slot)
    }

    private func validateRequest(_ identity: Identity) throws {
      guard !identity.spec.members.isEmpty,
            identity.spec.members.allSatisfy({ $0.route.peer == identity.peer }) else { throw Error.identity }
    }

    private func checkNext(_ identity: Identity, slot: Slot) throws {
      if identity.generation <= slot.generation { throw Error.stale }
      guard Self.nextGeneration(after: slot.generation) == identity.generation else { throw Error.generation }
    }

    func reserve(_ identity: Identity) throws -> Admission {
      let i = try index(identity)
      try validateRequest(identity)
      lock.lock(); defer { lock.unlock() }
      switch slots[i].state {
      case .retired:
        try checkNext(identity, slot: slots[i])
        slots[i] = Slot(generation: identity.generation, state: .reserved(identity))
        return .reserved
      case let .reserved(bound):
        guard bound == identity else { throw Error.conflict }
        return .reserved
      case let .executing(bound):
        guard bound == identity else { throw Error.conflict }
        return .pending
      case let .terminal(bound, outcome):
        guard bound == identity else { throw Error.conflict }
        return .terminal(outcome)
      }
    }

    /// This check must run immediately before the effect, on the executing queue.
    func begin(_ identity: Identity) -> Bool {
      guard let i = try? index(identity), (try? validateRequest(identity)) != nil else { return false }
      lock.lock(); defer { lock.unlock() }
      guard case let .reserved(bound) = slots[i].state, bound == identity else { return false }
      slots[i].state = .executing(identity)
      return true
    }

    /// Recovery may overtake the original submission and bind its next generation.
    func fence(_ identity: Identity, failure: FailureCause) throws -> Outcome {
      let i = try index(identity)
      try validateRequest(identity)
      lock.lock(); defer { lock.unlock() }
      switch slots[i].state {
      case .retired:
        try checkNext(identity, slot: slots[i])
        let outcome = Outcome.failed(failure, .notCreated)
        slots[i] = Slot(generation: identity.generation, state: .terminal(identity, outcome))
        return outcome
      case let .reserved(bound):
        guard bound == identity else { throw Error.conflict }
        let outcome = Outcome.failed(failure, .notCreated)
        slots[i].state = .terminal(identity, outcome)
        return outcome
      case let .executing(bound):
        guard bound == identity else { throw Error.conflict }
        return .pending
      case let .terminal(bound, outcome):
        guard bound == identity else { throw Error.conflict }
        return outcome
      }
    }

    /// Cancel only a reserved request; executing work keeps its eventual result.
    @discardableResult func cancel(requestID: UInt64, failure: FailureCause) -> Bool {
      lock.lock(); defer { lock.unlock() }
      for i in slots.indices {
        switch slots[i].state {
        case let .reserved(identity) where identity.requestID == requestID:
          slots[i].state = .terminal(identity, .failed(failure, .notCreated))
          return true
        case let .executing(identity) where identity.requestID == requestID: return true
        case let .terminal(identity, _) where identity.requestID == requestID: return true
        default: break
        }
      }
      return false
    }

    /// A late callback can only complete its own generation. An indeterminate
    /// result may be resolved later by new evidence for that same generation.
    /// False does not transfer ownership: the caller must retain and resolve any
    /// installed source and report the rejected or stale completion.
    @discardableResult func complete(_ identity: Identity, outcome: Outcome) -> Bool {
      guard outcome != .pending, let i = try? index(identity),
            (try? validateRequest(identity)) != nil else { return false }
      let descriptor: Descriptor?
      switch outcome {
      case let .opened(value), let .failed(_, .created(value)): descriptor = value
      default: descriptor = nil
      }
      if let descriptor {
        guard descriptor.groupID == identity.groupID, !descriptor.opaqueSourceGeneration.isEmpty,
              Set(descriptor.members.map(\.id)).count == descriptor.members.count,
              descriptor.members.allSatisfy({ member in
                identity.spec.members.contains { $0.id == member.id }
              }) else { return false }
        if case .opened = outcome, descriptor.members.count != identity.spec.members.count { return false }
      }
      lock.lock(); defer { lock.unlock() }
      switch slots[i].state {
      case let .executing(bound):
        guard bound == identity else { return false }
      case let .terminal(bound, .indeterminate):
        guard bound == identity else { return false }
      default: return false
      }
      slots[i].state = .terminal(identity, outcome)
      return true
    }

    /// A retired older generation is acknowledged by slot identity alone.
    @discardableResult func retire(_ identity: Identity) throws -> SlotIdentity {
      let i = try index(identity)
      lock.lock(); defer { lock.unlock() }
      let retired = SlotIdentity(owner: owner, slot: identity.slot, generation: identity.generation)
      if identity.generation < slots[i].generation { return retired }
      if identity.generation > slots[i].generation { throw Error.generation }
      switch slots[i].state {
      case .retired: return retired
      case let .terminal(bound, outcome):
        guard bound == identity else { throw Error.conflict }
        if case .indeterminate = outcome { throw Error.notTerminal }
        guard outcome != .pending else { throw Error.notTerminal }
        slots[i].state = .retired
        return retired
      case let .reserved(bound), let .executing(bound):
        guard bound == identity else { throw Error.conflict }
        throw Error.notTerminal
      }
    }
  }
  /// Thread-safe submission receipts live on this host instance, independently
  /// of the radio queue and of installed groups. The fingerprint is the exact
  /// equatable platform request, not a digest or a group-map lookup.
  struct OpenRequest: Equatable {
    let requestID: UInt64
    let groupID: UInt64
    let peer: String
    let fingerprint: AnyHashable
  }
  struct OpenToken: Equatable {
    fileprivate let host: UUID
    fileprivate let nonce: UUID
    fileprivate let request: OpenRequest
  }
  enum OpenOutcome: Equatable {
    case pending, notCreated, created(Descriptor), completed(AnyHashable), unresolved(AnyHashable), indeterminate
  }
  private enum OpenState { case reserved, executing, terminal(OpenOutcome) }
  private struct OpenReceipt {
    let request: OpenRequest
    let token: OpenToken
    var state: OpenState
    let capacityRefusal: Bool
  }
  private let openLock = NSLock()
  private let openHost = UUID()
  let admissionSlots = AdmissionSlots()
  // This state is shared by every adapter referring to this owner. It is
  // intentionally never promoted to quiescent until the radio, callback and
  // borrower joins have a proof-bearing implementation.
  final class OwnerRetirement {
    enum Reply { case pending, wrongOwner, conflicting, malformed }
    private let lock = NSLock()
    private var retirement: String?
    private enum AttachmentState { case queued, executing, relaying }
    private var attachments: [UUID: AttachmentState] = [:]
    private var borrowers: [ObjectIdentifier: AnyObject] = [:]

    private func valid(_ value: String) -> Bool {
      guard let id = UUID(uuidString: value), id.uuidString.lowercased() == value else { return false }
      let characters = Array(value)
      return characters[14] == "4" && ["8", "9", "a", "b"].contains(characters[19])
    }

    func control(owner: String, expected: String, retirement value: String, fence: Bool) -> Reply {
      lock.lock(); defer { lock.unlock() }
      guard valid(owner), valid(value) else { return .malformed }
      guard owner == expected else { return .wrongOwner }
      if let existing = retirement { return existing == value ? .pending : .conflicting }
      guard fence else { return .malformed }
      retirement = value
      return .pending
    }

    func admitsWork() -> Bool {
      lock.lock(); defer { lock.unlock() }
      return retirement == nil
    }

    /// Synchronous local reservations must finish publication before a
    /// concurrent fence can report that their identity was not created.
    func admitLocalReservation(_ reserve: () -> Void) -> Bool {
      lock.lock(); defer { lock.unlock() }
      guard retirement == nil else { return false }
      reserve()
      return true
    }

    /// A cached radio borrower is a separate claim from a callback route.
    /// Retain its exact owner until physical release succeeds; a failed
    /// cleanup and a replacement attempt must not erase the claim.
    func admitBorrower(_ borrower: AnyObject, attach: () -> Bool) -> Bool {
      lock.lock(); defer { lock.unlock() }
      guard retirement == nil, attach() else { return false }
      borrowers[ObjectIdentifier(borrower)] = borrower
      return true
    }

    func releaseBorrower(_ borrower: AnyObject) {
      lock.lock(); defer { lock.unlock() }
      borrowers.removeValue(forKey: ObjectIdentifier(borrower))
    }

    /// The ticket exists before the closure is published to the radio queue.
    /// The closure only updates local state and enqueues work under this lock.
    func admitAttachment(_ attach: (UUID) -> (accepted: Bool, queued: Bool)) -> Bool {
      lock.lock(); defer { lock.unlock() }
      guard retirement == nil else { return false }
      let ticket = UUID()
      attachments[ticket] = .queued
      let result = attach(ticket)
      if !result.queued { attachments.removeValue(forKey: ticket) }
      return result.accepted
    }

    /// A queued attachment may start only if it wins against the fence.
    /// Executing attachments remain owned until their asynchronous callback
    /// and its relay have returned.
    func beginAttachment(_ ticket: UUID) -> Bool {
      lock.lock(); defer { lock.unlock() }
      guard case .queued? = attachments[ticket] else { return false }
      guard retirement == nil else { attachments.removeValue(forKey: ticket); return false }
      attachments[ticket] = .executing
      return true
    }

    /// The first callback settles the executing attachment. Later duplicate
    /// callbacks have no claim on the old route, including during replacement.
    func beginAttachmentCallback(_ ticket: UUID) -> Bool {
      lock.lock(); defer { lock.unlock() }
      guard case .executing? = attachments[ticket] else { return false }
      attachments[ticket] = .relaying
      return true
    }

    func finishAttachment(_ ticket: UUID) {
      lock.lock(); defer { lock.unlock() }
      attachments.removeValue(forKey: ticket)
    }

  }
  let ownerRetirement = OwnerRetirement()
  func admissionOwner(version: UInt32) -> String {
    switch version {
    case 3: return admissionSlots.owner.uuidString.lowercased()
    default: return ""
    }
  }
  private var openReceipts: [UInt64: OpenReceipt] = [:]
  private let maxOpenReceipts = 64
  // Every live receipt reserves a retirement identity. Never evict one: an
  // arbitrarily delayed duplicate must not become a new open.
  private let maxOpenIdentities = 128
  private var retiredRequests: [UInt64: OpenRequest] = [:]

  func reserveOpen(_ request: OpenRequest, refusal: OpenOutcome = .notCreated) throws -> OpenToken {
    openLock.lock(); defer { openLock.unlock() }
    guard retiredRequests[request.requestID] == nil, openReceipts[request.groupID] == nil,
          !openReceipts.values.contains(where: { $0.request.requestID == request.requestID }) else {
      throw Failure.openAdmissionConflict
    }
    guard openReceipts.count + retiredRequests.count < maxOpenIdentities else { throw Failure.openAdmissionUntracked }
    let token = OpenToken(host: openHost, nonce: UUID(), request: request)
    let admittedCount = openReceipts.values.filter { !$0.capacityRefusal }.count
    if admittedCount >= maxOpenReceipts {
      openReceipts[request.groupID] = OpenReceipt(request: request, token: token, state: .terminal(refusal), capacityRefusal: true)
      throw Failure.openAdmissionUnavailable
    }
    openReceipts[request.groupID] = OpenReceipt(request: request, token: token, state: .reserved, capacityRefusal: false)
    return token
  }

  func beginOpen(_ request: OpenRequest, token: OpenToken) -> Bool {
    openLock.lock(); defer { openLock.unlock() }
    guard var receipt = openReceipts[request.groupID], receipt.request == request,
          receipt.token == token, case .reserved = receipt.state else { return false }
    receipt.state = .executing
    openReceipts[request.groupID] = receipt
    return true
  }

  @discardableResult
  func cancelOpen(_ request: OpenRequest, token: OpenToken, outcome: OpenOutcome = .notCreated) -> OpenOutcome? {
    openLock.lock(); defer { openLock.unlock() }
    guard var receipt = openReceipts[request.groupID], receipt.request == request,
          receipt.token == token else { return nil }
    switch receipt.state {
    case .reserved:
      receipt.state = .terminal(outcome)
      openReceipts[request.groupID] = receipt
      return outcome
    case .executing: return .pending
    case let .terminal(existing): return existing
    }
  }

  func cancelOpen(requestID: UInt64) {
    openLock.lock(); defer { openLock.unlock() }
    guard let (groupID, existing) = openReceipts.first(where: { $0.value.request.requestID == requestID }),
          case .reserved = existing.state else { return }
    var receipt = existing
    receipt.state = .terminal(.notCreated)
    openReceipts[groupID] = receipt
  }

  func completeOpen(_ request: OpenRequest, token: OpenToken, outcome: OpenOutcome) {
    openLock.lock(); defer { openLock.unlock() }
    guard var receipt = openReceipts[request.groupID], receipt.request == request,
          receipt.token == token, case .executing = receipt.state else { return }
    receipt.state = .terminal(outcome)
    openReceipts[request.groupID] = receipt
  }

  /// The host must retain this exact identity across a lost acknowledgement.
  /// It is scoped to this bridge instance and cannot identify another host.
  func openReceiptIdentity(groupID: UInt64) -> (request: OpenRequest, token: OpenToken)? {
    openLock.lock(); defer { openLock.unlock() }
    guard let receipt = openReceipts[groupID] else { return nil }
    return (receipt.request, receipt.token)
  }

  /// Non-destructive observation and exact-identity fence for a future shared
  /// protocol. In particular, status must not cancel an intended open.
  func openStatus(_ request: OpenRequest, token: OpenToken) throws -> OpenOutcome {
    openLock.lock(); defer { openLock.unlock() }
    guard let receipt = openReceipts[request.groupID], receipt.request == request,
          receipt.token == token else { throw Failure.unknownOpenReceipt }
    switch receipt.state {
    case .reserved, .executing: return .pending
    case let .terminal(outcome): return outcome
    }
  }

  func fenceOpen(_ request: OpenRequest, token: OpenToken) throws -> OpenOutcome {
    openLock.lock(); defer { openLock.unlock() }
    guard let receipt = openReceipts[request.groupID], receipt.request == request,
          receipt.token == token else { throw Failure.unknownOpenReceipt }
    return try fenceOpenLocked(groupID: request.groupID)
  }

  func fenceOpen(groupID: UInt64, requestID: UInt64, peer: String) throws -> OpenOutcome {
    openLock.lock(); defer { openLock.unlock() }
    guard let receipt = openReceipts[groupID], receipt.request.requestID == requestID,
          receipt.request.peer == peer else { throw Failure.unknownOpenReceipt }
    return try fenceOpenLocked(groupID: groupID)
  }

  /// Fencing is destructive to a reserved open; a status query must not call it.
  func fenceOpen(groupID: UInt64) throws -> OpenOutcome {
    openLock.lock(); defer { openLock.unlock() }
    return try fenceOpenLocked(groupID: groupID)
  }

  private func fenceOpenLocked(groupID: UInt64) throws -> OpenOutcome {
    guard var receipt = openReceipts[groupID] else { throw Failure.unknownOpenReceipt }
    switch receipt.state {
    case .reserved:
      receipt.state = .terminal(.notCreated)
      openReceipts[groupID] = receipt
      return .notCreated
    case .executing: return .pending
    case let .terminal(outcome): return outcome
    }
  }

  /// Token and host identity make an acknowledgement retry harmless, including
  /// after the slot has been reused for a different request.
  func acknowledgeOpen(_ request: OpenRequest, token: OpenToken) throws {
    openLock.lock(); defer { openLock.unlock() }
    guard token.host == openHost, token.request == request else { throw Failure.unknownOpenReceipt }
    if let retired = retiredRequests[request.requestID] {
      guard retired == request else { throw Failure.unknownOpenReceipt }
      return
    }
    guard let receipt = openReceipts[request.groupID], receipt.token == token else { throw Failure.unknownOpenReceipt }
    try acknowledgeOpenLocked(receipt)
  }

  func acknowledgeOpen(version: UInt32, groupID: UInt64, requestID: UInt64, peer: String) throws {
    openLock.lock(); defer { openLock.unlock() }
    guard version == 1 else { throw Failure.unknownOpenReceipt }
    if let retired = retiredRequests[requestID] {
      guard retired.groupID == groupID, retired.peer == peer else { throw Failure.unknownOpenReceipt }
      return
    }
    guard let receipt = openReceipts[groupID], receipt.request.requestID == requestID,
          receipt.request.peer == peer else { throw Failure.unknownOpenReceipt }
    try acknowledgeOpenLocked(receipt)
  }

  private func acknowledgeOpenLocked(_ receipt: OpenReceipt) throws {
    guard case let .terminal(outcome) = receipt.state else { throw Failure.cleanupPending }
    guard outcome != .pending && outcome != .indeterminate else { throw Failure.cleanupPending }
    if case .unresolved = outcome { throw Failure.cleanupPending }
    retiredRequests[receipt.request.requestID] = receipt.request
    openReceipts.removeValue(forKey: receipt.request.groupID)
  }
  struct Member: Equatable {
    let id: UInt64
    let route: OrderedNotificationRoute
  }

  struct RegisteredMember: Equatable {
    let id: UInt64
    let route: OrderedNotificationRoute
    let sourceMember: UInt64
    let epoch: UInt64
  }

  struct Descriptor: Equatable {
    let groupID: UInt64
    /// CoreBluetooth's source generation is an opaque UUID, never a counter.
    let opaqueSourceGeneration: String
    let members: [RegisteredMember]
  }

  struct PhysicalClaim: Hashable {
    let groupID: UInt64
    let memberID: UInt64
    let sourceClaim: OrderedNotificationClaim
    let epoch: UInt64
  }

  struct MarkerInput: Equatable {
    let connection: String
    let snapshot: OwnedGattSnapshotIdentity
    let route: OrderedNotificationRoute
    let value: Data
    let withResponse: Bool
  }
  struct MarkerFailure: Equatable {
    let kind: String
    let gattStatus: Int32?
    let domain: String?
    let code: Int64?
    let detail: String
    let dispatched: Bool
  }
  enum MarkerAdmission: Equatable { case pending, notAdmitted, admitted(UInt64), rejected(MarkerFailure) }
  enum MarkerWrite: Equatable { case initiating, notStarted(MarkerFailure?), started(MarkerFailure?), settled(MarkerFailure?) }
  struct MarkerStatus: Equatable { var admission: MarkerAdmission; var write: MarkerWrite }
  enum MarkerRetryState: Equatable { case waiting, ready, settled }
  private struct MarkerReceipt {
    let id: UInt64
    let input: MarkerInput
    var status: MarkerStatus
    var acknowledged: Bool
  }

  enum Rejected: Equatable {
    case value(memberID: UInt64, sourceMember: UInt64, epoch: UInt64, bytes: UInt64)
    case sourceLoss(memberID: UInt64?, skippedRecords: UInt64?)
    case fence(memberID: UInt64, sourceMember: UInt64, epoch: UInt64)
    case barrier(id: UInt64)
    case marker(id: UInt64)
    case unmapped
  }

  enum End: Equatable {
    case closed, connectionLost, serviceChanged, sourceReplaced, shutdown, adapterLost, overflow, ordinalExhausted, wakeFailed
    case sourceFailed(OwnedOrderedNotificationIngress.NativeFailure, memberID: UInt64, sourceMember: UInt64, epoch: UInt64)
    case sourceFailureUnmapped(OwnedOrderedNotificationIngress.NativeFailure, sourceMember: UInt64, epoch: UInt64)
  }

  enum Record: Equatable {
    case value(memberID: UInt64, sourceMember: UInt64, epoch: UInt64, ordinal: UInt64, value: Data, provenance: OwnedOrderedNotificationIngress.Provenance)
    case fence(memberID: UInt64, sourceMember: UInt64, epoch: UInt64, platformTail: OwnedOrderedNotificationIngress.PlatformTail)
    case barrier(id: UInt64, platformTail: OwnedOrderedNotificationIngress.PlatformTail)
    case marker(id: UInt64, ordinal: UInt64)
    case loss(memberID: UInt64?, sourceRecords: UInt64?)
    case terminal(End, platformTail: OwnedOrderedNotificationIngress.PlatformTail, rejected: Rejected?)
    case subsequentSourceFailure(OwnedOrderedNotificationIngress.NativeFailure)
    case failureObservationsOmitted
    /// An internal route mismatch is explicit, never a silently omitted record.
    case bridgeFailure
  }

  /// A bounded native batch stays owned until Rust acknowledges every record.
  struct Batch: Equatable {
    let id: UInt64
    let records: [Record]
  }

  struct CaptureBudget {
    let maxGroups: Int
    let maxSlots: Int
    let maxBytes: Int
    let slotsPerGroup: Int
    let bytesPerGroup: Int
    static let `default` = CaptureBudget(maxGroups: 64, maxSlots: 256, maxBytes: 4 * 1024 * 1024,
                                         slotsPerGroup: 4, bytesPerGroup: 64 * 1024)
  }

  struct OpenFailureCause: Equatable {
    let domain: String
    let code: Int64
    let detail: String
  }

  enum Failure: Error, Equatable {
    case openAdmissionUnavailable, openAdmissionUntracked, openAdmissionConflict, unknownOpenReceipt
    case duplicateGroup, invalidMembers, unknownGroup, unknownMember, inactiveMember, captureCapacity, capturePending
    case sourceClaimExhausted, alreadyRetained, unknownPhysicalClaim, cleanupPending, staleGeneration
    case unknownBatch, batchIDExhausted, markerBusy, markerStale, markerMismatch, markerNotSettled
    case openIndeterminate(generation: String, groupID: UInt64, members: [RegisteredMember], cause: OpenFailureCause)
    case factoryIndeterminate(OpenFailureCause)
    case source(OwnedOrderedNotificationIngress.Failure)
  }

  typealias Factory = ([OrderedNotificationRoute], Int, Int, @escaping () -> Void) throws ->
    (OwnedOrderedNotificationSource, OwnedOrderedNotificationIngress)

  /// The factory may assert this only when no group was installed and any
  /// newly published source was rolled back. All other throws are uncertain.
  struct FactoryNoEffect: Error {
    let underlying: Error
  }

  private struct RouteEpoch: Hashable {
    let route: OrderedNotificationRoute
    let epoch: UInt64
  }
  private struct Binding {
    let memberID: UInt64
    let claim: OrderedNotificationClaim
    let epoch: UInt64
  }
  /// A retirement requested after a callback has already reserved its
  /// observation position.  The request is accepted exactly once, but its
  /// source mutation waits until that earlier observation has been classified.
  /// Keeping the original binding here is essential: a later registration for
  /// the same route must never be retired in its predecessor's place.
  private struct PendingRetirement {
    let binding: Binding
  }
  private final class Group {
    var descriptor: Descriptor
    let identity: UUID
    let requestedMembers: [Member]
    var snapshot: OwnedGattSnapshotIdentity?
    let source: OwnedOrderedNotificationSource
    let ingress: OwnedOrderedNotificationIngress
    let captureReservation: AppleOrderedGroupCaptureOwner.Reservation
    let wake: (UInt64, String) -> Void
    let memberByRoute: [OrderedNotificationRoute: UInt64]
    var current: [UInt64: Binding] = [:]
    var byEpoch: [RouteEpoch: Binding] = [:]
    var lastRetired: [UInt64: RegisteredMember] = [:]
    var lastRebind: [UInt64: (old: RegisteredMember, new: RegisteredMember)] = [:]
    var pending: [PhysicalClaim: OrderedNotificationClaim] = [:]
    /// Host serializes confirmations per member and resolves an uncertain
    /// response before confirming a newer activation for that member.
    var lastConfirmed: [UInt64: PhysicalClaim] = [:]
    var fenced: Set<RouteEpoch> = []
    var markerReceipt: MarkerReceipt?
    var markerWaitingForCapture = false
    var lastRetiredMarker: UInt64?
    var outstandingBarrier: UInt64?
    var lastAcknowledgedBarrier: UInt64?
    var terminalDrained = false
    var sealed = false
    var rollbackEvents: [OwnedOrderedNotificationIngress.Event] = []
    var pendingBatch: Batch?
    var pendingCaptureBatchID: UInt64?
    var captureWakePending = false
    var captureWakeScheduled = false
    /// Set when the first unresolved capture was observed after an already
    /// admitted ingress prefix. That prefix is still entitled to drain before
    /// the capture; observations admitted after the capture are not.
    var captureHasEarlierIngress = false
    var captureIngressBarriers: [AppleOrderedGroupCaptureOwner.Ticket: UUID] = [:]
    /// One ingress barrier per coalesced omission run, never one per callback.
    var omissionIngressBarriers: [UUID: [UUID]] = [:]
    var pendingFailureCredits = 0
    /// Bounded by immutable group membership: one logical retirement per
    /// active member.  It is not a second unbounded callback queue.
    var pendingRetirements: [UInt64: PendingRetirement] = [:]
    var lastAcknowledgedBatchID: UInt64?
    var versionedBatchID: UInt64?
    var legacyBatchID: UInt64?
    var lastLegacyAcknowledgement: UInt64?
    var lastVersionedAcknowledgement: UInt64?
    var nextBatchID: UInt64 = 0
    var batchIDAvailable = true

    init(groupID: UInt64, identity: UUID, members: [Member], source: OwnedOrderedNotificationSource, ingress: OwnedOrderedNotificationIngress,
         captureReservation: AppleOrderedGroupCaptureOwner.Reservation, wake: @escaping (UInt64, String) -> Void) {
      self.identity = identity
      self.descriptor = Descriptor(groupID: groupID, opaqueSourceGeneration: source.generation, members: [])
      self.requestedMembers = members
      self.source = source
      self.ingress = ingress
      self.captureReservation = captureReservation
      self.wake = wake
      self.memberByRoute = Dictionary(uniqueKeysWithValues: members.map { ($0.route, $0.id) })
    }
  }

  private let queue: DispatchQueue
  private let factory: Factory
  private let captureOwner: AppleOrderedGroupCaptureOwner
  private let captureBudget: CaptureBudget
  private var uncertainCaptureReservations: [UInt64: AppleOrderedGroupCaptureOwner.Reservation] = [:]
  private let normalize: (OrderedNotificationRoute) -> OrderedNotificationRoute
  private var validateSnapshot: (String, OwnedGattSnapshotIdentity) -> Bool
  private let removeSourceGroup: (OwnedOrderedNotificationSource, OwnedOrderedNotificationIngress) -> Bool
  private var wake: (UInt64, String) -> Void
  private var groups: [UInt64: Group] = [:]
  private var removed: [UInt64: String] = [:]
  private let maxGroups = 64

  init(queue: DispatchQueue, factory: @escaping Factory,
       normalize: @escaping (OrderedNotificationRoute) -> OrderedNotificationRoute = { $0 },
       validateSnapshot: @escaping (String, OwnedGattSnapshotIdentity) -> Bool = { _, _ in false },
       removeSourceGroup: @escaping (OwnedOrderedNotificationSource, OwnedOrderedNotificationIngress) -> Bool = { $0.remove($1) },
       captureBudget: CaptureBudget = .default,
       wake: @escaping (UInt64, String) -> Void) {
    self.queue = queue
    self.factory = factory
    precondition(captureBudget.maxGroups > 0 && captureBudget.maxSlots > 0 && captureBudget.maxBytes > 0 &&
                 captureBudget.slotsPerGroup > 0 && captureBudget.bytesPerGroup > 0)
    self.captureBudget = captureBudget
    self.captureOwner = try! AppleOrderedGroupCaptureOwner(queue: queue, maxGroups: captureBudget.maxGroups,
      maxSlots: captureBudget.maxSlots, maxBytes: captureBudget.maxBytes)
    self.normalize = normalize
    self.validateSnapshot = validateSnapshot
    self.removeSourceGroup = removeSourceGroup
    self.wake = wake
  }

  /// Set during adapter initialization before the bridge is exposed to callers.
  func setSnapshotValidator(_ validator: @escaping (String, OwnedGattSnapshotIdentity) -> Bool) {
    validateSnapshot = validator
  }

  /// Bind the host's reliable group wake after the sink is installed.
  func setWake(_ wake: @escaping (UInt64, String) -> Void) {
    dispatchPrecondition(condition: .onQueue(queue))
    self.wake = wake
  }

  /// Installs every logical route atomically on the callback queue before the
  /// caller can dispatch a physical enable through the existing radio owner.
  func open(groupID: UInt64, members: [Member], capacity: Int, byteCapacity: Int,
            wake: ((UInt64, String) -> Void)? = nil) throws -> Descriptor {
    try install(groupID: groupID, members: members, capacity: capacity, byteCapacity: byteCapacity, snapshot: nil, wake: wake)
  }

  private func install(groupID: UInt64, members: [Member], capacity: Int, byteCapacity: Int,
                       snapshot: OwnedGattSnapshotIdentity?, wake: ((UInt64, String) -> Void)?) throws -> Descriptor {
    dispatchPrecondition(condition: .onQueue(queue))
    guard groups[groupID] == nil, removed[groupID] == nil, uncertainCaptureReservations[groupID] == nil else { throw Failure.duplicateGroup }
    let members = members.map { Member(id: $0.id, route: normalize($0.route)) }
    guard groups.count + removed.count < maxGroups, !members.isEmpty,
          Set(members.map(\.id)).count == members.count,
          Set(members.map(\.route)).count == members.count,
          Set(members.map(\.route.peer)).count == 1 else { throw Failure.invalidMembers }
    let routes = members.map(\.route)
    let identity = UUID()
    let groupWake = wake ?? self.wake
    let reservation: AppleOrderedGroupCaptureOwner.Reservation
    do { reservation = try captureOwner.reserve(groupID: groupID, slots: captureBudget.slotsPerGroup, bytes: captureBudget.bytesPerGroup) }
    catch AppleOrderedGroupCaptureOwner.Failure.capacity { throw Failure.captureCapacity }
    let (source, ingress): (OwnedOrderedNotificationSource, OwnedOrderedNotificationIngress)
    do {
      (source, ingress) = try factory(routes, capacity, byteCapacity) { [weak self, queue] in
        queue.async { [weak self] in
          guard let self, let current = self.groups[groupID], current.identity == identity, !current.sealed else { return }
          groupWake(groupID, current.descriptor.opaqueSourceGeneration)
        }
      }
    } catch let error as FactoryNoEffect {
      try captureOwner.retire(reservation)
      try captureOwner.release(reservation)
      throw error
    } catch {
      // A throwing factory has not handed this bridge a source identity. It
      // may nevertheless have installed a source before throwing.
      let native = error as NSError
      uncertainCaptureReservations[groupID] = reservation
      throw Failure.factoryIndeterminate(OpenFailureCause(domain: native.domain, code: Int64(native.code), detail: native.localizedDescription))
    }
    let boundReservation: AppleOrderedGroupCaptureOwner.Reservation
    do { boundReservation = try captureOwner.bind(reservation, sourceGeneration: source.generation) }
    catch {
      // The factory returned the exact installed pair. Keep it reachable for
      // normal terminal drain and cleanup instead of treating a known source
      // as an anonymous factory effect. No member has been registered yet.
      let group = Group(groupID: groupID, identity: identity, members: members, source: source, ingress: ingress,
                        captureReservation: reservation, wake: groupWake)
      group.snapshot = snapshot
      ingress.close()
      groups[groupID] = group
      let native = error as NSError
      // The mobile contract rejects an empty source generation, so it cannot
      // serve as an external cleanup identity even though this pair is held.
      if source.generation.isEmpty {
        throw Failure.factoryIndeterminate(OpenFailureCause(domain: native.domain, code: Int64(native.code), detail: String(describing: error)))
      }
      throw Failure.openIndeterminate(generation: source.generation, groupID: groupID, members: [],
        cause: OpenFailureCause(domain: native.domain, code: Int64(native.code), detail: String(describing: error)))
    }
    let group = Group(groupID: groupID, identity: identity, members: members, source: source, ingress: ingress,
                      captureReservation: boundReservation, wake: groupWake)
    group.snapshot = snapshot
    var registered: [RegisteredMember] = []
    do {
      for member in members { registered.append(try register(member, in: group)) }
    } catch {
      ingress.close()
      try ingress.seal()
      let rollbackEvents = ingress.drain()
      guard removeSourceGroup(source, ingress) else {
        group.descriptor = Descriptor(groupID: groupID, opaqueSourceGeneration: source.generation, members: registered)
        group.rollbackEvents = rollbackEvents
        groups[groupID] = group
        let native = error as NSError
        let cause = OpenFailureCause(domain: native.domain, code: Int64(native.code), detail: String(describing: error))
        throw Failure.openIndeterminate(generation: source.generation, groupID: groupID, members: registered, cause: cause)
      }
      try captureOwner.retire(boundReservation)
      try captureOwner.release(boundReservation)
      throw error
    }
    group.descriptor = Descriptor(groupID: groupID, opaqueSourceGeneration: source.generation, members: registered)
    groups[groupID] = group
    return group.descriptor
  }

  func open(groupID: UInt64, members: [Member], capacity: Int, byteCapacity: Int, snapshot: OwnedGattSnapshotIdentity,
            wake: ((UInt64, String) -> Void)? = nil) throws -> Descriptor {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let peer = members.first?.route.peer, validateSnapshot(peer, snapshot) else { throw Failure.staleGeneration }
    return try install(groupID: groupID, members: members, capacity: capacity, byteCapacity: byteCapacity, snapshot: snapshot, wake: wake)
  }

  /// Every post-open foreign request must name the generation returned by open.
  func validateSource(groupID: UInt64, opaqueSourceGeneration: String) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    guard try lookup(groupID).descriptor.opaqueSourceGeneration == opaqueSourceGeneration else {
      throw Failure.staleGeneration
    }
  }

  func recoverOpen(groupID: UInt64) throws -> Descriptor {
    dispatchPrecondition(condition: .onQueue(queue))
    return try lookup(groupID).descriptor
  }

  /// Re-register after an admitted retirement fence without reusing its epoch.
  @discardableResult
  func register(groupID: UInt64, memberID: UInt64) throws -> RegisteredMember {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard !hasUnresolvedCapturePosition(group) else { throw Failure.capturePending }
    guard let member = group.requestedMembers.first(where: { $0.id == memberID }) else { throw Failure.unknownMember }
    guard group.current[memberID] == nil else { throw Failure.source(.alreadyRegistered) }
    return try register(member, in: group)
  }

  func register(groupID: UInt64, expected: RegisteredMember) throws -> RegisteredMember {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard !hasUnresolvedCapturePosition(group) else { throw Failure.capturePending }
    if let current = group.current[expected.id] {
      guard let receipt = group.lastRebind[expected.id], receipt.old == expected,
            receipt.new.sourceMember == current.claim.token, receipt.new.epoch == current.epoch else { throw Failure.inactiveMember }
      return receipt.new
    }
    guard group.lastRetired[expected.id] == expected else { throw Failure.inactiveMember }
    guard let member = group.requestedMembers.first(where: { $0.id == expected.id && $0.route == expected.route }) else { throw Failure.unknownMember }
    let replacement = try register(member, in: group)
    group.lastRebind[expected.id] = (expected, replacement)
    return replacement
  }

  func expectedMember(groupID: UInt64, memberID: UInt64, sourceMember: UInt64, epoch: UInt64) throws -> RegisteredMember {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard let member = group.requestedMembers.first(where: { $0.id == memberID }) else { throw Failure.unknownMember }
    return RegisteredMember(id: memberID, route: member.route, sourceMember: sourceMember, epoch: epoch)
  }

  func validateCurrentMember(groupID: UInt64, expected: RegisteredMember) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard let binding = group.current[expected.id], binding.claim.route == expected.route,
          binding.claim.token == expected.sourceMember, binding.epoch == expected.epoch else { throw Failure.inactiveMember }
  }

  /// Explicit capture boundary for a future callback/gate integration. It
  /// captures the current binding now; replay never looks up a replacement.
  func captureFailure(groupID: UInt64, expected: RegisteredMember,
                      failure: OwnedOrderedNotificationIngress.NativeFailure) throws -> AppleOrderedGroupCaptureOwner.Capture {
    try validateCurrentMember(groupID: groupID, expected: expected)
    let group = try lookup(groupID)
    guard let binding = group.current[expected.id] else { throw Failure.inactiveMember }
    if !group.captureHasEarlierIngress { group.captureHasEarlierIngress = group.ingress.hasQueuedObservations }
    do {
      let capture = try captureOwner.capture(group.captureReservation, claim: binding.claim, epoch: binding.epoch, failure: failure)
      switch capture {
      case let .captured(ticket):
        group.captureIngressBarriers[ticket] = group.ingress.beginExternalCaptureBarrier()
      case let .omitted(ticket):
        // The first omitted observation fences its complete coalesced run.
        // Later omissions are represented by the same bounded barrier.
        if let barriers = group.omissionIngressBarriers[ticket.run], let last = barriers.last {
          // A single run remains coalesced until a real ingress observation
          // separates two omitted callbacks.  The number of barriers is thus
          // bounded by ingress capacity, not callback volume.
          if group.ingress.hasObservationAfterExternalCaptureBarrier(last) {
            group.omissionIngressBarriers[ticket.run] = barriers + [group.ingress.beginExternalCaptureBarrier()]
          }
        } else {
          group.omissionIngressBarriers[ticket.run] = [group.ingress.beginExternalCaptureBarrier()]
        }
      }
      return capture
    } catch {
      throw error
    }
  }

  /// Production CoreBluetooth callback ingress.  Capture every group that
  /// owned the exact claim at observation; replay is deliberately returned as
  /// a one-shot closure so a graph permit cannot re-resolve a replacement.
  func capturePhysicalFailure(route: OrderedNotificationRoute,
                              failure: OwnedOrderedNotificationIngress.NativeFailure,
                              claim: OrderedNotificationClaim?) -> (() -> Void)? {
    dispatchPrecondition(condition: .onQueue(queue))
    let captures: [(UInt64, AppleOrderedGroupCaptureOwner.Capture)] = groups.compactMap { groupID, group -> (UInt64, AppleOrderedGroupCaptureOwner.Capture)? in
      guard claim == nil || claim?.generation == group.descriptor.opaqueSourceGeneration,
            let binding = group.current.values.first(where: { $0.claim.route == route && (claim == nil || $0.claim == claim) }) else { return nil }
      let expected = RegisteredMember(id: binding.memberID, route: binding.claim.route, sourceMember: binding.claim.token, epoch: binding.epoch)
      guard let capture = try? captureFailure(groupID: groupID, expected: expected, failure: failure) else { return nil }
      return (groupID, capture)
    }
    guard !captures.isEmpty else { return nil }
    var replayed = false
    return { [weak self] in
      guard !replayed else { return }
      replayed = true
      guard let self else { return }
      for (groupID, capture) in captures { try? self.replayCapturedFailure(groupID: groupID, capture: capture) }
    }
  }

  func replayCapturedFailure(groupID: UInt64, capture: AppleOrderedGroupCaptureOwner.Capture) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    switch capture {
    case let .captured(ticket):
      try captureOwner.replay(group.captureReservation, ticket: ticket)
    case let .omitted(ticket):
      try captureOwner.replayOmissions(group.captureReservation, through: ticket)
    }
    group.captureWakePending = true
    scheduleCaptureWake(groupID: groupID, group: group)
  }

  func cancelCapturedFailure(groupID: UInt64, ticket: AppleOrderedGroupCaptureOwner.Ticket) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    try captureOwner.cancel(group.captureReservation, ticket: ticket)
    if let barrier = group.captureIngressBarriers.removeValue(forKey: ticket) {
      // A replay/cancellation wake already queued for this group is the
      // authoritative retry signal; do not create a duplicate host control.
      group.ingress.endExternalCaptureBarrier(barrier, notify: !group.captureWakeScheduled)
    }
    try settlePendingRetirements(in: group)
    group.captureWakePending = true
    scheduleCaptureWake(groupID: groupID, group: group)
  }

  /// A capture owner reserves an observation position outside the ingress
  /// until replay or cancellation.  A source retirement observed after that
  /// position must remain logical-only until the position is resolved: doing
  /// it first makes the earlier failure look like late evidence instead of
  /// the source terminal it actually was.
  private func hasUnresolvedCapturePosition(_ group: Group) -> Bool {
    !group.captureIngressBarriers.isEmpty ||
      !group.omissionIngressBarriers.isEmpty
  }

  /// Apply every retirement whose earlier capture has reached an ingress
  /// position.  If that capture ended the source, the binding is retired
  /// without submitting another source fence; the terminal itself is the
  /// authoritative deactivation.  Otherwise submit the exact original route
  /// fence before releasing the bridge binding.
  private func settlePendingRetirements(in group: Group) throws {
    guard !hasUnresolvedCapturePosition(group) else { return }
    let pending = group.pendingRetirements.values.sorted {
      if $0.binding.epoch != $1.binding.epoch { return $0.binding.epoch < $1.binding.epoch }
      return $0.binding.memberID < $1.binding.memberID
    }
    for retirement in pending {
      let binding = retirement.binding
      guard let current = group.current[binding.memberID],
            current.claim == binding.claim, current.epoch == binding.epoch else {
        // A terminal or an earlier exact retirement already settled this
        // identity.  Never touch a replacement binding.
        group.pendingRetirements.removeValue(forKey: binding.memberID)
        continue
      }
      if group.ingress.ownsCurrentClaim(binding.claim) {
        do { try group.ingress.retire(binding.claim.route) }
        catch let error as OwnedOrderedNotificationIngress.Failure { throw Failure.source(error) }
      }
      group.current.removeValue(forKey: binding.memberID)
      group.lastRetired[binding.memberID] = RegisteredMember(id: binding.memberID, route: binding.claim.route,
        sourceMember: binding.claim.token, epoch: binding.epoch)
      group.pendingRetirements.removeValue(forKey: binding.memberID)
    }
  }

  private func scheduleCaptureWake(groupID: UInt64, group: Group) {
    guard !group.captureWakeScheduled else { return }
    group.captureWakeScheduled = true
    let identity = group.identity
    queue.async { [weak self] in
      guard let self, let current = self.groups[groupID], current.identity == identity else { return }
      current.captureWakeScheduled = false
      current.wake(groupID, current.descriptor.opaqueSourceGeneration)
    }
  }

  private func register(_ member: Member, in group: Group) throws -> RegisteredMember {
    // A retained native failure is already position-owned by the ingress.
    // Re-registration must not require a caller retry once the predecessor
    // has been logically retired: the old receipt carries its epoch and will
    // settle as late evidence rather than terminating this replacement.
    guard group.byEpoch.count < 64 else { throw Failure.cleanupPending }
    guard let claim = group.source.makeClaim(route: member.route) else { throw Failure.sourceClaimExhausted }
    let epoch: UInt64
    do { epoch = try group.ingress.register(claim) }
    catch let error as OwnedOrderedNotificationIngress.Failure { throw Failure.source(error) }
    let binding = Binding(memberID: member.id, claim: claim, epoch: epoch)
    group.current[member.id] = binding
    group.byEpoch[RouteEpoch(route: member.route, epoch: epoch)] = binding
    return RegisteredMember(id: member.id, route: member.route, sourceMember: claim.token, epoch: epoch)
  }

  /// Call before dispatching this group's logical claim through the existing
  /// shared physical subscription owner. Confirmation requires its real cleanup result.
  func retainPhysicalClaim(groupID: UInt64, memberID: UInt64) throws -> PhysicalClaim {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard let binding = group.current[memberID] else { throw Failure.inactiveMember }
    return try retainPhysicalClaim(groupID: groupID, memberID: memberID, sourceMember: binding.claim.token, epoch: binding.epoch)
  }

  func retainPhysicalClaim(groupID: UInt64, memberID: UInt64, sourceMember: UInt64, epoch: UInt64) throws -> PhysicalClaim {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard let binding = group.current[memberID], binding.claim.token == sourceMember, binding.epoch == epoch else { throw Failure.inactiveMember }
    let claim = PhysicalClaim(groupID: groupID, memberID: memberID, sourceClaim: binding.claim, epoch: binding.epoch)
    if group.pending[claim] != nil { return claim }
    guard group.lastConfirmed[memberID] != claim else { throw Failure.alreadyRetained }
    group.ingress.retainCleanup(binding.claim)
    group.pending[claim] = binding.claim
    return claim
  }

  /// Only a confirmed physical release (or authoritative generation fence)
  /// may retire this claim; an error leaves it owned for retry.
  func confirmPhysicalCleanup(_ claim: PhysicalClaim) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(claim.groupID)
    if group.lastConfirmed[claim.memberID] == claim { return }
    guard let owned = group.pending.removeValue(forKey: claim) else { throw Failure.unknownPhysicalClaim }
    group.ingress.cleanupFinished(owned)
    group.lastConfirmed[claim.memberID] = claim
    forgetFencedBinding(claim, in: group)
  }

  /// FFI-facing exact identity; unlike a path lookup this cannot retire a
  /// replacement claim for the same characteristic.
  func confirmPhysicalCleanup(groupID: UInt64, memberID: UInt64, opaqueSourceGeneration: String, sourceMember: UInt64, epoch: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard group.descriptor.opaqueSourceGeneration == opaqueSourceGeneration else { throw Failure.unknownPhysicalClaim }
    if let confirmed = group.lastConfirmed[memberID], confirmed.sourceClaim.token == sourceMember, confirmed.epoch == epoch { return }
    guard let claim = group.pending.keys.first(where: {
      $0.memberID == memberID && $0.sourceClaim.token == sourceMember && $0.epoch == epoch
    }) else {
      guard let binding = group.byEpoch.values.first(where: { $0.memberID == memberID && $0.claim.token == sourceMember && $0.epoch == epoch }) else { throw Failure.unknownPhysicalClaim }
      let claim = PhysicalClaim(groupID: groupID, memberID: memberID, sourceClaim: binding.claim, epoch: epoch)
      group.lastConfirmed[memberID] = claim
      forgetFencedBinding(claim, in: group)
      return
    }
    try confirmPhysicalCleanup(claim)
  }

  private func forgetFencedBinding(_ claim: PhysicalClaim, in group: Group) {
    let key = RouteEpoch(route: claim.sourceClaim.route, epoch: claim.epoch)
    if group.fenced.remove(key) != nil { group.byEpoch.removeValue(forKey: key) }
  }

  /// The source fence is queued before this route can be re-registered. It
  /// says nothing about callbacks still waiting in CoreBluetooth's queue.
  func retire(groupID: UInt64, memberID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard let binding = group.current[memberID] else { throw Failure.inactiveMember }
    if group.pendingRetirements[memberID] != nil { return }
    if hasUnresolvedCapturePosition(group) {
      // The retention is bounded by `requestedMembers`; the next replay or
      // cancellation wake calls `settlePendingRetirements` without requiring
      // the caller to repeat this exact retirement.
      group.pendingRetirements[memberID] = PendingRetirement(binding: binding)
      return
    }
    group.pendingRetirements[memberID] = PendingRetirement(binding: binding)
    try settlePendingRetirements(in: group)
  }

  func retire(groupID: UInt64, expected: RegisteredMember) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard let current = group.current[expected.id] else {
      if let pending = group.pendingRetirements[expected.id]?.binding,
         pending.claim.route == expected.route,
         pending.claim.token == expected.sourceMember,
         pending.epoch == expected.epoch { return }
      guard group.lastRetired[expected.id] == expected else { throw Failure.inactiveMember }
      return
    }
    guard current.claim.route == expected.route, current.claim.token == expected.sourceMember, current.epoch == expected.epoch else { throw Failure.inactiveMember }
    try retire(groupID: groupID, memberID: expected.id)
  }

  func close(groupID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    group.ingress.close()
    group.current.removeAll()
  }

  func seal(groupID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard group.pendingRetirements.isEmpty,
          try !captureOwner.hasPending(group.captureReservation) else { throw Failure.cleanupPending }
    do { try group.ingress.seal(); try captureOwner.retire(group.captureReservation); group.sealed = true }
    catch let error as OwnedOrderedNotificationIngress.Failure { throw Failure.source(error) }
  }

  func barrier(groupID: UInt64, id: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    if group.outstandingBarrier == id || group.lastAcknowledgedBarrier == id { return }
    guard group.outstandingBarrier == nil else { throw Failure.source(.controlOverflow) }
    guard !group.ingress.hasDeferredFailureCaptures,
          try !captureOwner.hasPending(group.captureReservation) else { throw Failure.capturePending }
    do { try group.ingress.barrier(id) }
    catch let error as OwnedOrderedNotificationIngress.Failure { throw Failure.source(error) }
    group.outstandingBarrier = id
  }

  /// Only one receipt is active per group, keeping retry and retirement bounded.
  func beginMarker(groupID: UInt64, generation: String, id: UInt64, input: MarkerInput) throws -> MarkerStatus? {
    dispatchPrecondition(condition: .onQueue(queue))
    try validateSource(groupID: groupID, opaqueSourceGeneration: generation)
    let group = try lookup(groupID)
    if let receipt = group.markerReceipt {
      guard receipt.id == id else { throw Failure.markerBusy }
      guard receipt.input == input else { throw Failure.markerMismatch }
      if group.markerWaitingForCapture { return nil }
      return receipt.status
    }
    if let retired = group.lastRetiredMarker {
      guard retired < UInt64.max, id == retired + 1 else { throw Failure.markerStale }
    } else if id != 0 { throw Failure.markerStale }
    group.markerReceipt = MarkerReceipt(id: id, input: input,
      status: MarkerStatus(admission: .pending, write: .initiating), acknowledged: false)
    return nil
  }

  func markerStatus(groupID: UInt64, generation: String, id: UInt64) throws -> MarkerStatus {
    try validateSource(groupID: groupID, opaqueSourceGeneration: generation)
    guard let receipt = try lookup(groupID).markerReceipt, receipt.id == id else { throw Failure.markerStale }
    return receipt.status
  }

  func markerRejected(groupID: UInt64, id: UInt64, failure: MarkerFailure, requiresAcknowledgement: Bool = false) throws -> MarkerStatus {
    let group = try lookup(groupID)
    guard var receipt = group.markerReceipt, receipt.id == id else { throw Failure.markerStale }
    receipt.status = MarkerStatus(admission: requiresAcknowledgement ? .rejected(failure) : .notAdmitted, write: .notStarted(failure))
    receipt.acknowledged = !requiresAcknowledgement
    group.markerReceipt = receipt
    group.markerWaitingForCapture = false
    return receipt.status
  }

  /// Check before claiming the cancellation gate or dispatching a write.
  /// An identical request can retry once the earlier capture has settled.
  func checkMarkerCaptureOrder(groupID: UInt64, opaqueSourceGeneration: String) throws {
    try validateSource(groupID: groupID, opaqueSourceGeneration: opaqueSourceGeneration)
    let group = try lookup(groupID)
    if try group.ingress.hasDeferredFailureCaptures || captureOwner.hasPending(group.captureReservation) {
      group.markerWaitingForCapture = true
      throw Failure.capturePending
    }
    group.markerWaitingForCapture = false
  }

  /// Re-check one retained, not-yet-dispatched marker after the capture
  /// boundary wakes. A settled receipt (for example through cancellation)
  /// must not be dispatched again. This is deliberately narrower than
  /// `checkMarkerCaptureOrder`: a settled receipt cannot be re-admitted just
  /// because an old retry timer fired late.
  func retryWaitingMarker(groupID: UInt64, opaqueSourceGeneration: String, id: UInt64) throws -> MarkerRetryState {
    try validateSource(groupID: groupID, opaqueSourceGeneration: opaqueSourceGeneration)
    let group = try lookup(groupID)
    guard group.markerWaitingForCapture,
          let receipt = group.markerReceipt,
          receipt.id == id,
          case .pending = receipt.status.admission,
          case .initiating = receipt.status.write else { return .settled }
    do {
      try checkMarkerCaptureOrder(groupID: groupID, opaqueSourceGeneration: opaqueSourceGeneration)
      return .ready
    } catch Failure.capturePending {
      return .waiting
    }
  }

  func cancelWaitingMarker(groupID: UInt64, generation: String, id: UInt64) throws -> MarkerStatus? {
    try validateSource(groupID: groupID, opaqueSourceGeneration: generation)
    let group = try lookup(groupID)
    guard group.markerWaitingForCapture, group.markerReceipt?.id == id else { return nil }
    let failure = MarkerFailure(kind: "cancelled", gattStatus: nil, domain: nil, code: nil,
                                detail: "marker cancelled before admission", dispatched: false)
    return try markerRejected(groupID: groupID, id: id, failure: failure)
  }

  func markerAdmitted(groupID: UInt64, id: UInt64, ordinal: UInt64) throws {
    let group = try lookup(groupID)
    guard var receipt = group.markerReceipt, receipt.id == id else { throw Failure.markerStale }
    receipt.status = MarkerStatus(admission: .admitted(ordinal), write: .initiating)
    group.markerReceipt = receipt
  }

  func markerStarted(groupID: UInt64, id: UInt64) throws -> MarkerStatus {
    let group = try lookup(groupID)
    guard var receipt = group.markerReceipt, receipt.id == id else { throw Failure.markerStale }
    guard case .admitted = receipt.status.admission, case .initiating = receipt.status.write else { throw Failure.markerNotSettled }
    receipt.status.write = .started(nil)
    group.markerReceipt = receipt
    return receipt.status
  }

  func markerSettled(groupID: UInt64, id: UInt64, failure: MarkerFailure?) throws {
    let group = try lookup(groupID)
    guard var receipt = group.markerReceipt, receipt.id == id else { throw Failure.markerStale }
    guard case .started = receipt.status.write else { throw Failure.markerNotSettled }
    receipt.status.write = .settled(failure)
    group.markerReceipt = receipt
  }

  func forgetMarker(groupID: UInt64, generation: String, id: UInt64) throws {
    try validateSource(groupID: groupID, opaqueSourceGeneration: generation)
    let group = try lookup(groupID)
    if let retired = group.lastRetiredMarker, id <= retired { return }
    guard let receipt = group.markerReceipt, receipt.id == id else { throw Failure.markerStale }
    guard receipt.acknowledged else { throw Failure.markerNotSettled }
    switch receipt.status.write {
    case .initiating, .started: throw Failure.markerNotSettled
    case .notStarted, .settled: break
    }
    group.markerReceipt = nil
    group.lastRetiredMarker = id
  }

  /// End this bridge's groups without invalidating the peer-wide source shared
  /// with other borrowers. Queued records and cleanup receipts remain owned.
  func disconnectGroups(peer: String) {
    dispatchPrecondition(condition: .onQueue(queue))
    for group in groups.values where group.requestedMembers.first?.route.peer == peer {
      group.ingress.invalidate(.connectionLost)
      group.current.removeAll()
    }
  }

  func peerForGroup(_ groupID: UInt64) -> String? {
    dispatchPrecondition(condition: .onQueue(queue))
    return groups[groupID]?.requestedMembers.first?.route.peer
  }

  func validateMarkerTarget(groupID: UInt64, opaqueSourceGeneration: String,
                            peer: String, snapshot: OwnedGattSnapshotIdentity) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    try validateSource(groupID: groupID, opaqueSourceGeneration: opaqueSourceGeneration)
    let group = try lookup(groupID)
    guard group.requestedMembers.first?.route.peer == peer, group.snapshot == snapshot, validateSnapshot(peer, snapshot) else {
      throw Failure.staleGeneration
    }
  }

  /// The caller must validate its write route and initiate the write on this
  /// same owner queue immediately after admission.
  @discardableResult
  func admitMarker(groupID: UInt64, opaqueSourceGeneration: String, id: UInt64) throws -> UInt64 {
    dispatchPrecondition(condition: .onQueue(queue))
    try checkMarkerCaptureOrder(groupID: groupID, opaqueSourceGeneration: opaqueSourceGeneration)
    let group = try lookup(groupID)
    do { return try group.source.admitMarker(group: group.ingress, id: id) }
    catch let error as OwnedOrderedNotificationIngress.Failure { throw Failure.source(error) }
  }

  /// A refused host wake is terminal; the retained FIFO and physical claims
  /// stay available to an explicit drain/cleanup retry.
  func failWake(groupID: UInt64, opaqueSourceGeneration: String) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    try validateSource(groupID: groupID, opaqueSourceGeneration: opaqueSourceGeneration)
    try lookup(groupID).ingress.failWake()
  }

  /// Materialize exactly the capture-owner prefix at its original ingress
  /// positions.  The owner retains payloads until ACK; this method owns only
  /// the positional barriers, which are removed as they become records in the
  /// bridge's retained batch.
  private func resolveCapturedPrefix(_ owned: AppleOrderedGroupCaptureOwner.Batch, in group: Group) {
    for record in owned.records {
      switch record {
      case let .failure(failure):
        guard let ticket = group.captureIngressBarriers.keys.min(by: { $0.sequence < $1.sequence }),
              let barrier = group.captureIngressBarriers.removeValue(forKey: ticket) else {
          preconditionFailure("captured failure has no ingress position")
        }
        group.ingress.resolveOwnedFailureCaptureBarrier(barrier, failure: failure, notify: false)
      case .omitted:
        guard let omission = owned.omission, var barriers = group.omissionIngressBarriers[omission.run], !barriers.isEmpty else {
          preconditionFailure("omission has no ingress position")
        }
        if omission.continues {
          // Resolve only the earliest segment.  The final barrier remains a
          // hard prefix fence for the still-unready omission suffix.
          let barrier = barriers.removeFirst()
          guard let replacement = group.ingress.replaceExternalCaptureBarrier(barrier, with: .failureObservationsOmitted, notify: false) else {
            preconditionFailure("omission barrier was not retained")
          }
          group.omissionIngressBarriers[omission.run] = [replacement] + barriers
        } else {
          for barrier in barriers {
            group.ingress.resolveExternalCaptureBarrier(barrier, with: .failureObservationsOmitted, notify: false)
          }
          group.omissionIngressBarriers.removeValue(forKey: omission.run)
        }
      }
    }
  }

  /// A lost/cancelled foreign completion replays the same bounded batch;
  /// accepted values and fences remain owned until exact acknowledgement.
  func drain(groupID: UInt64) throws -> Batch? {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    if let pending = group.pendingBatch { return pending }
    guard group.batchIDAvailable else { throw Failure.batchIDExhausted }
    // A capture-owner position can resolve asynchronously between host wakes;
    // re-check its retained retirements on every drain so the source fence
    // follows that exact position automatically.
    try settlePendingRetirements(in: group)
    // Capture storage is a prefix owner, not a second delivery lane.  Resolve
    // its ready prefix into the ingress before draining ingress events so a
    // value/control observed later cannot overtake a retained callback.
    if try captureOwner.hasPending(group.captureReservation), !group.captureHasEarlierIngress {
      guard let owned = try captureOwner.drain(group.captureReservation) else { return nil }
      group.pendingCaptureBatchID = owned.id
      resolveCapturedPrefix(owned, in: group)
      try settlePendingRetirements(in: group)
      group.captureWakePending = false
    }
    var (newEvents, failureCredits) = group.ingress.drainForAcknowledgement()
    var events = group.rollbackEvents + newEvents
    group.rollbackEvents.removeAll()
    if events.isEmpty {
      // A later capture cannot pass a value reserved before the graph gate.
      guard !group.ingress.hasPendingValues else { return nil }
      // Any admitted prefix from before the capture has now drained.  The
      // remaining ingress observations are necessarily after it.
      group.captureHasEarlierIngress = false
      guard let owned = try captureOwner.drain(group.captureReservation) else { return nil }
      group.pendingCaptureBatchID = owned.id
      resolveCapturedPrefix(owned, in: group)
      try settlePendingRetirements(in: group)
      (newEvents, failureCredits) = group.ingress.drainForAcknowledgement()
      events = newEvents
      group.captureWakePending = false
    }
    let records: [Record] = events.map { event in
      switch event {
      case let .value(route, epoch, ordinal, bytes, provenance):
        guard let binding = group.byEpoch[RouteEpoch(route: route, epoch: epoch)] else { return .bridgeFailure }
        return .value(memberID: binding.memberID, sourceMember: binding.claim.token, epoch: epoch, ordinal: ordinal, value: bytes, provenance: provenance)
      case let .fence(route, epoch, tail):
        guard let binding = group.byEpoch[RouteEpoch(route: route, epoch: epoch)] else { return .bridgeFailure }
        return .fence(memberID: binding.memberID, sourceMember: binding.claim.token, epoch: epoch, platformTail: tail)
      case let .barrier(id, tail): return .barrier(id: id, platformTail: tail)
      case let .marker(id, ordinal): return .marker(id: id, ordinal: ordinal)
      case let .loss(route, count):
        guard let route else { return .loss(memberID: nil, sourceRecords: count) }
        guard let member = group.memberByRoute[route] else { return .bridgeFailure }
        return .loss(memberID: member, sourceRecords: count)
      case let .subsequentSourceFailure(failure): return .subsequentSourceFailure(failure)
      case .failureObservationsOmitted: return .failureObservationsOmitted
      case let .terminal(reason, tail, rejected):
        let mapped: Rejected?
        switch rejected {
        case nil: mapped = nil
        case let .value(route, epoch, bytes)?:
          if let binding = group.byEpoch[RouteEpoch(route: route, epoch: epoch)] {
            mapped = .value(memberID: binding.memberID, sourceMember: binding.claim.token, epoch: epoch, bytes: bytes)
          } else { mapped = .unmapped }
        case let .sourceLoss(route, count)?:
          if let route {
            if let id = group.memberByRoute[route] { mapped = .sourceLoss(memberID: id, skippedRecords: count) }
            else { mapped = .unmapped }
          } else { mapped = .sourceLoss(memberID: nil, skippedRecords: count) }
        case let .fence(route, epoch)?:
          if let binding = group.byEpoch[RouteEpoch(route: route, epoch: epoch)] {
            mapped = .fence(memberID: binding.memberID, sourceMember: binding.claim.token, epoch: epoch)
          } else { mapped = .unmapped }
        case let .barrier(id)?: mapped = .barrier(id: id)
        case let .marker(id)?: mapped = .marker(id: id)
        }
        let mappedReason: End
        switch reason {
        case .closed: mappedReason = .closed
        case .connectionLost: mappedReason = .connectionLost
        case .serviceChanged: mappedReason = .serviceChanged
        case .sourceReplaced: mappedReason = .sourceReplaced
        case .shutdown: mappedReason = .shutdown
        case .adapterLost: mappedReason = .adapterLost
        case .overflow: mappedReason = .overflow
        case .ordinalExhausted: mappedReason = .ordinalExhausted
        case .wakeFailed: mappedReason = .wakeFailed
        case let .sourceFailed(failure, route, sourceMember, epoch):
          if let binding = group.byEpoch[RouteEpoch(route: route, epoch: epoch)], binding.claim.token == sourceMember {
            mappedReason = .sourceFailed(failure, memberID: binding.memberID, sourceMember: sourceMember, epoch: epoch)
          } else {
            mappedReason = .sourceFailureUnmapped(failure, sourceMember: sourceMember, epoch: epoch)
          }
        }
        return .terminal(mappedReason, platformTail: tail, rejected: mapped)
      }
    }
    let id = group.nextBatchID
    if id == UInt64.max { group.batchIDAvailable = false } else { group.nextBatchID += 1 }
    let batch = Batch(id: id, records: records)
    group.pendingBatch = batch
    group.pendingFailureCredits = failureCredits
    return batch
  }

  /// Inspect only the retained batch. An acknowledgement retry for an older
  /// batch must not consume or create a later batch as a side effect.
  func pendingBatch(groupID: UInt64, batchID: UInt64) throws -> Batch? {
    dispatchPrecondition(condition: .onQueue(queue))
    let batch = try lookup(groupID).pendingBatch
    return batch?.id == batchID ? batch : nil
  }

  /// A v2 acknowledgement is valid only for an exact batch already returned
  /// in v2 form. Retain the last receipt for lost-reply retries.
  func markVersionedBatch(groupID: UInt64, batchID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard group.pendingBatch?.id == batchID, group.legacyBatchID == nil else { throw Failure.unknownBatch }
    group.versionedBatchID = batchID
  }

  func markLegacyBatch(groupID: UInt64, batchID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard group.pendingBatch?.id == batchID, group.versionedBatchID == nil else { throw Failure.unknownBatch }
    group.legacyBatchID = batchID
  }

  func acknowledgeVersionedBatch(groupID: UInt64, batchID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    if group.lastVersionedAcknowledgement == batchID { group.ingress.wakeIfPending(); return }
    guard group.versionedBatchID == batchID, group.pendingBatch?.id == batchID else { throw Failure.unknownBatch }
    try consumeBatch(groupID: groupID, batchID: batchID)
    group.versionedBatchID = nil
    group.lastVersionedAcknowledgement = batchID
  }

  func acknowledgeLegacyBatch(groupID: UInt64, batchID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    if group.lastLegacyAcknowledgement == batchID { group.ingress.wakeIfPending(); return }
    guard group.legacyBatchID == batchID, group.pendingBatch?.id == batchID else { throw Failure.unknownBatch }
    try acknowledge(groupID: groupID, batchID: batchID)
    group.lastLegacyAcknowledgement = batchID
  }

  /// Acknowledgement is valid only after every record in that exact batch was
  /// admitted by Rust; this is also the consumed-fence boundary.
  func acknowledge(groupID: UInt64, batchID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard group.versionedBatchID != batchID, group.lastVersionedAcknowledgement != batchID else { throw Failure.unknownBatch }
    try consumeBatch(groupID: groupID, batchID: batchID)
    if group.legacyBatchID == batchID { group.legacyBatchID = nil }
  }

  private func consumeBatch(groupID: UInt64, batchID: UInt64) throws {
    let group = try lookup(groupID)
    if group.lastAcknowledgedBatchID == batchID {
      group.ingress.wakeIfPending()
      return
    }
    guard let batch = group.pendingBatch, batch.id == batchID else { throw Failure.unknownBatch }
    if let captureBatchID = group.pendingCaptureBatchID {
      try captureOwner.acknowledge(group.captureReservation, batchID: captureBatchID)
      group.pendingCaptureBatchID = nil
    }
    for record in batch.records {
      switch record {
      case let .fence(_, sourceMember, epoch, _):
        if let key = group.byEpoch.first(where: { $0.value.claim.token == sourceMember && $0.key.epoch == epoch })?.key {
          group.fenced.insert(key)
          if let binding = group.byEpoch[key], let confirmed = group.lastConfirmed[binding.memberID], confirmed.sourceClaim == binding.claim, confirmed.epoch == epoch {
            forgetFencedBinding(confirmed, in: group)
          }
        }
      case let .marker(id, _):
        if var receipt = group.markerReceipt, receipt.id == id {
          receipt.acknowledged = true
          group.markerReceipt = receipt
        }
      case let .barrier(id, _):
        group.outstandingBarrier = nil
        group.lastAcknowledgedBarrier = id
      case let .terminal(_, _, rejected):
        if case let .marker(id)? = rejected, var receipt = group.markerReceipt, receipt.id == id {
          receipt.acknowledged = true
          group.markerReceipt = receipt
        }
        group.terminalDrained = true
      default: break
      }
    }
    group.pendingBatch = nil
    group.ingress.acknowledgeFailureCredits(group.pendingFailureCredits)
    group.pendingFailureCredits = 0
    group.lastAcknowledgedBatchID = batchID
    group.ingress.wakeIfPending()
    if group.captureWakePending { scheduleCaptureWake(groupID: groupID, group: group) }
  }

  func remove(groupID: UInt64) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    let group = try lookup(groupID)
    guard group.terminalDrained, group.pendingBatch == nil, group.pending.isEmpty, group.markerReceipt == nil,
          group.pendingRetirements.isEmpty,
          try captureOwner.canRelease(group.captureReservation), removeSourceGroup(group.source, group.ingress) else { throw Failure.cleanupPending }
    try captureOwner.release(group.captureReservation)
    removed[groupID] = group.descriptor.opaqueSourceGeneration
    groups.removeValue(forKey: groupID)
  }

  func remove(groupID: UInt64, opaqueSourceGeneration: String) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    if let generation = removed[groupID] {
      guard generation == opaqueSourceGeneration else { throw Failure.staleGeneration }
      return
    }
    try validateSource(groupID: groupID, opaqueSourceGeneration: opaqueSourceGeneration)
    try remove(groupID: groupID)
  }

  /// Called only when the host has durably acknowledged the removal receipt.
  func acknowledgeRemoval(groupID: UInt64, opaqueSourceGeneration: String) throws {
    dispatchPrecondition(condition: .onQueue(queue))
    guard let generation = removed[groupID] else {
      if let group = groups[groupID] {
        guard group.descriptor.opaqueSourceGeneration == opaqueSourceGeneration else { throw Failure.staleGeneration }
        throw Failure.duplicateGroup
      }
      return
    }
    guard generation == opaqueSourceGeneration else { throw Failure.staleGeneration }
    removed.removeValue(forKey: groupID)
  }

  private func lookup(_ groupID: UInt64) throws -> Group {
    guard let group = groups[groupID] else { throw Failure.unknownGroup }
    return group
  }
}
