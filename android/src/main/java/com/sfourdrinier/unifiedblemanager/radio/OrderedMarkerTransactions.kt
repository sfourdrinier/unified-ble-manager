// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OrderedMarkerTransactions.kt
package com.sfourdrinier.unifiedblemanager.radio

internal class OrderedMarkerCancelled : IllegalStateException("marker write cancelled before admission")

/** Bounded per-group ownership of marker/write attempts and their retry receipts. */
internal class OrderedMarkerTransactions(private val capacity: Int = 64) {
  internal data class Identity(
    val graph: AndroidNativeGraphIdentity,
    val connectionGeneration: String,
    val target: OwnedAndroidNotificationPath,
    val bytes: ByteArray,
    val withResponse: Boolean
  ) {
    fun same(other: Identity): Boolean = graph == other.graph && connectionGeneration == other.connectionGeneration &&
      target == other.target && bytes.contentEquals(other.bytes) && withResponse == other.withResponse
  }
  internal sealed interface State {
    data object Reserved : State
    data class NotAdmitted(val error: Throwable) : State
    data class Rejected(val error: Throwable, val evidenceRequired: Boolean) : State
    data class AdmittedNotStarted(val ordinal: ULong, val error: Throwable) : State
    data class Initiating(val ordinal: ULong) : State
    data class Started(val ordinal: ULong, val uncertainty: Throwable? = null) : State
    data class Settled(val ordinal: ULong, val result: Result<ByteArray?>) : State
  }
  internal data class Reservation(val state: State, val created: Boolean, val cancellation: OrderedMarkerCancellation)
  private data class Entry(val identity: Identity, val cancellation: OrderedMarkerCancellation, var state: State, var evidenceAcknowledged: Boolean = false, var scheduled: Boolean = false, var admitting: Boolean = false, var driverPending: Boolean = true, var operationId: Long? = null)
  private val lock = Any()
  private val entries = mutableMapOf<ULong, Entry>()
  private var lastId: ULong? = null
  private var retired: ULong? = null

  /** The ID is never reused. An exact retry observes its existing state. */
  fun reserve(id: ULong, identity: Identity, cancellation: OrderedMarkerCancellation = OrderedMarkerCancellation()): Reservation = synchronized(lock) {
    entries[id]?.let { check(it.identity.same(identity)) { "marker transaction identity changed" }; return Reservation(it.state, false, it.cancellation) }
    check(entries.isEmpty() && entries.size < capacity) { "a marker transaction is still retained" }
    val expected = lastId?.let { check(it != ULong.MAX_VALUE) { "marker transaction ID exhausted" }; it + 1uL } ?: 0uL
    check(id == expected) { "marker transaction ID is not the next contiguous ID" }
    lastId = id
    entries[id] = Entry(identity.copy(bytes = identity.bytes.copyOf()), cancellation, State.Reserved)
    Reservation(State.Reserved, true, cancellation)
  }

  fun schedule(id: ULong): Boolean = synchronized(lock) {
    val entry = checkNotNull(entries[id])
    if (entry.state != State.Reserved || entry.scheduled) return false
    entry.scheduled = true
    entry.driverPending = true
    true
  }

  /** Cancellation and write admission serialize on this lock. The effect must
   * defer external callbacks until its enclosing native admission gate unlocks. */
  fun admit(id: ULong, start: ((ULong) -> Unit) -> State): State {
    val entry = synchronized(lock) {
      val current = checkNotNull(entries[id])
      if (current.state != State.Reserved || current.admitting) return current.state
      current.admitting = true
      current
    }
    val state = try { start { ordinal -> synchronized(lock) { entry.state = State.Initiating(ordinal) } } }
    catch (error: Throwable) {
      synchronized(lock) {
        when (val current = entry.state) {
          is State.Initiating -> State.Started(current.ordinal, error)
          is State.Started -> current.copy(uncertainty = error)
          else -> State.NotAdmitted(error)
        }
      }
    }
    check(state != State.Reserved)
    return synchronized(lock) { entry.state = state; entry.admitting = false; state }
  }

