// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattConnectOwnershipTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothProfile
import android.content.BroadcastReceiver
import android.content.Intent
import android.content.IntentFilter
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.anyBoolean
import org.mockito.ArgumentMatchers.anyInt
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito.doAnswer
import org.mockito.Mockito.doNothing
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.doThrow
import org.mockito.Mockito.never
import org.mockito.Mockito.mock
import org.mockito.Mockito.times
import org.mockito.Mockito.verify
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

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
  @Test
  fun synchronousNativeCallbackWaitsForItsOwnGattPublication() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val outcomes = Outcomes().also { it.attach(f) }
    val attempt = GattConnectAttempt()
    doAnswer { invocation ->
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(
        gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED
      )
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    f.radio.connect(f.peer, false, 0, attempt)

    assertEquals(listOf(Triple(true, BluetoothGatt.GATT_SUCCESS, attempt)), outcomes.seen)
    f.nativeDisconnected(gatt)
    assertSame(attempt, outcomes.seen.last().third)
    verify(gatt, times(1)).close()
  }

  @Test
  fun replacingQueuedConnectSettlesDisplacedAttemptBeforeOpeningReplacement() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val next = f.gatt()
    connectGattReturns(f, next)
    val outcomes = Outcomes().also { it.attach(f) }
    val displaced = GattConnectAttempt()
    val replacement = GattConnectAttempt()
    f.radio.connect(f.peer, false, 0, displaced)
    f.radio.connect(f.peer, false, 0, replacement)

    assertEquals(listOf(Triple(false, BluetoothGatt.GATT_FAILURE, displaced)), outcomes.seen)
    f.nativeDisconnected(prior)
    f.radio.nativeGattCallback().onConnectionStateChange(next, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    assertEquals(1, outcomes.seen.count { it.third === displaced })
    assertEquals(Triple(true, BluetoothGatt.GATT_SUCCESS, replacement), outcomes.seen.last())
  }

  @Test
  fun failedOpenRejectsEarlyCallbackAndCannotReviveOnReconnect() {
    val f = GattRadioFixture()
    val failedGatt = f.gatt()
    val nextGatt = f.gatt()
    val outcomes = Outcomes().also { it.attach(f) }
    val failed = GattConnectAttempt()
    val next = GattConnectAttempt()
    lateinit var failedCallback: BluetoothGattCallback
    doAnswer { invocation ->
      failedCallback = invocation.getArgument(2)
      failedCallback.onConnectionStateChange(failedGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
      throw IllegalStateException("open refused")
    }.doReturn(nextGatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    assertTrue(thrownBy { f.radio.connect(f.peer, false, 0, failed) } is IllegalStateException)
    assertTrue(outcomes.seen.isEmpty())
    verify(failedGatt, times(1)).close()
    f.radio.connect(f.peer, false, 0, next)
    failedCallback.onConnectionStateChange(failedGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    f.radio.nativeGattCallback().onConnectionStateChange(nextGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    assertEquals(listOf(Triple(true, BluetoothGatt.GATT_SUCCESS, next)), outcomes.seen)
  }

  @Test
  fun failedOpenRetainsUnpublishedGattUntilCloseRetrySucceeds() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val closeError = IllegalStateException("close refused")
    doThrow(closeError).doNothing().`when`(gatt).close()
    doAnswer { invocation ->
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_CONNECTED)
      throw IllegalStateException("open refused")
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val failure = checkNotNull(thrownBy { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) })
    assertTrue(failure.cause?.suppressed?.contains(closeError) == true)
    verify(gatt, times(1)).close()
    assertTrue(f.radio.destroy().isSuccessful)
    verify(gatt, times(2)).close()
  }

  @Test
  fun queuedOpenCanSynchronouslyDisconnectAndLaterReconnectWithoutRevivingIt() {
    val f = GattRadioFixture()
    val first = f.connected()
    val closed = f.gatt()
    val next = f.gatt()
    val outcomes = Outcomes().also { it.attach(f) }
    val queued = GattConnectAttempt()
    val later = GattConnectAttempt()
    lateinit var oldCallback: BluetoothGattCallback
    doAnswer { invocation ->
      oldCallback = invocation.getArgument(2)
      oldCallback.onConnectionStateChange(closed, 133, BluetoothProfile.STATE_DISCONNECTED)
      closed
    }.doReturn(next).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    f.radio.connect(f.peer, false, 0, queued)
    f.nativeDisconnected(first)
    assertEquals(Triple(false, 133, queued), outcomes.seen.last())
    verify(closed, times(1)).close()
    f.radio.connect(f.peer, false, 0, later)
    oldCallback.onConnectionStateChange(closed, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    f.radio.nativeGattCallback().onConnectionStateChange(next, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    assertEquals(1, outcomes.seen.count { it.third === queued })
    assertEquals(Triple(true, BluetoothGatt.GATT_SUCCESS, later), outcomes.seen.last())
  }

  @Test
  fun synchronousObserverFailureCannotLeaveAConnectionBehindTheFailedOpen() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val boom = Boom("observer")
    f.radio.onConnectionOutcome = { _, _, _, _ -> throw boom }
    doAnswer { invocation ->
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    assertSame(boom, thrownBy { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) })
    verify(gatt, times(1)).close()
  }

  @Test
  fun synchronousObserverFailureTerminatesThePublishedOwnedConnection() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val attempt = GattConnectAttempt()
    val owner = OwnedAndroidGattRadio.ReceiverOwner()
    val states = mutableListOf<Boolean>()
    f.radio.onConnectionOutcome = { _, connected, _, _ -> if (connected) throw Boom("observer") }
    doAnswer { invocation ->
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_CONNECTED)
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    assertTrue(thrownBy {
      f.radio.connectOwned(f.peer, false, 0, attempt, owner, { _, connected, _, _ -> states.add(connected) }, {})
    } is Boom)
    assertEquals(listOf(true, false), states)
    verify(gatt, times(1)).close()
  }

  @Test
  fun earlyServiceChangeFollowsEarlyConnectionAfterPublication() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val events = mutableListOf<String>()
    f.radio.onConnectionOutcome = { _, connected, _, _ -> if (connected) events.add("connected") }
    f.radio.onServicesChanged = { events.add("services") }
    doAnswer { invocation ->
      val callback = invocation.getArgument<BluetoothGattCallback>(2)
      callback.onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_CONNECTED)
      callback.onServiceChanged(gatt)
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    f.radio.connect(f.peer, false, 0, GattConnectAttempt())
    assertEquals(listOf("connected", "services"), events)
  }

  @Test
  fun overlappingOpensCannotLeaveEitherGattUnowned() {
    val f = GattRadioFixture()
    val firstGatt = f.gatt()
    val secondGatt = f.gatt()
    val first = GattConnectAttempt()
    val second = GattConnectAttempt()
    val outcomes = Outcomes().also { it.attach(f) }
    var opens = 0
    doAnswer { invocation ->
      opens++
      if (opens == 1) {
        f.radio.connect(f.peer, false, 0, second)
        invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(firstGatt, 0, BluetoothProfile.STATE_CONNECTED)
        firstGatt
      } else {
        invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(secondGatt, 0, BluetoothProfile.STATE_CONNECTED)
        secondGatt
      }
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    f.radio.connect(f.peer, false, 0, first)
    f.radio.disconnect(f.peer)
    f.nativeDisconnected(firstGatt)
    f.nativeDisconnected(secondGatt)
    verify(firstGatt, times(1)).close()
    if (opens == 2) verify(secondGatt, times(1)).close()
    assertTrue(outcomes.seen.any { it.third === first })
    assertTrue(outcomes.seen.any { it.third === second })
  }

  @Test
  fun replacementWaitsForFailedOpeningThenOwnsItsOwnCallbacks() {
    val f = GattRadioFixture()
    val next = f.gatt()
    val first = GattConnectAttempt()
    val replacement = GattConnectAttempt()
    val outcomes = Outcomes().also { it.attach(f) }
    var calls = 0
    var callsAfterReentrantConnect = 0
    doAnswer { invocation ->
      calls++
      if (calls == 1) {
        f.radio.connect(f.peer, false, 0, replacement)
        callsAfterReentrantConnect = calls
        throw IllegalStateException("first open refused")
      }
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(next, 0, BluetoothProfile.STATE_CONNECTED)
      next
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    assertTrue(thrownBy { f.radio.connect(f.peer, false, 0, first) } is IllegalStateException)
    assertEquals("the replacement must wait for the first native open", 1, callsAfterReentrantConnect)
    assertEquals(2, calls)
    assertEquals(listOf(Triple(true, 0, replacement)), outcomes.seen)
    f.nativeDisconnected(next)
    verify(next, times(1)).close()
    assertSame(replacement, outcomes.seen.last().third)
  }

  @Test
  fun disconnectDuringNativeOpenWaitsForPublishedGattToClose() {
    val f = GattRadioFixture()
    val opened = f.gatt()
    val results = GattDisconnectResults()
    doAnswer { invocation ->
      f.radio.disconnect(f.peer, results.callback)
      assertTrue(results.values.isEmpty())
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(opened, 0, BluetoothProfile.STATE_CONNECTED)
      opened
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    f.radio.connect(f.peer, false, 0, GattConnectAttempt())
    verify(opened, times(1)).disconnect()
    assertTrue(results.values.isEmpty())
    f.nativeDisconnected(opened)
    verify(opened, times(1)).close()
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), results.values)
  }

  @Test
  fun disconnectQueuedDuringFailedOpenCompletesAfterFailure() {
    val f = GattRadioFixture()
    val results = GattDisconnectResults()
    doAnswer {
      f.radio.disconnect(f.peer, results.callback)
      assertTrue(results.values.isEmpty())
      throw IllegalStateException("open refused")
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    assertTrue(thrownBy { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) } is IllegalStateException)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), results.values)
  }

  @Test
  fun multipleConnectsDuringOpenSettleDisplacedAttemptOnce() {
    val f = GattRadioFixture()
    val firstGatt = f.gatt()
    val lastGatt = f.gatt()
    val first = GattConnectAttempt()
    val displaced = GattConnectAttempt()
    val last = GattConnectAttempt()
    val outcomes = Outcomes().also { it.attach(f) }
    var opens = 0
    doAnswer { invocation ->
      opens++
      if (opens == 1) {
        f.radio.connect(f.peer, false, 0, displaced)
        f.radio.connect(f.peer, false, 0, last)
        assertEquals(1, opens)
        invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(firstGatt, 0, BluetoothProfile.STATE_CONNECTED)
        firstGatt
      } else {
        invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(lastGatt, 0, BluetoothProfile.STATE_CONNECTED)
        lastGatt
      }
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    f.radio.connect(f.peer, false, 0, first)
    assertEquals(listOf(Triple(true, 0, first), Triple(false, BluetoothGatt.GATT_FAILURE, displaced)), outcomes.seen)
    f.nativeDisconnected(firstGatt)
    assertEquals(2, opens)
    assertEquals(1, outcomes.seen.count { it.third === displaced })
    assertEquals(Triple(true, 0, last), outcomes.seen.last())
    f.nativeDisconnected(lastGatt)
    verify(firstGatt, times(1)).close()
    verify(lastGatt, times(1)).close()
  }

  @Test
  fun anotherThreadCannotOpenSamePeerBeforeNativeOpenReturns() {
    val f = GattRadioFixture()
    val firstGatt = f.gatt()
    val secondGatt = f.gatt()
    val first = GattConnectAttempt()
    val second = GattConnectAttempt()
    val outcomes = Outcomes().also { it.attach(f) }
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val failure = AtomicReference<Throwable?>()
    var opens = 0
    doAnswer { invocation ->
      opens++
      val gatt = if (opens == 1) {
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
        firstGatt
      } else secondGatt
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_CONNECTED)
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    val worker = Thread {
      try { f.radio.connect(f.peer, false, 0, first) } catch (error: Throwable) { failure.set(error) }
    }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      f.radio.connect(f.peer, false, 0, second)
      assertEquals(1, opens)
    } finally {
      release.countDown()
      worker.join(5000)
    }
    assertTrue(!worker.isAlive)
    assertNull(failure.get())
    f.nativeDisconnected(firstGatt)
    assertEquals(2, opens)
    assertEquals(Triple(true, 0, second), outcomes.seen.last())
    f.nativeDisconnected(secondGatt)
    verify(firstGatt, times(1)).close()
    verify(secondGatt, times(1)).close()
  }

  @Test
  fun stalledOpenBoundsQueuedPeerLifecycleAdmissions() {
    val f = GattRadioFixture(peerLifecycleCapacity = 3)
    val gatt = f.gatt()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    doAnswer {
      entered.countDown()
      check(release.await(5, TimeUnit.SECONDS))
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val worker = Thread { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      repeat(3) { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) }
      val failure = thrownBy { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) }
      assertTrue("overflow must be reported to its caller", failure is IllegalStateException)
      val rejectedOwner = OwnedAndroidGattRadio.ReceiverOwner()
      val ownedFailure = thrownBy {
        f.radio.connectOwned(f.peer, false, 0, GattConnectAttempt(), rejectedOwner, { _, _, _, _ -> }, {})
      }
      assertTrue(ownedFailure is IllegalStateException)
      assertEquals("rejected admission must not retain its route", 0, f.radio.retireReceiverOwner(rejectedOwner).callbacks)
    } finally {
      release.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
  }

  @Test
  fun destroyReportsPeerLifecycleExecutionAfterNativeOpenReturns() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    doAnswer {
      entered.countDown()
      check(release.await(5, TimeUnit.SECONDS))
      null
    }.`when`(gatt).disconnect()
    doAnswer {
      f.radio.connect(f.peer, false, 0, GattConnectAttempt())
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val worker = Thread { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      assertFalse("executing lane work is still owned", f.radio.destroy().isSuccessful)
    } finally {
      release.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
  }

  @Test
  fun deferredReplacementOpenFailureSettlesItsAttemptWithoutFailingOriginalCall() {
    val f = GattRadioFixture()
    val firstGatt = f.gatt()
    val first = GattConnectAttempt()
    val replacement = GattConnectAttempt()
    val outcomes = Outcomes().also { it.attach(f) }
    var opens = 0
    doAnswer { f.nativeDisconnected(firstGatt); null }.`when`(firstGatt).disconnect()
    doAnswer { invocation ->
      opens++
      if (opens > 1) throw IllegalStateException("replacement refused")
      f.radio.connect(f.peer, false, 0, replacement)
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(firstGatt, 0, BluetoothProfile.STATE_CONNECTED)
      firstGatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    assertNull(thrownBy { f.radio.connect(f.peer, false, 0, first) })
    assertEquals(2, opens)
    assertEquals(1, outcomes.seen.count { it.third === replacement })
    assertEquals(Triple(false, BluetoothGatt.GATT_FAILURE, replacement), outcomes.seen.last())
    verify(firstGatt, times(1)).close()
  }

  @Test
  fun synchronousLossAndReconnectDrainWithoutNestedNativeOpens() {
    val f = GattRadioFixture()
    val attempts = List(40) { GattConnectAttempt() }
    val gatts = attempts.map { f.gatt() }
    val outcomes = Outcomes().also { it.attach(f) }
    var opens = 0
    var depth = 0
    var maximumDepth = 0
    doAnswer { invocation ->
      val index = opens++
      depth++
      maximumDepth = maxOf(maximumDepth, depth)
      if (index + 1 < attempts.size) f.radio.connect(f.peer, false, 0, attempts[index + 1])
      val callback = invocation.getArgument<BluetoothGattCallback>(2)
      callback.onConnectionStateChange(gatts[index], 0, BluetoothProfile.STATE_CONNECTED)
      callback.onConnectionStateChange(gatts[index], 0, BluetoothProfile.STATE_DISCONNECTED)
      depth--
      gatts[index]
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    f.radio.connect(f.peer, false, 0, attempts.first())

    assertEquals(attempts.size, opens)
    assertEquals(1, maximumDepth)
    attempts.forEach { attempt -> assertEquals(2, outcomes.seen.count { it.third === attempt }) }
    gatts.forEach { gatt -> verify(gatt, times(1)).close() }
  }

  @Test
  fun destroyDuringNativeOpenReportsPendingAndClosesReturnedGatt() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    var pending = false
    doAnswer { invocation ->
      pending = !f.radio.destroy().isSuccessful
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_CONNECTED)
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    assertTrue(thrownBy { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) } is IllegalStateException)
    assertTrue(pending)
    verify(gatt, times(1)).close()
    assertTrue(f.radio.destroy().isSuccessful)
  }

  @Test
  fun destroySettlesQueuedAttemptAndClosesEvenIfItsObserverThrows() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val states = mutableListOf<Boolean>()
    f.radio.connectOwned(f.peer, false, 0, GattConnectAttempt(), OwnedAndroidGattRadio.ReceiverOwner(),
      { _, connected, _, _ -> states.add(connected); throw Boom("owner observer") }, {})

    assertTrue(thrownBy { f.radio.destroy() } is Boom)
    assertEquals(listOf(false), states)
    verify(prior).close()
  }

  @Test
  fun destroyReportsCloseFailureAlongsideQueuedObserverFailure() {
    val f = GattRadioFixture()
    val prior = f.connected()
    val closeError = IllegalStateException("close refused")
    doThrow(closeError).doNothing().`when`(prior).close()
    val observerError = Boom("owner observer")
    f.radio.connectOwned(f.peer, false, 0, GattConnectAttempt(), OwnedAndroidGattRadio.ReceiverOwner(),
      { _, _, _, _ -> throw observerError }, {})

    val failure = thrownBy { f.radio.destroy() }
    assertSame(observerError, failure)
    assertTrue("close failure must remain visible", checkNotNull(failure).suppressed.contains(closeError))
    assertTrue(f.radio.destroy().isSuccessful)
    verify(prior, times(2)).close()
  }

  @Test
  fun destroyContinuesClosingOtherGattWhenCleanupObserverThrows() {
    val f = GattRadioFixture()
    val unpublished = f.gatt()
    val closeError = IllegalStateException("unpublished close refused")
    doThrow(closeError).`when`(unpublished).close()
    doAnswer { invocation ->
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(unpublished, 0, BluetoothProfile.STATE_CONNECTED)
      throw IllegalStateException("open refused")
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    assertTrue(thrownBy { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) } is IllegalStateException)
    val otherGatt = f.connected()
    val observerError = Boom("cleanup observer")
    f.radio.onCleanupFailure = { throw observerError }

    val failure = thrownBy { f.radio.destroy() }
    assertSame(observerError, failure)
    assertTrue(checkNotNull(failure).suppressed.contains(closeError))
    verify(otherGatt).close()
  }

  @Test
  fun adapterLossDuringNativeOpenClosesReturnedGattAndFailsQueuedConnect() {
    val f = GattRadioFixture()
    var receiver: BroadcastReceiver? = null
    doAnswer { call -> receiver = call.getArgument(0); null }
      .`when`(f.context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    f.radio.registerAdapterStateReceiver()
    val opened = f.gatt()
    val replacement = GattConnectAttempt()
    val outcomes = Outcomes().also { it.attach(f) }
    val intent = mock(Intent::class.java)
    doReturn(BluetoothAdapter.ACTION_STATE_CHANGED).`when`(intent).action
    doReturn(BluetoothAdapter.STATE_OFF).`when`(intent).getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)
    doAnswer { invocation ->
      f.radio.connect(f.peer, false, 0, replacement)
      doReturn(BluetoothAdapter.STATE_OFF).`when`(f.adapter).state
      checkNotNull(receiver).onReceive(f.context, intent)
      invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(opened, 0, BluetoothProfile.STATE_CONNECTED)
      opened
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))

    assertTrue(thrownBy { f.radio.connect(f.peer, false, 0, GattConnectAttempt()) } is IllegalStateException)
    assertEquals(1, connectGattCalls(f))
    assertEquals(listOf(Triple(false, BluetoothGatt.GATT_FAILURE, replacement)), outcomes.seen)
    verify(opened, times(1)).close()
  }

  @Test
  fun adapterLossSettlesQueuedOwnerAndNotifiesAdapterEvenWhenCleanupObserverThrows() {
    val f = GattRadioFixture()
    var receiver: BroadcastReceiver? = null
    doAnswer { call -> receiver = call.getArgument(0); null }
      .`when`(f.context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    f.radio.registerAdapterStateReceiver()
    val prior = f.connected()
    doThrow(IllegalStateException("close refused")).`when`(prior).close()
    val attempt = GattConnectAttempt()
    val states = mutableListOf<Boolean>()
    f.radio.connectOwned(f.peer, false, 0, attempt, OwnedAndroidGattRadio.ReceiverOwner(),
      { _, connected, _, _ -> states.add(connected) }, {})
    var adapterNotified = false
    f.radio.onAdapterState = { adapterNotified = true }
    f.radio.onCleanupFailure = { throw Boom("cleanup observer") }
    val intent = mock(Intent::class.java)
    doReturn(BluetoothAdapter.ACTION_STATE_CHANGED).`when`(intent).action
    doReturn(BluetoothAdapter.STATE_OFF).`when`(intent).getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)

    assertTrue(thrownBy { checkNotNull(receiver).onReceive(f.context, intent) } is Boom)
    assertEquals(listOf(false), states)
    assertTrue(adapterNotified)
    verify(prior).close()
  }

  @Test
  fun adapterLossSettlesAllQueuedOwnersWhenOneObserverThrows() {
    val f = GattRadioFixture()
    var receiver: BroadcastReceiver? = null
    doAnswer { call -> receiver = call.getArgument(0); null }
      .`when`(f.context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    f.radio.registerAdapterStateReceiver()
    val firstGatt = f.connected()
    val secondPeer = "A0:9E:1A:00:00:02"
    val secondDevice = mock(BluetoothDevice::class.java)
    val secondGatt = mock(BluetoothGatt::class.java)
    doReturn(secondPeer).`when`(secondDevice).address
    doReturn(secondDevice).`when`(secondGatt).device
    f.radio.attachConnectedGatt(secondPeer, secondGatt, emptyList())
    val outcomes = mutableListOf<String>()
    f.radio.connectOwned(f.peer, false, 0, GattConnectAttempt(), OwnedAndroidGattRadio.ReceiverOwner(),
      { _, _, _, _ -> outcomes.add("first"); throw Boom("first observer") }, {})
    f.radio.connectOwned(secondPeer, false, 0, GattConnectAttempt(), OwnedAndroidGattRadio.ReceiverOwner(),
      { _, _, _, _ -> outcomes.add("second") }, {})
    var notified = false
    f.radio.onAdapterState = { notified = true }
    val intent = mock(Intent::class.java)
    doReturn(BluetoothAdapter.ACTION_STATE_CHANGED).`when`(intent).action
    doReturn(BluetoothAdapter.STATE_OFF).`when`(intent).getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)

    assertTrue(thrownBy { checkNotNull(receiver).onReceive(f.context, intent) } is Boom)
    assertEquals(setOf("first", "second"), outcomes.toSet())
    assertTrue(notified)
    verify(firstGatt).close()
    verify(secondGatt).close()
  }

}
