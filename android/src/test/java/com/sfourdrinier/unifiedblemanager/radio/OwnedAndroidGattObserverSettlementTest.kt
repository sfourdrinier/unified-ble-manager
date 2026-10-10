// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattObserverSettlementTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothProfile
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Test
import org.mockito.Mockito.times
import org.mockito.Mockito.verify

/** Diagnostic callbacks must not skip owned outcomes or physical teardown. */
class OwnedAndroidGattObserverSettlementTest {
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
