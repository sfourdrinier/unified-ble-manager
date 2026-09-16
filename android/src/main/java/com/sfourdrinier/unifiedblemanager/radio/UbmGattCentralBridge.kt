// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/UbmGattCentralBridge.kt

package com.sfourdrinier.unifiedblemanager.radio

import java.util.concurrent.Executor
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

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
 * drop). Enqueue/JNI failures surface as [PostResult.EnqueueFailed]; only
 * [PostResult.Queued] means the line reached the core queue. After
 * [releaseOnDestroy], posts are refused outright as [PostResult.Shutdown]
 * (never a misleading success for a line nothing will drain).
 *
 * The [hasBlePermissions] provider owns the API-level split: `BLUETOOTH_SCAN`/
 * `BLUETOOTH_CONNECT` exist only on API 31+ (this library's minSdk is 24),
 * so below 31 the provider must gate on `BLUETOOTH` (+ location where the
 * platform requires it) instead of probing for permissions that cannot exist.
 *
 * Lifecycle: call [releaseOnDestroy] from the owner's destroy path. It
 * enqueues the `release` line (real destroy transition, idempotent,
 * unconditional — teardown is not a radio op), performs a final drain on
 * the calling thread, stops the worker, and shuts down the owned executor.
 * The session handle itself is closed by the owner afterwards.
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
  private val workerShutdown = AtomicBoolean(false)
  private val drainInFlight = AtomicBoolean(false)

  /**
   * Monotonic count of lines enqueued via [postEvent] (incremented AFTER
   * the enqueue lands, BEFORE scheduling). Lets the worker loop until
   * quiet and re-check for a lost wakeup after clearing [drainInFlight].
   */
  private val postCount = AtomicLong(0)

  /** Result of [postEvent]: never throws. */
  sealed interface PostResult {
    /** The line reached the core queue; [depth] is the queue depth after. */
    data class Queued(val depth: Int) : PostResult

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
    if (workerShutdown.get()) {
      return PostResult.Shutdown
    }
    if (!hasBlePermissions()) {
      return PostResult.PermissionDenied()
    }
    val depth = try {
      enqueue(wire)
    } catch (th: Throwable) {
      return PostResult.EnqueueFailed(th.message ?: th.javaClass.simpleName)
    }
    // Count AFTER the enqueue lands: the worker's lost-wakeup re-check may
    // only observe counts whose lines are already queued.
    postCount.incrementAndGet()
    scheduleDrain()
    return PostResult.Queued(depth)
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
   */
  fun scheduleDrain() {
    if (workerShutdown.get()) return
    if (!drainInFlight.compareAndSet(false, true)) return
    try {
      executor.execute {
        var drained = 0L
        try {
          do {
            drained = postCount.get()
            drainNow()
          } while (postCount.get() != drained)
        } finally {
          drainInFlight.set(false)
          if (postCount.get() != drained) {
            scheduleDrain()
          }
        }
      }
    } catch (_: Throwable) {
      drainInFlight.set(false)
    }
  }

  /**
   * Shuts down the owned worker executor, if this bridge created one (an
   * injected [worker] stays the caller's to shut down). Idempotent; safe
   * to call after [releaseOnDestroy] (which already shuts down) or when
   * the bridge is abandoned without destroy.
   */
  fun shutdown() {
    workerShutdown.set(true)
    if (ownsExecutor) {
      (executor as? ExecutorService)?.shutdown()
    }
  }

  /**
   * Destroy path: enqueues `release` unconditionally (teardown is not a
   * radio op — a missing BLE permission must not skip the destroy
   * transition), runs a final synchronous drain, stops scheduling worker
   * drains, and shuts down the owned executor. The owner still closes the
   * native session handle afterwards.
   */
  fun releaseOnDestroy(): List<GattObservation> {
    workerShutdown.set(true)
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
  }
}