  fun cancel(id: ULong, error: Throwable): State = synchronized(lock) {
    val entry = checkNotNull(entries[id])
    if (entry.state == State.Reserved && !entry.admitting) entry.state = State.NotAdmitted(error)
    entry.state
  }

  /** A failure outside the write effect still preserves any observed initiation. */
  fun failAdmission(id: ULong, error: Throwable): State = synchronized(lock) {
    val entry = checkNotNull(entries[id])
    when (val current = entry.state) {
      is State.Initiating -> State.Started(current.ordinal, error)
      is State.Started -> if (current.uncertainty == null) current.copy(uncertainty = error) else current
      State.Reserved -> State.NotAdmitted(error)
      else -> current
    }.also { entry.state = it }
  }

  fun settle(id: ULong, result: Result<ByteArray?>): State = synchronized(lock) {
    val entry = checkNotNull(entries[id])
    val started = entry.state as? State.Started ?: return entry.state
    State.Settled(started.ordinal, result.map { it?.copyOf() }).also { entry.state = it }
  }

  /** A retained Android callback may arrive after lifecycle settlement and receipt retirement. */
  fun settleIfRetained(id: ULong, result: Result<ByteArray?>): State? = synchronized(lock) {
    val entry = entries[id] ?: return null
    val started = entry.state as? State.Started ?: return entry.state
    State.Settled(started.ordinal, result.map { it?.copyOf() }).also { entry.state = it }
  }

  /** Request cancellation without holding the receipt lock across native admission. */
  fun cancelExact(id: ULong, error: Throwable): Pair<State, Long?> {
    val gate = synchronized(lock) { checkNotNull(entries[id]).cancellation }
    gate.cancel()
    return synchronized(lock) {
      val entry = checkNotNull(entries[id])
      if (entry.state == State.Reserved && !entry.admitting) entry.state = State.NotAdmitted(error)
      entry.state to entry.operationId
    }
  }
  /** If cancellation won during submission, cancel the newly associated cell. */
  fun bindOperation(id: ULong, operationId: Long): Boolean = synchronized(lock) {
    val entry = entries[id] ?: return false
    if (!entry.driverPending) return false
    entry.operationId = operationId
    entry.state is State.NotAdmitted
  }
  /** The adapter task returned before submitting any GATT queue cell. */
  fun adapterSettledBeforeSchedule(id: ULong) = synchronized(lock) {
    entries[id]?.let { if (!it.scheduled) it.driverPending = false }
  }
  fun driverSettled(id: ULong) = synchronized(lock) {
    entries[id]?.let { it.driverPending = false; it.operationId = null }
  }
  fun acknowledgeEvidence(id: ULong) = synchronized(lock) { checkNotNull(entries[id]).evidenceAcknowledged = true }
  fun isEmpty(): Boolean = synchronized(lock) { entries.isEmpty() }
  fun retainedCount(): Int = synchronized(lock) { entries.size }
  fun state(id: ULong): State? = synchronized(lock) { entries[id]?.state }
  fun forget(id: ULong): Boolean = synchronized(lock) {
    val entry = entries[id] ?: return retired?.let { id <= it } ?: false
    if (entry.driverPending || entry.state is State.Reserved || entry.state is State.Initiating || entry.state is State.Started) return false
    val needsEvidence = entry.state is State.AdmittedNotStarted || entry.state is State.Settled ||
      (entry.state as? State.Rejected)?.evidenceRequired == true
    if (needsEvidence && !entry.evidenceAcknowledged) return false
    entries.remove(id)
    retired = id
    true
  }
}

/** Linearizes caller cancellation against the marker and write admission. */
internal class OrderedMarkerCancellation {
  private enum class Phase { OPEN, CANCELLED, ADMITTED }
  private val lock = Any()
  private var phase = Phase.OPEN
  fun cancel() = synchronized(lock) { if (phase == Phase.OPEN) phase = Phase.CANCELLED }
  fun <T> admit(action: () -> T): T? {
    synchronized(lock) {
      if (phase != Phase.OPEN) return null
      phase = Phase.ADMITTED
    }
    return action()
  }
}
