// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/CompanionPresenceObserverTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.companion.CompanionDeviceManager
import com.sfourdrinier.unifiedblemanager.rustcore.RadioFailureKind
import com.sfourdrinier.unifiedblemanager.rustcore.RadioPortFailure
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.mockito.Mockito.mock
import org.mockito.Mockito.`when`
import android.companion.AssociationInfo
import android.net.MacAddress
import android.companion.newDeviceNotAssociatedException

/** Device-presence observation (issue #212) without a radio or a system service. */
class CompanionPresenceObserverTest {
  private val peer = "AA:BB:CC:DD:EE:FF"
  private val manager = mock(CompanionDeviceManager::class.java)
  private val started = mutableListOf<String>()
  private val stopped = mutableListOf<String>()
  private var failStart: Throwable? = null

  private fun observer(
    sdkInt: Int = 31,
    feature: Boolean = true,
    source: () -> CompanionDeviceManager? = { manager }
  ) = CompanionPresenceObserver(
    sdkInt = sdkInt,
    hasCompanionFeature = { feature },
    manager = source,
    startObserving = { _, address ->
      failStart?.let { throw it }
      started.add(address)
    },
    stopObserving = { _, address -> stopped.add(address) }
  )

  private fun observeFailsWith(observer: CompanionPresenceObserver, peerId: String): RadioPortFailure {
    try {
      observer.observe(peerId) { fail("callback must not run after a refusal") }
    } catch (error: RadioPortFailure) {
      return error
    }
    fail("observe must refuse")
    throw IllegalStateException("unreachable")
  }

  @Test
  fun modernObservationResolvesOnlyTheExactAssociationAndPreservesRefusals() {
    val association = mock(AssociationInfo::class.java)
    val mac = mock(MacAddress::class.java)
    `when`(mac.toString()).thenReturn(peer.lowercase())
    `when`(association.deviceMacAddress).thenReturn(mac)
    `when`(association.id).thenReturn(17)
    `when`(manager.myAssociations).thenReturn(listOf(association))
    val calls = mutableListOf<Int>()
    CompanionPresenceObserver.routeObservation(36, manager, peer, { fail("legacy route") }, { calls.add(it) })
    assertEquals(listOf(17), calls)
    CompanionPresenceObserver.routeObservation(35, manager, peer, { calls.add(-1) }, { fail("modern route") })
    assertEquals(listOf(17, -1), calls)
    val refusal = SecurityException("denied")
    try {
      CompanionPresenceObserver.routeObservation(36, manager, peer, {}, { throw refusal })
      fail("refusal lost")
    } catch (actual: SecurityException) { org.junit.Assert.assertSame(refusal, actual) }
    val second = mock(AssociationInfo::class.java)
    `when`(second.deviceMacAddress).thenReturn(mac)
    `when`(second.id).thenReturn(9)
    `when`(manager.myAssociations).thenReturn(listOf(association, second, association))
    calls.clear()
    CompanionPresenceObserver.routeObservation(36, manager, peer, {}, { calls.add(it) })
    assertEquals(listOf(9), calls)
    calls.clear()
    try {
      CompanionPresenceObserver.routeObservation(36, manager, peer, {}, {
        calls.add(it); if (it == 9) throw refusal
      }, stopping = true)
      fail("partial stop refusal lost")
    } catch (actual: SecurityException) { org.junit.Assert.assertSame(refusal, actual) }
    assertEquals(listOf(9, 17), calls)
    calls.clear()
    CompanionPresenceObserver.routeObservation(36, manager, peer, {}, { calls.add(it) }, stopping = true)
    assertEquals(listOf(9, 17), calls)
    `when`(manager.myAssociations).thenReturn(emptyList())
    try {
      CompanionPresenceObserver.routeObservation(36, manager, peer, {}, { fail("unknown effect") })
      fail("unknown association accepted")
    } catch (actual: RadioPortFailure) { assertEquals("deviceNotAssociated", actual.nativeCode) }
    CompanionPresenceObserver.routeObservation(36, manager, peer, {}, { fail("absent stop effect") }, stopping = true)
  }

  @Test
  fun refusedStopNeverRetiresContinuationAndRetryKeepsOwnership() {
    val failure = SecurityException("stop refused")
    var refusing = true
    var released = 0
    val subject = CompanionPresenceObserver(36, { true }, { manager }, { _, _ -> },
      { _, _ -> if (refusing) throw failure }, { released += 1 })
    try { subject.unobserve(peer) { fail("idle after refusal") }; fail("failure missing") }
    catch (actual: SecurityException) { org.junit.Assert.assertSame(failure, actual) }
    assertEquals(0, released)
    refusing = false
    subject.unobserve(peer) { assertTrue(it.isSuccess) }
    assertEquals(1, released)
  }

  @Test
  fun observeArmsTheAssociatedPeerAddress() {
    var observed = false
    observer().observe(peer) { observed = it.isSuccess }
    assertTrue(observed)
    assertEquals(listOf(peer), started)
  }

  @Test
  fun belowApi31ObserveIsUnsupportedBeforeAnyEffect() {
    val error = observeFailsWith(observer(sdkInt = 30), peer)
    assertEquals(RadioFailureKind.UNSUPPORTED, error.kind)
    assertTrue(started.isEmpty())
  }

  @Test
  fun withoutTheSetupFeatureObserveIsUnsupported() {
    val error = observeFailsWith(observer(feature = false), peer)
    assertEquals(RadioFailureKind.UNSUPPORTED, error.kind)
    assertTrue(started.isEmpty())
  }

  @Test
  fun withoutTheSystemServiceObserveIsUnsupported() {
    val error = observeFailsWith(observer(source = { null }), peer)
    assertEquals(RadioFailureKind.UNSUPPORTED, error.kind)
    assertTrue(started.isEmpty())
  }

  @Test
  fun anUnassociatedPeerFailsAsThePlatformAnswers() {
    failStart = newDeviceNotAssociatedException()
    val error = observeFailsWith(observer(), peer)
    assertEquals(RadioFailureKind.PLATFORM, error.kind)
    assertEquals("deviceNotAssociated", error.nativeCode)
  }

  @Test
  fun unobserveAlwaysAsksTheOsEvenWhenThisObserverDidNotArmThePeer() {
    val presence = observer()
    var idle = false
    presence.unobserve(peer) { idle = it.isSuccess }
    assertTrue(idle)
    assertEquals(listOf(peer), stopped)
    var observed = false
    presence.observe(peer) { observed = it.isSuccess }
    assertTrue(observed)
    var stoppedOk = false
    presence.unobserve(peer) { stoppedOk = it.isSuccess }
    assertTrue(stoppedOk)
    assertEquals(listOf(peer, peer), stopped)
  }
}
