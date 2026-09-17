// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/UbmGattCoreBindingTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.content.Context
import android.content.pm.PackageManager
import java.util.ArrayDeque
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.Mockito

/**
 * HOST-JVM tests for the production shared-core binding (no emulator, no
 * BLE hardware, no native library): session open/revision gating, exact wire
 * lines for the lifecycle/link/scan/discovery slice, scan-op tracking,
 * permission fail-closed behavior, rejection forwarding, and release order.
 */
class UbmGattCoreBindingTest {

  private val direct = Executor { command -> command.run() }

  private class FakeJni(
    var linkedRevision: String = UbmGattCoreBinding.CONTRACT_REVISION,
    var openFailure: Throwable? = null,
    var drains: ArrayDeque<String> = ArrayDeque()
  ) : UbmGattCoreBinding.CoreJni {
    val opened = mutableListOf<String>()
    val enqueued = mutableListOf<String>()
    var closed = 0

    override fun open(revision: String): Long {
      opened.add(revision)
      openFailure?.let { throw it }
      return 7L
    }

    override fun revision(): String = linkedRevision
    override fun enqueue(handle: Long, wire: String): Int {
      check(handle == 7L) { "enqueue on foreign handle $handle" }
      enqueued.add(wire)
      return enqueued.size
    }

    override fun drain(handle: Long): String {
      check(handle == 7L) { "drain on foreign handle $handle" }
      return if (drains.isEmpty()) "" else drains.removeFirst()
    }

    override fun depth(handle: Long): Int = enqueued.size
    override fun close(handle: Long) {
      check(handle == 7L) { "close on foreign handle $handle" }
      closed++
    }
  }

  private fun permittedContext(granted: Boolean = true): Context {
    val context = Mockito.mock(Context::class.java)
    Mockito.`when`(context.applicationContext).thenReturn(context)
    Mockito.`when`(context.checkSelfPermission(Mockito.anyString())).thenReturn(
      if (granted) PackageManager.PERMISSION_GRANTED else PackageManager.PERMISSION_DENIED
    )
    return context
  }

  private fun binding(
    jni: FakeJni,
    granted: Boolean = true,
    rejections: MutableList<GattObservation> = mutableListOf()
  ): UbmGattCoreBinding {
    return UbmGattCoreBinding(
      permittedContext(granted),
      jni = jni,
      onCoreRejection = { rejections.add(it) },
      worker = direct,
      clockMs = { 1000L }
    )
  }

  @Test
  fun openGatesTheLinkedRevision() {
    val jni = FakeJni()
    val bound = binding(jni)
    assertTrue(bound.isOpen)
    assertNull(bound.openFailure)
    assertEquals(listOf(UbmGattCoreBinding.CONTRACT_REVISION), jni.opened)
  }

  @Test
  fun foreignLinkedRevisionFailsClosedAndCloses() {
    val jni = FakeJni(linkedRevision = "C-UBM.9.9.9-DRAFT")
    val bound = binding(jni)
    assertFalse(bound.isOpen)
    assertNotNull(bound.openFailure)
    assertTrue(bound.openFailure!!.contains("protocol.incompatible"))
    assertEquals(1, jni.closed)
    assertNull(bound.postScanStart(listOf("180d"), true))
  }

  @Test
  fun openThrowFailsClosedWithoutBricking() {
    val jni = FakeJni(openFailure = UnsatisfiedLinkError("no ubm5_jni_echo in java.library.path"))
    val bound = binding(jni)
    assertFalse(bound.isOpen)
    assertNotNull(bound.openFailure)
    assertTrue(bound.openFailure!!.contains("platform.failure"))
  }

  @Test
  fun scanStartPostsTheExactWireLine() {
    val jni = FakeJni()
    val bound = binding(jni)
    val result = bound.postScanStart(listOf("180d", "180f"), true)
    assertTrue(result is UbmGattCentralBridge.PostResult.Queued)
    assertEquals(1, jni.enqueued.size)
    val line = jni.enqueued.single()
    assertTrue(line, line.startsWith("scan.start|android-protocol-scan|2147483647|"))
    assertTrue(line, line.endsWith("|180d,180f|all|none"))
  }

  @Test
  fun duplicatePolicyFollowsAllowDuplicates() {
    val jni = FakeJni()
    val bound = binding(jni)
    bound.postScanStart(listOf("180d"), false)
    assertTrue(jni.enqueued.single().endsWith("|180d|first|none"))
  }

  @Test
  fun connectResolvesThenConnectsWithTheDerivedKey() {
    val jni = FakeJni()
    val bound = binding(jni)
    bound.postConnect("AA:BB:CC:DD:EE:FF", "android-link-AA:BB:CC:DD:EE:FF")
    assertEquals(2, jni.enqueued.size)
    assertEquals("peer.resolve|public-address|AA:BB:CC:DD:EE:FF", jni.enqueued[0])
    assertTrue(
      jni.enqueued[1],
      jni.enqueued[1].startsWith("connect|public-address:AA:BB:CC:DD:EE:FF|android-link-AA:BB:CC:DD:EE:FF|2147483647|")
    )
  }

