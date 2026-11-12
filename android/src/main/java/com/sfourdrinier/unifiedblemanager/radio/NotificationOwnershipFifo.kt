// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/NotificationOwnershipFifo.kt
package com.sfourdrinier.unifiedblemanager.radio

import java.util.ArrayDeque

/**
 * A single physical owner's ingress, not a shared logical-subscription manager. Arm before native
 * registration, activate after success, and retire on every outcome. Native callbacks carry their
 * exact claim and generation. The integration retains this object for cleanup after fail-stop.
 */
internal class NotificationOwnershipFifo(
  private val itemCapacity: Int,
  private val byteCapacity: Long,
  private val controlCapacity: Int,
  private val onStopped: (Stop) -> Unit,
  private val deliver: (Event) -> Unit,
) {
  init { require(itemCapacity > 0 && byteCapacity > 0 && controlCapacity > 0) }

  internal class Claim internal constructor(val generation: Long) {
    internal var active = false
    internal var retired = false
    internal var loss: PendingLoss? = null
  }
  internal sealed class Event {
    abstract val claim: Claim
    data class Value(override val claim: Claim, val bytes: ByteArray) : Event()
    data class Loss(override val claim: Claim, val items: Long, val bytes: Long) : Event()
    data class Terminal(override val claim: Claim, val failure: Throwable?) : Event()
  }
  internal sealed class Stop {
    abstract val uncertainItems: Long
    abstract val uncertainBytes: Long
    data class DeliveryUncertain(
      val cause: Throwable,
      override val uncertainItems: Long,
      override val uncertainBytes: Long,
    ) : Stop()
    data class ControlCapacity(
      override val uncertainItems: Long,
      override val uncertainBytes: Long,
    ) : Stop()
  }
  internal sealed class Admission {
    data class Armed(val claim: Claim) : Admission()
    data object OwnerActive : Admission()
    data class Stopped(val reason: Stop) : Admission()
  }
  internal class PendingLoss(val claim: Claim, var items: Long, var bytes: Long)
  private sealed class Entry {
    data class Value(val event: Event.Value) : Entry()
    data class Loss(val pending: PendingLoss) : Entry()
    data class Terminal(val event: Event.Terminal) : Entry()
    val owner: Claim get() = when (this) {
      is Value -> event.claim
      is Loss -> pending.claim
      is Terminal -> event.claim
    }
  }

  private val lock = Any()
  private val queue = ArrayDeque<Entry>()
  private var current: Claim? = null
  private var controls = 0
  private var draining = false
  private var items = 0
  private var bytes = 0L
  private var stopped: Stop? = null
  private var inFlight: Event? = null
  private var stopNotified = false

  /** The stopped state and all queued entries remain owned until integration cleanup. */
  fun stopReason(): Stop? = synchronized(lock) { stopped }

  fun arm(generation: Long): Admission {
    val result = synchronized(lock) {
      val reason = stopped
      when {
        reason != null -> Admission.Stopped(reason)
        current?.retired == false -> Admission.OwnerActive
        controls >= controlCapacity -> Admission.Stopped(failCapacityLocked())
        else -> Admission.Armed(Claim(generation).also { current = it })
      }
    }
    notifyStop()
    return result
  }

  /** Invoked at most once, outside the lock. Handler exceptions propagate to its caller. */
  private fun notifyStop() {
    val reason = synchronized(lock) {
      val result = stopped
      if (result == null || stopNotified) return
      stopNotified = true
      result
    }
    onStopped(reason)
  }

  /** True means the callback was claimed and accounted for, including as loss on overflow;
   * it does not guarantee that the value was enqueued. False means the callback was fenced. */
  fun offer(owner: Claim, generation: Long, value: ByteArray): Boolean {
    val start = synchronized(lock) {
      if (stopped != null || current !== owner || owner.generation != generation || owner.retired) return false
      val size = value.size.toLong()
      val loss = owner.loss
      if (loss != null) {
        loss.items = Math.addExact(loss.items, 1L)
        loss.bytes = Math.addExact(loss.bytes, size)
      } else if (items == itemCapacity || size > byteCapacity - bytes) {
        PendingLoss(owner, 1L, size).also { owner.loss = it; queue.addLast(Entry.Loss(it)) }
      } else {
        queue.addLast(Entry.Value(Event.Value(owner, value.copyOf())))
        items++
        bytes += size
      }
      beginDrainLocked()
    }
    if (start) drain()
    return true
  }

  fun activate(owner: Claim, generation: Long): Boolean {
    val start = synchronized(lock) {
      if (stopped != null || current !== owner || owner.generation != generation || owner.retired) return false
      owner.active = true
      beginDrainLocked()
    }
    if (start) drain()
    return true
  }

  fun retire(owner: Claim, generation: Long, failure: Throwable? = null): Boolean {
    var accepted = true
    val start = synchronized(lock) {
      if (stopped != null || current !== owner || owner.generation != generation || owner.retired) return false
      if (controls >= controlCapacity) {
        failCapacityLocked()
        accepted = false
        false
      } else {
        owner.retired = true
        owner.active = true
        queue.addLast(Entry.Terminal(Event.Terminal(owner, failure)))
        controls++
        beginDrainLocked()
      }
    }
    notifyStop()
    if (start) drain()
    return accepted
  }

  /** Converts staged, never-activated observations into explicit known loss before retirement. */
  fun retireStagedAsLoss(owner: Claim, generation: Long): Boolean {
    var accepted = true
    val start = synchronized(lock) {
      if (stopped != null || current !== owner || owner.generation != generation || owner.retired) return false
      if (controls >= controlCapacity) {
        failCapacityLocked()
        accepted = false
        return@synchronized false
      }
      if (owner.active) {
        owner.retired = true
        queue.addLast(Entry.Terminal(Event.Terminal(owner, null)))
        controls++
        return@synchronized beginDrainLocked()
      }
      var lostItems = 0L
      var lostBytes = 0L
      val entries = queue.iterator()
      while (entries.hasNext()) {
        val entry = entries.next()
        if (entry.owner !== owner) continue
        entries.remove()
        when (entry) {
          is Entry.Value -> {
            lostItems = Math.addExact(lostItems, 1L)
            lostBytes = Math.addExact(lostBytes, entry.event.bytes.size.toLong())
            items--
            bytes -= entry.event.bytes.size.toLong()
          }
          is Entry.Loss -> {
            lostItems = Math.addExact(lostItems, entry.pending.items)
            lostBytes = Math.addExact(lostBytes, entry.pending.bytes)
          }
          is Entry.Terminal -> error("staged claim already has a terminal")
        }
      }
      owner.loss = null
      if (lostItems > 0) {
        val loss = PendingLoss(owner, lostItems, lostBytes)
        owner.loss = loss
        queue.addLast(Entry.Loss(loss))
      }
      owner.retired = true
      owner.active = true
      queue.addLast(Entry.Terminal(Event.Terminal(owner, null)))
      controls++
      beginDrainLocked()
    }
    notifyStop()
    if (start) drain()
    return accepted
  }

  private fun failCapacityLocked(): Stop {
    val (count, size) = uncertainLocked(inFlight)
    return Stop.ControlCapacity(count, size).also { stopped = it }
  }

  /** Includes undelivered loss records because those records have not reached the consumer. */
  private fun uncertainLocked(inFlight: Event?): Pair<Long, Long> {
    var count = 0L
    var size = 0L
    fun add(event: Event) {
      when (event) {
        is Event.Value -> { count = Math.addExact(count, 1); size = Math.addExact(size, event.bytes.size.toLong()) }
        is Event.Loss -> { count = Math.addExact(count, event.items); size = Math.addExact(size, event.bytes) }
        is Event.Terminal -> Unit
      }
    }
    if (inFlight != null) add(inFlight)
    for (entry in queue) when (entry) {
      is Entry.Value -> add(entry.event)
      is Entry.Loss -> add(Event.Loss(entry.pending.claim, entry.pending.items, entry.pending.bytes))
      is Entry.Terminal -> Unit
    }
    return count to size
  }

  private fun beginDrainLocked(): Boolean {
    if (stopped != null || draining || queue.peekFirst()?.owner?.active != true) return false
    draining = true
    return true
  }

  private fun drain() {
    while (true) {
      val event = synchronized(lock) {
        val entry = queue.peekFirst()
        if (stopped != null || entry == null || !entry.owner.active) {
          draining = false
          return
        }
        queue.removeFirst()
        val selected = when (entry) {
          is Entry.Value -> entry.event.also { items--; bytes -= it.bytes.size.toLong() }
          is Entry.Loss -> entry.pending.let {
            it.claim.loss = null
            Event.Loss(it.claim, it.items, it.bytes)
          }
          is Entry.Terminal -> entry.event
        }
        inFlight = selected
        selected
      }
      try {
        deliver(event)
      } catch (cause: Throwable) {
        val alreadyStopped = synchronized(lock) {
          val previous = stopped != null
          if (!previous) {
            val (count, size) = uncertainLocked(event)
            stopped = Stop.DeliveryUncertain(cause, count, size)
          }
          inFlight = null
          draining = false
          previous
        }
        // A reentrant stop handler may have thrown from inside deliver; preserve that error.
        if (alreadyStopped) throw cause
        notifyStop()
        return
      }
      synchronized(lock) {
        inFlight = null
        if (event is Event.Terminal) {
          controls--
          if (current === event.claim) current = null
        }
      }
    }
  }
}
