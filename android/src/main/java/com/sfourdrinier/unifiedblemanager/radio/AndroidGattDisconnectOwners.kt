// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/AndroidGattDisconnectOwners.kt

package com.sfourdrinier.unifiedblemanager.radio

/**
 * One in-flight disconnect owner per concrete GATT generation of a device.
 *
 * Every disconnect request for a generation joins the same owner: the owner
 * holds the single close deadline and the ordered list of waiters, and whoever
 * retires the owner (native `STATE_DISCONNECTED`, the deadline, adapter loss,
 * a failed `connectGatt` or a failed native `disconnect()`) takes the waiters
 * out under the lock, so each waiter is completed exactly once.
 *
 * The owner is keyed by device but compares the GATT instance and generation,
 * so a deadline armed for one generation can never act on its replacement.
 * Scheduling itself stays with the radio: the injected scheduler has no cancel
 * handle, so [isCurrent] is the fence a timer must pass before it acts.
 */
internal class AndroidGattDisconnectOwners {
  /**
   * Result of [join]: [token] identifies the owner's deadline when [created].
   * [rejected] means the generation was no longer joinable: no owner was created
   * or changed and the waiter was not recorded, so the caller settles it itself.
   */
  internal class Admission(
    val created: Boolean,
    val token: Long,
    /** Waiters of an owner for a different generation that this owner replaced. */
    val superseded: List<(OwnedRadioTeardownFailure?) -> Unit>,
    val rejected: Boolean = false
  )

  private class Owner(
    val gatt: Any,
    val generation: Long,
    val token: Long,
    val waiters: MutableList<(OwnedRadioTeardownFailure?) -> Unit> = mutableListOf()
  )

  private val owners = HashMap<String, Owner>()
  private var nextToken = 1L

  /**
   * Joins the owner for ([gatt], [generation]) or creates it. Only the call that
   * creates the owner may arm the deadline and request the native disconnect.
   *
   * [joinable] is evaluated under the owner lock, atomically with the admission,
   * so a generation that the radio's own teardown has already closed (or whose
   * close is retained) can never gain an owner nothing would retire. It must only
   * read radio state and must not block or call back into this class.
   */
  @Synchronized
  fun join(
    key: String,
    gatt: Any,
    generation: Long,
    waiter: ((OwnedRadioTeardownFailure?) -> Unit)?,
    joinable: () -> Boolean = { true }
  ): Admission {
    if (!joinable()) {
      return Admission(created = false, token = 0L, superseded = emptyList(), rejected = true)
    }
    val existing = owners[key]
    if (existing != null && existing.gatt === gatt && existing.generation == generation) {
      waiter?.let { existing.waiters.add(it) }
      return Admission(created = false, token = existing.token, superseded = emptyList())
    }
    val owner = Owner(gatt, generation, nextToken++)
    waiter?.let { owner.waiters.add(it) }
    owners[key] = owner
    return Admission(
      created = true,
      token = owner.token,
      superseded = existing?.waiters?.toList() ?: emptyList()
    )
  }

  /** Whether the deadline identified by [token] still belongs to the device's live owner. */
  @Synchronized
  fun isCurrent(key: String, token: Long): Boolean = owners[key]?.token == token

  /** Removes the owner of exactly ([gatt], [generation]) and returns its waiters. */
  @Synchronized
  fun retire(key: String, gatt: Any, generation: Long): List<(OwnedRadioTeardownFailure?) -> Unit> {
    val owner = owners[key] ?: return emptyList()
    if (owner.gatt !== gatt || owner.generation != generation) return emptyList()
    owners.remove(key)
    return owner.waiters.toList()
  }

  /** Drops every owner and waiter without completing them (manager shutdown). */
  @Synchronized
  fun discardAll() {
    owners.clear()
  }
}

/** [first] with [second] suppressed, or whichever is non-null: an error is never lost or replaced. */
internal fun mergeErrors(first: Throwable?, second: Throwable?): Throwable? {
  if (first == null) return second
  if (second != null && second !== first) first.addSuppressed(second)
  return first
}

/** Runs [step] and returns what it threw, so the caller can finish its cleanup before rethrowing. */
internal inline fun errorOf(step: () -> Unit): Throwable? =
  try {
    step()
    null
  } catch (throwable: Throwable) {
    throwable
  }

/** Runs every step even when one throws; returns the first error with the later ones suppressed. */
internal fun runEvery(vararg steps: () -> Unit): Throwable? {
  var held: Throwable? = null
  steps.forEach { step -> held = mergeErrors(held, errorOf(step)) }
  return held
}

/** Runs [action] for every element even when one throws, then rethrows the first error. */
internal inline fun <T> Iterable<T>.forEachSettled(action: (T) -> Unit) {
  var held: Throwable? = null
  forEach { element -> held = mergeErrors(held, errorOf { action(element) }) }
  held?.let { throw it }
}
