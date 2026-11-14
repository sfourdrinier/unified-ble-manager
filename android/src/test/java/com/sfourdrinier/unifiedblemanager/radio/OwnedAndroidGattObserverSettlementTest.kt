// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattObserverSettlementTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothProfile
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Test
import org.mockito.Mockito.times
import org.mockito.Mockito.verify
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.doThrow
import org.junit.Assert.assertTrue

/** Diagnostic callbacks must not skip owned outcomes or physical teardown. */
class OwnedAndroidGattObserverSettlementTest {
  @Test
  fun throwingQueuedSettlementDoesNotSkipOtherQueuedOrActiveOperations() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    doReturn(true).`when`(gatt).requestMtu(100)
    val failure = IllegalStateException("queued observer")
    val outcomes = mutableListOf<String>()
    f.radio.requestMtu(f.peer, 100) { outcomes.add("active"); throw failure }
    f.radio.readRemoteRssi(f.peer) { outcomes.add("first"); throw IllegalStateException("queued observer") }
    f.radio.readRemoteRssi(f.peer) { outcomes.add("second") }

    assertSame(failure, thrownBy { f.radio.disconnect(f.peer) {} })
    assertEquals(listOf("first", "second", "active"), outcomes)
    verify(gatt, times(1)).disconnect()
    f.nativeDisconnected(gatt)
    verify(gatt, times(1)).close()
  }

  @Test
  fun throwingPendingCallbackDoesNotSkipAnotherPendingKind() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    doReturn(true).`when`(gatt).requestMtu(100)
    val failure = IllegalStateException("MTU observer")
    val outcomes = mutableListOf<String>()
    f.radio.requestMtu(f.peer, 100) { outcomes.add("mtu"); throw failure }
    // Simulate another already-owned callback independently of the serial queue.
    val field = f.radio.javaClass.getDeclaredField("pendingRssi").apply { isAccessible = true }
    val registry = field.get(f.radio)
    val callback: (Result<Int>) -> Unit = { outcomes.add("rssi") }
    registry.javaClass.getMethod("put", Any::class.java, Any::class.java)
      .invoke(registry, "rssi:${f.peer}", callback)

    assertSame(failure, thrownBy { f.radio.disconnect(f.peer) {} })
    assertEquals(listOf("mtu", "rssi"), outcomes)
    verify(gatt, times(1)).disconnect()
    f.nativeDisconnected(gatt)
    verify(gatt, times(1)).close()
  }
  @Test
  fun reconnectRequestsPriorDisconnectBeforeReportingThrowingPendingCallback() {
    val f = GattRadioFixture()
    val prior = f.connected()
    doReturn(true).`when`(prior).requestMtu(100)
    val failure = IllegalStateException("pending callback failed")
    f.radio.requestMtu(f.peer, 100) { throw failure }
    assertSame(failure, thrownBy { f.radio.connect(f.peer, false) })
    verify(prior, times(1)).disconnect()
    assertEquals(1, f.closeDeadlines().size)
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    verify(prior, times(1)).close()
    assertEquals(0, connectGattCalls(f))
  }

  @Test
  fun reconnectReportsDisconnectRefusalAlongsideThrowingPendingCallback() {
    val f = GattRadioFixture()
    val prior = f.connected()
    doReturn(true).`when`(prior).requestMtu(100)
    val pendingFailure = IllegalStateException("pending callback failed")
    val disconnectFailure = IllegalStateException("disconnect refused")
    doThrow(disconnectFailure).`when`(prior).disconnect()
    f.radio.requestMtu(f.peer, 100) { throw pendingFailure }
    val error = thrownBy { f.radio.connect(f.peer, false) }
    assertSame(pendingFailure, error)
    assertTrue(error!!.suppressed.any { it === disconnectFailure })
    verify(prior, times(1)).close()
    assertEquals(0, connectGattCalls(f))
  }

  @Test
  fun forcedCloseDeliversTheOwnedOutcomeEvenWhenTheDiagnosticObserverThrows() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val diagnosticFailure = IllegalStateException("diagnostic observer failed")
    var ownedOutcomes = 0
    var releases = 0
    f.radio.onConnectionState = { _, _, _ -> throw diagnosticFailure }
    f.radio.onConnectionOutcome = { _, _, _, _ -> ownedOutcomes++ }
    f.refuseDeadline = true

    val error = thrownBy { f.radio.disconnect(f.peer) { releases++ } }

    assertSame(diagnosticFailure, error)
    assertEquals(1, ownedOutcomes)
    assertEquals(1, releases)
    verify(prior, times(1)).close()
  }

  @Test
  fun nativeDisconnectClosesAndSettlesWaitersBeforeRethrowingAnObserverFailure() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val diagnosticFailure = IllegalStateException("diagnostic observer failed")
    var ownedOutcomes = 0
    var releases = 0
    f.radio.disconnect(f.peer) { releases++ }
    f.radio.onConnectionState = { _, _, _ -> throw diagnosticFailure }
    f.radio.onConnectionOutcome = { _, _, _, _ -> ownedOutcomes++ }

    val error = thrownBy { f.nativeDisconnected(prior) }

    assertSame(diagnosticFailure, error)
    assertEquals(1, ownedOutcomes)
    assertEquals(1, releases)
    verify(prior, times(1)).close()
  }

  @Test
  fun nativeFailedConnectClosesBeforeRethrowingAnObserverFailure() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val diagnosticFailure = IllegalStateException("diagnostic observer failed")
    var ownedOutcomes = 0
    f.radio.onConnectionState = { _, _, _ -> throw diagnosticFailure }
    f.radio.onConnectionOutcome = { _, _, _, _ -> ownedOutcomes++ }

    val error = thrownBy {
      f.radio.nativeGattCallback().onConnectionStateChange(
        prior, BluetoothGatt.GATT_FAILURE, BluetoothProfile.STATE_CONNECTED
      )
    }

    assertSame(diagnosticFailure, error)
    assertEquals(1, ownedOutcomes)
    verify(prior, times(1)).close()
  }

  @Test
  fun nativeDisconnectResumesTheQueuedReplacementDespiteAnObserverFailure() {
    val f = GattRadioFixture()
    val prior = f.connected()
    connectGattReturns(f, f.gatt())
    f.radio.connect(f.peer, false)
    val diagnosticFailure = IllegalStateException("diagnostic observer failed")
    f.radio.onConnectionState = { _, _, _ -> throw diagnosticFailure }

    val error = thrownBy { f.nativeDisconnected(prior) }

    assertSame(diagnosticFailure, error)
    verify(prior, times(1)).close()
    assertEquals(1, connectGattCalls(f))
  }

}
