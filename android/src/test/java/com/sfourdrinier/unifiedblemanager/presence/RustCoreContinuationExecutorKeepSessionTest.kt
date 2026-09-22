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
  val scripted = ArrayDeque<String>()

  override fun invoke(sessionId: Long, op: String, argsJson: String, callback: com.ubm.core.MobileCoreBridge.InvokeCallback) {
    if (op == "session.quiesce") {
      inner.invokes.add(Triple(sessionId, op, argsJson))
      callback.onResult("{\"ok\":true,\"value\":{\"state\":\"sealed\",\"afterCutoffItems\":0,\"afterCutoffBytes\":0}}")
      return
    }
    inner.invoke(sessionId, op, argsJson, callback)
  }

  override fun drain(sessionId: Long, maxItems: Int, maxBytes: Int): String {
    error?.let { throw it }
    if (scripted.isNotEmpty()) return scripted.removeFirst()
    return inner.drain(sessionId, maxItems, maxBytes)
  }
}

/**
 * The continuation handoff owns the cutoff.  This seam answers the two
 * lifecycle operations synchronously so malformed drain tests prove whether
 * the executor authorized cleanup, without timing a callback race.
 */
private class SealingCore(private val inner: FakeCore) : MobileCorePort by inner {
  override fun invoke(sessionId: Long, op: String, argsJson: String, callback: com.ubm.core.MobileCoreBridge.InvokeCallback) {
    inner.invokes.add(Triple(sessionId, op, argsJson))
    when (op) {
      "session.quiesce" -> callback.onResult("{\"ok\":true,\"value\":{\"state\":\"sealed\",\"afterCutoffItems\":0,\"afterCutoffBytes\":0}}")
      "session.continuation-dispose" -> callback.onResult("{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[],\"afterCutoffItems\":0,\"afterCutoffBytes\":0}}")
      else -> inner.callbacks.add(callback)
    }
  }
}

/** Deterministic native boundary for the final-drain/cleanup interleaving. */
private class CutoffBoundaryCore(private val inner: FakeCore) : MobileCorePort by inner {
  val queued = mutableListOf<String>()
  var sealed = false
  var injectedAfterCutoff = false

  override fun invoke(sessionId: Long, op: String, argsJson: String, callback: com.ubm.core.MobileCoreBridge.InvokeCallback) {
    inner.invokes.add(Triple(sessionId, op, argsJson))
    when (op) {
      "session.quiesce" -> {
        sealed = true
        callback.onResult("{\"ok\":true,\"value\":{\"state\":\"sealed\",\"afterCutoffItems\":0,\"afterCutoffBytes\":0}}")
      }
      "session.continuation-dispose" -> {
        // This is the review's old race point. The native cutoff has already
        // happened, so the marker is loss-accounted rather than dropped.
        injectedAfterCutoff = true
        callback.onResult("{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[],\"afterCutoffItems\":1,\"afterCutoffBytes\":17}}")
      }
      else -> inner.callbacks.add(callback)
    }
  }

  override fun drain(sessionId: Long, maxItems: Int, maxBytes: Int): String {
    val records = queued.toList()
    queued.clear()
    return "{\"more\":false,\"records\":${records.joinToString(prefix = "[", postfix = "]")},\"controlLost\":0}"
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

  private fun released() = "{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[],\"afterCutoffItems\":0,\"afterCutoffBytes\":0}}"

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
    assertEquals(1, fake.invokes.count { it.second == "session.continuation-dispose" })
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
    fake.drainAnswer = "{\"more\":false,\"records\":[],\"controlLost\":0}"
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
    assertEquals(emptyList<String>(), claim.batches)
    assertEquals(0, fake.invokes.count { it.second == "session.dispose" })
    fake.drainAnswer = "{\"more\":false,\"records\":[],\"controlLost\":0}"
    retryDisposes()
  }

  @Test
  fun aFullBatchCapWithMoreQueuedKeepsTheSessionForAnotherClaim() {
    establishSession()
    fake.drainAnswer = "{\"more\":true,\"records\":[],\"controlLost\":0}"
    val claim = executor.claimAndDispose(256, 65536, maxBatches = 3)
    assertFalse("a full batch cap with more queued must keep the session", claim.disposed)
    assertTrue(
      "a full batch cap must report the queued tail: ${claim.disposeFailure}",
      claim.disposeFailure!!.contains("more queued")
    )
    assertTrue(logs.any { it.contains("more queued") })
    assertEquals(3, claim.batches.size)
    assertEquals(0, fake.invokes.count { it.second == "session.dispose" })
    fake.drainAnswer = "{\"more\":false,\"records\":[],\"controlLost\":0}"
    retryDisposes()
  }

