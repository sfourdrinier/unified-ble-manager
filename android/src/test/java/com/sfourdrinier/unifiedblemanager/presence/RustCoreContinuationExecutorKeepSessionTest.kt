// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/RustCoreContinuationExecutorKeepSessionTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.FakeCore
import com.sfourdrinier.unifiedblemanager.rustcore.MobileCorePort
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Delegates everything to [inner] except a drain that throws on demand. */
private class DrainFailingCore(private val inner: FakeCore) : MobileCorePort by inner {
  var error: RuntimeException? = null

  override fun drain(sessionId: Long, maxItems: Int, maxBytes: Int): String {
    error?.let { throw it }
    return inner.drain(sessionId, maxItems, maxBytes)
  }
}

/**
 * The P1's siblings: every incomplete drain must KEEP the continuation
 * session for a follow-up claim instead of disposing the unread tail.
 * Each test proves the session id is retained (the next claim drains and
 * disposes) and the caller is told to claim again (`disposed: false`).
 */
class RustCoreContinuationExecutorKeepSessionTest {
  private val peer = "A0:9E:1A:E9:B9:3D"
  private val hrService = "0000180d-0000-1000-8000-00805f9b34fb"
  private val hrMeasurement = "00002a37-0000-1000-8000-00805f9b34fb"

  private val fake = FakeCore()
  private val core = DrainFailingCore(fake)
  private val logs = mutableListOf<String>()
  private val executor = RustCoreContinuationExecutor(
    core = core,
    wireRevision = "ubm-mobile-wire/1",
    log = { logs.add(it) },
    connectBudgetMs = 5_000L,
    opTimeoutMs = 5_000L
  )

  private fun declaration() = BackgroundContinuationDeclaration(
    strategy = ContinuationStrategy.NATIVE,
    peerId = null,
    resubscribe = listOf(ContinuationSelector(hrService, 1, hrMeasurement, 1)),
    headlessTaskName = null,
    foregroundService = null
  )

  private fun ok(valueJson: String) = "{\"ok\":true,\"value\":$valueJson}"

  private fun released() = "{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[]}}"

  /** Answers one issued invoke by index, waiting until the executor issues it. */
  private fun answerAt(index: Int, json: String) {
    val deadline = System.currentTimeMillis() + 10_000L
    while (System.currentTimeMillis() < deadline) {
      if (fake.callbacks.size > index) {
        fake.callbacks[index].onResult(json)
        return
      }
      Thread.sleep(5)
    }
    throw AssertionError("invoke #$index was never issued")
  }

  /** Answers every invoke the executor issues, in order, on a helper thread. */
  private fun answerInvokes(answers: List<String>): Thread {
    val thread = Thread {
      var answered = 0
      val deadline = System.currentTimeMillis() + 10_000L
      while (answered < answers.size && System.currentTimeMillis() < deadline) {
        if (fake.callbacks.size > answered) {
          fake.callbacks[answered].onResult(answers[answered])
          answered += 1
        } else {
          Thread.sleep(5)
        }
      }
    }
    thread.isDaemon = true
    thread.start()
    return thread
  }

  /** Runs one wake so a continuation session is alive for the claim. */
  private fun establishSession() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = answerInvokes(
      listOf(
        ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"),
        ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"),
        ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}")
      )
    )
    executor.execute(peer, declaration())
    answering.join(10_000)
  }

  /** Retries the claim after the drain recovers; the kept session must dispose. */
  private fun retryDisposes() {
    val retry = Thread { answerAt(3, released()) }
    retry.isDaemon = true
    retry.start()
    val second = executor.claimAndDispose(256, 65536)
    retry.join(10_000)
    assertTrue(second.disposed)
    assertEquals(1, fake.invokes.count { it.second == "session.dispose" })
  }

  @Test
  fun aDrainThrowKeepsTheSessionAndReportsTheFailure() {
    establishSession()
    core.error = RuntimeException("drain boom")
    val claim = executor.claimAndDispose(256, 65536)
    assertFalse("a drain throw must keep the session", claim.disposed)
    assertTrue(
      "a drain throw must report the drain reason: ${claim.disposeFailure}",
      claim.disposeFailure!!.contains("continuation drain failed")
    )
    assertTrue(logs.any { it.contains("continuation drain failed") })
    assertEquals(0, fake.invokes.count { it.second == "session.dispose" })
    core.error = null
    fake.drainAnswer = "{\"more\":false,\"records\":[]}"
    retryDisposes()
  }

  @Test
  fun anUnparseableBatchKeepsTheSessionAndReportsTheFailure() {
    establishSession()
    fake.drainAnswer = "[[[not json"
    val claim = executor.claimAndDispose(256, 65536)
    assertFalse("an unparseable batch must keep the session", claim.disposed)
    assertTrue(
      "an unparseable batch must report the parse reason: ${claim.disposeFailure}",
      claim.disposeFailure!!.contains("unparseable")
    )
    assertTrue(logs.any { it.contains("unparseable") })
    assertEquals(listOf("[[[not json"), claim.batches)
    assertEquals(0, fake.invokes.count { it.second == "session.dispose" })
    fake.drainAnswer = "{\"more\":false,\"records\":[]}"
    retryDisposes()
  }

  @Test
  fun aFullBatchCapWithMoreQueuedKeepsTheSessionForAnotherClaim() {
    establishSession()
    fake.drainAnswer = "{\"more\":true,\"records\":[]}"
    val claim = executor.claimAndDispose(256, 65536, maxBatches = 3)
    assertFalse("a full batch cap with more queued must keep the session", claim.disposed)
    assertTrue(
      "a full batch cap must report the queued tail: ${claim.disposeFailure}",
      claim.disposeFailure!!.contains("more queued")
    )
    assertTrue(logs.any { it.contains("more queued") })
    assertEquals(3, claim.batches.size)
    assertEquals(0, fake.invokes.count { it.second == "session.dispose" })
    fake.drainAnswer = "{\"more\":false,\"records\":[]}"
    retryDisposes()
  }
}
