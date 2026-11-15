package com.sfourdrinier.unifiedblemanager.rustcore

import android.content.Context
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import com.sfourdrinier.unifiedblemanager.radio.GattConnectAttempt
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattNotSubmitted
import com.sfourdrinier.unifiedblemanager.radio.GattRadioFixture
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.ubm.core.MobileCoreBridge
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executor
import java.util.concurrent.TimeUnit
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.mock
import org.mockito.Mockito.doAnswer
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.doThrow
import org.mockito.Mockito.never
import org.mockito.Mockito.verify
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq

/** Host-JVM tests of adapter ownership; no Android radio or JNI is involved. */
class ConnectCleanupConcurrencyTest {
  private class Fixture(radio: OwnedAndroidGattRadio = OwnedAndroidGattRadio(
    mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true }
  )) {
    val owner = OwnedOrderedGroupPort(radio)
    val fake = FakeRadio()
    val callbacks = mutableListOf<(Throwable?) -> Unit>()
    val attempts = mutableListOf<GattConnectAttempt>()
    var connectHook: ((GattConnectAttempt) -> Unit)? = null
    var cancelHook: ((String, GattConnectAttempt, (Throwable?) -> Unit) -> Unit)? = null
    val port: AndroidRadioPort = object : AndroidRadioPort by fake, OrderedGroupPort by owner {
      override fun connect(peerId: String, autoConnect: Boolean, phyMask: Int, attempt: GattConnectAttempt) {
        fake.connect(peerId, autoConnect, phyMask, attempt)
        connectHook?.invoke(attempt)
      }
      override fun cancelConnect(peerId: String, attempt: GattConnectAttempt, onComplete: (Throwable?) -> Unit) {
        attempts.add(attempt)
        val hook = cancelHook
        if (hook == null) callbacks.add(onComplete) else hook(peerId, attempt, onComplete)
      }
    }
    val fakeCore = FakeCore()
    var unitHook: ((Long) -> Unit)? = null
    var failureHook: ((Long) -> Unit)? = null
    var status: MobileCoreBridge.OwnerRetirementResult? = null
    val core = object : MobileCorePort by fakeCore {
      override fun completeUnit(requestId: Long): Int { unitHook?.invoke(requestId); return fakeCore.completeUnit(requestId) }
      override fun completeFailure(requestId: Long, failure: RadioFailure): Int { failureHook?.invoke(requestId); return fakeCore.completeFailure(requestId, failure) }
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        status = result
        return MobileCoreBridge.STATUS_DELIVERED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val retirement = UUID.randomUUID()
    fun fence() = adapter.ownerFence(90, owner.orderedAdmissionOwner(3), retirement.toString())
    fun status() = adapter.ownerStatus(91, owner.orderedAdmissionOwner(3), retirement.toString())
    fun work() = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
    fun connect() = adapter.connect(1, "peer", false, emptyArray(), "generation")
  }

  @Test fun fullPeerQueueRefusalRetiresOnlyTheUnsubmittedCallbackOnRetry() {
    val real = GattRadioFixture(peerLifecycleCapacity = 1)
    val f = Fixture(real.radio)
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val workerFailure = java.util.concurrent.atomic.AtomicReference<Throwable?>()
    doAnswer {
      entered.countDown()
      check(release.await(10, TimeUnit.SECONDS))
      real.gatt()
    }.`when`(real.device).connectGatt(eq(real.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val realPort = OwnedRadioPort(real.radio) {}
    f.cancelHook = { _, attempt, callback -> realPort.cancelConnect(real.peer, attempt, callback) }
    f.connect()
    val worker = Thread {
      try { real.radio.connect(real.peer, false, 0, GattConnectAttempt()) }
      catch (error: Throwable) { workerFailure.set(error) }
    }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      real.radio.connect(real.peer, false, 0, GattConnectAttempt())
      f.adapter.disconnect(3, "peer")
      f.fence()
      assertEquals(1, f.work())
      assertEquals("fenced-pending", f.status!!.state)
      val statusFailure = f.status!!.failures.single { it.detail.contains("peer lifecycle deferred operation capacity exhausted") }
      assertEquals("peer lifecycle deferred operation capacity exhausted", statusFailure.detail)
      assertEquals("platform", statusFailure.failureKind)
      assertFalse(statusFailure.dispatched)
      val disconnectFailure = f.fakeCore.failures.getValue(3)
      assertEquals(RadioFailureKind.PLATFORM, disconnectFailure.kind)
      assertEquals(statusFailure.detail, disconnectFailure.detail)
      assertFalse(disconnectFailure.dispatched)
    } finally {
      release.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
    assertNull(workerFailure.get())
    f.adapter.disconnect(2, "peer")
    assertEquals(2, f.attempts.size)
    assertSame(f.attempts[0], f.attempts[1])
    assertTrue(f.fakeCore.calls.contains("unit:2"))
    f.status()
    assertEquals("fenced-pending", f.status!!.state)
    assertTrue(f.status!!.failures.none { it.detail.contains("peer lifecycle deferred operation capacity exhausted") })
    assertEquals(0, f.work())
  }

  @Test fun uncertainThrowKeepsFailureAndCallbackOwnedUntilLateSettlement() {
    val f = Fixture()
    val error = IllegalStateException("submission outcome unknown")
    f.cancelHook = { _, _, callback -> f.callbacks.add(callback); if (f.callbacks.size == 1) throw error }
    f.connect()
    f.adapter.cancel(1)
    f.fence()
    assertEquals(1, f.work())
    assertTrue(f.status!!.failures.any { it.detail.contains("submission outcome unknown") })
    f.adapter.disconnect(2, "peer")
    assertSame(f.attempts[0], f.attempts[1])
    f.callbacks[1](null)
    f.status()
    assertEquals(1, f.work())
    assertEquals("fenced-pending", f.status!!.state)
    assertTrue(f.status!!.failures.any { it.detail.contains("submission outcome unknown") })
    f.callbacks[0](null)
    f.status()
    assertEquals(0, f.work())
    assertTrue(f.status!!.failures.none { it.detail.contains("submission outcome unknown") })
  }

  @Test fun lateFailureAfterSuccessfulRetryRemainsOwnedUntilAnotherRetrySucceeds() {
    val f = Fixture()
    f.cancelHook = { _, _, callback ->
      f.callbacks.add(callback)
      if (f.callbacks.size == 1) throw IllegalStateException("submission outcome unknown")
    }
    f.connect()
    f.adapter.cancel(1)
    f.fence()
    f.adapter.disconnect(2, "peer")
    f.callbacks[1](null)
    assertTrue(f.fakeCore.calls.contains("unit:2"))
    f.callbacks[0](IllegalStateException("late original cleanup failed"))
    f.status()
    assertEquals(1, f.work())
    assertTrue(f.status!!.failures.any { it.detail == "late original cleanup failed" })
    f.adapter.disconnect(3, "peer")
    assertEquals(3, f.attempts.size)
    assertSame(f.attempts[0], f.attempts[2])
    f.callbacks[2](null)
    assertTrue(f.fakeCore.calls.contains("unit:3"))
    f.status()
    assertEquals(0, f.work())
    assertTrue(f.status!!.failures.none { it.detail == "late original cleanup failed" })
  }

  @Test fun lateOriginalAndFailedRetriesRemainVisibleUntilLaterSuccess() {
    val f = Fixture()
    f.cancelHook = { _, _, callback ->
      f.callbacks.add(callback)
      if (f.callbacks.size == 1) throw IllegalStateException("original submission uncertain")
      if (f.callbacks.size == 4) throw IllegalStateException("retry threw")
    }
    f.connect()
    f.adapter.cancel(1)
    f.fence()
    f.adapter.disconnect(2, "peer")
    f.callbacks[1](null)
    f.callbacks[0](IllegalStateException("late original failed"))
    f.adapter.disconnect(3, "peer")
    f.callbacks[2](IllegalStateException("retry callback failed"))
    f.status()
    assertTrue(f.status!!.failures.any { it.detail == "late original failed" })
    assertTrue(f.status!!.failures.any { it.detail == "retry callback failed" })
    f.adapter.disconnect(4, "peer")
    f.status()
    assertTrue(f.status!!.failures.any { it.detail == "late original failed" })
    assertTrue(f.status!!.failures.any { it.detail == "retry callback failed" })
    assertTrue(f.status!!.failures.any { it.detail == "retry threw" })
    f.adapter.disconnect(5, "peer")
    f.callbacks[4](null)
    f.callbacks[3](null)
    f.status()
    assertEquals(0, f.work())
    assertTrue(f.status!!.failures.none { it.detail in listOf("late original failed", "retry callback failed", "retry threw") })
  }

  @Test fun fullGraphQueueReportsCleanupWasNotSubmitted() {
    val real = GattRadioFixture()
    val gatt = real.gatt()
    doReturn(gatt).`when`(real.device).connectGatt(eq(real.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    doReturn(true).`when`(gatt).discoverServices()
    doReturn(emptyList<android.bluetooth.BluetoothGattService>()).`when`(gatt).services
    val f = Fixture(real.radio)
    val realPort = OwnedRadioPort(real.radio) {}
    var reported: Throwable? = null
    f.connectHook = { attempt -> real.radio.connect(real.peer, false, 0, attempt) }
    f.cancelHook = { _, attempt, callback -> realPort.cancelConnect(real.peer, attempt) { reported = it; callback(it) } }
    f.connect()
    real.radio.discover(real.peer) { check(it.isSuccess) }
    real.radio.nativeGattCallback().onServicesDiscovered(gatt, BluetoothGatt.GATT_SUCCESS)
    val graph = real.radio.withNativeGraph(real.peer) { Unit }.first
    val receipt = (real.radio.reserveGraphAuthority(1uL) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
    val authority = (real.radio.openGraphAuthority(receipt) as OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Opened).authority
    val request = OwnedAndroidGattRadio.GraphPermitRequest(authority, 1uL, real.peer, "connection", graph)
    val permit = (real.radio.reserveGraphPermit(request) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
    val key = OwnedAndroidGattRadio.GraphPermitKey(authority, real.peer, "connection", graph, permit)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, real.radio.acquireGraphPermit(key))
    repeat(64) { real.radio.nativeGattCallback().onMtuChanged(gatt, 23, BluetoothGatt.GATT_SUCCESS) }
    f.adapter.disconnect(3, "peer")
    val failure = f.fakeCore.failures.getValue(3)
    assertEquals("graph permit deferred operation capacity exhausted", failure.detail)
    assertFalse(failure.dispatched)
    assertTrue(reported is AndroidGattNotSubmitted)
    assertTrue(reported!!.cause is IllegalStateException)
    assertEquals(failure.detail, reported!!.cause!!.message)
    f.fence()
    val statusFailure = f.status!!.failures.single { it.detail == failure.detail }
    assertFalse(statusFailure.dispatched)
    verify(gatt, never()).disconnect()
    verify(gatt, never()).close()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.INVALIDATED, real.radio.commitGraphPermit(key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, real.radio.releaseGraphPermit(key))
    f.adapter.disconnect(4, "peer")
    real.advanceTo(10000)
    assertTrue(f.fakeCore.calls.contains("unit:4"))
    f.status()
    assertTrue(f.status!!.failures.none { it.detail == failure.detail })
  }

  @Test fun disconnectJoiningAfterRetrySuccessReceivesLateOriginalFailure() {
    val f = Fixture()
    f.cancelHook = { _, _, callback ->
      f.callbacks.add(callback)
      if (f.callbacks.size == 1) throw IllegalStateException("submission outcome unknown")
    }
    f.connect()
    f.adapter.cancel(1)
    f.adapter.disconnect(2, "peer")
    f.callbacks[1](null)
    f.adapter.disconnect(3, "peer")
    assertFalse(f.fakeCore.calls.contains("unit:3"))
    f.callbacks[0](IllegalStateException("late original cleanup failed"))
    assertEquals("late original cleanup failed", f.fakeCore.failures.getValue(3).detail)
    f.adapter.disconnect(4, "peer")
    f.callbacks[2](null)
    assertTrue(f.fakeCore.calls.contains("unit:4"))
  }

  @Test fun originalFailureDuringRetryStaysVisibleUntilRetrySettles() {
    val f = Fixture()
    f.cancelHook = { _, _, callback ->
      f.callbacks.add(callback)
      if (f.callbacks.size == 1) throw IllegalStateException("submission outcome unknown")
    }
    f.connect()
    f.adapter.cancel(1)
    f.fence()
    f.adapter.disconnect(2, "peer")
    f.callbacks[0](IllegalStateException("original cleanup failed"))
    f.status()
    assertEquals(1, f.work())
    assertTrue(f.status!!.failures.any { it.detail == "original cleanup failed" })
    assertFalse(f.fakeCore.calls.any { it == "unit:2" || it.startsWith("failure:2:") })
    f.callbacks[1](null)
    f.status()
    assertTrue(f.fakeCore.calls.contains("unit:2"))
    assertEquals(0, f.work())
    assertTrue(f.status!!.failures.none { it.detail == "original cleanup failed" })
  }

  @Test fun synchronousSuccessThenWithdrawalKeepsCleanupOwned() {
    val f = Fixture()
    f.connectHook = { attempt ->
      f.fake.events.onConnection("peer", true, 0, attempt)
      f.adapter.disconnect(2, "peer")
    }
    f.connect()
    f.fence()
    assertEquals(1, f.work())
    f.callbacks.single()(null)
    assertEquals(0, f.work())
  }

  @Test fun overlappingDuplicateCallbackCannotRetireFirstCompletion() {
    val f = Fixture()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    f.unitHook = { if (it == 2L) { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) } }
    f.connect()
    f.adapter.disconnect(2, "peer")
    f.fence()
    val worker = Thread { f.callbacks.single()(null) }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      f.callbacks.single()(null)
      assertEquals(1, f.work())
    } finally { release.countDown(); worker.join(5000) }
    assertFalse(worker.isAlive)
    assertEquals(0, f.work())
  }

  @Test fun failedCleanupIsVisibleAndRetriedForExactAttempt() {
    val f = Fixture()
    f.connect()
    f.adapter.cancel(1)
    val error = IllegalStateException("cleanup refused")
    f.callbacks.single()(error)
    f.fence()
    assertEquals("fenced-pending", f.status?.state)
    assertTrue(f.status!!.failures.any { it.detail.contains("cleanup refused") })
    f.adapter.disconnect(2, "peer")
    assertEquals(2, f.attempts.size)
    assertSame(f.attempts[0], f.attempts[1])
    f.callbacks[1](null)
    assertTrue(f.fakeCore.calls.contains("unit:2"))
    f.status()
    assertTrue(f.status!!.failures.none { it.detail.contains("cleanup refused") })
  }

  @Test fun failureWhileWithdrawalIsDeliveringSettlesEveryJoinedWaiter() {
    val f = Fixture()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    f.failureHook = { if (it == 1L) { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) } }
    f.connect()
    val worker = Thread { f.adapter.disconnect(2, "peer") }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      f.adapter.disconnect(3, "peer")
      assertTrue(f.callbacks.isEmpty())
    } finally { release.countDown(); worker.join(5000) }
    assertFalse(worker.isAlive)
    f.callbacks.single()(IllegalStateException("cleanup failed"))
    assertTrue(f.fakeCore.calls.any { it.startsWith("failure:2:") })
    assertTrue(f.fakeCore.calls.any { it.startsWith("failure:3:") })
    assertFalse(f.fakeCore.calls.any { it == "unit:2" || it == "unit:3" })
  }

  @Test fun throwingConnectCompletionDoesNotPreventWithdrawalCleanup() {
    val f = Fixture()
    f.failureHook = { if (it == 1L) throw IllegalStateException("connect delivery failed") }
    f.connect()
    f.adapter.disconnect(2, "peer")
    assertEquals(1, f.callbacks.size)
    f.callbacks.single()(null)
    assertTrue(f.fakeCore.calls.contains("unit:2"))
    f.fence()
    assertTrue(f.status!!.failures.any { it.detail.contains("connect delivery failed") })
  }

  @Test fun throwingFailureDeliveryStillSettlesOtherWaiters() {
    val f = Fixture()
    f.failureHook = { if (it == 2L) throw IllegalStateException("failure delivery failed") }
    f.connect()
    f.adapter.disconnect(2, "peer")
    f.adapter.disconnect(3, "peer")
    f.callbacks.single()(IllegalStateException("cleanup refused"))
    assertTrue(f.fakeCore.calls.any { it.startsWith("failure:3:") })
    f.fence()
    assertTrue(f.status!!.failures.any { it.detail.contains("failure delivery failed") })
    assertTrue(f.status!!.failures.any { it.detail.contains("cleanup refused") })
    f.adapter.disconnect(4, "peer")
    f.callbacks[1](null)
    f.status()
    assertTrue(f.status!!.failures.any { it.detail == "cleanup refused" })
    assertTrue(f.status!!.failures.any { it.detail == "failure delivery failed" })
  }

  @Test fun successfulRetryOverlappingThrowingFailureDeliveryRetainsBothOutcomes() {
    val f = Fixture()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    f.failureHook = { if (it == 2L) {
      entered.countDown()
      check(release.await(5, TimeUnit.SECONDS))
      throw IllegalStateException("blocked delivery failed")
    } }
    f.connect()
    f.adapter.disconnect(2, "peer")
    val worker = Thread { f.callbacks[0](SecurityException("original cleanup denied")) }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      f.adapter.disconnect(3, "peer")
      f.callbacks[1](null)
      assertTrue(f.fakeCore.calls.contains("unit:3"))
    } finally { release.countDown(); worker.join(5000) }
    assertFalse(worker.isAlive)
    f.fence()
    assertTrue(f.status!!.failures.any { it.detail == "original cleanup denied" && it.failureKind == "permission-denied" })
    assertTrue(f.status!!.failures.any { it.detail == "blocked delivery failed" })
  }

  @Test fun realPortCloseFailureSurvivesThrowingDeliveryAndSuccessfulRetry() {
    val real = GattRadioFixture()
    val gatt = real.gatt()
    doReturn(gatt).`when`(real.device).connectGatt(eq(real.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    doThrow(SecurityException("native close denied")).doNothing().`when`(gatt).close()
    val f = Fixture(real.radio)
    val realPort = OwnedRadioPort(real.radio) {}
    f.connectHook = { attempt -> real.radio.connect(real.peer, false, 0, attempt) }
    f.cancelHook = { _, attempt, callback -> realPort.cancelConnect(real.peer, attempt, callback) }
    f.failureHook = { if (it == 2L) throw IllegalStateException("native failure delivery failed") }
    f.connect()
    f.adapter.disconnect(2, "peer")
    real.nativeDisconnected(gatt)
    f.adapter.disconnect(3, "peer")
    assertTrue(f.fakeCore.calls.contains("unit:3"))
    f.fence()
    assertTrue(f.status!!.failures.any { it.detail == "native close denied" && it.failureKind == "permission-denied" })
    assertTrue(f.status!!.failures.any { it.detail == "native failure delivery failed" })
  }

  @Test fun throwingWaiterDoesNotStrandOtherWaitersAndIsReported() {
    val f = Fixture()
    f.unitHook = { if (it == 2L) throw IllegalStateException("delivery failed") }
    f.connect()
    f.adapter.disconnect(2, "peer")
    f.adapter.disconnect(3, "peer")
    try { f.callbacks.single()(null) } catch (_: IllegalStateException) { }
    assertTrue(f.fakeCore.calls.contains("unit:3"))
    f.fence()
    assertTrue(f.status!!.failures.any { it.detail.contains("delivery failed") })
  }
}
