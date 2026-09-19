// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/UbmGattCentralBridge.kt

package com.sfourdrinier.unifiedblemanager.radio

import java.util.concurrent.Executor
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.locks.ReentrantLock

/**
 * HOST-ANDROID bridge runtime (UBM 5.0, trackourhealth/bun-mono#1188).
 *
 * Binder threads (GATT/scan callbacks) must never drive the core or wait
 * on radio I/O: [postEvent] only runs the injected permission gate plus
 * one enqueue call and returns (the enqueue takes the session mutex, which
 * a worker drain holds for its run — no I/O waits, only a short in-memory
 * critical section), while a single-thread worker applies the
 * queue through [drainNow] (see [scheduleDrain]). The JNI functions are
 * injected so this class carries no `android.*` dependency and stays
 * HOST-JVM unit-testable; the probe app wires the real
 * `com.ubm.gatt.GattBridge` natives, checking permissions with
 * `ContextCompat.checkSelfPermission` for `BLUETOOTH_SCAN`/`BLUETOOTH_CONNECT`.
 *
 * Fail-closed contract: without BLE permission the event is NOT enqueued and
 * [PostResult.PermissionDenied] carries the explicit
 * `permission.denied|rn-android-boundary` identity (never an empty or silent
 * drop). A throwing permission provider is the same fail-closed refusal
 * with the provider-failed identity — [postEvent] never throws.
 * Enqueue/JNI failures surface as [PostResult.EnqueueFailed]; only
 * [PostResult.Queued] means the line reached the core queue AND a drain is
 * arranged for it. After [releaseOnDestroy], posts are refused outright as
 * [PostResult.Shutdown] (never a misleading success for a line nothing will
 * drain).
 *
 * Lifecycle (F21): admission, closing, and scheduling linearize on one
 * lifecycle lock. A post admitted before close is always drained — by the
 * worker, or by the destroy path's final drain when it races the close —
 * so no accepted line is ever stranded as [PostResult.Queued]. When the
 * worker executor rejects the drain task while the bridge is still open,
 * the post reports [PostResult.ScheduleFailed] (the line is queued, but no
 * drain was scheduled); a later post or the destroy drain still picks the
 * line up, so callers must tolerate redelivery rather than blindly
 * reposting state-changing lines. [releaseOnDestroy] joins a running
 * worker drain before its final drain, so no observation is delivered
 * after it returns and the owner may then close the native session.
 *
 * The [hasBlePermissions] provider owns the API-level split: `BLUETOOTH_SCAN`/
 * `BLUETOOTH_CONNECT` exist only on API 31+ (this library's minSdk is 24),
 * so below 31 the provider must gate on `BLUETOOTH` (+ location where the
 * platform requires it) instead of probing for permissions that cannot exist.
 *
 * Lifecycle: call [releaseOnDestroy] from the owner's destroy path. It
 * closes admission, joins the worker, enqueues the `release` line (real
 * destroy transition, idempotent, unconditional — teardown is not a radio
 * op), performs a final drain on the calling thread, and shuts down the
 * owned executor. The session handle itself is closed by the owner
 * afterwards. [releaseOnDestroy] blocks until a running drain finishes —
 * call it from the destroy path, never from a binder thread.
 */
