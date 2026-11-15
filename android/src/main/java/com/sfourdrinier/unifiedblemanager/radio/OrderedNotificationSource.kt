// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OrderedNotificationSource.kt
package com.sfourdrinier.unifiedblemanager.radio

import java.util.ArrayDeque

internal class OrderedRegistrationStale : IllegalStateException("ordered registration is not current")
internal class OrderedOpenNotCreated(cause: Throwable) : IllegalStateException("ordered source was not created", cause)

/** Connection-scoped callback ingress. No callback is made while the owner lock is held. */
internal class OrderedNotificationSource<K : Any>(
  val generation: Long, val sourceGeneration: ULong = generation.toULong(), initialOrdinal: ULong = 0uL
) {
  internal sealed class Event<K> {
    data class Value<K>(val member: K, val sourceMember: ULong, val epoch: ULong, val ordinal: ULong, val bytes: ByteArray) : Event<K>()
    data class Fence<K>(val member: K, val sourceMember: ULong, val epoch: ULong) : Event<K>()
    data class Barrier<K>(val barrierId: ULong) : Event<K>()
    data class Marker<K>(val markerId: ULong, val ordinal: ULong) : Event<K>()
    data class Loss<K>(val scope: LossScope<K>, val unit: LossUnit, val count: ULong?, val bytes: ULong?) : Event<K>()
    data class Terminal<K>(val reason: Reason, val rejectedValues: ULong?, val uncertainTail: Boolean,
                           val rejectedLoss: Loss<K>? = null, val wakeFailed: Boolean = false,
                           val rejectedValue: RejectedValue<K>? = null, val rejectedFence: Fence<K>? = null,
                           val rejectedBarrier: ULong? = null, val rejectedMarker: ULong? = null,
                           val sourceFailure: SourceFailure<K>? = null) : Event<K>()
    /** An observed later cause, or evidence that further observations exceeded the bounded slot. */
    data class SubsequentFailure<K>(val failure: SourceFailure<K>?, val overflow: Boolean) : Event<K>()
  }
  internal data class RejectedValue<K>(val member: K, val sourceMember: ULong, val epoch: ULong, val bytes: ULong)
  internal sealed class LossScope<K> {
    data class Member<K>(val member: K) : LossScope<K>()
    class Group<K> : LossScope<K>()
    class Unknown<K> : LossScope<K>()
  }
  internal enum class LossUnit { VALUES, SOURCE_RECORDS }
  internal data class Membership(val sourceMember: ULong, val epoch: ULong)
  internal data class SourceFailure<K>(val sourceGeneration: ULong, val member: K, val membership: Membership, val cause: Throwable)
  internal data class PhysicalClaim<K>(val sourceGeneration: ULong, val groupId: ULong, val member: K, val membership: Membership)
  internal enum class Reason { CLOSED, CONNECTION_LOST, DATABASE_CHANGED, OVERFLOW, ORDINAL_EXHAUSTED, WAKE_FAILED, SOURCE_FAILED }
  internal class WakeFailure(val sourceGeneration: ULong, val groupId: ULong, cause: Throwable) : RuntimeException("ordered group wake failed", cause)
  internal inner class Group internal constructor(
    val id: ULong, internal val members: Set<K>, internal val capacity: Int, internal val byteCapacity: Int,
    internal val wake: () -> Unit
  ) {
    internal val events = ArrayDeque<Event<K>>()
    internal val epochs = mutableMapOf<K, ULong>()
    internal val active = mutableMapOf<K, Membership>()
    internal val lastRetired = mutableMapOf<K, Membership>()
    internal val lastRegistered = mutableMapOf<K, Pair<Membership, Membership>>()
    internal val cleanupPending = mutableSetOf<PhysicalClaim<K>>()
    internal val cleanupConfirmed = mutableMapOf<K, PhysicalClaim<K>>()
    internal var bytes = 0
    internal var terminal: Event.Terminal<K>? = null
    internal var sourceFailure: SourceFailure<K>? = null
    internal val subsequentFailures = ArrayDeque<SourceFailure<K>>()
    // Identity deduplication is deliberately bounded. Once an identity ages
    // out, a later callback is another observation, not a claimed new cause.
    internal val recentFailureCauses = ArrayDeque<Throwable>()
    internal var failureOverflowPending = false
    internal var ended = false
    internal var wakeFailureId = id
    internal var lastBarrierId: ULong? = null
    internal var pendingBarrierId: ULong? = null
    internal var pendingWakes = 0
    internal var lostDeferredWakes = 0
    internal var sealed = false
    internal var onWakeFailure: ((ULong, ULong, Throwable) -> Unit)? = null
  }
  private val lock = Any()
  internal fun lockHeldByCurrentThread(): Boolean = Thread.holdsLock(lock)
  private val groups = mutableSetOf<Group>()
  private var nextOrdinal = initialOrdinal
  private var ordinalAvailable = true
  private var nextGroupId = 0uL
  private var groupIdAvailable = true
  private var nextSourceMember = 0uL
  private var sourceMemberAvailable = true
  private var invalidated = false

  /** Installs the bounded group before the caller requests any CCCD enable. */
  fun open(members: Set<K>, capacity: Int, byteCapacity: Int, wake: () -> Unit): Group = synchronized(lock) {
    require(members.isNotEmpty() && capacity > 0 && byteCapacity > 0)
    check(!invalidated)
    check(groupIdAvailable)
    val id = nextGroupId
    if (id == ULong.MAX_VALUE) groupIdAvailable = false else nextGroupId++
    Group(id, members.toSet(), capacity, byteCapacity, wake).also { groups.add(it) }
  }

  /** Installs every logical member in the same callback-admission critical section. */
  fun openRegistered(members: Set<K>, capacity: Int, byteCapacity: Int, failureId: ULong? = null,
                     onWakeFailure: ((ULong, ULong, Throwable) -> Unit)? = null,
                     onRegistered: ((Group, Map<K, Membership>) -> Unit)? = null,
                     wake: () -> Unit): Pair<Group, Map<K, Membership>> = synchronized(lock) {
    val group = open(members, capacity, byteCapacity, wake)
    val registered = try {
      if (failureId != null) {
        group.wakeFailureId = failureId
        group.onWakeFailure = onWakeFailure
      }
      members.associateWith { register(group, it) }
    } catch (error: Throwable) {
      groups.remove(group)
      throw error
    }
    // Handoff happens while the source lock still protects the installed
    // group. Once handed off, the caller owns recovery even if it throws.
    onRegistered?.invoke(group, registered)
    group to registered
  }

  fun register(group: Group, member: K): Membership = synchronized(lock) {
    check(member !in group.epochs) { "initial ordered registration already exists" }
    registerLocked(group, member)
  }

  fun register(group: Group, member: K, expected: Membership): Membership = synchronized(lock) {
    check(group in groups && !group.ended && group.terminal == null && member in group.members)
    val previous = group.lastRegistered[member]
    if (previous?.first == expected && group.active[member] == previous.second) return previous.second
    if (group.lastRetired[member] != expected || member in group.active) throw OrderedRegistrationStale()
    val replacement = registerLocked(group, member)
    group.lastRegistered[member] = expected to replacement
    replacement
  }

  private fun registerLocked(group: Group, member: K): Membership {
    check(group in groups && !group.ended && group.terminal == null && member in group.members && member !in group.active)
    val previous = group.epochs[member] ?: 0uL
    check(previous != ULong.MAX_VALUE)
    check(sourceMemberAvailable)
    val sourceMember = nextSourceMember
    if (sourceMember == ULong.MAX_VALUE) sourceMemberAvailable = false else nextSourceMember++
    val epoch = previous + 1uL
    group.epochs[member] = epoch
    return Membership(sourceMember, epoch).also { group.active[member] = it }
  }

  /** A successful return means the old epoch's fence is retained before re-registration. */
  fun retire(group: Group, member: K, expected: Membership): Boolean {
    var wake = false
    val result = synchronized(lock) {
      check(group in groups && !group.ended && group.terminal == null)
      if (group.lastRetired[member] == expected) return@synchronized true
      if (group.active[member] != expected) throw OrderedRegistrationStale()
      val accepted = if (group.events.size == group.capacity) {
        val membership = group.active.getValue(member)
        wake = endLocked(group, Reason.OVERFLOW, null, rejectedFence = Event.Fence(member, membership.sourceMember, membership.epoch))
        false
      } else {
        wake = group.events.isEmpty()
        val membership = group.active.getValue(member)
        group.events.addLast(Event.Fence(member, membership.sourceMember, membership.epoch))
        group.active.remove(member)
        group.lastRetired[member] = membership
        true
      }
      if (wake) group.pendingWakes++
      accepted
    }
    if (wake) wakeGroup(group)
    return result
  }

  /** Places a non-retiring control in the same critical section as callback admission. */
  fun barrier(group: Group, barrierId: ULong): Boolean {
    var wake = false
    val accepted = synchronized(lock) {
      check(group in groups && !group.ended)
      if (group.lastBarrierId == barrierId) return@synchronized true
      group.lastBarrierId?.let { check(barrierId > it) }
      check(group.pendingBarrierId == null) { "ordered barrier awaits batch acknowledgment" }
      check(group.terminal == null)
      val result = if (group.events.size == group.capacity) {
        wake = endLocked(group, Reason.OVERFLOW, null, rejectedBarrier = barrierId)
        false
      } else {
        wake = group.events.isEmpty()
        group.events.addLast(Event.Barrier(barrierId))
        group.lastBarrierId = barrierId
        group.pendingBarrierId = barrierId
        true
      }
      if (wake) group.pendingWakes++
      result
    }
    if (wake) wakeGroup(group)
    return accepted
  }

  internal sealed class MarkerAdmission<out T> {
    data class Rejected(val terminalEvidence: Boolean) : MarkerAdmission<Nothing>()
    data class Admitted<T>(val ordinal: ULong, val effect: Result<T>) : MarkerAdmission<T>()
  }

  /** Atomically append the boundary and initiate the native effect. The caller
   * holds the connection admission gate and defers wake delivery until unlock. */
  fun <T> markerAndRun(group: Group, markerId: ULong, deferWake: ((() -> Unit) -> Boolean), effect: (ULong) -> T): MarkerAdmission<T> {
    var wake = false
    val result = synchronized(lock) {
      check(group in groups)
      if (group.ended || group.terminal != null || group.sealed || invalidated) return@synchronized MarkerAdmission.Rejected(false)
      if (group.events.size == group.capacity || !ordinalAvailable) {
        wake = endLocked(group, if (ordinalAvailable) Reason.OVERFLOW else Reason.ORDINAL_EXHAUSTED, null, rejectedMarker = markerId)
        if (wake) group.pendingWakes++
        return@synchronized MarkerAdmission.Rejected(true)
      }
      val ordinal = nextOrdinal
      if (ordinal == ULong.MAX_VALUE) ordinalAvailable = false else nextOrdinal++
      wake = group.events.isEmpty()
      group.events.addLast(Event.Marker(markerId, ordinal))
      if (wake) group.pendingWakes++
      MarkerAdmission.Admitted(ordinal, runCatching { effect(ordinal) })
    }
    if (wake && !deferWake { wakeGroup(group) }) synchronized(lock) { group.lostDeferredWakes++ }
    return result
  }

  fun acknowledgeBarrier(group: Group, barrierId: ULong) = synchronized(lock) {
    check(group in groups && group.pendingBarrierId == barrierId)
    group.pendingBarrierId = null
  }

  /** Called before any per-characteristic staging, directly from the native callback. */
  fun admit(member: K, value: ByteArray, deferWake: ((() -> Unit) -> Boolean)? = null) {
    val wakes = mutableListOf<Group>()
    synchronized(lock) {
      if (invalidated) return
      if (!ordinalAvailable) {
        groups.forEach { if (endLocked(it, Reason.ORDINAL_EXHAUSTED, null)) { it.pendingWakes++; wakes.add(it) } }
      } else {
        val ordinal = nextOrdinal
        if (ordinal == ULong.MAX_VALUE) ordinalAvailable = false else nextOrdinal++
        groups.forEach { group ->
          if (member in group.active && group.terminal == null && !group.ended) {
            if (group.events.size == group.capacity || value.size > group.byteCapacity - group.bytes) {
              val membership = group.active.getValue(member)
              val rejected = RejectedValue(member, membership.sourceMember, membership.epoch, value.size.toULong())
              if (endLocked(group, Reason.OVERFLOW, 1uL, rejectedValue = rejected)) { group.pendingWakes++; wakes.add(group) }
            } else {
              if (group.events.isEmpty()) { group.pendingWakes++; wakes.add(group) }
              val membership = group.active.getValue(member)
              group.events.addLast(Event.Value(member, membership.sourceMember, membership.epoch, ordinal, value.copyOf()))
              group.bytes += value.size
            }
          }
        }
      }
    }
    if (deferWake != null && wakes.isNotEmpty()) {
      if (!deferWake { wakeAll(wakes) }) synchronized(lock) { wakes.forEach { it.lostDeferredWakes++ } }
    } else wakeAll(wakes)
  }
  fun failDeferredIngress() {
    val wakes = synchronized(lock) {
      groups.filter { group ->
        group.pendingWakes -= group.lostDeferredWakes
        group.lostDeferredWakes = 0
        if (group.sealed || group.ended) false else {
          endLocked(group, Reason.OVERFLOW, null)
          val terminal = group.terminal
          if (terminal != null && terminal.reason != Reason.OVERFLOW && terminal.rejectedLoss == null) {
            group.terminal = terminal.copy(rejectedLoss = Event.Loss(LossScope.Unknown(), LossUnit.SOURCE_RECORDS, null, null))
          }
          group.terminal != null
        }
      }.also { list -> list.forEach { it.pendingWakes++ } }
    }
    wakeAll(wakes)
  }
  fun close(group: Group) { end(group, Reason.CLOSED) }
  /** Retain before dispatching physical acquisition, including an indeterminate outcome. */
  fun retainPhysicalClaim(group: Group, member: K, expected: Membership): PhysicalClaim<K> = synchronized(lock) {
    check(group in groups && !group.ended && group.terminal == null)
    if (group.active[member] != expected) throw OrderedRegistrationStale()
    val claim = PhysicalClaim(sourceGeneration, group.id, member, expected)
    check(group.cleanupConfirmed[member] != claim) { "ordered claim registration was already released" }
    group.cleanupPending.add(claim)
    claim
  }
  fun hasPhysicalClaim(group: Group, member: K, expected: Membership): Boolean = synchronized(lock) {
    group in groups && !group.ended && group.terminal == null && group.active[member] == expected &&
      PhysicalClaim(sourceGeneration, group.id, member, expected) in group.cleanupPending
  }
  fun <T : Any> withPhysicalClaim(group: Group, member: K, expected: Membership, effect: () -> T): T? = synchronized(lock) {
    if (group !in groups || group.ended || group.terminal != null || group.active[member] != expected ||
      PhysicalClaim(sourceGeneration, group.id, member, expected) !in group.cleanupPending) return null
    effect()
  }
  /** Call only after the physical owner confirms release or an authoritative link fence. */
  fun confirmPhysicalCleanup(group: Group, claim: PhysicalClaim<K>) = synchronized(lock) {
    check(group in groups)
    if (group.cleanupPending.remove(claim)) group.cleanupConfirmed[claim.member] = claim
    else if (group.cleanupConfirmed[claim.member] != claim) {
      check(group.active[claim.member] == claim.membership) { "ordered cleanup claim is not current" }
      group.cleanupConfirmed[claim.member] = claim
    }
  }
  fun reportLoss(group: Group, scope: LossScope<K>, unit: LossUnit, count: ULong?, bytes: ULong?) {
    var wake = false
    synchronized(lock) {
      check(group in groups && (scope !is LossScope.Member || scope.member in group.members))
      if (!group.ended && group.terminal == null) {
        if (group.events.size == group.capacity) {
          val rejected = Event.Loss(scope, unit, count, bytes)
          wake = endLocked(group, Reason.OVERFLOW, null, rejected)
        } else {
          wake = group.events.isEmpty()
          group.events.addLast(Event.Loss(scope, unit, count, bytes))
        }
      }
      if (wake) group.pendingWakes++
    }
    if (wake) wakeGroup(group)
  }
  /** Atomically fence exactly these active memberships. A stale target rejects the
   * whole request; retries of recently retained causes are idempotent. Later
   * observations use bounded capacity and an unknown-overflow indication.
   * Physical claims remain owned until their cleanup is confirmed. Wake callbacks
   * run after the source lock is released, or through the caller's admission gate. */
  fun failMember(member: K, expected: Map<Group, Membership>, cause: Throwable,
                 deferWake: ((() -> Unit) -> Boolean)? = null): Boolean =
    failMembers(member, expected, cause, false, deferWake)

  /** Capture exactly the memberships receiving this physical characteristic now. */
  fun activeMemberships(member: K): Map<Group, Membership> = synchronized(lock) {
    groups.mapNotNull { group ->
      if (group.ended || group.sealed || group.terminal != null) null
      else group.active[member]?.let { group to it }
    }.toMap()
  }

  /** Reconcile a physical operation's snapshot atomically: fence each still-current
   * membership and discard stale entries rather than retargeting a replacement. */
  fun failCurrentMembers(member: K, expected: Map<Group, Membership>, cause: Throwable,
                         deferWake: ((() -> Unit) -> Boolean)? = null) {
    failMembers(member, expected, cause, true, deferWake)
  }

  private fun failMembers(member: K, expected: Map<Group, Membership>, cause: Throwable,
                          skipStale: Boolean, deferWake: ((() -> Unit) -> Boolean)?): Boolean {
    require(expected.isNotEmpty())
    val wakes = mutableListOf<Group>()
    val accepted = synchronized(lock) {
      fun current(group: Group, membership: Membership) =
        group in groups && !group.sealed && (group.sourceFailure?.let { it.member == member && it.membership == membership } == true ||
          (!group.ended && !group.sealed && group.terminal == null && group.active[member] == membership))
      if (!skipStale && expected.any { (group, membership) -> !current(group, membership) }) return@synchronized false
      expected.filter { (group, membership) -> current(group, membership) }.forEach { (group, membership) ->
        if (group.sourceFailure == null) {
          val failure = SourceFailure(sourceGeneration, member, membership, cause)
          group.sourceFailure = failure
          endLocked(group, Reason.SOURCE_FAILED, null, sourceFailure = failure)
          // Terminal notification is required even if an earlier record had
          // already scheduled a wake; the wake may have drained concurrently.
          group.pendingWakes++
          wakes.add(group)
        } else if (group.sourceFailure?.cause !== cause && group.recentFailureCauses.none { it === cause }) {
          if (group.subsequentFailures.size < group.capacity && !group.failureOverflowPending) {
            group.subsequentFailures.addLast(SourceFailure(sourceGeneration, member, membership, cause))
            if (group.recentFailureCauses.size == group.capacity) group.recentFailureCauses.removeFirst()
            group.recentFailureCauses.addLast(cause)
          } else if (!group.failureOverflowPending) {
            // The number and identities of omitted observations are unknown.
            group.failureOverflowPending = true
          } else return@forEach
          group.pendingWakes++
          wakes.add(group)
        }
      }
      expected.all { (group, membership) -> current(group, membership) }
    }
    if (deferWake != null && wakes.isNotEmpty()) {
      if (!deferWake { wakeAll(wakes) }) synchronized(lock) { wakes.forEach { it.lostDeferredWakes++ } }
    } else wakeAll(wakes)
    return accepted
  }
  fun invalidate(reason: Reason, deferWake: ((() -> Unit) -> Boolean)? = null) {
    require(reason == Reason.CONNECTION_LOST || reason == Reason.DATABASE_CHANGED)
    val wakes = synchronized(lock) {
      invalidated = true
      groups.filter { endLocked(it, reason, null) }.also { list -> list.forEach { it.pendingWakes++ } }
    }
    if (deferWake != null && wakes.isNotEmpty()) {
      if (!deferWake { wakeAll(wakes) }) synchronized(lock) { wakes.forEach { it.lostDeferredWakes++ } }
    } else wakeAll(wakes)
  }
  private fun wakeAll(groups: List<Group>) {
    var failure: Throwable? = null
    groups.forEach { group ->
      try { wakeGroup(group) } catch (error: Throwable) {
        if (failure == null) failure = error else failure?.addSuppressed(error)
      }
    }
    failure?.let { throw it }
  }
  private fun wakeGroup(group: Group) {
    try { group.wake() } catch (error: Throwable) {
      synchronized(lock) {
        if (!group.ended) {
          val terminal = group.terminal
          group.active.clear()
          group.terminal = terminal?.copy(wakeFailed = true)
            ?: Event.Terminal(Reason.WAKE_FAILED, null, true, wakeFailed = true)
        }
      }
      try { group.onWakeFailure?.invoke(sourceGeneration, group.wakeFailureId, error) }
      catch (signalError: Throwable) { error.addSuppressed(signalError) }
      throw WakeFailure(sourceGeneration, group.wakeFailureId, error)
    } finally {
      synchronized(lock) { group.pendingWakes-- }
    }
  }
  private fun end(group: Group, reason: Reason) {
    val wake = synchronized(lock) { endLocked(group, reason, null).also { if (it) group.pendingWakes++ } }
    if (wake) wakeGroup(group)
  }
  private fun endLocked(group: Group, reason: Reason, rejected: ULong?, rejectedLoss: Event.Loss<K>? = null,
                        rejectedValue: RejectedValue<K>? = null, rejectedFence: Event.Fence<K>? = null,
                        rejectedBarrier: ULong? = null, rejectedMarker: ULong? = null,
                        sourceFailure: SourceFailure<K>? = null): Boolean {
    if (group !in groups || group.ended || group.terminal != null) return false
    group.active.clear()
    group.terminal = Event.Terminal(reason, rejected, true, rejectedLoss, rejectedValue = rejectedValue, rejectedFence = rejectedFence, rejectedBarrier = rejectedBarrier, rejectedMarker = rejectedMarker, sourceFailure = sourceFailure)
    return group.events.isEmpty()
  }
  fun drain(group: Group): List<Event<K>> = synchronized(lock) {
    val result = group.events.toMutableList()
    group.events.clear()
    group.bytes = 0
    group.terminal?.let { result.add(it); group.terminal = null; group.ended = true }
      ?: if (group.ended && (group.subsequentFailures.isNotEmpty() || group.failureOverflowPending)) {
        while (group.subsequentFailures.isNotEmpty()) {
          result.add(Event.SubsequentFailure(group.subsequentFailures.removeFirst(), false))
        }
        if (group.failureOverflowPending) result.add(Event.SubsequentFailure(null, true))
        group.failureOverflowPending = false
      } else Unit
    result
  }
  /** Re-signal records queued while an earlier drained batch awaited acknowledgment. */
  fun wakeIfPending(group: Group) {
    val pending = synchronized(lock) {
      (group in groups && !group.sealed && (group.events.isNotEmpty() || group.terminal != null ||
        group.subsequentFailures.isNotEmpty() || group.failureOverflowPending)).also { if (it) group.pendingWakes++ }
    }
    if (pending) wakeGroup(group)
  }
  fun seal(group: Group) = synchronized(lock) {
    check(group in groups)
    if (group.sealed) return
    check(group.terminal != null || group.ended) { "ordered group must be closed before sealing" }
    check(group.pendingWakes == 0) { "ordered wake is still in flight" }
    group.sealed = true
  }
  /** Removes only this source route after its physical owner has confirmed every claim. */
  fun remove(group: Group): Boolean = synchronized(lock) {
    group.ended && group.cleanupPending.isEmpty() && group.subsequentFailures.isEmpty() && !group.failureOverflowPending && groups.remove(group)
  }
}
