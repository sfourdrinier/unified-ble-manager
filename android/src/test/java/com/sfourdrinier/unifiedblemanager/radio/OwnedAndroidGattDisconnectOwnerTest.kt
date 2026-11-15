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
import com.sfourdrinier.unifiedblemanager.rustcore.OwnedRadioPort
import com.sfourdrinier.unifiedblemanager.rustcore.RadioPortEvents
import com.sfourdrinier.unifiedblemanager.rustcore.CharacteristicInstance
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread
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
  fun destroySettlesJoinedWaitersAndDeadlineStaysInert() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val first = GattDisconnectResults()
    val second = GattDisconnectResults()
    f.radio.disconnect(f.peer, first.callback)
    f.radio.disconnect(f.peer, second.callback)

    val result = f.radio.destroy()
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    f.nativeDisconnected(gatt)

    assertTrue(result.isSuccessful)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), first.values)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), second.values)
    verify(gatt, times(1)).close()
  }

  @Test
  fun destroyReportsFailedCloseToWaitersAndRetriesTheSameGatt() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val refusal = IllegalStateException("close refused")
    doThrow(refusal).doNothing().`when`(gatt).close()
    val results = GattDisconnectResults()
    f.radio.disconnect(f.peer, results.callback)

    val first = f.radio.destroy()
    assertFalse(first.isSuccessful)
    assertSame(refusal, checkNotNull(results.values.single()).throwable)
    assertTrue(f.radio.destroy().isSuccessful)
    assertEquals(1, results.values.size)
    verify(gatt, times(2)).close()
  }

  @Test
  fun concurrentDestroyCannotReportSuccessWhileGattCloseIsInFlight() {
    val f = GattRadioFixture()
    val gatt = f.connected()
    val entered = CountDownLatch(1)
    val leave = CountDownLatch(1)
    doAnswer {
      entered.countDown()
      check(leave.await(5, TimeUnit.SECONDS))
      null
    }.`when`(gatt).close()
    val results = GattDisconnectResults()
    f.radio.disconnect(f.peer, results.callback)
    val worker = thread { f.radio.destroy() }
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      assertFalse(f.radio.destroy().isSuccessful)
      assertTrue(results.values.isEmpty())
    } finally {
      leave.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), results.values)
    assertTrue(f.radio.destroy().isSuccessful)
    verify(gatt, times(1)).close()
  }

  @Test
  fun concurrentDestroyWaitsForDisconnectWaiterToReturn() {
    val f = GattRadioFixture()
    f.connected()
    val entered = CountDownLatch(1)
    val leave = CountDownLatch(1)
    f.radio.disconnect(f.peer) {
      entered.countDown()
      check(leave.await(5, TimeUnit.SECONDS))
    }
    val worker = thread { f.radio.destroy() }
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      assertFalse(f.radio.destroy().isSuccessful)
    } finally {
      leave.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
    assertTrue(f.radio.destroy().isSuccessful)
  }

  /** A connected GATT with one active notification whose native disable can be rejected. */
  private class Subscribed(val gatt: BluetoothGatt, val characteristic: BluetoothGattCharacteristic, val generation: Long) {
    /** While true, the native notification disable is rejected and the ledger entry stays retained. */
    var rejectDisable = true
  }

  private fun subscribed(f: GattRadioFixture, gatt: BluetoothGatt = f.gatt(), peer: String = f.peer, port: OwnedRadioPort? = null): Subscribed {
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
    val generation = f.radio.attachConnectedGatt(peer, gatt, listOf(service))
    f.radio.nativeGattCallback().onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_CONNECTED)
    val result = Subscribed(gatt, characteristic, generation)
    doAnswer { !result.rejectDisable }.`when`(gatt).setCharacteristicNotification(characteristic, false)
    val enabled = mutableListOf<Result<Unit>>()
    if (port == null) f.radio.setNotifyExact(peer, serviceUuid, 0, charUuid, 0, true, "notification") { enabled.add(it) }
    else port.setNotification(CharacteristicInstance(peer, serviceUuid.toString(), 0, charUuid.toString(), 0), true, "notification", 1) { enabled.add(it) }
    f.radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
    assertTrue(enabled.single().isSuccess)
    return result
  }

  private fun errorTree(error: Throwable?): List<Throwable> = if (error == null) emptyList() else
    listOf(error) + errorTree(error.cause) + error.suppressed.flatMap { errorTree(it) }

  private fun errorMessages(error: Throwable?) = errorTree(error).mapNotNull { it.message }

  private fun retainedCleanupKeys(f: GattRadioFixture): Set<String> {
    val field = OwnedAndroidGattRadio::class.java.getDeclaredField("retryableCleanups")
    field.isAccessible = true
    return (field.get(f.radio) as Map<*, *>).keys.filterIsInstance<String>().toSet()
  }

  private fun asynchronousClosePreservesCallerFailures(timeout: Boolean) {
    val f = GattRadioFixture()
    val firstPort = OwnedRadioPort(f.radio) {}
    val secondPort = OwnedRadioPort(f.radio) {}
    firstPort.setEvents(mock(RadioPortEvents::class.java))
    secondPort.setEvents(mock(RadioPortEvents::class.java))
    val sub = subscribed(f, port = firstPort)
    f.radio.nativeGattCallback().onServiceChanged(sub.gatt)
    val close = IllegalStateException("asynchronous close refused")
    doThrow(close).`when`(sub.gatt).close()
    val first = mutableListOf<Throwable?>()
    val second = mutableListOf<Throwable?>()
    firstPort.disconnect(f.peer) { first.add(it) }
    secondPort.disconnect(f.peer) { second.add(it) }
    assertTrue(first.isEmpty())
    assertTrue(second.isEmpty())
    if (timeout) f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    else f.nativeDisconnected(sub.gatt)
    assertEquals(1, first.size)
    assertEquals(1, second.size)
    assertTrue(errorMessages(first.single()).contains("setCharacteristicNotification invalidation was rejected"))
    assertTrue(errorTree(first.single()).any { it === close })
    assertTrue(errorTree(second.single()).any { it === close })
    assertFalse(errorMessages(second.single()).contains("setCharacteristicNotification invalidation was rejected"))
    f.advanceTo(OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS)
    assertEquals(1, first.size)
    assertEquals(1, second.size)
  }

  @Test fun nativeCallbackCombinesEachCallersCleanupAndCloseFailure() = asynchronousClosePreservesCallerFailures(false)

  @Test fun timeoutCombinesEachCallersCleanupAndCloseFailure() = asynchronousClosePreservesCallerFailures(true)

  @Test
  fun successfulExactGenerationCloseRetiresOnlyItsLedgerEntries() {
    val f = GattRadioFixture()
    val old = subscribed(f)
    f.radio.nativeGattCallback().onServiceChanged(old.gatt)
    val oldEntries = retainedCleanupKeys(f)
    assertEquals(1, oldEntries.size)
    val otherPeer = "A0:9E:1A:00:00:02"
    val otherDevice = mock(BluetoothDevice::class.java)
    val otherGatt = mock(BluetoothGatt::class.java)
    doReturn(otherPeer).`when`(otherDevice).address
    doReturn(otherDevice).`when`(otherGatt).device
    val other = subscribed(f, otherGatt, otherPeer)
    f.radio.nativeGattCallback().onServiceChanged(other.gatt)
    val both = retainedCleanupKeys(f)
    assertEquals(2, both.size)
    doThrow(IllegalStateException("close failed")).doNothing().`when`(old.gatt).close()
    f.nativeDisconnected(old.gatt)
    assertEquals(both, retainedCleanupKeys(f))
    f.radio.disconnect(f.peer)
    assertEquals(both - oldEntries, retainedCleanupKeys(f))
  }

  @Test
  fun disconnectThroughPortReportsBothCleanupAndCloseFailures() {
    val f = GattRadioFixture()
    val port = OwnedRadioPort(f.radio) {}
    port.setEvents(mock(RadioPortEvents::class.java))
    val sub = subscribed(f, port = port)
    f.radio.nativeGattCallback().onServiceChanged(sub.gatt)
    val close = IllegalStateException("close refused through port")
    doThrow(IllegalStateException("disconnect refused")).`when`(sub.gatt).disconnect()
    doThrow(close).`when`(sub.gatt).close()
    var result: Throwable? = null
    port.disconnect(f.peer) { result = it }
    assertTrue(errorTree(result).any { it === close })
    assertTrue(errorMessages(result).contains("setCharacteristicNotification invalidation was rejected"))
    assertTrue(errorMessages(result).contains("notification GATT generation is retiring"))
  }

  @Test
  fun disconnectOtherPeerDoesNotRetryOrReportRetainedCleanup() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    f.radio.nativeGattCallback().onServiceChanged(sub.gatt)
    val otherPeer = "A0:9E:1A:00:00:02"
    val otherDevice = mock(BluetoothDevice::class.java)
    val otherGatt = mock(BluetoothGatt::class.java)
    doReturn(otherPeer).`when`(otherDevice).address
    doReturn(otherDevice).`when`(otherGatt).device
    f.radio.attachConnectedGatt(otherPeer, otherGatt, emptyList())
    val result = GattDisconnectResults()
    assertNull(f.radio.disconnect(otherPeer, result.callback))
    assertTrue(result.values.isEmpty())
    verify(sub.gatt, times(1)).setCharacteristicNotification(sub.characteristic, false)
    f.nativeDisconnected(otherGatt)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), result.values)
  }

  @Test
  fun disconnectFromAnotherPortDoesNotRetryOrReportForeignOwnerCleanup() {
    val f = GattRadioFixture()
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    first.setEvents(mock(RadioPortEvents::class.java))
    second.setEvents(mock(RadioPortEvents::class.java))
    val sub = subscribed(f, port = first)
    f.radio.nativeGattCallback().onServiceChanged(sub.gatt)
    val foreign = mutableListOf<Throwable?>()
    second.disconnect(f.peer) { foreign.add(it) }
    assertTrue(foreign.isEmpty())
    verify(sub.gatt, times(1)).setCharacteristicNotification(sub.characteristic, false)
    val owned = mutableListOf<Throwable?>()
    first.disconnect(f.peer) { owned.add(it) }
    assertTrue(owned.isEmpty())
    f.nativeDisconnected(sub.gatt)
    assertEquals(listOf<Throwable?>(null), foreign)
    assertTrue(errorMessages(owned.single()).contains("setCharacteristicNotification invalidation was rejected"))
  }

  @Test
  fun disconnectReplacementGenerationDoesNotRetryOldGenerationCleanup() {
    val f = GattRadioFixture()
    val old = subscribed(f)
    f.radio.nativeGattCallback().onServiceChanged(old.gatt)
    f.nativeDisconnected(old.gatt)
    val replacement = f.connected()
    val result = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, result.callback))
    assertTrue(result.values.isEmpty())
    verify(old.gatt, times(1)).setCharacteristicNotification(old.characteristic, false)
    f.nativeDisconnected(replacement)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), result.values)
  }

  @Test
  fun disconnectReportsEveryCleanupFailureForTheSameOwnerAndGeneration() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    val service = sub.characteristic.service
    val other = mock(BluetoothGattCharacteristic::class.java)
    val descriptor = mock(BluetoothGattDescriptor::class.java)
    val otherUuid = UUID.fromString("00002a38-0000-1000-8000-00805f9b34fb")
    doReturn(otherUuid).`when`(other).uuid
    doReturn(service).`when`(other).service
    doReturn(0x10).`when`(other).properties
    doReturn(descriptor).`when`(other).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(other).`when`(descriptor).characteristic
    doReturn(OwnedAndroidGattRadio.CCCD_UUID).`when`(descriptor).uuid
    doReturn(listOf(sub.characteristic, other)).`when`(service).characteristics
    doReturn(true).`when`(sub.gatt).setCharacteristicNotification(other, true)
    doReturn(BluetoothGatt.GATT_SUCCESS).`when`(sub.gatt).writeDescriptor(eq(descriptor), any(ByteArray::class.java))
    doReturn(true).`when`(sub.gatt).writeDescriptor(descriptor)
    val refusal = IllegalStateException("second disable refused")
    doThrow(refusal).`when`(sub.gatt).setCharacteristicNotification(other, false)
    val enabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, service.uuid, 0, otherUuid, 0, true, "notification") { enabled.add(it) }
    f.radio.nativeGattCallback().onDescriptorWrite(sub.gatt, descriptor, BluetoothGatt.GATT_SUCCESS)
    assertTrue(enabled.single().isSuccess)
    f.radio.nativeGattCallback().onServiceChanged(sub.gatt)
    val failure = f.radio.disconnect(f.peer)
    assertTrue(errorMessages(failure?.throwable).contains("setCharacteristicNotification invalidation was rejected"))
    assertTrue(errorTree(failure?.throwable).any { it === refusal })
    assertEquals(2, errorMessages(failure?.throwable).count { it == "notification GATT generation is retiring" })
  }

  @Test
  fun serviceChangeContinuesInvalidationAndObserversAfterPendingCallbackThrows() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    val (source, group) = f.radio.openOrderedSourceGroup(sub.gatt, sub.generation, setOf(sub.characteristic), 8, 32) {}
    source.register(group, sub.characteristic)
    doReturn(true).`when`(sub.gatt).requestMtu(100)
    val failure = IllegalStateException("pending callback failed")
    val observer = IllegalStateException("service observer failed")
    var observed = 0
    f.radio.requestMtu(f.peer, 100) { throw failure }
    f.radio.onServicesChanged = { observed++; throw observer }
    val error = thrownBy { f.radio.nativeGattCallback().onServiceChanged(sub.gatt) }
    assertSame(failure, error)
    assertTrue(errorTree(error).any { it === observer })
    assertEquals(1, observed)
    assertFalse(f.radio.isNativeSubscriptionActive(f.peer, sub.generation, sub.characteristic))
    assertEquals(OrderedNotificationSource.Reason.DATABASE_CHANGED,
      (source.drain(group).single() as OrderedNotificationSource.Event.Terminal).reason)
    verify(sub.gatt, times(1)).setCharacteristicNotification(sub.characteristic, false)
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
    assertTrue(observer.values.isEmpty())
    assertTrue(joined.values.isEmpty())
    f.nativeDisconnected(gatt)
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), joined.values)
    assertEquals(listOf(failure), observer.values)
    verify(gatt, times(1)).close()
    assertFalse(f.radio.isNativeSubscriptionActive(f.peer, sub.generation, sub.characteristic))
  }

  /**
   * The native `disconnect()` throws while a retained ledger failure exists. The
   * ledger failure keeps priority in the caller's result, but the physical close
   * still runs. A second disconnect inside the throwing native call observes its
   * own retained cleanup failure while the generation is already fenced.
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
  fun throwingLedgerDiagnosticDoesNotPreventDisconnectOwnership() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    f.radio.nativeGattCallback().onServiceChanged(sub.gatt)
    val diagnostic = IllegalStateException("cleanup diagnostic")
    f.radio.onCleanupFailure = { throw diagnostic }
    val result = GattDisconnectResults()

    assertSame(diagnostic, thrownBy { f.radio.disconnect(f.peer, result.callback) })
    assertTrue(result.values.isEmpty())
    verify(sub.gatt, times(1)).disconnect()
    f.nativeDisconnected(sub.gatt)
    assertEquals(1, result.values.size)
    assertTrue(result.values.single()!!.operation.startsWith("serviceChangedDisable:"))
    verify(sub.gatt, times(1)).close()
  }

  @Test
  fun nativeDisconnectThrowingWithRetainedLedgerFailureClosesAndSettlesBothCallers() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    val lost = mutableListOf<Int>()
    f.radio.onConnectionState = { _, connected, status -> if (!connected) lost.add(status) }
    val earlier = GattDisconnectResults()

    val (failure, caller) = throwingNativeDisconnectWithRetainedLedger(f, sub, earlier)

    // Ledger failure wins the result for the caller that observed it...
    assertNotNull(failure)
    assertEquals(listOf(failure), caller.values)
    // The reentrant caller independently observes the still-retained cleanup while
    // this generation is fenced and joins the same close waiter.
    assertEquals(1, earlier.values.size)
    assertEquals(failure?.operation, earlier.values.single()?.operation)
    assertTrue(errorMessages(earlier.values.single()?.throwable).contains("notification GATT generation is retiring"))
    assertTrue(errorMessages(earlier.values.single()?.throwable).contains("setCharacteristicNotification invalidation was rejected"))
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
  fun nativeDisconnectThrowingRetainsCloseFailureAndReportsEachCallersLedgerFailure() {
    val f = GattRadioFixture()
    val sub = subscribed(f)
    val closeRefusal = IllegalStateException("close refused")
    doThrow(closeRefusal).doNothing().`when`(sub.gatt).close()
    val cleanup = mutableListOf<OwnedRadioTeardownFailure>()
    f.radio.onCleanupFailure = { cleanup.add(it) }
    val earlier = GattDisconnectResults()

    val (failure, caller) = throwingNativeDisconnectWithRetainedLedger(f, sub, earlier)

    assertNotNull(failure)
    assertEquals(failure?.operation, caller.values.single()?.operation)
    assertTrue(errorTree(caller.values.single()?.throwable).any { it === closeRefusal })
    assertTrue(errorTree(earlier.values.single()?.throwable).any { it === closeRefusal })
    assertEquals(1, earlier.values.size)
    assertEquals(failure?.operation, earlier.values.single()?.operation)
    assertTrue(errorMessages(earlier.values.single()?.throwable).contains("notification GATT generation is retiring"))
    assertTrue(errorMessages(earlier.values.single()?.throwable).contains("setCharacteristicNotification invalidation was rejected"))
    assertTrue(errorTree(failure?.throwable).any { it === closeRefusal })
    assertEquals(1, cleanup.count { it.throwable === closeRefusal })
    try {
      f.radio.connect(f.peer, false)
      throw AssertionError("connect must refuse while GATT cleanup is retained")
    } catch (error: IllegalStateException) {
      assertTrue(error.message!!.contains("cleanup is still pending"))
    }
    // The retained generation is retried by the next disconnect and settles nobody twice.
    val retry = GattDisconnectResults()
    val retryFailure = f.radio.disconnect(f.peer, retry.callback)
    assertEquals(failure?.operation, retryFailure?.operation)
    assertEquals(listOf(retryFailure), retry.values)
    verify(sub.gatt, times(2)).close()
    val afterClose = GattDisconnectResults()
    assertNull(f.radio.disconnect(f.peer, afterClose.callback))
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), afterClose.values)
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
