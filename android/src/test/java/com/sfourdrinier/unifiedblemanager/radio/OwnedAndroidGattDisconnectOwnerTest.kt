// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattDisconnectOwnerTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothProfile
import android.content.BroadcastReceiver
import android.content.Intent
import android.content.IntentFilter
import java.util.UUID
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito.doAnswer
import org.mockito.Mockito.doNothing
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.doThrow
import org.mockito.Mockito.mock
import org.mockito.Mockito.never
import org.mockito.Mockito.times
import org.mockito.Mockito.verify

/**
 * Drives the real [OwnedAndroidGattRadio] through `attachConnectedGatt` and the
 * native GATT callback with injected `post`/`scheduleDelayed`, so the single
 * disconnect owner per GATT generation is observed at the Android boundary.
 */
class OwnedAndroidGattDisconnectOwnerTest {
  @Test
  fun mismatchedClosingOwnerIsDistinctFromAnAbsentOwnerAndPreservesItsWaiter() {
    val owners = AndroidGattDisconnectOwners()
    val first = Any()
    val replacement = Any()
    val results = mutableListOf<OwnedRadioTeardownFailure?>()
    val admitted = owners.join("peer", first, 1L, { results.add(it) })
    val absent = owners.markClosing("other", Any(), 3L)
    val mismatch = owners.markClosing("peer", replacement, 2L)
    assertFalse("a mismatched owner must not authorize native close", absent == mismatch)
    assertFalse(owners.isClosing("peer", replacement, 2L))
    assertTrue(owners.isCurrent("peer", admitted.token))
    val waiters = owners.retire("peer", first, 1L)
    assertEquals(1, waiters.size)
    waiters.single().invoke(null)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), results)
  }

  @Test
  fun twoDisconnectCallersBothCompleteOnceWhenAndroidReportsDisconnected() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()

    assertNull(f.radio.disconnect(f.peer, first.callback))
    assertNull(f.radio.disconnect(f.peer, second.callback))
    assertTrue(first.values.isEmpty())
    assertTrue(second.values.isEmpty())

    f.nativeDisconnected(gatt)

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), first.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), second.values)
    verify(gatt, times(1)).disconnect()
    verify(gatt, times(1)).close()
  }

  @Test
  fun laterDisconnectJoinsTheOriginalDeadlineInsteadOfExtendingIt() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()

    f.radio.disconnect(f.peer, first.callback)
    assertEquals(listOf(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS), f.closeDeadlines().map { it.dueAt })
    f.advanceTo(4_000L)
    f.radio.disconnect(f.peer, second.callback)

    // One owner, one deadline: the second caller did not re-arm or move it.
    assertEquals(listOf(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS), f.closeDeadlines().map { it.dueAt })
    assertTrue(first.values.isEmpty() && second.values.isEmpty())

    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), first.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), second.values)
    verify(gatt, times(1)).close()
  }

  @Test
  fun forcedCloseCompletesEveryWaiterOnceAndLateNativeDisconnectedIsInert() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()
    val lost = mutableListOf<Int>()
    f.radio.onConnectionState = { _, connected, status -> if (!connected) lost.add(status) }

    f.radio.disconnect(f.peer, first.callback)
    f.radio.disconnect(f.peer, second.callback)
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    f.nativeDisconnected(gatt)

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), first.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), second.values)
    assertEquals(listOf(BluetoothGatt.GATT_FAILURE), lost)
    verify(gatt, times(1)).close()
  }

  @Test
  fun closeFailureReachesEveryWaiterOnceAndRetainedOwnershipRetriesLater() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val closeRefusal = IllegalStateException("close refused")
    doThrow(closeRefusal).doNothing().`when`(gatt).close()
    val cleanup = mutableListOf<OwnedRadioTeardownFailure>()
    f.radio.onCleanupFailure = { cleanup.add(it) }
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()
    f.radio.disconnect(f.peer, first.callback)
    f.radio.disconnect(f.peer, second.callback)

    f.nativeDisconnected(gatt)

    assertEquals(1, first.values.size)
    assertEquals(1, second.values.size)
    assertSame(closeRefusal, first.values.single()?.throwable)
    assertSame(first.values.single(), second.values.single())
    assertEquals(1, cleanup.size)
    // The retained generation blocks a new connect until the close is retried.
    try {
      f.radio.connect(f.peer, false)
      throw AssertionError("connect must refuse while GATT cleanup is retained")
    } catch (error: IllegalStateException) {
      assertTrue(error.message!!.contains("cleanup is still pending"))
    }
    // A deadline armed before the failure is stale and neither retries nor re-reports.
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, first.values.size)
    assertEquals(1, second.values.size)

    val retry = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, retry.callback))
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), retry.values)
    verify(gatt, times(2)).close()
    assertEquals(1, first.values.size)
    assertEquals(1, second.values.size)
  }

  @Test
  fun staleDeadlineCannotCloseAReplacementGatt() {
    val f = GattRadioFixture()
    val oldGatt = f.connected()
    val oldWaiter = GattDisconnectResults()
    f.radio.disconnect(f.peer, oldWaiter.callback)
    f.nativeDisconnected(oldGatt)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), oldWaiter.values)
    val staleDeadline = f.closeDeadlines().single()

    val replacement = f.connected()
    val replacementWaiter = GattDisconnectResults()
    f.advanceTo(1_000L)
    f.radio.disconnect(f.peer, replacementWaiter.callback)
    assertEquals(2, f.closeDeadlines().size)

    staleDeadline.action()
    assertTrue(replacementWaiter.values.isEmpty())
    verify(replacement, never()).close()

    f.advanceTo(1_000L + OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), replacementWaiter.values)
    verify(replacement, times(1)).close()
    verify(oldGatt, times(1)).close()
    assertEquals(1, oldWaiter.values.size)
  }

  @Test
  fun staleDeadlineCannotCloseAReplacementThatNeverRequestedDisconnect() {
    val f = GattRadioFixture()
    val oldGatt = f.connected()
    f.radio.disconnect(f.peer) {}
    f.nativeDisconnected(oldGatt)
    val replacement = f.connected()

    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)

    verify(replacement, never()).close()
    verify(oldGatt, times(1)).close()
  }

  @Test
  fun unavailableAdapterRouteCompletesAnAlreadyJoinedWaiterOnce() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val joined = GattDisconnectResults()
    f.radio.disconnect(f.peer, joined.callback)
    doReturn(BluetoothAdapter.STATE_OFF).`when`(f.adapter).state
    val immediate = GattDisconnectResults()

    assertNull(f.radio.disconnect(f.peer, immediate.callback))

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), joined.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), immediate.values)
    verify(gatt, times(1)).close()
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, joined.values.size)
  }

  @Test
  fun adapterLossTransitionCompletesEveryWaiterOnce() {
    val f = GattRadioFixture()
    var receiver: BroadcastReceiver? = null
    doAnswer { call -> receiver = call.getArgument(0); null }
      .`when`(f.context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    f.radio.registerAdapterStateReceiver()
    val gatt = f.connected()
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()
    f.radio.disconnect(f.peer, first.callback)
    f.radio.disconnect(f.peer, second.callback)
    val intent = mock(Intent::class.java)
    doReturn(BluetoothAdapter.ACTION_STATE_CHANGED).`when`(intent).action
    doReturn(BluetoothAdapter.STATE_OFF).`when`(intent)
      .getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)

    checkNotNull(receiver).onReceive(f.context, intent)

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), first.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), second.values)
    verify(gatt, times(1)).close()
  }

  @Test
  fun nativeDisconnectCallThrowingCompletesJoinedWaitersOnceWithTheTeardownResult() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    doThrow(IllegalStateException("disconnect refused")).`when`(gatt).disconnect()
    val results = GattDisconnectResults()

    assertNull(f.radio.disconnect(f.peer, results.callback))

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), results.values)
    verify(gatt, times(1)).close()
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, results.values.size)
  }

  @Test
  fun destroyDropsWaitersWithoutCompletingThemAndDeadlineStaysInert() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()
    f.radio.disconnect(f.peer, first.callback)
    f.radio.disconnect(f.peer, second.callback)

    val result = f.radio.destroy()
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    f.nativeDisconnected(gatt)

    // Manager shutdown owns terminal results for pending commands; the radio
    // neither completes nor re-closes anything after destroy.
    assertTrue(result.isSuccessful)
    assertTrue(first.values.isEmpty())
    assertTrue(second.values.isEmpty())
    verify(gatt, times(1)).close()
  }

  /** A connected GATT with one active notification whose native disable can be rejected. */
  private class Subscribed(val gatt: BluetoothGatt, val characteristic: BluetoothGattCharacteristic, val generation: Long) {
    /** While true, the native notification disable is rejected and the ledger entry stays retained. */
    var rejectDisable = true
  }

  private fun subscribed(f: GattRadioFixture, gatt: BluetoothGatt = f.gatt()): Subscribed {
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val cccd = mock(BluetoothGattDescriptor::class.java)
    val serviceUuid = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charUuid = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    doReturn(serviceUuid).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charUuid).`when`(characteristic).uuid
    doReturn(0x10).`when`(characteristic).getProperties()
    doReturn(service).`when`(characteristic).service
    doReturn(cccd).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(OwnedAndroidGattRadio.CCCD_UUID).`when`(cccd).uuid
    doReturn(characteristic).`when`(cccd).characteristic
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    doReturn(BluetoothGatt.GATT_SUCCESS).`when`(gatt).writeDescriptor(eq(cccd), any(ByteArray::class.java))
    doReturn(true).`when`(gatt).writeDescriptor(cccd)
    val generation = f.radio.attachConnectedGatt(f.peer, gatt, listOf(service))
    f.radio.nativeGattCallback().onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_CONNECTED)
    val result = Subscribed(gatt, characteristic, generation)
    doAnswer { !result.rejectDisable }.`when`(gatt).setCharacteristicNotification(characteristic, false)
    val enabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, serviceUuid, 0, charUuid, 0, true, "notification") { enabled.add(it) }
    f.radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
    assertTrue(enabled.single().isSuccess)
    return result
  }

  @Test
  fun cleanupLedgerFailureReachesOnlyTheCallerThatObservedItWhileOtherWaitersKeepTheirOwner() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    val gatt = sub.gatt
    val joined = GattDisconnectResults()
    val observer = GattDisconnectResults()
    f.radio.disconnect(f.peer, joined.callback)
    // A rejected native disable leaves a retained cleanup the next disconnect reports.
    f.radio.nativeGattCallback().onServiceChanged(gatt)
    val failure = f.radio.disconnect(f.peer, observer.callback)

    assertNotNull(failure)
    assertEquals(listOf(failure), observer.values)
    assertTrue(joined.values.isEmpty())
    f.nativeDisconnected(gatt)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), joined.values)
    assertEquals(1, observer.values.size)
    verify(gatt, times(1)).close()
    assertFalse(f.radio.isNativeSubscriptionActive(f.peer, sub.generation, sub.characteristic))
  }

  /**
   * The native `disconnect()` throws while a retained ledger failure exists. The
   * ledger failure keeps priority in the caller's result, but the physical close
   * still runs: [duringThrow] runs inside the throwing native call, i.e. after the
   * owner exists, and lets a second disconnect (ledger now clean) join it.
   */
  private fun throwingNativeDisconnectWithRetainedLedger(
    f: GattRadioFixture,
    sub: Subscribed,
    earlier: GattDisconnectResults
  ): Pair<OwnedRadioTeardownFailure?, GattDisconnectResults> {
    f.radio.nativeGattCallback().onServiceChanged(sub.gatt)
    doAnswer {
      sub.rejectDisable = false
      assertNull(f.radio.disconnect(f.peer, earlier.callback))
      throw IllegalStateException("disconnect refused")
    }.`when`(sub.gatt).disconnect()
    val caller = GattDisconnectResults()
    return f.radio.disconnect(f.peer, caller.callback) to caller
  }

  @Test
  fun nativeDisconnectThrowingWithRetainedLedgerFailureStillClosesOnceAndSettlesEarlierWaiter() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    val lost = mutableListOf<Int>()
    f.radio.onConnectionState = { _, connected, status -> if (!connected) lost.add(status) }
    val earlier = GattDisconnectResults()

    val (failure, caller) = throwingNativeDisconnectWithRetainedLedger(f, sub, earlier)

    // Ledger failure wins the result for the caller that observed it...
    assertNotNull(failure)
    assertEquals(listOf(failure), caller.values)
    // ...while the joined earlier waiter gets the actual teardown outcome exactly once.
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), earlier.values)
    verify(sub.gatt, times(1)).disconnect()
    verify(sub.gatt, times(1)).close()
    // No stranded owner: the deadline is inert and a later disconnect has no GATT to wait on.
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertTrue(lost.isEmpty())
    verify(sub.gatt, times(1)).close()
    assertEquals(1, earlier.values.size)
    val later = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, later.callback))
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), later.values)
    // The next connect is not fenced and joins no stale owner.
    val replacement = f.gatt()
    doReturn(replacement).`when`(f.device)
      .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    f.radio.connect(f.peer, false)
    verify(f.device, times(1)).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    assertEquals(1, earlier.values.size)
  }

  @Test
  fun nativeDisconnectThrowingWithRetainedLedgerFailureKeepsRetainedCloseFailureForEveryWaiter() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    val closeRefusal = IllegalStateException("close refused")
    doThrow(closeRefusal).doNothing().`when`(sub.gatt).close()
    val cleanup = mutableListOf<OwnedRadioTeardownFailure>()
    f.radio.onCleanupFailure = { cleanup.add(it) }
    val earlier = GattDisconnectResults()

    val (failure, caller) = throwingNativeDisconnectWithRetainedLedger(f, sub, earlier)

    assertNotNull(failure)
    assertEquals(listOf(failure), caller.values)
    assertEquals(1, earlier.values.size)
    assertSame(closeRefusal, earlier.values.single()?.throwable)
    assertEquals(1, cleanup.count { it.throwable === closeRefusal })
    try {
      f.radio.connect(f.peer, false)
      throw AssertionError("connect must refuse while GATT cleanup is retained")
    } catch (error: IllegalStateException) {
      assertTrue(error.message!!.contains("cleanup is still pending"))
    }
    // The retained generation is retried by the next disconnect and settles nobody twice.
    val retry = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, retry.callback))
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), retry.values)
    verify(sub.gatt, times(2)).close()
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, earlier.values.size)
    assertEquals(1, caller.values.size)
  }

  /**
   * Runs [binderTeardown] on the first adapter-state read inside the next
   * `disconnect()`: that read sits after `disconnect()` read the GATT and
   * generation and before it joins the owner, which is exactly where the binder
   * thread's own DISCONNECTED teardown races a user disconnect.
   */
  private fun interleaveBeforeJoin(f: GattRadioFixture, binderTeardown: () -> Unit) {
    var armed = true
    doAnswer {
      if (armed) {
        armed = false
        binderTeardown()
      }
      BluetoothAdapter.STATE_ON
    }.`when`(f.adapter).state
  }

  @Test
  fun disconnectRacingTheGenerationsOwnCleanTeardownSettlesOnceWithoutAnOrphanOwner() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val raced = GattDisconnectResults()
    interleaveBeforeJoin(f) { f.nativeDisconnected(gatt) }

    val result = f.radio.disconnect(f.peer, raced.callback)

    // The link was already closed cleanly by its own teardown: that is the outcome.
    assertNull(result)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), raced.values)
    verify(gatt, times(1)).close()
    verify(gatt, never()).disconnect()
    assertTrue(f.closeDeadlines().isEmpty())

    // A replacement generation neither supersedes the old caller nor is closed by it.
    val replacement = f.connected()
    val replacementWaiter = GattDisconnectResults()
    f.advanceTo(1_000L)
    f.radio.disconnect(f.peer, replacementWaiter.callback)
    assertEquals(1, f.closeDeadlines().size)
    f.nativeDisconnected(replacement)
    f.advanceTo(1_000L + OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), raced.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), replacementWaiter.values)
    verify(replacement, times(1)).close()
  }

  @Test
  fun disconnectRacingARetainedCloseFailureReportsThatGenerationsRetryAndNotALaterGatt() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val firstRefusal = IllegalStateException("close refused by teardown")
    val retryRefusal = IllegalStateException("close refused by retry")
    doThrow(firstRefusal).doThrow(retryRefusal).doNothing().`when`(gatt).close()
    val cleanup = mutableListOf<OwnedRadioTeardownFailure>()
    f.radio.onCleanupFailure = { cleanup.add(it) }
    val raced = GattDisconnectResults()
    interleaveBeforeJoin(f) { f.nativeDisconnected(gatt) }

    val result = f.radio.disconnect(f.peer, raced.callback)

    // The binder teardown's close failed and is retained; this caller's own retry of that
    // exact generation also failed, and that is what it reports - once, from one source.
    assertNotNull(result)
    assertSame(retryRefusal, result?.throwable)
    assertEquals(listOf(result), raced.values)
    assertEquals(listOf<Throwable>(firstRefusal, retryRefusal), cleanup.map { it.throwable })
    verify(gatt, never()).disconnect()
    assertTrue(f.closeDeadlines().isEmpty())
    try {
      f.radio.connect(f.peer, false)
      throw AssertionError("connect must refuse while GATT cleanup is retained")
    } catch (error: IllegalStateException) {
      assertTrue(error.message!!.contains("cleanup is still pending"))
    }
    val retry = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, retry.callback))
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), retry.values)
    verify(gatt, times(3)).close()
    assertEquals(1, raced.values.size)
  }

  @Test
  fun disconnectJoiningWhileTheGenerationIsBeingClosedIsSettledByTheTeardownSweepOnce() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val joinedMidClose = GattDisconnectResults()
    val losses = mutableListOf<Int>()
    f.radio.onConnectionState = { _, connected, status -> if (!connected) losses.add(status) }
    // Close is fenced before the native call: a disconnect arriving reentrantly joins the
    // closing owner and cannot create a second deadline or native disconnect.
    doAnswer {
      f.nativeDisconnected(gatt)
      assertNull(f.radio.disconnect(f.peer, joinedMidClose.callback))
      assertEquals(1, f.closeDeadlines().size)
      null
    }.`when`(gatt).close()
    val starter = GattDisconnectResults()
    f.radio.disconnect(f.peer, starter.callback)

    f.nativeDisconnected(gatt)

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), starter.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), joinedMidClose.values)
    assertEquals(listOf(BluetoothGatt.GATT_SUCCESS), losses)
    verify(gatt, times(1)).close()
    // The orphaned owner's deadline is inert and a replacement is not blamed for it.
    val replacement = f.connected()
    val replacementWaiter = GattDisconnectResults()
    f.radio.disconnect(f.peer, replacementWaiter.callback)
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS * 2)
    assertEquals(1, joinedMidClose.values.size)
    assertEquals(1, starter.values.size)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), replacementWaiter.values)
    verify(replacement, times(1)).close()
  }

  @Test
  fun refusedCloseDeadlineClosesTheGenerationExplicitlyAndSettlesTheWaiterOnce() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    f.refuseDeadline = true
    doAnswer {
      f.nativeDisconnected(gatt)
      null
    }.`when`(gatt).close()
    val lost = mutableListOf<Int>()
    f.radio.onConnectionState = { _, connected, status -> if (!connected) lost.add(status) }
    val waiter = GattDisconnectResults()

    // No deadline can bound the native callback, so nothing may wait for it.
    assertNull(f.radio.disconnect(f.peer, waiter.callback))

    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), waiter.values)
    verify(gatt, times(1)).close()
    verify(gatt, never()).disconnect()
    assertEquals(listOf(BluetoothGatt.GATT_FAILURE), lost)
    assertTrue(f.closeDeadlines().isEmpty())
    // A late native DISCONNECTED and a later disconnect are inert: no stranded owner remains.
    f.nativeDisconnected(gatt)
    val later = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, later.callback))
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), later.values)
    assertEquals(1, waiter.values.size)
    verify(gatt, times(1)).close()
    val replacement = f.gatt()
    connectGattReturns(f, replacement)
    f.radio.connect(f.peer, false)
    assertEquals(1, connectGattCalls(f))
  }

  @Test
  fun refusedCloseDeadlineWithCloseFailureRetainsOwnershipAndNeverReportsRelease() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    f.refuseDeadline = true
    val closeRefusal = IllegalStateException("close refused")
    doThrow(closeRefusal).doNothing().`when`(gatt).close()
    val cleanup = mutableListOf<OwnedRadioTeardownFailure>()
    f.radio.onCleanupFailure = { cleanup.add(it) }
    val waiter = GattDisconnectResults()

    val failure = f.radio.disconnect(f.peer, waiter.callback)

    assertNotNull(failure)
    assertSame(closeRefusal, failure?.throwable)
    assertEquals(listOf(failure), waiter.values)
    assertEquals(1, cleanup.size)
    verify(gatt, never()).disconnect()
    assertTrue(f.closeDeadlines().isEmpty())
    try {
      f.radio.connect(f.peer, false)
      throw AssertionError("connect must refuse while GATT cleanup is retained")
    } catch (error: IllegalStateException) {
      assertTrue(error.message!!.contains("cleanup is still pending"))
    }
    val retry = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, retry.callback))
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), retry.values)
    verify(gatt, times(2)).close()
    assertEquals(1, waiter.values.size)
  }

  @Test
  fun rejectedCloseDeadlineSchedulerFailsClosedThenRethrowsTheSchedulerError() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val rejection = java.util.concurrent.RejectedExecutionException("scheduler is shut down")
    f.rejectDeadline = rejection
    val waiter = GattDisconnectResults()

    val thrown = thrownBy { f.radio.disconnect(f.peer, waiter.callback) }

    // Cleanup and settlement happened first; the scheduler error is never swallowed.
    assertSame(rejection, thrown)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), waiter.values)
    verify(gatt, times(1)).close()
    verify(gatt, never()).disconnect()
    f.nativeDisconnected(gatt)
    assertEquals(1, waiter.values.size)
    verify(gatt, times(1)).close()
  }

  @Test
  fun refusedCloseDeadlineWithLedgerFailureStillClosesAndReportsTheLedgerFailureOnce() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    f.radio.nativeGattCallback().onServiceChanged(sub.gatt)
    f.refuseDeadline = true
    val caller = GattDisconnectResults()

    val failure = f.radio.disconnect(f.peer, caller.callback)

    assertNotNull(failure)
    assertEquals(listOf(failure), caller.values)
    verify(sub.gatt, times(1)).close()
    verify(sub.gatt, never()).disconnect()
    assertTrue(f.closeDeadlines().isEmpty())
  }

  @Test
  fun throwingCleanupObserverCannotStarveWaitersOfAFailedClose() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val closeRefusal = IllegalStateException("close refused")
    doThrow(closeRefusal).doNothing().`when`(gatt).close()
    val observer = Boom("diagnostic observer")
    var reports = 0
    f.radio.onCleanupFailure = { reports += 1; throw observer }
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()
    f.radio.disconnect(f.peer, first.callback)
    f.radio.disconnect(f.peer, second.callback)

    val thrown = thrownBy { f.nativeDisconnected(gatt) }

    // Every waiter was settled once before the observer's error left the radio.
    assertSame(observer, thrown)
    assertEquals(1, reports)
    assertEquals(1, first.values.size)
    assertEquals(1, second.values.size)
    assertSame(closeRefusal, first.values.single()?.throwable)
    assertSame(first.values.single(), second.values.single())
    // Ownership stays retained and no deadline acts on it.
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, first.values.size)
    assertEquals(1, second.values.size)
    assertTrue(thrownBy { f.radio.connect(f.peer, false) }?.message!!.contains("cleanup is still pending"))
    val retry = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, retry.callback))
    verify(gatt, times(2)).close()
  }

  @Test
  fun observerAndWaiterErrorsAreBothPreservedAndTheOtherWaiterStillSettles() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    doThrow(IllegalStateException("close refused")).doNothing().`when`(gatt).close()
    val observer = Boom("diagnostic observer")
    f.radio.onCleanupFailure = { throw observer }
    val waiterBoom = Boom("waiter")
    val survivor = GattDisconnectResults()
    f.radio.disconnect(f.peer) { throw waiterBoom }
    f.radio.disconnect(f.peer, survivor.callback)

    val thrown = thrownBy { f.nativeDisconnected(gatt) }

    assertSame(waiterBoom, thrown)
    assertTrue(thrown!!.suppressed.any { it === observer })
    assertEquals(1, survivor.values.size)
    assertNotNull(survivor.values.single())
  }

  @Test
  fun throwingConnectionObserverCannotStarveTheDeadlinesForcedClose() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val observer = Boom("connection observer")
    f.radio.onConnectionState = { _, connected, _ -> if (!connected) throw observer }
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()
    f.radio.disconnect(f.peer, first.callback)
    f.radio.disconnect(f.peer, second.callback)

    val thrown = thrownBy { f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS) }

    assertSame(observer, thrown)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), first.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), second.values)
    verify(gatt, times(1)).close()
    f.nativeDisconnected(gatt)
    assertEquals(1, first.values.size)
  }
}
