// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/RustCoreContinuationExecutorF242Test.kt

package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.FakeCore
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Finding 242 (P1): the Android `native` continuation wake connects but never
 * completes GATT discovery. Two wakes on the just-appeared peer
 * A0:9E:1A:E9:B9:3D failed while the same strap connects/discovers/streams
 * reliably in the foreground. The wake-context differences under test:
 *
 * - the wake connected `when-available` (background `autoConnect`) although
 *   Companion Device Manager only fires on an absent-to-present transition,
 *   so the peer is already there and the opportunistic wait buys nothing;
 *   the foreground uses `direct`;
 * - the wake gave `gatt.discover` no `budgetMs` and cut it with its own
 *   10 s latch, while the core would run it up to its 120 s liveness
 *   window: wake 2's `platform.failure: gatt.discover timed out after
 *   10000ms` is the executor's own latch, not the operation's answer. The
 *   foreground budgets the same discovery at 20 s.
 */
class RustCoreContinuationExecutorF242Test {
  private val peer = "A0:9E:1A:E9:B9:3D"
  private val hrService = "0000180d-0000-1000-8000-00805f9b34fb"
  private val hrMeasurement = "00002a37-0000-1000-8000-00805f9b34fb"

  private val fake = FakeCore()
  private val logs = mutableListOf<String>()

  /** Tight latch (500 ms) so a slow-but-healthy discovery proves the point fast. */
  private val executor = RustCoreContinuationExecutor(
    core = fake,
    wireRevision = "ubm-mobile-wire/1",
    log = { logs.add(it) },
    connectBudgetMs = 5_000L,
    opTimeoutMs = 500L
  )

  private fun declaration() = BackgroundContinuationDeclaration(
    strategy = ContinuationStrategy.NATIVE,
    peerId = null,
    resubscribe = listOf(ContinuationSelector(hrService, 1, hrMeasurement, 1)),
    headlessTaskName = null,
    foregroundService = null
  )

  private fun ok(valueJson: String) = "{\"ok\":true,\"value\":$valueJson}"

  private fun waitForInvoke(index: Int) {
    val deadline = System.currentTimeMillis() + 10_000L
    while (System.currentTimeMillis() < deadline) {
      if (fake.callbacks.size > index) return
      Thread.sleep(5)
    }
    throw AssertionError("invoke #$index was never issued")
  }

  @Test
  fun wakeConnectsDirectToTheJustAppearedPeer() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = Thread {
      waitForInvoke(0)
      fake.callbacks[0].onResult(ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"))
      waitForInvoke(1)
      fake.callbacks[1].onResult(ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"))
      waitForInvoke(2)
      fake.callbacks[2].onResult(ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}"))
    }
    answering.isDaemon = true
    answering.start()
    val outcome = executor.execute(peer, declaration())
    answering.join(15_000)
    assertEquals(ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1), outcome)
    val connectArgs = fake.invokes.first { it.second == "connection.connect" }.third
    assertTrue(
      "the wake fires on presence, so the peer is already there: connect direct, not when-available: $connectArgs",
      connectArgs.contains("\"intent\":\"direct\"")
    )
  }

  @Test
  fun wakeDiscoveryCarriesAForegroundScaleBudget() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = Thread {
      waitForInvoke(0)
      fake.callbacks[0].onResult(ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"))
      waitForInvoke(1)
      fake.callbacks[1].onResult(ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"))
      waitForInvoke(2)
      fake.callbacks[2].onResult(ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}"))
    }
    answering.isDaemon = true
    answering.start()
    executor.execute(peer, declaration())
    answering.join(15_000)
    val discoverArgs = fake.invokes.first { it.second == "gatt.discover" }.third
    assertTrue(
      "discovery must carry the budget the latch honors, or the latch can only contradict the core: $discoverArgs",
      discoverArgs.contains("\"budgetMs\":")
    )
  }

  @Test
  fun wakeSubscribeCarriesABudget() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = Thread {
      waitForInvoke(0)
      fake.callbacks[0].onResult(ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"))
      waitForInvoke(1)
      fake.callbacks[1].onResult(ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"))
      waitForInvoke(2)
      fake.callbacks[2].onResult(ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}"))
    }
    answering.isDaemon = true
    answering.start()
    executor.execute(peer, declaration())
    answering.join(15_000)
    val subscribeArgs = fake.invokes.first { it.second == "gatt.subscribe" }.third
    assertTrue(
      "subscribe must carry the budget the latch honors, or the latch can only contradict the core: $subscribeArgs",
      subscribeArgs.contains("\"budgetMs\":")
    )
  }

  @Test
  fun slowDiscoveryReportsTheCoresAnswerNotTheLatch() {
    fake.openRecord = { "{\"sessionId\":7,\"contractRevision\":\"c\",\"wireRevision\":\"ubm-mobile-wire/1\"}" }
    val answering = Thread {
      waitForInvoke(0)
      fake.callbacks[0].onResult(ok("{\"peerKey\":\"k\",\"connectionGeneration\":\"cg-1\"}"))
      waitForInvoke(1)
      // A healthy-but-slow discovery: answers well past the old 500 ms
      // latch, well inside a foreground-scale budget.
      Thread.sleep(1_500)
      fake.callbacks[1].onResult(ok("{\"connectionGeneration\":\"cg-1\",\"databaseGeneration\":\"db-1\",\"services\":[]}"))
      waitForInvoke(2)
      fake.callbacks[2].onResult(ok("{\"consumer\":\"ubm-continuation-0\",\"delivery\":\"notification\"}"))
    }
    answering.isDaemon = true
    answering.start()
    val outcome = executor.execute(peer, declaration())
    answering.join(30_000)
    assertEquals(
      "a discovery the core completes must complete the wake, not fail it with the latch: $outcome",
      ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1),
      outcome
    )
  }
}
