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
  fun unobserveStopsCallbacksAndIsIdleWhenNothingIsArmed() {
    val presence = observer()
    var idle = false
    presence.unobserve(peer) { idle = it.isSuccess }
    assertTrue(idle)
    assertTrue(stopped.isEmpty())
    var observed = false
    presence.observe(peer) { observed = it.isSuccess }
    assertTrue(observed)
    var stoppedOk = false
    presence.unobserve(peer) { stoppedOk = it.isSuccess }
    assertTrue(stoppedOk)
    assertEquals(listOf(peer), stopped)
  }
}
