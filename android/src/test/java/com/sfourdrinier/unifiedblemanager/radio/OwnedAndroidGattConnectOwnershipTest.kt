// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattConnectOwnershipTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.anyBoolean
import org.mockito.ArgumentMatchers.anyInt
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito.doAnswer
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.doThrow
import org.mockito.Mockito.never
import org.mockito.Mockito.times
import org.mockito.Mockito.verify

/**
 * `connect()` over a prior GATT on the real [OwnedAndroidGattRadio] (mocked adapter/GATT, injected
 * scheduler). The protocol dispatcher treats a throw from `connect()` as the command's terminal, so
 * a connect that reports failure must leave no replacement queued, and a link must open exactly
 * once and only for a connect that did not fail. A close that failed stays retained and retried.
 */
class OwnedAndroidGattConnectOwnershipTest {
  @Test
  fun reconnectJoiningAnExistingDisconnectDoesNotRequestNativeDisconnectAgain() {
    val f = GattRadioFixture()
    val prior = f.connected()
    connectGattReturns(f, f.gatt())
    var released = 0
    f.radio.disconnect(f.peer) { released++ }

    f.radio.connect(f.peer, false)

    verify(prior, times(1)).disconnect()
    assertEquals(0, connectGattCalls(f))
    f.nativeDisconnected(prior)
    assertEquals(1, released)
    assertEquals(1, connectGattCalls(f))
    verify(prior, times(1)).close()
  }

  private fun priorDisconnectDeliveringItsOwnNativeDisconnected(f: GattRadioFixture, prior: BluetoothGatt, error: Throwable) {
    doAnswer {
      f.nativeDisconnected(prior)
      throw error
    }.`when`(prior).disconnect()
  }

  /** A later generation's teardown must not open an entry a failed connect left queued. */
  private fun assertNoStaleReconnectOpensAfterTheNextGeneration(f: GattRadioFixture, expectedOpens: Int) {
    val next = f.connected()
    f.nativeDisconnected(next)
    f.advanceTo(f.now + OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(expectedOpens, connectGattCalls(f))
  }

  @Test
  fun priorDisconnectThrowingAfterItsOwnNativeDisconnectedOpensTheReplacementOnce() {
    val f = GattRadioFixture()
    val prior = f.connected()
    connectGattReturns(f, f.gatt())
    priorDisconnectDeliveringItsOwnNativeDisconnected(f, prior, SecurityException("disconnect refused"))

    f.radio.connect(f.peer, false)

    verify(prior, times(1)).close()
    assertEquals(1, connectGattCalls(f))
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, connectGattCalls(f))
  }

  @Test
  fun priorDisconnectThrowingWithAFailedCloseDropsTheFailedConnectsReconnect() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val closeRefusal = IllegalStateException("close refused")
    val disconnectRefusal = SecurityException("disconnect refused")
    doThrow(closeRefusal).doNothing().`when`(prior).close()
    doThrow(disconnectRefusal).`when`(prior).disconnect()
    connectGattReturns(f, f.gatt())

    val error = thrownBy { f.radio.connect(f.peer, false) }

