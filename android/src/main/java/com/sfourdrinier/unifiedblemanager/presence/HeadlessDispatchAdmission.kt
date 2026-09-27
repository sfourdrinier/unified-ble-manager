package com.sfourdrinier.unifiedblemanager.presence

import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** One startup receipt. Expired admission cannot launch a late JavaScript task. */
internal class HeadlessDispatchAdmission {
  private val settled = CountDownLatch(1)
  private var outcome: ContinuationOutcome? = null

  @Synchronized fun dispatch(action: () -> ContinuationOutcome): Boolean {
    if (outcome != null) return false
    outcome = try { action() } catch (error: RuntimeException) {
      continuationPlatformFailure(ContinuationStrategy.HEADLESS_TASK, error)
    }
    settled.countDown()
    return true
  }

  fun await(timeoutMs: Long): ContinuationOutcome {
    try { settled.await(timeoutMs, TimeUnit.MILLISECONDS) } catch (error: InterruptedException) {
      Thread.currentThread().interrupt()
      cancel("operation.aborted", "Headless task startup wait was interrupted")
    }
    synchronized(this) {
      if (outcome == null) cancel("operation.timed-out", "Headless task startup did not settle before its deadline")
      return checkNotNull(outcome)
    }
  }

  @Synchronized fun cancel(code: String, reason: String) {
    if (outcome != null) return
    outcome = ContinuationOutcome.Failed(ContinuationStrategy.HEADLESS_TASK, code, reason, null)
    settled.countDown()
  }
}
