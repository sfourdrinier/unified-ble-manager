// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/UbmGattCoreBindingBarrierTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.content.Context
import android.content.pm.PackageManager
import java.util.ArrayDeque
import java.util.concurrent.Executor
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.Mockito

/**
 * R02 (core-shadow/bridge authority race) barriers for [UbmGattCoreBinding].
 *
 * The bridge drains on a worker thread while stops run on the dispatcher
 * thread: a `central.scan-start` observation for an already-stopped scan can
 * land after [UbmGattCoreBinding.postScanStop] (or
 * [UbmGattCoreBinding.postAdapterReset]) cleared the tracked op. Without a
 * post-stop barrier that late callback resurrects the dead op and the next
 * stop posts a stale `scan.stop` for a scan that no longer exists. Likewise a
 * callback landing after [UbmGattCoreBinding.shutdown] must not reach the
 * (gone) owner. These tests pin the barriers; each failing test names the
 * stale line it would have posted.
 */
class UbmGattCoreBindingBarrierTest {

  private val direct = Executor { command -> command.run() }

  @Test
  fun heldRealCoreAdmissionCannotOverwriteReplacementAfterReset() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val tasks = ArrayDeque<Runnable>()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val error = AtomicReference<Throwable>()
    val bound = UbmGattCoreBinding(permittedContext(), jni = UbmGattCoreBinding.RealCoreJni,
      worker = Executor { tasks.add(it) }, clockMs = { 1000L }, onCoreRejection = {
        if (it.event == "invalid") { entered.countDown(); release.await() }
      })
    bound.bridge.postEvent("invalid")
    bound.postScanStart(emptyList(), true, "old-command")
    val thread = Thread {
      try { tasks.removeFirst().run() } catch (t: Throwable) { error.set(t) }
    }
    try {
      thread.start()
      assertTrue("worker must hold the old observation", entered.await(5, TimeUnit.SECONDS))
      bound.postAdapterReset()
      bound.bridge.drainNow()
      bound.postScanStart(emptyList(), true, "replacement-command")
      bound.bridge.drainNow()
      val replacement = bound.currentScanOperation()
      assertTrue("replacement must be admitted", replacement != null)
      assertEquals(UbmGattCoreBinding.ScanAdmission.Missing, bound.currentScanAdmission("old-command"))
      assertEquals(UbmGattCoreBinding.ScanAdmission.Admitted(replacement!!), bound.currentScanAdmission("replacement-command"))
      release.countDown()
      thread.join(5000)
      assertTrue("worker did not finish", !thread.isAlive)
      error.get()?.let { throw AssertionError(it) }
      assertEquals("old admission must not replace the new generation", replacement, bound.currentScanOperation())
      assertTrue(bound.postScanStop(replacement) is UbmGattCentralBridge.PostResult.Queued)
      bound.bridge.drainNow()
      assertTrue(bound.postScanStopped(replacement) is UbmGattCentralBridge.PostResult.Queued)
      bound.bridge.drainNow()
      assertEquals(true, bound.scanSettlementVerdict(replacement))
    } finally {
      release.countDown()
      thread.join(5000)
      bound.shutdown()
    }
  }

  @Test
  fun heldOldSettlementCannotAttestReplacement() {
    val jni = FakeJni()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val failure = AtomicReference<Throwable>()
    val bound = UbmGattCoreBinding(permittedContext(), jni = jni,
      worker = Executor { }, clockMs = { 1000L }, onCoreRejection = {
        if (it.event == "invalid") { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
      })
    val thread = Thread {
      try { bound.bridge.drainNow() } catch (t: Throwable) { failure.set(t) }
    }
    try {
      bound.postScanStart(emptyList(), true, "old")
      jni.drains.add(scanStartDrain("old-op")); bound.bridge.drainNow()
      bound.postScanStop("old-op"); bound.postScanStopped("old-op")
      jni.drains.add("""{"ok":false,"event":"invalid","effects":[],"observations":[]}
{"ok":true,"event":"scan.platform-event","effects":[],"observations":[]}""")
      thread.start()
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      bound.postAdapterReset()
      jni.drains.add("""{"ok":true,"event":"adapter.reset","effects":[],"observations":[]}""")
      bound.bridge.drainNow(); bound.forgetAdapterReset()
      bound.postScanStart(emptyList(), true, "new")
      jni.drains.add(scanStartDrain("new-op")); bound.bridge.drainNow()
      bound.postScanStop("new-op"); bound.postScanStopped("new-op")
      release.countDown(); thread.join(5000)
      assertTrue(!thread.isAlive)
      failure.get()?.let { throw AssertionError(it) }
      assertNull("old verdict must not settle the replacement", bound.scanSettlementVerdict("new-op"))
      jni.drains.add("""{"ok":true,"event":"scan.platform-event","effects":[],"observations":[]}""")
      bound.bridge.drainNow()
      assertEquals(true, bound.scanSettlementVerdict("new-op"))
    } finally { release.countDown(); thread.join(5000); bound.shutdown() }
  }

  @Test
  fun heldRejectedSettlementDoesNotEnterReplacementRejectionJournal() {
    val jni = FakeJni()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val rejections = java.util.Collections.synchronizedList(mutableListOf<String>())
    val bound = UbmGattCoreBinding(permittedContext(), jni = jni,
      worker = Executor { }, clockMs = { 1000L }, onCoreRejection = {
        if (it.event == "invalid") { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
        rejections.add(it.event)
      })
    val thread = Thread { bound.bridge.drainNow() }
    try {
      bound.postScanStart(emptyList(), true, "old")
      jni.drains.add(scanStartDrain("old-op")); bound.bridge.drainNow()
      bound.postScanStop("old-op"); bound.postScanStopped("old-op")
      jni.drains.add("""{"ok":false,"event":"invalid","effects":[],"observations":[]}
{"ok":false,"event":"scan.platform-event","effects":[],"observations":[]}""")
      thread.start()
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      bound.postAdapterReset()
      jni.drains.add("""{"ok":true,"event":"adapter.reset","effects":[],"observations":[]}""")
      bound.bridge.drainNow(); bound.forgetAdapterReset()
      bound.postScanStart(emptyList(), true, "new")
      jni.drains.add(scanStartDrain("new-op")); bound.bridge.drainNow()
      bound.postScanStop("new-op"); bound.postScanStopped("new-op")
      release.countDown(); thread.join(5000)
      assertTrue(!thread.isAlive)
      jni.drains.add("""{"ok":true,"event":"scan.platform-event","effects":[],"observations":[]}""")
      bound.bridge.drainNow()
      assertEquals(true, bound.scanSettlementVerdict("new-op"))
      assertEquals(listOf("invalid"), rejections.toList())
    } finally { release.countDown(); thread.join(5000); bound.shutdown() }
  }

  @Test
  fun rejectedSettlementCanBeRetriedForTheSameOperation() {
    val jni = FakeJni()
    val bound = binding(jni)
    jni.drains.add(scanStartDrain("scan-op")); bound.postScanStart(emptyList(), true)
    bound.postScanStop("scan-op")
    jni.drains.add("""{"ok":false,"event":"scan.platform-event","effects":[],"observations":[]}""")
    bound.postScanStopped("scan-op")
    assertEquals(false, bound.scanSettlementVerdict("scan-op"))
    jni.drains.add("""{"ok":true,"event":"scan.platform-event","effects":[],"observations":[]}""")
    assertTrue(bound.postScanStopped("scan-op") is UbmGattCentralBridge.PostResult.Queued)
    assertEquals(true, bound.scanSettlementVerdict("scan-op"))
    bound.shutdown()
  }

  private class FakeJni(
    var linkedRevision: String = UbmGattCoreBinding.CONTRACT_REVISION,
    var drains: ArrayDeque<String> = ArrayDeque()
  ) : UbmGattCoreBinding.CoreJni {
    val enqueued = mutableListOf<String>()
    var closed = 0

    override fun open(revision: String): Long = 7L
    override fun revision(): String = linkedRevision
    override fun enqueue(handle: Long, wire: String): Int {
      enqueued.add(wire)
      return enqueued.size
    }

    // No handle gate: an in-flight callback does not know the session has
    // closed, which is exactly the race under test.
    override fun drain(handle: Long): String =
      if (drains.isEmpty()) "" else drains.removeFirst()

    override fun depth(handle: Long): Int = enqueued.size
    override fun close(handle: Long) {
      closed++
    }
  }

  private fun permittedContext(): Context {
    val context = Mockito.mock(Context::class.java)
    Mockito.`when`(context.applicationContext).thenReturn(context)
    Mockito.`when`(context.checkSelfPermission(Mockito.anyString())).thenReturn(
      PackageManager.PERMISSION_GRANTED
    )
    return context
  }

  private fun binding(
    jni: FakeJni,
    rejections: MutableList<GattObservation> = mutableListOf()
  ): UbmGattCoreBinding {
    return UbmGattCoreBinding(
      permittedContext(),
      jni = jni,
      onCoreRejection = { rejections.add(it) },
      worker = direct,
      clockMs = { 1000L }
    )
  }

  private fun scanStartDrain(op: String): String =
    "{\"ok\":true,\"event\":\"scan\",\"op\":\"$op\"," +
      "\"observations\":[{\"kind\":\"central.scan-start\",\"op\":\"$op\",\"detail\":\"\"}],\"effects\":[]}"

  private fun rejectionDrain(): String =
    "{\"ok\":false,\"event\":\"connect\",\"code\":\"peer.not-found\",\"domain\":\"connection\"," +
      "\"operation\":\"connection.connect\",\"detail\":\"unknown\",\"effects\":[],\"observations\":[]}"

  @Test
  fun postScanStopBarsLateScanStartObservations() {
    val jni = FakeJni()
    val bound = binding(jni)
    // Live scan: the start drains and the core-minted op is tracked.
    jni.drains.add(scanStartDrain("scan-op-1"))
    bound.postScanStart(listOf("180d"), true)
    // Stop consumes the op and posts the stop line.
    val stop = bound.postScanStop()
    assertTrue(stop is UbmGattCentralBridge.PostResult.Queued)
    assertTrue(jni.enqueued.last().startsWith("scan.stop|scan-op-1|"))
    val stopsPosted = jni.enqueued.size
    // The dead scan's in-flight callback lands AFTER the stop.
    jni.drains.add(scanStartDrain("scan-op-1"))
    bound.bridge.drainNow()
    // The late observation must not resurrect the op: no second stop.
    assertNull(bound.postScanStop())
    assertEquals(stopsPosted, jni.enqueued.size)
  }

  @Test
  fun queuedResetRetainsIdentityUntilItsOwnVerdict() {
    val jni = FakeJni()
    val bound = binding(jni)
    jni.drains.add(scanStartDrain("scan-op-pending-reset"))
    bound.postScanStart(listOf("180d"), true)
    bound.postAdapterReset()
    assertEquals("scan-op-pending-reset", bound.currentScanOperation())
    assertNull(bound.adapterResetVerdict())
    jni.drains.add("""{"ok":true,"event":"adapter.reset","effects":[],"observations":[]}""")
    bound.bridge.drainNow()
    assertEquals(true, bound.adapterResetVerdict())
    assertNull(bound.currentScanOperation())
  }

  @Test
  fun postAdapterResetBarsLateScanStartObservations() {
    val jni = FakeJni()
    val bound = binding(jni)
    jni.drains.add(scanStartDrain("scan-op-2"))
    bound.postScanStart(listOf("180d"), true)
    jni.drains.add("""{"ok":true,"event":"adapter.reset","effects":[],"observations":[]}""")
    bound.postAdapterReset()
    val linesPosted = jni.enqueued.size
    // The dead scan's in-flight callback lands AFTER the reset.
    jni.drains.add(scanStartDrain("scan-op-2"))
    bound.bridge.drainNow()
    // The reset cleared the op for good: a stop now is a null shadow.
    assertNull(bound.postScanStop())
    assertEquals(linesPosted, jni.enqueued.size)
  }

  @Test
  fun scanStartRearmsAfterStop() {
    val jni = FakeJni()
    val bound = binding(jni)
    jni.drains.add(scanStartDrain("scan-op-1"))
    bound.postScanStart(listOf("180d"), true)
    bound.postScanStop()
    // A new scan re-arms tracking: its op must flow to the next stop.
    jni.drains.add(scanStartDrain("scan-op-2"))
    bound.postScanStart(listOf("180d"), true)
    val stop = bound.postScanStop()
    assertTrue(stop is UbmGattCentralBridge.PostResult.Queued)
    assertTrue(jni.enqueued.last().startsWith("scan.stop|scan-op-2|"))
  }

  @Test
  fun shutdownDropsLateCallbacks() {
    val jni = FakeJni()
    val rejections = mutableListOf<GattObservation>()
    val bound = binding(jni, rejections)
    bound.shutdown()
    // An in-flight rejection lands AFTER shutdown: the owner is gone, so
    // nothing may be forwarded and nothing may be posted.
    jni.drains.add(rejectionDrain())
    bound.bridge.drainNow()
    assertTrue(rejections.isEmpty())
    assertNull(bound.postScanStart(listOf("180d"), true))
    assertNull(bound.postScanStop())
  }
}