    assertTrue(error is IllegalStateException)
    assertSame(closeRefusal, error?.cause)
    assertTrue(error!!.suppressed.any { it === disconnectRefusal })
    assertEquals(0, connectGattCalls(f))
    // The retained close is still owned and retried by the generation's own callback.
    f.nativeDisconnected(prior)
    verify(prior, times(2)).close()
    assertEquals(0, connectGattCalls(f))
    assertNoStaleReconnectOpensAfterTheNextGeneration(f, expectedOpens = 0)
  }

  @Test
  fun joinedDisconnectWithAThrowingWaiterReportsItAfterCleanupAndOpensTheOwnedReplacement() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val waiterBoom = Boom("waiter")
    f.radio.disconnect(f.peer) { throw waiterBoom }
    doThrow(SecurityException("disconnect refused")).`when`(prior).disconnect()
    connectGattReturns(f, f.gatt())

    f.radio.connect(f.peer, false)
    verify(prior, times(1)).disconnect()
    val error = thrownBy { f.nativeDisconnected(prior) }

    assertSame(waiterBoom, error)
    verify(prior, times(1)).close()
    assertEquals(1, connectGattCalls(f))
  }

  @Test
  fun refusedCloseDeadlineWithACleanCloseAndAThrowingObserverDoesNotOpenBehindTheFailure() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val observer = Boom("connection observer")
    f.radio.onConnectionState = { _, connected, _ -> if (!connected) throw observer }
    connectGattReturns(f, f.gatt())
    f.refuseDeadline = true

    val error = thrownBy { f.radio.connect(f.peer, false) }

    assertSame(observer, error)
    verify(prior, times(1)).close()
    assertEquals(0, connectGattCalls(f))
    f.radio.onConnectionState = null
    assertNoStaleReconnectOpensAfterTheNextGeneration(f, expectedOpens = 0)
  }

  @Test
  fun rejectedCloseDeadlineSchedulerErrorFailsConnectWithoutOpeningTheReplacement() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val rejection = java.util.concurrent.RejectedExecutionException("scheduler is shut down")
    f.rejectDeadline = rejection
    connectGattReturns(f, f.gatt())

    val error = thrownBy { f.radio.connect(f.peer, false) }

    assertSame(rejection, error)
    verify(prior, times(1)).close()
    verify(prior, never()).disconnect()
    assertEquals(0, connectGattCalls(f))
    f.rejectDeadline = null
    assertNoStaleReconnectOpensAfterTheNextGeneration(f, expectedOpens = 0)
  }

  @Test
  fun supersededWaiterErrorFailsConnectBeforeTheNativeDisconnectedCanOpenTheReplacement() {
    val f = GattRadioFixture()
    val older = f.connected()
    val waiterBoom = Boom("superseded waiter")
    f.radio.disconnect(f.peer) { throw waiterBoom }
    // A newer generation for the same device: connect() supersedes the older owner's waiter.
    val prior = f.connected()
    connectGattReturns(f, f.gatt())

    val error = thrownBy { f.radio.connect(f.peer, false) }

    assertSame(waiterBoom, error)
    verify(prior, times(1)).disconnect()
    // The disconnect was requested, but its DISCONNECTED must not open a link behind the failure.
    f.nativeDisconnected(prior)
    verify(prior, times(1)).close()
    assertEquals(0, connectGattCalls(f))
    assertNoStaleReconnectOpensAfterTheNextGeneration(f, expectedOpens = 0)
    verify(older, never()).close()
  }

  @Test
  fun supersededWaiterErrorIsNotLostWhenThePriorDisconnectThrowsAfterACleanClose() {
    val f = GattRadioFixture()
    f.connected()
    val waiterBoom = Boom("superseded waiter")
    f.radio.disconnect(f.peer) { throw waiterBoom }
    val prior = f.connected()
    doThrow(SecurityException("disconnect refused")).`when`(prior).disconnect()
    connectGattReturns(f, f.gatt())

    val error = thrownBy { f.radio.connect(f.peer, false) }

    assertSame(waiterBoom, error)
    verify(prior, times(1)).close()
    assertEquals(0, connectGattCalls(f))
  }

  @Test
  fun supersededWaiterErrorIsSuppressedOntoAFailedCloseOfThePriorDisconnectThrowRoute() {
    val f = GattRadioFixture()
    f.connected()
    val waiterBoom = Boom("superseded waiter")
    f.radio.disconnect(f.peer) { throw waiterBoom }
    val prior = f.connected()
    val closeRefusal = IllegalStateException("close refused")
    doThrow(closeRefusal).doNothing().`when`(prior).close()
    doThrow(SecurityException("disconnect refused")).`when`(prior).disconnect()
    connectGattReturns(f, f.gatt())

    val error = thrownBy { f.radio.connect(f.peer, false) }

    assertSame(closeRefusal, error?.cause)
    assertTrue(error!!.suppressed.any { it === waiterBoom })
    assertEquals(0, connectGattCalls(f))
  }

  @Test
  fun forcedCloseRefusalAfterTheGenerationWasTornDownPublishesNothingAboutTheReplacement() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val lost = mutableListOf<Int>()
    f.radio.onConnectionState = { _, connected, status -> if (!connected) lost.add(status) }
    val waiter = GattDisconnectResults()
    val replacementRssi = mutableListOf<Boolean>()
    lateinit var replacement: BluetoothGatt
    f.refuseDeadline = true
    // The prior's own teardown and a replacement's attach land between the join and the refusal.
    f.onCloseDeadlineRequested = {
      f.onCloseDeadlineRequested = null
      f.nativeDisconnected(prior)
      replacement = f.connected()
      doReturn(true).`when`(replacement).readRemoteRssi()
      f.radio.readRemoteRssi(f.peer) { replacementRssi.add(it.isSuccess) }
    }

    f.radio.disconnect(f.peer, waiter.callback)

    // Only the prior's real DISCONNECTED was published; the replacement's operation is still
    // pending and the replacement was neither closed nor failed by the stale refusal.
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), waiter.values)
    assertEquals(listOf(BluetoothGatt.GATT_SUCCESS), lost)
    assertTrue(replacementRssi.isEmpty())
    verify(replacement, never()).close()
    verify(prior, times(1)).close()
    val later = GattDisconnectResults()
    f.refuseDeadline = false
    assertNull(f.radio.disconnect(f.peer, later.callback))
    f.nativeDisconnected(replacement)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), later.values)
    verify(replacement, times(1)).close()
  }

  @Test
  fun connectWhoseForcedCloseRefusalRacedThePriorsOwnTeardownOpensOnceAndPublishesOneLoss() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val lost = mutableListOf<Int>()
    f.radio.onConnectionState = { _, connected, status -> if (!connected) lost.add(status) }
    connectGattReturns(f, f.gatt())
    f.refuseDeadline = true
    f.onCloseDeadlineRequested = {
      f.onCloseDeadlineRequested = null
      f.nativeDisconnected(prior)
    }

    f.radio.connect(f.peer, false)

    // The prior's own teardown opened the queued reconnect; the stale refusal must not publish a
    // second loss (which the dispatcher would attribute to the replacement) or reopen anything.
    verify(prior, times(1)).close()
    assertEquals(1, connectGattCalls(f))
    assertEquals(listOf(BluetoothGatt.GATT_SUCCESS), lost)
  }

  /**
   * Runs [binderTeardown] while `connect()` fails the prior generation's pending
   * operations, i.e. after it read the prior GATT and before it joins the owner.
   */
  private fun interleaveInsideConnect(f: GattRadioFixture, gatt: BluetoothGatt, binderTeardown: () -> Unit) {
    doReturn(true).`when`(gatt).readRemoteRssi()
    f.radio.readRemoteRssi(f.peer) { binderTeardown() }
  }

  @Test
  fun refusedCloseDeadlineDuringQueuedReconnectReopensExactlyOnceAfterTheCleanClose() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val replacement = f.gatt()
    connectGattReturns(f, replacement)
    f.refuseDeadline = true

    f.radio.connect(f.peer, false)

    verify(prior, times(1)).close()
    verify(prior, never()).disconnect()
    assertEquals(1, connectGattCalls(f))
    assertTrue(f.closeDeadlines().isEmpty())
    f.nativeDisconnected(prior)
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, connectGattCalls(f))
    verify(replacement, never()).close()
  }

  @Test
  fun refusedCloseDeadlineWithFailedCloseKeepsTheRetainedCloseButDropsTheFailedConnectsReconnect() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val closeRefusal = IllegalStateException("close refused")
    doThrow(closeRefusal).doNothing().`when`(prior).close()
    connectGattReturns(f, f.gatt())
    f.refuseDeadline = true

    val error = thrownBy { f.radio.connect(f.peer, false) }

    assertTrue(error is IllegalStateException)
    assertSame(closeRefusal, error?.cause)
    assertEquals(0, connectGattCalls(f))
    verify(prior, never()).disconnect()
    // connect() reported failure: the retained generation's own native DISCONNECTED retries the
    // close, but the failed connect's replacement is gone and must not open behind it.
    f.nativeDisconnected(prior)
    verify(prior, times(2)).close()
    assertEquals(0, connectGattCalls(f))
    f.nativeDisconnected(prior)
    assertEquals(0, connectGattCalls(f))
  }

  @Test
  fun throwingWaiterAfterACleanCloseStillOpensTheQueuedReconnectExactlyOnce() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val waiterBoom = Boom("waiter")
    f.radio.disconnect(f.peer) { throw waiterBoom }
    connectGattReturns(f, f.gatt())
    f.radio.connect(f.peer, false)
    assertEquals(0, connectGattCalls(f))

    val thrown = thrownBy { f.nativeDisconnected(prior) }

    assertSame(waiterBoom, thrown)
    verify(prior, times(1)).close()
    assertEquals(1, connectGattCalls(f))
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, connectGattCalls(f))
  }

  @Test
  fun connectRacingThePriorGenerationsOwnCleanTeardownReopensOnceWithoutNativeDisconnect() {
    val f = GattRadioFixture()
    val prior = f.connected()
    connectGattReturns(f, f.gatt())
    interleaveInsideConnect(f, prior) { f.nativeDisconnected(prior) }

    f.radio.connect(f.peer, false)

    verify(prior, times(1)).close()
    verify(prior, never()).disconnect()
    assertEquals(1, connectGattCalls(f))
    assertTrue(f.closeDeadlines().isEmpty())
    f.nativeDisconnected(prior)
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, connectGattCalls(f))
  }

  @Test
  fun connectRacingARetainedCloseFailureIsRefusedAndLeavesNoReconnectQueued() {
    val f = GattRadioFixture()
    val prior = f.connected()
    doThrow(IllegalStateException("close refused")).doNothing().`when`(prior).close()
    connectGattReturns(f, f.gatt())
    interleaveInsideConnect(f, prior) { f.nativeDisconnected(prior) }

    val error = thrownBy { f.radio.connect(f.peer, false) }

    assertTrue(error is IllegalStateException)
    assertTrue(error!!.message!!.contains("cleanup is still pending"))
    verify(prior, never()).disconnect()
    assertEquals(0, connectGattCalls(f))
    // connect() reported failure, so the retried clean close must not open a link behind it.
    f.nativeDisconnected(prior)
    verify(prior, times(2)).close()
    assertEquals(0, connectGattCalls(f))
  }

  @Test
  fun queuedReconnectOpensReplacementOnlyAfterNativeDisconnectedAndTheOldDeadlineIsFenced() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val replacement = f.gatt()
    doReturn(replacement).`when`(f.device)
      .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    f.radio.connect(f.peer, false)
    verify(prior).disconnect()
    verify(f.device, never()).connectGatt(any(), anyBoolean(), any(), anyInt())

    f.nativeDisconnected(prior)
    verify(prior, times(1)).close()
    verify(f.device, times(1)).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    verify(replacement, never()).close()
    verify(f.device, times(1)).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
  }

  @Test
  fun disconnectDuringQueuedReconnectJoinsItsDeadlineAndCancelsTheReconnect() {
    val f = GattRadioFixture()
    val prior = f.connected()
    f.radio.connect(f.peer, false)
    val waiter = GattDisconnectResults()
    f.advanceTo(2_000L)

    f.radio.disconnect(f.peer, waiter.callback)
    assertEquals(1, f.closeDeadlines().size)
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), waiter.values)
    verify(prior, times(1)).close()
    verify(f.device, never()).connectGatt(any(), anyBoolean(), any(), anyInt())
  }

  /** Every observation the radio publishes, with the connect request its GATT generation was opened for. */
  private class Outcomes {
    val seen = mutableListOf<Triple<Boolean, Int, GattConnectAttempt?>>()
    fun attach(f: GattRadioFixture) {
      f.radio.onConnectionOutcome = { _, connected, status, attempt -> seen.add(Triple(connected, status, attempt)) }
    }
  }

  @Test
  fun priorGenerationLossCarriesThePriorsAttemptAndTheReplacementOutcomeItsOwn() {
    val f = GattRadioFixture()
    val first = f.gatt()
    val second = f.gatt()
    doReturn(first).doReturn(second).`when`(f.device)
      .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val outcomes = Outcomes().also { it.attach(f) }
    val priorAttempt = GattConnectAttempt()
    val replacementAttempt = GattConnectAttempt()

    f.radio.connect(f.peer, false, 0, priorAttempt)
    f.radio.nativeGattCallback().onConnectionStateChange(
      first, BluetoothGatt.GATT_SUCCESS, android.bluetooth.BluetoothProfile.STATE_CONNECTED
    )
    f.radio.connect(f.peer, false, 0, replacementAttempt)
    f.nativeDisconnected(first)
    f.radio.nativeGattCallback().onConnectionStateChange(
      second, BluetoothGatt.GATT_SUCCESS, android.bluetooth.BluetoothProfile.STATE_CONNECTED
    )

    assertEquals(3, outcomes.seen.size)
    assertEquals(Triple(true, 0, priorAttempt), outcomes.seen[0])
    assertSame("the prior's loss stays attributed to the prior", priorAttempt, outcomes.seen[1].third)
    assertEquals(false, outcomes.seen[1].first)
    assertSame(replacementAttempt, outcomes.seen[2].third)
    assertEquals(true, outcomes.seen[2].first)
  }

  @Test
  fun forcedCloseOfThePriorIsPublishedWithThePriorsAttemptBeforeTheReplacementOpens() {
    val f = GattRadioFixture()
    val first = f.gatt()
    val second = f.gatt()
    doReturn(first).doReturn(second).`when`(f.device)
      .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val outcomes = Outcomes().also { it.attach(f) }
    val priorAttempt = GattConnectAttempt()
    f.radio.connect(f.peer, false, 0, priorAttempt)
    f.refuseDeadline = true

    f.radio.connect(f.peer, false, 0, GattConnectAttempt())

    assertEquals(1, outcomes.seen.size)
    assertEquals(Triple(false, 257, priorAttempt), outcomes.seen.single())
    assertEquals(2, connectGattCalls(f))
  }

  @Test
  fun replacementThatCannotOpenFailsWithItsOwnAttemptNotThePriors() {
    val f = GattRadioFixture()
    val first = f.gatt()
    doReturn(first).doReturn(null).`when`(f.device)
      .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val outcomes = Outcomes().also { it.attach(f) }
    val priorAttempt = GattConnectAttempt()
    val replacementAttempt = GattConnectAttempt()
    f.radio.connect(f.peer, false, 0, priorAttempt)
    f.radio.connect(f.peer, false, 0, replacementAttempt)

    f.nativeDisconnected(first)

    assertEquals(2, outcomes.seen.size)
    assertSame(priorAttempt, outcomes.seen[0].third)
    assertEquals(Triple(false, BluetoothGatt.GATT_FAILURE, replacementAttempt), outcomes.seen[1])
  }
}