  @Test
  fun malformedDrainCompletionNeverAuthorizesDisposal() {
    val malformed = listOf(
      "[]",
      "{\"records\":[]}",
      "{\"more\":\"not-a-boolean\",\"records\":[]}",
      "{\"more\":false,\"records\":{}}"
    )
    malformed.forEach { batch ->
      val raw = FakeCore()
      val sealing = SealingCore(raw)
      val local = RustCoreContinuationExecutor(
        core = sealing,
        wireRevision = "ubm-mobile-wire/1",
        log = {},
        connectBudgetMs = 5_000L,
        opTimeoutMs = 5_000L
      )
      raw.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
      val answering = Thread {
        val answers = listOf(
          ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"),
          ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"),
          ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}")
        )
        answers.indices.forEach { index ->
          while (raw.callbacks.size <= index) Thread.yield()
          raw.callbacks[index].onResult(answers[index])
        }
      }
      answering.start()
      assertTrue(local.execute(peer, declaration()) is ContinuationOutcome.Completed)
      answering.join(5_000)
      raw.drainAnswer = batch

      val claim = local.claimAndDispose(256, 65536)

      assertFalse("malformed batch $batch must keep the session", claim.disposed)
      assertTrue("malformed batch $batch must report protocol failure", claim.disposeFailure!!.contains("malformed"))
      assertEquals(
        "malformed batch $batch must not authorize cleanup",
        0,
        raw.invokes.count { it.second == "session.continuation-dispose" }
      )
      assertEquals(1, raw.invokes.count { it.second == "session.quiesce" })
    }
  }

  @Test
  fun aMalformedLaterBatchReturnsOnlyTheValidatedPrefixAndKeepsTheClaim() {
    establishSession()
    core.scripted.add("{\"more\":true,\"records\":[{\"t\":\"value\",\"ordinal\":1,\"consumer\":\"ubm-continuation-0\",\"valueB64\":\"AQ==\",\"delivery\":\"notification\"}],\"controlLost\":0}")
    core.scripted.add("{\"more\":\"bad\",\"records\":[],\"controlLost\":0}")

    val claim = executor.claimAndDispose(256, 65536)

    assertFalse(claim.disposed)
    assertEquals(1, claim.batches.size)
    assertTrue(claim.batches.single().contains("\"ordinal\":1"))
    assertTrue(claim.disposeFailure!!.contains("malformed"))
    assertEquals(0, fake.invokes.count { it.second == "session.continuation-dispose" })
  }

  @Test
  fun acknowledgingAnIncompletePrefixAdvancesToTheTailWithoutDisposing() {
    establishSession()
    core.scripted.add("{\"more\":true,\"records\":[{\"t\":\"value\",\"ordinal\":1,\"consumer\":\"ubm-continuation-0\",\"valueB64\":\"AQ==\",\"delivery\":\"notification\"}],\"controlLost\":0}")
    core.scripted.add("{\"more\":\"bad\",\"records\":[],\"controlLost\":0}")
    core.scripted.add("{\"more\":false,\"records\":[{\"t\":\"value\",\"ordinal\":3,\"consumer\":\"ubm-continuation-0\",\"valueB64\":\"Aw==\",\"delivery\":\"notification\"}],\"controlLost\":0}")

    val prefix = executor.prepareClaim(256, 65536)

    assertTrue(prefix.claimToken.isNotEmpty())
    assertEquals(1, prefix.batches.size)
    assertTrue(prefix.disposeFailure!!.contains("malformed"))
    val advanced = executor.acknowledgeClaim(prefix.claimToken)
    assertFalse(advanced.disposed)
    assertEquals(prefix.disposeFailure, advanced.disposeFailure)
    assertEquals(0, fake.invokes.count { it.second == "session.continuation-dispose" })

    val tail = executor.prepareClaim(256, 65536)

    assertTrue(tail.claimToken.isNotEmpty())
    assertTrue(tail.claimToken != prefix.claimToken)
    assertEquals(1, tail.batches.size)
    assertTrue(tail.batches.single().contains("\"ordinal\":3"))
    val releasing = Thread { answerAt(3, released()) }
    releasing.start()
    val disposed = executor.acknowledgeClaim(tail.claimToken)
    releasing.join(10_000)
    assertTrue(disposed.disposed)
  }

  @Test
  fun cutoffReturnsEveryPreCutoffRecordAndAccountsForTheFinalCleanupInterleaving() {
    val raw = FakeCore()
    val cutoff = CutoffBoundaryCore(raw)
    val local = RustCoreContinuationExecutor(
      core = cutoff,
      wireRevision = "ubm-mobile-wire/1",
      log = {},
      connectBudgetMs = 5_000L,
      opTimeoutMs = 5_000L
    )
    raw.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = Thread {
      val answers = listOf(
        ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"),
        ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"),
        ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}")
      )
      answers.indices.forEach { index ->
        while (raw.callbacks.size <= index) Thread.yield()
        raw.callbacks[index].onResult(answers[index])
      }
    }
    answering.start()
    assertTrue(local.execute(peer, declaration()) is ContinuationOutcome.Completed)
    answering.join(5_000)
    cutoff.queued += "{\"t\":\"value\",\"ordinal\":1,\"consumer\":\"ubm-continuation-0\",\"valueB64\":\"AQ==\",\"delivery\":\"notification\"}"

    val claim = local.claimAndDispose(256, 65536)

    assertTrue(claim.disposed)
    assertTrue(cutoff.sealed)
    assertTrue(cutoff.injectedAfterCutoff)
    assertEquals(1, claim.batches.size)
    assertTrue(claim.batches.single().contains("\"ordinal\":1"))
    assertEquals(CutoffLoss(1, 17), claim.afterCutoffLoss)
  }
}