class UbmGattCentralBridge(
  private val enqueue: (String) -> Int,
  private val drain: () -> String,
  private val hasBlePermissions: () -> Boolean,
  private val onObservations: (List<GattObservation>) -> Unit = {},
  worker: Executor? = null
) {
  private val executor: Executor =
    worker ?: Executors.newSingleThreadExecutor { runnable ->
      Thread(runnable, "ubm-gatt-drain").apply { isDaemon = true }
    }
  private val ownsExecutor = worker == null

  /**
   * Lifecycle lock (F21): admission ([postEvent]), closing ([shutdown],
   * [releaseOnDestroy]), and the worker-completion barrier linearize here.
   * Held only across in-memory decisions plus the short session-mutex
   * enqueue — never across a drain or JNI wait.
   */
  private val lifecycleLock = ReentrantLock()
  private val workerIdle = lifecycleLock.newCondition()

  /** Set under [lifecycleLock]: no post is admitted once closed. */
  private var closed = false
  private val drainInFlight = AtomicBoolean(false)

  /**
   * Monotonic count of lines enqueued via [postEvent] (incremented AFTER
   * the enqueue lands, BEFORE scheduling). Lets the worker loop until
   * quiet and re-check for a lost wakeup after clearing [drainInFlight].
   */
  private val postCount = AtomicLong(0)

  /** Result of [postEvent]: never throws. */
  sealed interface PostResult {
    /** The line reached the core queue and a drain is arranged for it. */
    data class Queued(val depth: Int) : PostResult

    /**
     * The line reached the core queue (queue [depth] after) but no worker
     * drain could be scheduled ([message] names the rejection). A later
     * post or the destroy drain still picks the line up — tolerate
     * redelivery rather than blindly reposting state-changing lines.
     */
    data class ScheduleFailed(val depth: Int, val message: String) : PostResult

    /** Refused before JNI: BLE permission missing (fail-closed identity). */
    data class PermissionDenied(val identity: String = PERMISSION_DENIED_IDENTITY) : PostResult

    /** The enqueue call itself failed; the line did not reach the core. */
    data class EnqueueFailed(val message: String) : PostResult

    /** Refused: the bridge is released; nothing would ever drain the line. */
    object Shutdown : PostResult
  }

  /**
   * Posts one [GattCentralWire] line from any thread (binder-safe): runs the
   * permission gate, enqueues, and schedules a worker drain. Never drives
   * the core, never waits on I/O, and never throws.
   */
  fun postEvent(wire: String): PostResult {
    lifecycleLock.lock()
    try {
      if (closed) {
        return PostResult.Shutdown
      }
    } finally {
      lifecycleLock.unlock()
    }
    val permitted = try {
      hasBlePermissions()
    } catch (th: Throwable) {
      return PostResult.PermissionDenied(PERMISSION_PROVIDER_FAILED_IDENTITY)
    }
    if (!permitted) {
      return PostResult.PermissionDenied()
    }
    val depth: Int
    lifecycleLock.lock()
    try {
      if (closed) {
        return PostResult.Shutdown
      }
      depth = try {
        enqueue(wire)
      } catch (th: Throwable) {
        return PostResult.EnqueueFailed(th.message ?: th.javaClass.simpleName)
      }
      // Count AFTER the enqueue lands, under the same lock that closes
      // admission: the destroy path's final drain is ordered after every
      // admitted line, so no accepted line can miss it.
      postCount.incrementAndGet()
    } finally {
      lifecycleLock.unlock()
    }
    if (scheduleDrain()) {
      return PostResult.Queued(depth)
    }
    // Scheduling failed. When a destroy raced us past the close, the
    // destroy path's final drain is already ordered after our line, so
    // Queued stays truthful; otherwise the line is queued but no drain
    // is arranged for it, and the caller must hear that explicitly.
    lifecycleLock.lock()
    try {
      if (closed) {
        return PostResult.Queued(depth)
      }
    } finally {
      lifecycleLock.unlock()
    }
    return PostResult.ScheduleFailed(depth, "drain-schedule-rejected")
  }

  /**
   * Applies every queued line FIFO on the calling thread and forwards parsed
   * observations to [onObservations]. Call from the worker (see
   * [scheduleDrain]) or synchronously in tests — never from a binder thread.
   */
  fun drainNow(): List<GattObservation> {
    val observations = try {
      GattCentralWire.parseObservations(drain())
    } catch (th: Throwable) {
      listOf(
        GattObservation(
          ok = false,
          event = "gatt-drain",
          code = "platform.failure",
          domain = "platform",
          operation = "gatt-drain",
          detail = "drain-call-failed",
          raw = th.message ?: th.javaClass.simpleName
        )
      )
    }
    try {
      onObservations(observations)
    } catch (_: Throwable) {
      // Observation consumers must never break the drain loop.
    }
    return observations
  }

  /**
   * Schedules one worker [drainNow]; coalesces while a drain is in flight.
   * The worker loops until no new posts arrived mid-drain, then re-checks
   * once after clearing the flag: a post that landed between the last
   * drain and the clear lost its schedule (CAS failed while busy), so the
   * re-check reschedules for it. No wakeup is lost: every post either
   * schedules itself or is covered by the re-check, and the count only
   * advances for lines already queued.
   *
   * Returns false when no drain was arranged — the bridge is closed (the
   * destroy path's final drain covers admitted lines instead) or the
   * executor rejected the task (the caller must surface that explicitly;
   * a silent drop would strand the line as a false Queued).
   */
  fun scheduleDrain(): Boolean {
    lifecycleLock.lock()
    try {
      if (closed) return false
    } finally {
      lifecycleLock.unlock()
    }
    if (!drainInFlight.compareAndSet(false, true)) return true
    try {
      executor.execute {
        var drained = 0L
        try {
          do {
            drained = postCount.get()
            drainNow()
          } while (postCount.get() != drained)
        } finally {
          lifecycleLock.lock()
          try {
            drainInFlight.set(false)
            workerIdle.signalAll()
          } finally {
            lifecycleLock.unlock()
          }
          if (postCount.get() != drained) {
            scheduleDrain()
          }
        }
      }
    } catch (_: Throwable) {
      lifecycleLock.lock()
      try {
        drainInFlight.set(false)
        workerIdle.signalAll()
      } finally {
        lifecycleLock.unlock()
      }
      return false
    }
    return true
  }

  /**
   * Closes admission and shuts down the owned worker executor, if this
   * bridge created one (an injected [worker] stays the caller's to shut
   * down). Idempotent; safe to call after [releaseOnDestroy] (which already
   * shuts down) or when the bridge is abandoned without destroy. Unlike
   * [releaseOnDestroy], this does not join a running drain: the destroy
   * path owns the close barrier.
   */
  fun shutdown() {
    lifecycleLock.lock()
    try {
      closed = true
    } finally {
      lifecycleLock.unlock()
    }
    if (ownsExecutor) {
      (executor as? ExecutorService)?.shutdown()
    }
  }

  /**
   * Destroy path: closes admission, joins a running worker drain, enqueues
   * `release` unconditionally (teardown is not a radio op — a missing BLE
   * permission must not skip the destroy transition), runs a final
   * synchronous drain, and shuts down the owned executor. When it returns,
   * no observation will ever be delivered again, so the owner may close
   * the native session handle afterwards. Blocks until a running drain
   * finishes — call from the destroy path, never from a binder thread.
   */
  fun releaseOnDestroy(): List<GattObservation> {
    // Close admission first: every line admitted before this point is
    // already queued, so the final drain below cannot miss one.
    lifecycleLock.lock()
    try {
      closed = true
      // Join the worker: the owner closes the native session after we
      // return, so no drain may still be running then. The interrupt
      // status is preserved but the barrier still completes — a partial
      // join would let callbacks race the native close.
      var interrupted = false
      while (drainInFlight.get()) {
        try {
          workerIdle.await()
        } catch (_: InterruptedException) {
          interrupted = true
        }
      }
      if (interrupted) {
        Thread.currentThread().interrupt()
      }
    } finally {
      lifecycleLock.unlock()
    }
    try {
      enqueue(GattCentralWire.release())
    } catch (th: Throwable) {
      shutdown()
      return listOf(
        GattObservation(
          ok = false,
          event = "release",
          code = "platform.failure",
          domain = "platform",
          operation = "releaseOnDestroy",
          detail = "release-enqueue-failed",
          raw = th.message ?: th.javaClass.simpleName
        )
      )
    }
    val observations = drainNow()
    shutdown()
    return observations
  }

  companion object {
    /** Fail-closed identity for permission-gated refusals. */
    const val PERMISSION_DENIED_IDENTITY =
      "permission.denied|rn-android-boundary|postEvent|ble-permission-missing"

    /** Fail-closed identity when the permission provider itself throws. */
    const val PERMISSION_PROVIDER_FAILED_IDENTITY =
      "permission.denied|rn-android-boundary|postEvent|ble-permission-provider-failed"
  }
}
