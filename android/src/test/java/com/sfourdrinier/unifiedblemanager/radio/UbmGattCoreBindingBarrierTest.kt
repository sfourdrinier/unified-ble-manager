// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/UbmGattCoreBindingBarrierTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.content.Context
import android.content.pm.PackageManager
import java.util.ArrayDeque
import java.util.concurrent.Executor
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
  fun postAdapterResetBarsLateScanStartObservations() {
    val jni = FakeJni()
    val bound = binding(jni)
    jni.drains.add(scanStartDrain("scan-op-2"))
    bound.postScanStart(listOf("180d"), true)
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
