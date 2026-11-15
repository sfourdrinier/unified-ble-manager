package com.sfourdrinier.unifiedblemanager.radio

import java.util.UUID
import java.util.Collections

/** Version-2 admission receipts. This table does not own installed sources or removal receipts. */
internal class AndroidOrderedOpenSlots {
  val incarnation: UUID = UUID.randomUUID()

  data class Snapshot(val owner: String, val attachment: ULong, val revision: ULong)
  data class Member(val memberId: ULong, val peerId: String, val serviceUuid: String,
                    val serviceOccurrence: ULong, val characteristicUuid: String, val characteristicOccurrence: ULong)
  data class Spec(val connectionGeneration: String, val databaseGeneration: String, val snapshot: Snapshot,
                  val members: List<Member>, val itemCapacity: UInt, val byteCapacity: UInt)
  data class SlotIdentity(val ownerIncarnation: UUID, val slot: Int, val generation: ULong)
  data class Identity(val version: Int, val ownerIncarnation: UUID, val slot: Int, val slotGeneration: ULong,
                      val originalRequestId: ULong, val nativeGroupId: ULong, val peerId: String, val spec: Spec) {
    fun slotIdentity() = SlotIdentity(ownerIncarnation, slot, slotGeneration)
    internal fun snapshot() = copy(spec = spec.copy(members = Collections.unmodifiableList(ArrayList(spec.members))))
  }
  sealed interface Ownership {
    data object NotCreated : Ownership
    data class Created(val opened: AndroidOrderedGroupOwner.Opened) : Ownership
  }
  sealed interface Result {
    data object Pending : Result
    data class Opened(val opened: AndroidOrderedGroupOwner.Opened) : Result
    data class Failed(val error: Throwable, val ownership: Ownership) : Result
    data class Indeterminate(val reason: Throwable) : Result
  }
  data class Observation(val slot: SlotIdentity, val result: Result)
  data class Retired(val slot: SlotIdentity)
  /** Slot debt only. This is deliberately not a native-owner quiescence receipt. */
  data class OwnerSlotStatus(val ownerIncarnation: UUID, val retirementId: UUID,
                             val executing: Int, val terminal: Int, val adapterWork: Int, val effectWork: Int)
  class Fenced : IllegalStateException("ordered open was fenced before execution")
  class Execution internal constructor(internal val identity: Identity)
  class AdapterWork internal constructor() { internal var started = false }
  class EffectWork internal constructor() { internal var started = false }

  private sealed interface State {
    data object Retired : State
    data class Reserved(val identity: Identity) : State
    data class Executing(val permit: Execution, val observation: Result = Result.Pending) : State
    data class Terminal(val identity: Identity, val result: Result) : State
  }
  private class Slot(var generation: ULong = 0uL, var state: State = State.Retired)
  private val lock = Any()
  private val slots = Array(64) { Slot() }
  private val adapterWork = mutableSetOf<AdapterWork>()
  private val effectWork = mutableSetOf<EffectWork>()
  private var retirementId: UUID? = null

  /** Atomically reject stale/conflicting requests or establish the slot fence. */
  fun ownerControl(owner: UUID, retirement: UUID, fence: Boolean): String? = synchronized(lock) {
    if (owner != incarnation) return@synchronized "wrong-owner"
    if (retirementId != null && retirementId != retirement || !fence && retirementId != retirement)
      return@synchronized "conflicting-retirement"
    if (fence) fenceOwner(owner, retirement)
    null
  }

  /** Fence reservation and execution under the same monitor used for slot admission. */
  fun fenceOwner(owner: UUID, retirement: UUID): OwnerSlotStatus = synchronized(lock) {
    check(owner == incarnation) { "ordered owner incarnation does not match" }
    val existing = retirementId
    check(existing == null || existing == retirement) { "ordered owner retirement conflicts" }
    if (existing == null) {
      retirementId = retirement
      slots.forEach { slot ->
        val reserved = slot.state as? State.Reserved
        if (reserved != null) slot.state = State.Terminal(reserved.identity, Result.Failed(Fenced(), Ownership.NotCreated))
      }
    }
    ownerSlotStatus(retirement)
  }

