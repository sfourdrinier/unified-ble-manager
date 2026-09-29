// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/UbmCompanionPresenceServiceTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.companion.AssociationInfo
import android.companion.DevicePresenceEvent
import android.net.MacAddress
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.mockito.Mockito.mock
import org.mockito.Mockito.`when`
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * Finding 236: the OS may dispatch an appearance as `AssociationInfo`
 * (API 33+, with the real association id) or as a bare address. Both
 * overloads must reach the coordinator with the device address — otherwise
 * the wake never runs — and a second delivery of the same event must be
 * the coordinator's logged duplicate, not a second wake.
 */
class UbmCompanionPresenceServiceTest {
  private val peer = "A0:9E:1A:E9:B9:3D"
  private val store = InMemoryPresenceStore()
  private val ingested = mutableListOf<PresenceRestoredPeer>()
  @Volatile
  private var expectedIngestions = CountDownLatch(0)
  private lateinit var coordinator: PresenceWakeCoordinator

  @Before
  fun setUp() {
    coordinator = PresenceWakeCoordinator(
      associatedAddresses = { setOf(peer) },
      store = store,
      nowMs = { 12345L },
      ensureOwner = { true },
      ingest = { peers ->
        synchronized(ingested) { ingested.addAll(peers) }
        peers.forEach { expectedIngestions.countDown() }
        true
      },
      log = {}
    )
    UbmCompanionPresenceService.coordinatorOverride = coordinator
  }

  @After
  fun tearDown() {
    UbmCompanionPresenceService.observe(mock(android.content.Context::class.java), peer) {}
    UbmCompanionPresenceService.coordinatorOverride = null
  }

  private fun association(id: Int, mac: String?): AssociationInfo {
    val info = mock(AssociationInfo::class.java)
    `when`(info.id).thenReturn(id)
    if (mac == null) {
      `when`(info.deviceMacAddress).thenReturn(null)
    } else {
      val address = mock(MacAddress::class.java)
      `when`(address.toString()).thenReturn(mac)
      `when`(info.deviceMacAddress).thenReturn(address)
    }
    return info
  }

  private fun expectIngestions(count: Int) {
    expectedIngestions = CountDownLatch(count)
  }

  private fun awaitIngestions() {
    assertTrue("presence worker should deliver the admitted appearance", expectedIngestions.await(5, TimeUnit.SECONDS))
  }

  @Test
  fun modernSourcesReleaseOnlyTheLastSourceAndIgnoreLegacyDuplicates() {
    val releases = java.util.concurrent.atomic.AtomicInteger()
    coordinator = PresenceWakeCoordinator(
      associatedAddresses = { setOf(peer) }, store = store, nowMs = { 1L },
      ensureOwner = { true }, ingest = { peers ->
        synchronized(ingested) { ingested.addAll(peers) }
        expectedIngestions.countDown(); true
      }, log = {}, releaseContinuation = { releases.incrementAndGet(); null }
    )
    UbmCompanionPresenceService.coordinatorOverride = coordinator
    val service = object : UbmCompanionPresenceService() {
      override fun presenceSdkInt() = 36
      override fun associationForPresence(id: Int) = if (id == 4) association(4, peer) else null
    }
    fun event(kind: Int): DevicePresenceEvent = mock(DevicePresenceEvent::class.java).also {
      `when`(it.associationId).thenReturn(4)
      `when`(it.event).thenReturn(kind)
    }
    expectIngestions(2)
    service.onDevicePresenceEvent(event(DevicePresenceEvent.EVENT_BLE_APPEARED))
    service.onDeviceAppeared(association(4, peer))
    service.onDevicePresenceEvent(event(DevicePresenceEvent.EVENT_BT_CONNECTED))
    service.onDevicePresenceEvent(event(DevicePresenceEvent.EVENT_BLE_DISAPPEARED))
    service.onDeviceDisappeared(association(4, peer))
    service.onDevicePresenceEvent(event(DevicePresenceEvent.EVENT_BT_DISCONNECTED))
    service.onDevicePresenceEvent(event(DevicePresenceEvent.EVENT_BLE_APPEARED))
    awaitIngestions()
    assertEquals(1, releases.get())
    assertEquals(2, ingested.size)
    service.onDestroy()
  }

