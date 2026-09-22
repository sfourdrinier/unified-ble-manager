// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/PresenceWakeContinuationTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** The wake executes the declared standing order — and only that order (BGS4). */
class PresenceWakeContinuationTest {
  private val peer = "AA:BB:CC:DD:EE:FF"
  private val other = "11:22:33:44:55:66"
  private val hrService = "0000180d-0000-1000-8000-00805f9b34fb"
  private val hrMeasurement = "00002a37-0000-1000-8000-00805f9b34fb"

  private val store = InMemoryPresenceStore()
  private val outcomes = mutableListOf<ContinuationWakeRecord>()
  private val executed = mutableListOf<Pair<String, BackgroundContinuationDeclaration>>()
  private var executorAnswer: ContinuationOutcome = ContinuationOutcome.completed(
    ContinuationStrategy.NATIVE,
    peer,
    resubscribed = 1
  )
  private var declaration: BackgroundContinuationDeclaration = BackgroundContinuationDeclaration.recordOnly()

  private fun coordinator() = PresenceWakeCoordinator(
    associatedAddresses = { setOf(peer, other) },
    store = store,
    nowMs = { 12345L },
    ensureOwner = { true },
    ingest = { true },
    log = {},
    continuation = { declaration },
    executeContinuation = { address, declared ->
      executed.add(address to declared)
      executorAnswer
    },
    recordWakeOutcome = { outcomes.add(it) }
  )

  private fun nativeDeclaration(peerId: String?) = BackgroundContinuationDeclaration(
    strategy = ContinuationStrategy.NATIVE,
    peerId = peerId,
    resubscribe = listOf(ContinuationSelector(hrService, 1, hrMeasurement, 1)),
    headlessTaskName = null,
    foregroundService = null
  )

  @Test
  fun recordOnlyNeverExecutes() {
    declaration = BackgroundContinuationDeclaration.recordOnly()
    coordinator().appeared(peer, null)
    assertTrue(executed.isEmpty())
    assertTrue(outcomes.isEmpty())
  }

  @Test
  fun nativeExecutesForTheAppearedPeer() {
    declaration = nativeDeclaration(null)
    assertTrue(coordinator().appeared(peer, null))
    assertEquals(listOf(peer to declaration), executed)
    assertEquals(
      listOf(
        ContinuationWakeRecord(
          observedAtMs = 12345L,
          event = "continuation.completed",
          strategy = ContinuationStrategy.NATIVE,
          peerAddress = peer,
          code = null,
          reason = null
        )
      ),
      outcomes
    )
  }

  @Test
  fun nativeSkipsAnAppearanceOutsideTheDeclaredPeerButRecordsTheSkip() {
    declaration = nativeDeclaration(other)
    coordinator().appeared(peer, null)
    assertTrue(executed.isEmpty())
    // The wake happened and the order refused it: lastWake must show the
    // rejection, never stay null as if nothing woke.
    assertEquals(1, outcomes.size)
    assertEquals("continuation.failed", outcomes[0].event)
    assertEquals(ContinuationStrategy.NATIVE, outcomes[0].strategy)
    assertEquals(peer, outcomes[0].peerAddress)
    assertTrue(outcomes[0].code != null)
    assertTrue(outcomes[0].reason!!.contains(other))
  }

  @Test
  fun anUnassociatedAppearanceRecordsItsRejection() {
    declaration = nativeDeclaration(null)
    val stranger = "FF:FF:FF:FF:FF:FF"
    assertFalse(coordinator().appeared(stranger, null))
    assertTrue(executed.isEmpty())
    // "Never woken" (lastWake null) must stay distinguishable from "woken
    // and rejected": the rejection is recorded with its outcome.
    assertEquals(1, outcomes.size)
    assertEquals("continuation.failed", outcomes[0].event)
    assertEquals(ContinuationStrategy.NATIVE, outcomes[0].strategy)
    assertEquals(stranger, outcomes[0].peerAddress)
    assertEquals("association.unknown", outcomes[0].code)
    assertTrue(outcomes[0].reason != null)
  }

  @Test
  fun aDisappearanceDoesNotWaitForNativeContinuationIO() {
    declaration = nativeDeclaration(null)
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val blocking = PresenceWakeCoordinator(
      associatedAddresses = { setOf(peer, other) },
      store = store,
      nowMs = { 12345L },
      ensureOwner = { true },
      ingest = { true },
      log = {},
      continuation = { declaration },
      executeContinuation = { address, declared ->
        executed.add(address to declared)
        entered.countDown()
        release.await(15, TimeUnit.SECONDS)
        executorAnswer
      },
      recordWakeOutcome = { outcomes.add(it) }
    )
    val appearing = Thread { blocking.appeared(peer, null) }
    appearing.isDaemon = true
    appearing.start()
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    // Teardown state must not queue behind the held radio I/O.
    val disappearing = Thread { blocking.disappeared(peer, null) }
    disappearing.isDaemon = true
    disappearing.start()
    disappearing.join(5_000)
    assertFalse(disappearing.isAlive)
    release.countDown()
    appearing.join(5_000)
  }

  @Test
  fun nativeFailureIsRecordedWithItsReasonNeverSwallowed() {
    declaration = nativeDeclaration(null)
    executorAnswer = ContinuationOutcome.failed(
      ContinuationStrategy.NATIVE,
      code = "connection.failed",
      reason = "GATT 133",
      platform = "android:connectionFailed"
    )
    coordinator().appeared(peer, null)
    assertEquals(
      listOf(
        ContinuationWakeRecord(
          observedAtMs = 12345L,
          event = "continuation.failed",
          strategy = ContinuationStrategy.NATIVE,
          peerAddress = peer,
          code = "connection.failed",
          reason = "GATT 133"
        )
      ),
      outcomes
    )
  }

  @Test
  fun nativeWithNoLiveOwnerPersistsAndRecordsTheAbort() {
    declaration = nativeDeclaration(null)
    val dead = PresenceWakeCoordinator(
      associatedAddresses = { setOf(peer) },
      store = store,
      nowMs = { 12345L },
      ensureOwner = { false },
      ingest = { false },
      log = {},
      continuation = { declaration },
      executeContinuation = { address, declared ->
        executed.add(address to declared)
        executorAnswer
      },
      recordWakeOutcome = { outcomes.add(it) }
    )
    assertTrue(!dead.appeared(peer, null))
    assertTrue("no owner, nothing to execute through", executed.isEmpty())
    assertEquals(1, outcomes.size)
    assertEquals("continuation.failed", outcomes[0].event)
    assertEquals("operation.aborted", outcomes[0].code)
    assertTrue(store.drainAppearances().isNotEmpty())
  }

  @Test
  fun deferredStrategiesAnswerUnsupportedNeverFallBack() {
    for (strategy in listOf(ContinuationStrategy.HEADLESS_TASK, ContinuationStrategy.FOREGROUND_SERVICE)) {
      executed.clear()
      outcomes.clear()
      declaration = BackgroundContinuationDeclaration(
        strategy = strategy,
        peerId = null,
        resubscribe = emptyList(),
        headlessTaskName = if (strategy == ContinuationStrategy.HEADLESS_TASK) "BleWakeTask" else null,
        foregroundService = null
      )
      coordinator().appeared(peer, null)
      assertTrue("no executor for $strategy", executed.isEmpty())
      assertEquals(1, outcomes.size)
      assertEquals("continuation.failed", outcomes[0].event)
      assertEquals("capability.unsupported", outcomes[0].code)
      assertTrue(outcomes[0].reason!!.contains("not implemented in this release"))
    }
  }
}