  @Test
  fun opaqueDeviceIdsUsePlatformGuid() {
    val jni = FakeJni()
    val bound = binding(jni)
    assertEquals("platform-guid:opaque-1", bound.peerKeyFor("opaque-1"))
    bound.postLinkEstablished("opaque-1")
    assertEquals("link.established|platform-guid:opaque-1", jni.enqueued.single())
  }

  @Test
  fun linkAndDiscoveryLifecyclePostsDerivedKeys() {
    val jni = FakeJni()
    val bound = binding(jni)
    bound.postLinkEstablished("AA:BB:CC:DD:EE:FF")
    bound.postDiscoveryBegin("AA:BB:CC:DD:EE:FF")
    bound.postDiscoveryComplete("AA:BB:CC:DD:EE:FF")
    bound.postServicesChanged("AA:BB:CC:DD:EE:FF")
    bound.postLinkReleased("AA:BB:CC:DD:EE:FF")
    bound.postPeerLoss("AA:BB:CC:DD:EE:FF")
    bound.postDisconnect("AA:BB:CC:DD:EE:FF", "android-link-X")
    bound.postAdapterReset()
    val key = "public-address:AA:BB:CC:DD:EE:FF"
    assertTrue(jni.enqueued.any { it == "link.established|$key" })
    assertTrue(jni.enqueued.any { it == "discovery.begin|$key" })
    assertTrue(jni.enqueued.any { it == "discovery.complete|$key" })
    assertTrue(jni.enqueued.any { it == "services-changed|$key" })
    assertTrue(jni.enqueued.any { it == "link.released|$key" })
    assertTrue(jni.enqueued.any { it.startsWith("peer.loss|$key|") })
    assertTrue(jni.enqueued.any { it.startsWith("disconnect|$key|android-link-X|") })
    assertTrue(jni.enqueued.any { it.startsWith("adapter.reset|") })
  }

  @Test
  fun scanStopTracksTheCoreOpFromDrain() {
    val jni = FakeJni()
    jni.drains.add(
      "{\"ok\":true,\"event\":\"scan\",\"op\":\"scan-op-9\"," +
        "\"observations\":[{\"kind\":\"central.scan-start\",\"op\":\"scan-op-9\",\"detail\":\"\"}],\"effects\":[]}"
    )
    val bound = binding(jni)
    bound.postScanStart(listOf("180d"), true)
    val result = bound.postScanStop()
    assertTrue(result is UbmGattCentralBridge.PostResult.Queued)
    val stop = jni.enqueued.last()
    assertTrue(stop, stop.startsWith("scan.stop|scan-op-9|"))
  }

  @Test
  fun scanStopWithoutAnOpIsANullShadow() {
    val jni = FakeJni()
    val bound = binding(jni)
    assertNull(bound.postScanStop())
    assertTrue(jni.enqueued.isEmpty())
  }

  @Test
  fun permissionDeniedFailsClosedWithoutEnqueue() {
    val jni = FakeJni()
    val bound = binding(jni, granted = false)
    val result = bound.postScanStart(listOf("180d"), true)
    assertTrue(result is UbmGattCentralBridge.PostResult.PermissionDenied)
    assertTrue(jni.enqueued.isEmpty())
  }

  @Test
  fun coreRejectionsReachTheSink() {
    val jni = FakeJni()
    jni.drains.add(
      "{\"ok\":false,\"event\":\"connect\",\"code\":\"peer.not-found\",\"domain\":\"connection\"," +
        "\"operation\":\"connection.connect\",\"detail\":\"unknown\",\"effects\":[],\"observations\":[]}"
    )
    val rejections = mutableListOf<GattObservation>()
    val bound = binding(jni, rejections = rejections)
    bound.postConnect("AA:BB:CC:DD:EE:FF", "lease-a")
    assertEquals(1, rejections.size)
    assertEquals("peer.not-found", rejections.single().code)
    assertEquals("connection.connect", rejections.single().operation)
  }

  @Test
  fun releasePostsReleaseDrainsAndCloses() {
    val jni = FakeJni()
    val bound = binding(jni)
    bound.postScanStart(listOf("180d"), true)
    bound.release()
    assertTrue(jni.enqueued.last() == "release")
    assertEquals(1, jni.closed)
    assertFalse(bound.isOpen)
    assertNull(bound.postScanStart(listOf("180d"), true))
  }

  @Test
  fun scanOpExtraction() {
    assertEquals(
      "scan-op-9",
      scanOpFrom("{\"ok\":true,\"op\":\"scan-op-9\",\"observations\":[],\"effects\":[]}")
    )
    assertNull(scanOpFrom("{\"ok\":true,\"observations\":[],\"effects\":[]}"))
    assertNull(scanOpFrom(""))
  }
}