  @Test
  fun unobserveFencesHeldAndLaterCallbacksUntilSuccessfulObserve() {
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    val context = mock(android.content.Context::class.java)
    val executions = java.util.concurrent.atomic.AtomicInteger()
    val cleanup = java.util.concurrent.atomic.AtomicInteger()
    coordinator = PresenceWakeCoordinator(
      associatedAddresses = { setOf(peer) }, store = store, nowMs = { 1L },
      ensureOwner = { entered.countDown(); assertTrue(resume.await(5, TimeUnit.SECONDS)); true },
      ingest = { false }, log = {},
      continuation = { BackgroundContinuationDeclaration(ContinuationStrategy.NATIVE, null, emptyList(), null, null) },
      executeContinuation = { _, _ -> executions.incrementAndGet(); ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 0) }
    )
    UbmCompanionPresenceService.coordinatorOverride = coordinator
    val service = UbmCompanionPresenceService()
    val caller = Executors.newSingleThreadExecutor()
    val observer = CompanionPresenceObserver(36, { true }, { mock(android.companion.CompanionDeviceManager::class.java) },
      { _, _ -> }, { _, _ -> },
      { address -> UbmCompanionPresenceService.retireObservation(context, address) { cleanup.incrementAndGet() } })
    try {
      service.onDeviceAppeared(peer)
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      val stop = caller.submit { observer.unobserve(peer) { assertTrue(it.isSuccess) } }
      val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
      while (coordinator.acceptsTicket(peer, coordinator.admissionTicket()) && System.nanoTime() < deadline) Thread.yield()
      assertFalse("confirmed OS stop must retire admission before waiting for held work", coordinator.acceptsTicket(peer, coordinator.admissionTicket()))
      service.onDeviceAppeared(peer)
      resume.countDown()
      stop.get(5, TimeUnit.SECONDS)
      service.onDeviceAppeared(peer)
      val refusal = SecurityException("denied")
      try { UbmCompanionPresenceService.observe(context, peer) { throw refusal }; org.junit.Assert.fail("expected refusal") }
      catch (actual: SecurityException) { org.junit.Assert.assertSame(refusal, actual) }
      assertEquals(0, executions.get())
      assertEquals(1, cleanup.get())
      assertTrue(store.drainAppearances().isEmpty())
      UbmCompanionPresenceService.observe(context, peer) {}
      service.onDeviceAppeared(peer)
      UbmCompanionPresenceService.observe(context, peer) {}
      assertEquals(1, executions.get())
    } finally { resume.countDown(); service.onDestroy(); caller.shutdownNow() }
  }

  @Test
  fun reentrantUnobserveDoesNotDeadlockAndCleanupFailureIsPreserved() {
    val context = mock(android.content.Context::class.java)
    val failure = IllegalStateException("cleanup refused")
    try {
      UbmCompanionPresenceService.observe(context, peer) {
        UbmCompanionPresenceService.retireObservation(context, peer) { throw failure }
      }
      org.junit.Assert.fail("cleanup failure lost")
    } catch (actual: IllegalStateException) { org.junit.Assert.assertSame(failure, actual) }
    assertFalse(coordinator.acceptsTicket(peer, coordinator.admissionTicket()))
  }

  @Test
  fun modernUnknownUuidAndAssociationEventsDoNotAdmitAWake() {
    val service = object : UbmCompanionPresenceService() {
      override fun presenceSdkInt() = 36
      override fun associationForPresence(id: Int) = if (id == 4) association(4, peer) else null
    }
    fun event(id: Int, kind: Int, uuid: Boolean = false) = mock(DevicePresenceEvent::class.java).also {
      `when`(it.associationId).thenReturn(id); `when`(it.event).thenReturn(kind)
      if (uuid) `when`(it.uuid).thenReturn(mock(android.os.ParcelUuid::class.java))
    }
    service.onDevicePresenceEvent(event(99, 0))
    service.onDevicePresenceEvent(event(4, 99))
    service.onDevicePresenceEvent(event(-1, 0, true))
    UbmCompanionPresenceService.observe(mock(android.content.Context::class.java), peer) {}
    assertTrue(ingested.isEmpty())
    service.onDestroy()
  }

  @Test
  fun serviceRebindCannotOvertakeAnAdmittedAppearance() {
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    val finished = CountDownLatch(1)
    val calls = java.util.Collections.synchronizedList(mutableListOf<String>())
    var first = true
    coordinator = PresenceWakeCoordinator(
      associatedAddresses = { setOf(peer) }, store = store, nowMs = { 1L },
      ensureOwner = {
        if (first) { first = false; entered.countDown(); assertTrue(resume.await(5, TimeUnit.SECONDS)) }
        true
      }, ingest = { calls.add("ingest"); if (calls.size == 3) finished.countDown(); true },
      log = {}, releaseContinuation = { calls.add("release"); null }
    )
    UbmCompanionPresenceService.coordinatorOverride = coordinator
    val old = UbmCompanionPresenceService()
    val replacement = UbmCompanionPresenceService()
    try {
      old.onDeviceAppeared(peer)
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      old.onDestroy()
      replacement.onDeviceDisappeared(peer)
      replacement.onDeviceAppeared(peer)
      resume.countDown()
      assertTrue(finished.await(5, TimeUnit.SECONDS))
      assertEquals(listOf("ingest", "release", "ingest"), calls)
    } finally { resume.countDown(); replacement.onDestroy() }
  }

  @Test
  fun associationInfoAppearanceReachesTheCoordinatorWithItsId() {
    val service = UbmCompanionPresenceService()
    expectIngestions(1)

    service.onDeviceAppeared(association(4, peer))
    service.onDeviceAppeared(association(5, peer))
    awaitIngestions()

    // One physical event, two association callbacks: a single ingest.
    assertEquals(listOf(PresenceRestoredPeer(peer, null, false)), ingested)
  }

  @Test
  fun bareAddressAppearanceStillReachesTheCoordinator() {
    val service = UbmCompanionPresenceService()
    expectIngestions(1)

    service.onDeviceAppeared(peer)
    awaitIngestions()

    assertEquals(listOf(PresenceRestoredPeer(peer, null, false)), ingested)
  }

  @Test
  fun differentlyCasedCallbacksForOneDeviceDeliverOneWake() {
    // The platform stores one case and delivers another; both callbacks
    // describe one physical event, so the second must be the logged
    // duplicate, not a second wake.
    val service = UbmCompanionPresenceService()
    expectIngestions(1)

    service.onDeviceAppeared(association(4, peer.lowercase()))
    service.onDeviceAppeared(association(5, peer))
    awaitIngestions()

    assertEquals(listOf(PresenceRestoredPeer(peer, null, false)), ingested)
  }

  @Test
  fun associationInfoDisappearanceClearsTheDelivery() {
    val service = UbmCompanionPresenceService()
    expectIngestions(1)
    service.onDeviceAppeared(association(4, peer))
    awaitIngestions()

    expectIngestions(1)
    service.onDeviceDisappeared(association(4, peer))
    service.onDeviceAppeared(association(4, peer))
    awaitIngestions()

    assertEquals(
      listOf(
        PresenceRestoredPeer(peer, null, false),
        PresenceRestoredPeer(peer, null, false)
      ),
      ingested
    )
  }

  @Test
  fun appearanceWithoutAnAddressIsIgnored() {
    val service = UbmCompanionPresenceService()

    service.onDeviceAppeared(association(4, null))

    assertEquals(emptyList<PresenceRestoredPeer>(), ingested)
  }

  @Test
  fun callbackReturnsBeforeABlockedNativeContinuationCompletes() {
    val enteredContinuation = CountDownLatch(1)
    val releaseContinuation = CountDownLatch(1)
    val completedContinuation = CountDownLatch(1)
    val declaration = BackgroundContinuationDeclaration(
      strategy = ContinuationStrategy.NATIVE,
      peerId = null,
      resubscribe = emptyList(),
      headlessTaskName = null,
      foregroundService = null
    )
    coordinator = PresenceWakeCoordinator(
      associatedAddresses = { setOf(peer) },
      store = store,
      nowMs = { 12345L },
      ensureOwner = { true },
      ingest = { true },
      log = {},
      continuation = { declaration },
      executeContinuation = { _, _ ->
        enteredContinuation.countDown()
        assertTrue("test continuation should be released", releaseContinuation.await(5, TimeUnit.SECONDS))
        completedContinuation.countDown()
        ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, resubscribed = 0)
      }
    )
    UbmCompanionPresenceService.coordinatorOverride = coordinator
    val service = UbmCompanionPresenceService()
    val callbackThread = Executors.newSingleThreadExecutor()
    var serviceDestroyed = false
    try {
      val callback = callbackThread.submit<Unit> { service.onDeviceAppeared(peer) }

      // The system callback must return while the bounded native work is still running.
      callback.get(1, TimeUnit.SECONDS)
      assertTrue("continuation should have been admitted to the service worker", enteredContinuation.await(5, TimeUnit.SECONDS))
      assertFalse("callback return must not wait for continuation completion", completedContinuation.await(100, TimeUnit.MILLISECONDS))

      // Teardown closes new admission but must drain work already admitted.
      service.onDestroy()
      serviceDestroyed = true
      releaseContinuation.countDown()
      assertTrue("an admitted continuation must survive service teardown", completedContinuation.await(5, TimeUnit.SECONDS))
    } finally {
      // Ensure a failed assertion cannot strand either test executor.
      releaseContinuation.countDown()
      if (!serviceDestroyed) service.onDestroy()
      callbackThread.shutdownNow()
    }
  }
}
