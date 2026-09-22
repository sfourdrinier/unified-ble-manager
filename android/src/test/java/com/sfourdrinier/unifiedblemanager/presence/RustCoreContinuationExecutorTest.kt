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

  private fun twoSelectorDeclaration() = declaration().copy(
    resubscribe = listOf(
      ContinuationSelector(hrService, 1, hrMeasurement, 1),
      ContinuationSelector(hrService, 1, "00002a38-0000-1000-8000-00805f9b34fb", 1)
    )
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
  fun connectsDirectDiscoversAndResubscribes() {
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
    // Finding 242: the wake fires on presence, so the peer is already
    // there — connect direct, like the foreground, not when-available.
    assertTrue(connectArgs.contains("\"intent\":\"direct\""))
    assertTrue(connectArgs.contains("\"admission\":1"))
    assertTrue(fake.invokes[1].third.contains("\"admission\":2"))
    val subscribeArgs = fake.invokes[2].third
    assertTrue(subscribeArgs.contains("\"consumer\":\"ubm-continuation-0\""))
    assertTrue(subscribeArgs.contains(hrService))
    assertTrue(subscribeArgs.contains(hrMeasurement))
    assertTrue(
      "declarations are public 1-based paths while the Rust mobile wire is 0-based: $subscribeArgs",
      subscribeArgs.contains("\"serviceOccurrence\":0") &&
        subscribeArgs.contains("\"characteristicOccurrence\":0")
    )
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
  fun aPartialSubscriptionFailurePinsTheConsumersThatCanAlreadyQueueValues() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = answerInvokes(
      listOf(
        ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"),
        ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"),
        ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}"),
        "{\"ok\":false,\"error\":{\"code\":\"gatt.not-found\",\"detail\":\"second selector absent\"}}"
      )
    )
    val outcome = executor.execute(peer, twoSelectorDeclaration())
    answering.join(10_000)
    assertEquals("gatt.not-found", (outcome as ContinuationOutcome.Failed).code)

    fake.drainAnswer =
      "{\"more\":false,\"records\":[{\"t\":\"value\",\"ordinal\":1," +
        "\"consumer\":\"ubm-continuation-0\",\"valueB64\":\"AEg=\",\"delivery\":\"notification\"}],\"controlLost\":0}"
    val disposing = Thread { answerAt(4, "{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[]}}") }
    disposing.isDaemon = true
    disposing.start()
    val claim = executor.claimAndDispose(256, 65536)
    disposing.join(10_000)

    assertEquals(1, claim.consumerCount)
    assertTrue(claim.batches.single().contains("\"consumer\":\"ubm-continuation-0\""))
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
  fun aClaimDuringExecutionKeepsTheSessionUntilTheRadioWorkCompletes() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    var outcome: ContinuationOutcome? = null
    val executing = Thread { outcome = executor.execute(peer, declaration()) }
    executing.isDaemon = true
    executing.start()
    val deadline = System.currentTimeMillis() + 10_000L
    while (fake.invokes.isEmpty() && System.currentTimeMillis() < deadline) Thread.sleep(5)
    assertTrue("the wake must have started its connect before claiming", fake.invokes.isNotEmpty())

    val claimWhileExecuting = executor.claimAndDispose(256, 65536)
    assertFalse("a claim must not dispose a session whose radio work is still running", claimWhileExecuting.disposed)
    assertTrue("the app must be told why it needs to claim again", claimWhileExecuting.disposeFailure!!.contains("executing"))
    assertTrue("a busy claim must not drain the in-flight session", fake.calls.none { it.startsWith("drain:") })
    assertEquals(listOf("connection.connect"), fake.invokes.map { it.second })

    val answering = answerInvokes(
      listOf(
        ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"),
        ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"),
        ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}")
      )
    )
    executing.join(10_000)
    answering.join(10_000)
    assertEquals(ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1), outcome)

    val disposing = Thread { answerAt(3, "{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[]}}") }
    disposing.isDaemon = true
    disposing.start()
    val laterClaim = executor.claimAndDispose(256, 65536)
    disposing.join(10_000)
    assertEquals(1, laterClaim.consumerCount)
    assertTrue("a later claim must dispose after execution leaves the session", laterClaim.disposed)
  }

  @Test
  fun anExecutionDuringClaimDoesNotStartRadioWorkOnTheClaimedSession() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val establishing = answerInvokes(
      listOf(
        ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"),
        ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"),
        ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}")
      )
    )
    assertEquals(ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1), executor.execute(peer, declaration()))
    establishing.join(10_000)

    var claim: ContinuationClaim? = null
    val claiming = Thread { claim = executor.claimAndDispose(256, 65536) }
    claiming.isDaemon = true
    claiming.start()
    val claimDeadline = System.currentTimeMillis() + 10_000L
    while (fake.callbacks.size < 4 && System.currentTimeMillis() < claimDeadline) Thread.sleep(5)
    assertEquals("the claim must reserve the session through dispose", "session.dispose", fake.invokes[3].second)

    var outcome: ContinuationOutcome? = null
    val executing = Thread { outcome = executor.execute(peer, declaration()) }
    executing.isDaemon = true
    executing.start()
    Thread.sleep(100)
    fake.callbacks[3].onResult("{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[]}}")

    val extraInvokeDeadline = System.currentTimeMillis() + 2_000L
    while (fake.callbacks.size < 5 && executing.isAlive && System.currentTimeMillis() < extraInvokeDeadline) Thread.sleep(5)
    if (fake.callbacks.size >= 5) {
      fake.callbacks[4].onResult("{\"ok\":false,\"error\":{\"code\":\"lifecycle.destroyed\",\"detail\":\"claim owns disposal\"}}")
    }
    claiming.join(10_000)
    executing.join(10_000)

    assertTrue(claim!!.disposed)
    assertEquals("a claimed session must refuse a competing execute", "lifecycle.invalid-state", (outcome as ContinuationOutcome.Failed).code)
    assertEquals("the competing execute must not begin another connect", 1, fake.invokes.count { it.second == "connection.connect" })
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
