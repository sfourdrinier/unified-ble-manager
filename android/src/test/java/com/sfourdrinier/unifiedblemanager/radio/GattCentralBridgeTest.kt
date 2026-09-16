// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/GattCentralBridgeTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import java.util.ArrayDeque
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * HOST-JVM tests for the HOST-ANDROID GATT bridge runtime (no emulator, no
 * BLE hardware): wire shapes, fail-fast validation, permission fail-closed
 * behavior, binder-thread non-blocking posts, and observation parsing.
 */
class GattCentralBridgeTest {

  private val direct = Executor { command -> command.run() }

  @Test
  fun scanStartWireShape() {
    assertEquals(
      "scan.start|owner-a|5000|1000|180d,180f|all|none",
      GattCentralWire.scanStart("owner-a", 5000, 1000, listOf("180d", "180f"), "all", "none")
    )
  }

  @Test
  fun notifyWireEncodesHexAndBoundsValues() {
    assertEquals(
      "notify.deliver|3|0102ff",
      GattCentralWire.notifyDeliver(3, byteArrayOf(0x01, 0x02, 0xFF.toByte()))
    )
    try {
      GattCentralWire.notifyDeliver(0, ByteArray(GattCentralWire.NOTIFY_MAX_BYTES + 1))
      fail("oversize notify must reject before JNI")
    } catch (expected: IllegalArgumentException) {
      assertTrue(expected.message!!.contains("exceeds"))
    }
    // Hex doubles the size on the wire: a max-size value must still fit the
    // enqueue ceiling, or Kotlin validation would disagree with Rust.
    val maxLine = GattCentralWire.notifyDeliver(0, ByteArray(GattCentralWire.NOTIFY_MAX_BYTES))
    assertTrue(
      "max notify line must fit WIRE_MAX, was ${maxLine.length}",
      maxLine.length <= GattCentralWire.WIRE_MAX
    )
  }

  @Test
  fun wireRejectsPipeInjection() {
    try {
      GattCentralWire.linkEstablished("peer|evil")
      fail("pipe injection must reject")
    } catch (expected: IllegalArgumentException) {
      assertTrue(expected.message!!.contains("must not contain"))
    }
  }

  @Test
  fun writeStartSupportsAbsentMaximum() {
    assertEquals(
      "write.start|0|with-response|4|-|true|5000|1000",
      GattCentralWire.writeStart(0, "with-response", 4, null, true, 5000, 1000)
    )
  }

  @Test
  fun pathRegisterSupportsAbsentLevels() {
    assertEquals(
      "path.register|peer|180d|0|2a37|0|-|-|11|lease-a",
      GattCentralWire.pathRegister("peer", "180d", 0, "2a37", 0, null, null, 11, "lease-a")
    )
  }

  @Test
  fun postWithoutPermissionFailsClosedWithoutEnqueue() {
    val queued = ArrayDeque<String>()
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = { "" },
      hasBlePermissions = { false },
      worker = direct
    )
    val result = bridge.postEvent(GattCentralWire.linkEstablished("peer"))
    assertTrue(result is UbmGattCentralBridge.PostResult.PermissionDenied)
    assertEquals(
      "permission.denied|rn-android-boundary|postEvent|ble-permission-missing",
      (result as UbmGattCentralBridge.PostResult.PermissionDenied).identity
    )
    assertTrue("denied event must not reach the core queue", queued.isEmpty())
  }

  @Test
  fun postEnqueuesAndDrainsOnWorker() {
    val queued = ArrayDeque<String>()
    var drained = 0
    val seen = mutableListOf<GattObservation>()
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = {
        drained++
        queued.map { "{\"ok\":true,\"event\":\"probe\"}" }.joinToString("\n").also { queued.clear() }
      },
      hasBlePermissions = { true },
      onObservations = { seen.addAll(it) },
      worker = direct
    )
    val result = bridge.postEvent(GattCentralWire.expireSweep(1000))
    assertEquals(UbmGattCentralBridge.PostResult.Queued(1), result)
    assertEquals(1, drained)
    assertEquals(1, seen.size)
    assertTrue(seen[0].ok)
  }

  @Test
  fun enqueueFailureSurfacesWithoutThrowing() {
    val bridge = UbmGattCentralBridge(
      enqueue = { throw IllegalStateException("jni-detached") },
      drain = { "" },
      hasBlePermissions = { true },
      worker = direct
    )
    val result = bridge.postEvent(GattCentralWire.release())
    assertTrue(result is UbmGattCentralBridge.PostResult.EnqueueFailed)
    assertEquals(
      "jni-detached",
      (result as UbmGattCentralBridge.PostResult.EnqueueFailed).message
    )
  }

  @Test
  fun releaseOnDestroyEnqueuesReleaseAndStopsWorker() {
    val queued = ArrayDeque<String>()
    var drains = 0
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = {
        drains++
        "{\"ok\":true,\"event\":\"release\",\"state\":\"released\"}"
      },
      hasBlePermissions = { true },
      worker = direct
    )
    val observations = bridge.releaseOnDestroy()
    assertEquals(listOf("release"), queued.toList())
    assertEquals(1, drains)
    assertEquals("release", observations.single().event)
    // Worker stays stopped after destroy: later posts enqueue but schedule
    // nothing (no second drain).
    bridge.postEvent(GattCentralWire.expireSweep(2000))
    assertEquals(1, drains)
  }

  @Test
  fun releaseOnDestroyWithoutPermissionReportsIdentity() {
    val bridge = UbmGattCentralBridge(
      enqueue = { 1 },
      drain = { "" },
      hasBlePermissions = { false },
      worker = direct
    )
    val observations = bridge.releaseOnDestroy()
    assertEquals(1, observations.size)
    assertFalse(observations[0].ok)
    assertEquals("permission.denied", observations[0].code)
  }

  @Test
  fun parseObservationsKeepsFailClosedIdentities() {
    val parsed = GattCentralWire.parseObservations(
      "{\"ok\":true,\"event\":\"scan.start\",\"op\":\"op-1\"}\n" +
        "{\"ok\":false,\"event\":\"read.start\",\"code\":\"gatt.stale-handle\"," +
        "\"domain\":\"gatt\",\"operation\":\"gatt-drain\",\"detail\":\"central-rejected\"}"
    )
    assertEquals(2, parsed.size)
    assertTrue(parsed[0].ok)
    assertEquals("scan.start", parsed[0].event)
    assertFalse(parsed[1].ok)
    assertEquals("gatt.stale-handle", parsed[1].code)
    assertEquals("gatt", parsed[1].domain)
    assertEquals("empty drain parses to no observations", 0,
      GattCentralWire.parseObservations("").size)
  }
}
