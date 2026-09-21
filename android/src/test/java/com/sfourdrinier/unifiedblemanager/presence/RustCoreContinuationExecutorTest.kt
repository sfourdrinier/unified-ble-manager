// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/RustCoreContinuationExecutorTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.FakeCore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** The wake reconnects through the Rust core with no JavaScript (BGS4). */
class RustCoreContinuationExecutorTest {
  private val peer = "A0:9E:1A:E9:B9:3D"
  private val hrService = "0000180d-0000-1000-8000-00805f9b34fb"
  private val hrMeasurement = "00002a37-0000-1000-8000-00805f9b34fb"

  private val fake = FakeCore()
  private val logs = mutableListOf<String>()
  private val executor = RustCoreContinuationExecutor(
    core = fake,
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

  @Test
  fun connectsWhenAvailableDiscoversAndResubscribes() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = answerInvokes(
      listOf(
        ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"),
        ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"),
        ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}")
      )
    )
    val outcome = executor.execute(peer, declaration())
    answering.join(10_000)
    assertEquals(ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1), outcome)
    assertEquals(
      listOf("connection.connect", "gatt.discover", "gatt.subscribe"),
      fake.invokes.map { it.second }
    )
    val connectArgs = fake.invokes[0].third
    assertTrue(connectArgs.contains("\"peerId\":\"$peer\""))
    assertTrue(connectArgs.contains("\"intent\":\"when-available\""))
    assertTrue(connectArgs.contains("\"admission\":1"))
    assertTrue(fake.invokes[1].third.contains("\"admission\":2"))
    val subscribeArgs = fake.invokes[2].third
    assertTrue(subscribeArgs.contains("\"consumer\":\"ubm-continuation-0\""))
    assertTrue(subscribeArgs.contains(hrService))
    assertTrue(subscribeArgs.contains(hrMeasurement))
    assertTrue(fake.openScopes.contains(RustCoreContinuationExecutor.CONTINUATION_SCOPE))
  }

  @Test
  fun aRefusedConnectIsATypedOutcomeWithThePlatformReason() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = answerInvokes(
      listOf(
        "{\"ok\":false,\"error\":{\"code\":\"connection.failed\",\"domain\":\"connection\"," +
          "\"operation\":\"connection.connect\",\"detail\":\"GATT 133\"," +
          "\"platform\":{\"domain\":\"android\",\"code\":\"connectionFailed\",\"message\":\"m\",\"metadata\":{}}," +
          "\"commit\":null,\"retryability\":\"caller-decides\"}}"
      )
    )
    val outcome = executor.execute(peer, declaration())
    answering.join(10_000)
    val failed = outcome as ContinuationOutcome.Failed
    assertEquals("connection.failed", failed.code)
    assertTrue(failed.platform!!.contains("connectionFailed"))
    // Nothing after the refusal: no discover, no subscribe.
    assertEquals(listOf("connection.connect"), fake.invokes.map { it.second })
  }

  @Test
  fun aSecondAppearanceWhileHeldIsAlreadyContinuing() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = answerInvokes(
      listOf(
        ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"),
        ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"),
        ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}")
      )
    )
    assertEquals(
      ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1),
      executor.execute(peer, declaration())
    )
    answering.join(10_000)
    val invokesAfterFirst = fake.invokes.size
    assertEquals(
      ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1),
      executor.execute(peer, declaration())
    )
    assertEquals(invokesAfterFirst, fake.invokes.size)
  }

  @Test
  fun aDisposeReportingReleaseFailedKeepsTheSessionAndReportsTheFailure() {
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
    fake.drainAnswer = "{\"more\":false,\"records\":[],\"controlLost\":0}"
    val disposing = Thread { answerAt(3, "{\"ok\":true,\"value\":{\"state\":\"release-failed\",\"failures\":[{\"resourceKind\":\"connection\",\"code\":\"connection.failed\"}]}}") }
    disposing.isDaemon = true
    disposing.start()
    val claim = executor.claimAndDispose(256, 65536)
    disposing.join(10_000)
    // The Rust side keeps failed leases and subscriptions for retry and only
    // removes the session when clean: a release-failed dispose must NOT clear
    // the id, and the failure must reach the app instead of reading disposed.
    assertFalse(claim.disposed)
    assertTrue(logs.any { it.contains("release-failed") })
    // The kept session retries the dispose on the next claim.
    val retry = Thread { answerAt(4, "{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[]}}") }
    retry.isDaemon = true
    retry.start()
    val second = executor.claimAndDispose(256, 65536)
    retry.join(10_000)
    assertEquals(true, second.disposed)
    assertEquals(2, fake.invokes.count { it.second == "session.dispose" })
  }

  @Test
  fun stateReadsDoNotWaitForRadioIO() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    // The connect invoke is never answered: execute stays inside radio I/O.
    val slow = Thread { executor.execute(peer, declaration()) }
    slow.isDaemon = true
    slow.start()
    val deadline = System.currentTimeMillis() + 10_000L
    while (fake.invokes.isEmpty() && System.currentTimeMillis() < deadline) Thread.sleep(5)
    assertTrue(fake.invokes.isNotEmpty())
    // A backlog read must not queue behind the held radio I/O.
    var backlog: BacklogCounts? = null
    val done = CountDownLatch(1)
    val reading = Thread {
      backlog = executor.describeBacklog()
      done.countDown()
    }
    reading.isDaemon = true
    reading.start()
    val answeringDescribe = Thread {
      try {
        answerAt(1, ok("{\"counters\":{},\"process\":{}}"))
      } catch (_: AssertionError) {
      }
    }
    answeringDescribe.isDaemon = true
    answeringDescribe.start()
    assertTrue(done.await(5, TimeUnit.SECONDS))
    assertTrue(backlog != null)
  }

  @Test
  fun aSecondPeerWhileHeldIsRefusedTruthfully() {
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
    val outcome = executor.execute("11:22:33:44:55:66", declaration()) as ContinuationOutcome.Failed
    assertEquals("lifecycle.invalid-state", outcome.code)
  }
}
