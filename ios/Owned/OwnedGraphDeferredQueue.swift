// ios/Owned/OwnedGraphDeferredQueue.swift
import Foundation

/// Owner-queue-confined FIFO. Callers serialize every method on the radio queue.
/// Callbacks run only after the relevant entry has left the queue.
final class OwnedGraphDeferredQueue {
  struct Token: Hashable { fileprivate let id: UUID }
  private enum Entry {
    case value(Token, Int, () -> Void, () -> Void)
    case control(Token, Int, () -> Void, () -> Void)
    case request(Token, Int, () -> Void, () -> Void)
    case barrier(Token, (@escaping () -> Void) -> Void)
    case loss(Token, () -> Void)
    case barrierOverflow(Token)
    case reservedControl(Token, () -> Void)
    case failStop(Token, () -> Void)

    var token: Token {
      switch self {
      case let .value(token, _, _, _), let .control(token, _, _, _),
           let .request(token, _, _, _), let .barrier(token, _), let .loss(token, _), let .barrierOverflow(token),
           let .reservedControl(token, _), let .failStop(token, _): return token
      }
    }
    var bytes: Int {
      switch self {
      case let .value(_, bytes, _, _), let .control(_, bytes, _, _),
           let .request(_, bytes, _, _): return bytes
      case .barrier, .loss, .barrierOverflow, .reservedControl, .failStop: return 0
      }
    }
    var isReserved: Bool {
      switch self { case .barrier, .loss, .barrierOverflow, .reservedControl, .failStop: return true; default: return false }
    }
    func reject() {
      switch self {
      case let .value(_, _, _, loss), let .control(_, _, _, loss),
           let .request(_, _, _, loss): loss()
      case .barrier, .loss, .barrierOverflow, .reservedControl, .failStop: break
      }
    }
  }

  private let maxItems: Int
  private let maxBytes: Int
  private let onOverflow: () -> Void
  private let maxBarriers: Int
  private let onBarrierOverflow: () -> Void
  private let maxReservedControls: Int
  private var controlReservations: [Token: Bool] = [:] // true while queued or executing
  private var failStopOutstanding = false
  private var entries: [Entry] = []
  private var itemCount = 0
  private var byteCount = 0
  private var barrierCount = 0
  private var barrierOverflowOutstanding = false
  private var lossOutstanding = false
  private var pausedBarrier: Token?
  private var draining = false

  init(maxItems: Int, maxBytes: Int, maxBarriers: Int = 64, maxReservedControls: Int = 0, onOverflow: @escaping () -> Void = {}, onBarrierOverflow: @escaping () -> Void = {}) {
    precondition(maxItems > 0 && maxBytes > 0 && maxBarriers > 0 && maxReservedControls >= 0)
    self.maxItems = maxItems
    self.maxBytes = maxBytes
    self.onOverflow = onOverflow
    self.maxBarriers = maxBarriers
    self.onBarrierOverflow = onBarrierOverflow
    self.maxReservedControls = maxReservedControls
  }

  var isEmpty: Bool { entries.isEmpty && pausedBarrier == nil }
  var isPaused: Bool { pausedBarrier != nil }
  var isDraining: Bool { draining }
  var hasBarrier: Bool { barrierCount > 0 || barrierOverflowOutstanding }

  /// A caller reserves each control slot before admitting the owner that needs
  /// it. Reservations remain busy throughout a reentrant delivery.
  func reserveControl() -> Token? {
    reserveControls(count: 1)?.first
  }

  /// Reserve the complete control budget at once, before admitting an owner.
  /// A refused reservation does not consume any of the remaining capacity.
  func reserveControls(count: Int) -> [Token]? {
    guard count > 0, count <= maxReservedControls - controlReservations.count else { return nil }
    var tokens: [Token] = []
    tokens.reserveCapacity(count)
    for _ in 0..<count {
      let token = Token(id: UUID())
      controlReservations[token] = false
      tokens.append(token)
    }
    return tokens
  }

  @discardableResult
  func releaseControl(_ token: Token) -> Bool {
    releaseControls([token])
  }

  /// Do not free any credits while one of the owner's records is outstanding.
  @discardableResult
  func releaseControls(_ tokens: [Token]) -> Bool {
    guard !tokens.isEmpty, Set(tokens).count == tokens.count,
          tokens.allSatisfy({ controlReservations[$0] == false }) else { return false }
    for token in tokens { controlReservations.removeValue(forKey: token) }
    return true
  }

  @discardableResult
  func enqueueReservedControl(_ token: Token, run: @escaping () -> Void) -> Bool {
    guard controlReservations[token] == false else { return false }
    controlReservations[token] = true
    // A reserved observation owns capacity, not an earlier place in time.
    // In particular, it must not overtake an already scheduled fail-stop.
    entries.append(.reservedControl(token, run))
    return true
  }

