// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/DeferredCoreShadow.kt

package com.sfourdrinier.unifiedblemanager.radio

import java.util.concurrent.Executor
import java.util.concurrent.ExecutorService

/**
 * Owns the shared-core shadow's lifecycle off the constructing thread (R02).
 *
 * Constructing [UbmGattCoreBinding] touches JNI (`open` + `revision`), and
 * the first touch also loads `libubm5_jni_echo.so` — file IO plus dynamic
 * linking. Doing that synchronously inside dispatcher construction blocks
 * the installing (JS) thread, and a transient failure then disabled the
 * shadow permanently with no recovery.
 *
 * This gate instead:
 * - runs the initial open on [opener] (null opens inline, for deterministic
 *   tests), so construction never blocks on native work;
 * - publishes the ready shadow via [current], retrying a missing or failed
 *   shadow on demand — a replaced loser is shut down (releasing its owned
 *   worker), never leaked;
 * - reports each distinct failure cause exactly once through [diagnose]
 *   (consecutive duplicates are dropped; recovery resets the latch).
 *
 * Locking: [gate] serializes publication and replacement, so at most one
 * open is ever in flight and a healthy shadow is never reopened. Native
 * work (loser shutdown) and diagnostics always run outside the lock.
 *
 * A provided [opener] is owned by the gate and shut down on [release].
 */
class DeferredCoreShadow(
  private val factory: () -> UbmGattCoreBinding?,
  private val diagnose: (code: String, detail: String) -> Unit,
  opener: Executor? = null
) {
  private val gate = Any()

  /** Published shadow; guarded by [gate]. */
  private var shadow: UbmGattCoreBinding? = null

  /** Last reported failure cause, for consecutive-duplicate suppression; guarded by [gate]. */
  private var lastDiagnosis: String? = null

  private val opener: Executor? = opener

  init {
    val task = Runnable {
      attempt().let { diagnosis ->
        if (diagnosis != null) diagnose("coreShadowUnavailable", diagnosis)
      }
    }
    if (opener == null) task.run() else opener.execute(task)
  }

  /**
   * The ready shadow, or null while unavailable. A missing or failed shadow
   * is retried on demand (one attempt per call, coalesced under the gate),
   * so transient failures recover on the next use instead of wedging.
   */
  fun current(): UbmGattCoreBinding? {
    synchronized(gate) {
      shadow?.let { if (it.openFailure == null) return it }
    }
    attempt().let { diagnosis ->
      if (diagnosis != null) diagnose("coreShadowUnavailable", diagnosis)
    }
    synchronized(gate) {
      return shadow?.takeIf { it.openFailure == null }
    }
  }

  /**
   * Releases the published shadow (if any) without opening a new one, and
   * shuts down the owned opener. A later [current] reopens lazily.
   */
  fun release() {
    val taken = synchronized(gate) {
      shadow.also {
        shadow = null
        lastDiagnosis = null
      }
    }
    taken?.release()
    (opener as? ExecutorService)?.shutdown()
  }

  /**
   * One open attempt: keeps a healthy shadow, otherwise replaces a missing
   * or failed one with a fresh factory result. Returns a diagnosis when the
   * shadow is still unavailable AND the cause changed since the last report.
   */
  private fun attempt(): String? {
    var loser: UbmGattCoreBinding? = null
    val diagnosis = synchronized(gate) {
      shadow?.let { existing ->
        if (existing.openFailure == null) {
          lastDiagnosis = null
          return null
        }
      }
      loser = shadow
      shadow = null
      try {
        val opened = factory()
        if (opened == null) {
          freshDiagnosisLocked("shadow factory returned no binding")
        } else {
          shadow = opened
          val failure = opened.openFailure
          if (failure == null) {
            // Recovered: no failure is outstanding, so the latch resets and
            // a later outage reports fresh.
            lastDiagnosis = null
            null
          } else {
            freshDiagnosisLocked(failure)
          }
        }
      } catch (th: Throwable) {
        freshDiagnosisLocked("shadow open failed: ${th.message ?: th.javaClass.simpleName}")
      }
    }
    // Outside the gate: never hold the lock across native teardown.
    loser?.shutdown()
    return diagnosis
  }

  /** Caller holds [gate]. Null when the cause matches the last report. */
  private fun freshDiagnosisLocked(cause: String): String? {
    if (cause == lastDiagnosis) return null
    lastDiagnosis = cause
    return cause
  }
}