  fun ownerStatus(owner: UUID, retirement: UUID): OwnerSlotStatus = synchronized(lock) {
    check(owner == incarnation) { "ordered owner incarnation does not match" }
    check(retirementId == retirement) { "ordered owner retirement does not match" }
    ownerSlotStatus(retirement)
  }

  private fun ownerSlotStatus(retirement: UUID) = OwnerSlotStatus(incarnation, retirement,
    slots.count { it.state is State.Executing }, slots.count { it.state is State.Terminal }, adapterWork.size, effectWork.size)

  /** Admission and the owner fence share this lock. A queued effect must still begin here. */
  fun reserveEffectWork(): EffectWork? = synchronized(lock) {
    if (retirementId != null) null else EffectWork().also { effectWork.add(it) }
  }

  fun beginEffectWork(work: EffectWork): Boolean = synchronized(lock) {
    check(work in effectWork && !work.started) { "owner effect work is no longer queued" }
    if (retirementId != null) false else { work.started = true; true }
  }

  fun finishEffectWork(work: EffectWork) = synchronized(lock) { effectWork.remove(work); Unit }

  /** Reserve the slot and its adapter closure in the same critical section as owner fencing. */
  fun reserveAdapterWork(request: Identity): AdapterWork = synchronized(lock) {
    check(retirementId == null) { "ordered owner is retiring" }
    reserve(request)
    AdapterWork().also { adapterWork.add(it) }
  }

  fun startAdapterWork(work: AdapterWork) = synchronized(lock) {
    check(work in adapterWork) { "ordered adapter work is no longer admitted" }
    work.started = true
  }

  /** A rejected executor did not accept a closure that has not already begun. */
  fun rejectAdapterWork(work: AdapterWork) = synchronized(lock) {
    if (!work.started) adapterWork.remove(work)
    Unit
  }

  /** Only call after the closure and its completion relay have both returned. */
  fun finishAdapterWork(work: AdapterWork) = synchronized(lock) {
    adapterWork.remove(work)
    Unit
  }

  private fun slot(identity: Identity): Slot {
    require(identity.version == 2 && identity.slot in slots.indices && identity.slotGeneration != 0uL) { "invalid ordered slot identity" }
    check(identity.ownerIncarnation == incarnation) { "ordered slot belongs to another owner" }
    return slots[identity.slot]
  }
  private fun validate(identity: Identity) {
    require(identity.spec.members.all { it.peerId == identity.peerId }) { "ordered open peer mismatch" }
  }
  private fun next(slot: Slot, identity: Identity) {
    check(slot.generation != ULong.MAX_VALUE && identity.slotGeneration == slot.generation + 1uL) { "ordered slot generation is not next" }
  }
  private fun same(expected: Identity, actual: Identity) {
    check(expected == actual) { "ordered open identity does not match" }
  }
  private fun stateResult(state: State): Result = when (state) {
    is State.Reserved -> Result.Pending
    is State.Executing -> state.observation
    is State.Terminal -> state.result
    State.Retired -> error("ordered slot is retired")
  }
  private fun identity(state: State): Identity = when (state) {
    is State.Reserved -> state.identity
    is State.Executing -> state.permit.identity
    is State.Terminal -> state.identity
    State.Retired -> error("ordered slot is retired")
  }

  /** Bind before dispatch; duplicate submissions only observe the existing operation. */
  fun reserve(request: Identity): Observation = synchronized(lock) {
    val bound = request.snapshot()
    val slot = slot(bound)
    validate(bound)
    if (slot.state == State.Retired) {
      check(retirementId == null) { "ordered owner is retiring" }
      next(slot, bound)
      slot.generation = bound.slotGeneration
      slot.state = State.Reserved(bound)
    } else same(identity(slot.state), bound)
    Observation(bound.slotIdentity(), stateResult(slot.state))
  }