  /// One fail-stop action stays behind all work observed before it runs.
  func scheduleFailStop(_ run: @escaping () -> Void) {
    guard !failStopOutstanding else { return }
    failStopOutstanding = true
    entries.append(.failStop(Token(id: UUID()), run))
  }

  private func appendBeforeFailStop(_ entry: Entry) {
    if let last = entries.last, case .failStop = last {
      entries.insert(entry, at: entries.count - 1)
    } else {
      entries.append(entry)
    }
  }

  @discardableResult
  func enqueueValue(bytes: Int, run: @escaping () -> Void, onLoss: @escaping () -> Void) -> Token? {
    let token = Token(id: UUID())
    return append(.value(token, bytes, run, onLoss)) ? token : nil
  }

  @discardableResult
  func enqueueControl(bytes: Int, run: @escaping () -> Void, onLoss: @escaping () -> Void) -> Token? {
    let token = Token(id: UUID())
    return append(.control(token, bytes, run, onLoss)) ? token : nil
  }

  @discardableResult
  func enqueueRequest(bytes: Int, run: @escaping () -> Void, onReject: @escaping () -> Void) -> Token? {
    let token = Token(id: UUID())
    return append(.request(token, bytes, run, onReject)) ? token : nil
  }

  /// Barriers have their own bounded capacity. On exhaustion a single reserved
  /// fail-stop marker is queued in order; the caller must settle each rejected
  /// operation separately. Further barriers are rejected until that marker runs.
  @discardableResult
  func enqueueBarrier(arrived: @escaping (@escaping () -> Void) -> Void) -> Bool {
    guard !barrierOverflowOutstanding, barrierCount < maxBarriers else {
      if !barrierOverflowOutstanding {
        barrierOverflowOutstanding = true
        appendBeforeFailStop(.barrierOverflow(Token(id: UUID())))
      }
      return false
    }
    barrierCount += 1
    appendBeforeFailStop(.barrier(Token(id: UUID()), arrived))
    return true
  }

  private func append(_ entry: Entry) -> Bool {
    guard !lossOutstanding, entry.bytes >= 0, itemCount < maxItems, entry.bytes <= maxBytes - byteCount else {
      switch entry {
      case let .value(_, _, _, loss), let .control(_, _, _, loss):
        if !lossOutstanding {
          lossOutstanding = true
          appendBeforeFailStop(.loss(Token(id: UUID()), loss))
        }
      case .request: break
      case .barrier, .loss, .barrierOverflow, .reservedControl, .failStop: preconditionFailure("reserved entry passed to append")
      }
      onOverflow()
      if case .request = entry { entry.reject() }
      return false
    }
    appendBeforeFailStop(entry)
    itemCount += 1
    byteCount += entry.bytes
    return true
  }

  /// Rejects only entries still waiting; executing and already settled entries
  /// cannot be settled a second time. Reserved barriers cannot be cancelled.
  func cancel(_ token: Token) {
    guard let index = entries.firstIndex(where: { $0.token == token }), !entries[index].isReserved else { return }
    let entry = entries.remove(at: index)
    itemCount -= 1
    byteCount -= entry.bytes
    entry.reject()
  }

  /// Reject pending observations and requests, preserving reset barriers.
  func rejectPending() {
    let rejected = entries.filter { !$0.isReserved }
    entries.removeAll { !$0.isReserved }
    itemCount = 0
    byteCount = 0
    for entry in rejected { entry.reject() }
  }

  func drain() {
    guard !draining, pausedBarrier == nil else { return }
    draining = true
    while pausedBarrier == nil, !entries.isEmpty {
      let entry = entries.removeFirst()
      if !entry.isReserved { itemCount -= 1; byteCount -= entry.bytes }
      switch entry {
      case let .value(_, _, run, _), let .control(_, _, run, _), let .request(_, _, run, _):
        run()
      case let .loss(_, loss):
        lossOutstanding = false
        loss()
      case .barrierOverflow:
        barrierOverflowOutstanding = false
        onBarrierOverflow()
      case let .reservedControl(token, run):
        run()
        controlReservations[token] = false
      case let .failStop(_, run):
        failStopOutstanding = false
        run()
      case let .barrier(token, arrived):
        pausedBarrier = token
        arrived { [weak self] in self?.resumeBarrier(token) }
      }
    }
    draining = false
  }

  private func resumeBarrier(_ token: Token) {
    guard pausedBarrier == token else { return }
    pausedBarrier = nil
    barrierCount -= 1
    drain()
  }
}