  /** Observe an exact active identity without fencing a queued open. */
  fun observe(request: Identity): Observation = synchronized(lock) {
    val slot = slot(request)
    check(slot.state != State.Retired) { "ordered slot is retired" }
    same(identity(slot.state), request)
    Observation(request.slotIdentity(), stateResult(slot.state))
  }

  /** The queued closure calls this immediately before installation, outside the slot monitor. */
  fun begin(request: Identity): Execution? = synchronized(lock) {
    val slot = slot(request)
    if (request.slotGeneration < slot.generation || slot.state == State.Retired && request.slotGeneration <= slot.generation) return null
    if (slot.state == State.Retired) { next(slot, request); error("ordered open was not reserved") }
    same(identity(slot.state), request)
    if (slot.state !is State.Reserved) return null
    Execution(identity(slot.state)).also { slot.state = State.Executing(it) }
  }

  /** Fence is independent of execution and can overtake an open before native admission. */
  fun fence(request: Identity): Observation = synchronized(lock) {
    val bound = request.snapshot()
    val slot = slot(bound)
    validate(bound)
    if (slot.state == State.Retired) {
      check(retirementId == null) { "ordered owner is retiring" }
      next(slot, bound)
      slot.generation = bound.slotGeneration
      val result = Result.Failed(Fenced(), Ownership.NotCreated)
      slot.state = State.Terminal(bound, result)
      return Observation(bound.slotIdentity(), result)
    }
    same(identity(slot.state), bound)
    if (slot.state is State.Reserved) {
      val result = Result.Failed(Fenced(), Ownership.NotCreated)
      slot.state = State.Terminal(identity(slot.state), result)
    }
    Observation(request.slotIdentity(), stateResult(slot.state))
  }

  /** A dispatch failure is no-effect evidence only while still reserved. */
  fun rejectBeforeExecution(request: Identity, error: Throwable): Boolean = synchronized(lock) {
    val slot = slot(request)
    if (slot.state == State.Retired || request.slotGeneration < slot.generation) return false
    same(identity(slot.state), request)
    if (slot.state !is State.Reserved) return false
    slot.state = State.Terminal(identity(slot.state), Result.Failed(error, Ownership.NotCreated))
    true
  }

  /** An indeterminate observation keeps execution and its receipt alive for later resolution. */
  fun complete(permit: Execution, result: Result): Boolean = synchronized(lock) {
    require(result !is Result.Pending) { "pending is not a completion" }
    val slot = slot(permit.identity)
    val executing = slot.state as? State.Executing ?: return false
    if (executing.permit !== permit) return false
    slot.state = if (result is Result.Indeterminate) State.Executing(permit, result)
      else State.Terminal(permit.identity, result.snapshot())
    true
  }

  private fun Result.snapshot(): Result = when (this) {
    is Result.Opened -> copy(opened = opened.copy(members = Collections.unmodifiableList(ArrayList(opened.members))))
    is Result.Failed -> when (val owned = ownership) {
      is Ownership.Created -> copy(ownership = Ownership.Created(owned.opened.copy(members = Collections.unmodifiableList(ArrayList(owned.opened.members)))))
      Ownership.NotCreated -> this
    }
    else -> this
  }

  /** A retired older generation proves only retirement, never its original disposition. */
  fun acknowledge(request: Identity): Retired = synchronized(lock) {
    val slot = slot(request)
    if (request.slotGeneration < slot.generation || slot.state == State.Retired && request.slotGeneration == slot.generation)
      return Retired(request.slotIdentity())
    check(slot.state is State.Terminal) { "ordered open is not terminal" }
    same(identity(slot.state), request)
    slot.state = State.Retired
    Retired(request.slotIdentity())
  }
}
