// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/UbmGattCoreBinding.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.SystemClock
import com.ubm.echo.EchoBridge
import com.ubm.gatt.GattBridge
import java.util.concurrent.Executor
import java.util.concurrent.atomic.AtomicReference

/**
 * HOST-ANDROID production owner for the shared-core JNI session (UBM 5.0).
 *
 * This is the production call site for [UbmGattCentralBridge]: it opens a
 * real core session through `EchoBridge`, drives the real `GattBridge`
 * natives for enqueue/drain, gates posts on the real BLE permission state,
 * and forwards core rejections (ok:false drain observations) to the owner's
 * diagnostic sink. The bridge itself stays `android.*`-free and unit
 * testable; this class is the thin Android adapter over it.
 *
 * Fail-closed contract: when the session cannot open (missing native
 * library, foreign linked revision) the binding reports [isOpen] false and
 * every post returns null — the owner keeps driving the platform radio and
 * emits the [openFailure] as a diagnostic. A missing core never bricks BLE
 * and never passes silently.
 *
 * Threading: [postEvent]-adjacent methods are binder-safe (permission gate
 * plus one enqueue, never a drain). [release] joins the worker drain and
 * closes the native session: call it from the destroy path, never from a
 * binder thread (mirrors [UbmGattCentralBridge.releaseOnDestroy]).
 *
 * Key derivation: core peer keys are `domain:value` by construction
 * (`PeerIdentity::session_key`), so link/discovery lines use locally derived
 * keys with no drain round-trip. `peer.resolve` is still posted first so the
 * core registers (and admits/quota-checks) the peer before link lines
 * reference it. The scan op id is minted by the core: it is tracked from
 * `central.scan-start` drain observations (same first-`"op"` convention as
 * the JNI round-trip harness); a stop posted before the start observation
 * drains is a documented no-op shadow, never a fabricated stop.
 *
 * Follow-up (not in this slice): GATT IO shadowing (`path.register`,
 * `read.start`/`write.start`, `op.settle`, `notify.deliver`) needs the
 * path/lease context the dispatcher owns per command; wire it once the
 * dispatcher carries core path ids alongside its radio operation ids.
 */
class UbmGattCoreBinding(
  context: Context,
  revision: String = CONTRACT_REVISION,
  private val jni: CoreJni = RealCoreJni,
  private val onCoreRejection: (GattObservation) -> Unit = {},
  worker: Executor? = null,
  private val clockMs: () -> Long = { SystemClock.elapsedRealtime() }
) {
  companion object {
    /** Frozen shared-core contract revision (mirrors contracts/src/version.ts). */
    const val CONTRACT_REVISION = "C-UBM.0.1.2-DRAFT"

    /** No protocol deadline: the core-bound maximum (no host timer exists). */
    const val NO_PROTOCOL_DEADLINE_MS = 2_147_483_647L

    /** Owner label for protocol-driven scans. */
    const val PROTOCOL_SCAN_OWNER = "android-protocol-scan"

    private val MAC_ADDRESS = Regex("^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$")
  }

  private val appContext = context.applicationContext
  private var handle: Long = -1
  private var openFailureValue: String? = null
  private val scanOp = AtomicReference<String?>(null)

  /** Why the session failed to open, null when open. Never silent. */
  val openFailure: String? get() = openFailureValue

  /** True only with a live, revision-checked native session. */
  val isOpen: Boolean get() = handle >= 0

  val bridge: UbmGattCentralBridge = UbmGattCentralBridge(
    enqueue = { wire -> jni.enqueue(handle, wire) },
    drain = { jni.drain(handle) },
    hasBlePermissions = { hasBlePermissions() },
    onObservations = { observations -> onDrained(observations) },
    worker = worker
  )

  init {
    try {
      val opened = jni.open(revision)
      val linked = jni.revision()
      if (linked != revision) {
        try {
          jni.close(opened)
        } catch (_: Throwable) {
        }
        openFailureValue = "protocol.incompatible|core|ubm-gatt-bind|linked=$linked want=$revision"
      } else {
        handle = opened
      }
    } catch (th: Throwable) {
      openFailureValue = "platform.failure|platform|ubm-gatt-bind|${th.message ?: th.javaClass.simpleName}"
    }
  }

  // -- permission gate (owns the API-level split) --------------------------

  private fun hasBlePermissions(): Boolean {
    return if (Build.VERSION.SDK_INT >= 31) {
      appContext.checkSelfPermission(Manifest.permission.BLUETOOTH_SCAN) == PackageManager.PERMISSION_GRANTED &&
        appContext.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) == PackageManager.PERMISSION_GRANTED
    } else {
      // BLUETOOTH_SCAN/CONNECT cannot exist below API 31: gate on the
      // install-time BLUETOOTH grant instead of probing for the unprobbable.
      appContext.checkSelfPermission(Manifest.permission.BLUETOOTH) == PackageManager.PERMISSION_GRANTED
    }
  }

  // -- drain observation handling ------------------------------------------

  private fun onDrained(observations: List<GattObservation>) {
    for (observation in observations) {
      if (observation.ok && observation.raw.contains("\"kind\":\"central.scan-start\"")) {
        scanOpFrom(observation.raw)?.let { scanOp.set(it) }
      }
      if (!observation.ok) {
        try {
          onCoreRejection(observation)
        } catch (_: Throwable) {
        }
      }
    }
  }

  // -- posting --------------------------------------------------------------

  private fun post(wire: String): UbmGattCentralBridge.PostResult? {
    if (!isOpen) return null
    return bridge.postEvent(wire)
  }

  private fun nowMs(): Long = clockMs()

  /** Peer domain for one Android device id (matches the staged-driver convention). */
  fun peerDomainFor(deviceId: String): String =
    if (MAC_ADDRESS.matches(deviceId)) "public-address" else "platform-guid"

  /** Locally derived core peer key (`domain:value`; no drain round-trip). */
  fun peerKeyFor(deviceId: String): String = "${peerDomainFor(deviceId)}:$deviceId"

  fun postScanStart(serviceUuids: List<String>, allowDuplicates: Boolean): UbmGattCentralBridge.PostResult? {
    return post(
      GattCentralWire.scanStart(
        PROTOCOL_SCAN_OWNER,
        NO_PROTOCOL_DEADLINE_MS,
        nowMs(),
        serviceUuids,
        if (allowDuplicates) "all" else "first",
        "none"
      )
    )
  }

  fun postScanStop(): UbmGattCentralBridge.PostResult? {
    val op = scanOp.getAndSet(null) ?: return null
    return post(GattCentralWire.scanStop(op, nowMs()))
  }

  fun postPeerResolve(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.peerResolve(peerDomainFor(deviceId), deviceId))
  }

  fun postConnect(deviceId: String, lease: String): UbmGattCentralBridge.PostResult? {
    postPeerResolve(deviceId)
    return post(GattCentralWire.connect(peerKeyFor(deviceId), lease, NO_PROTOCOL_DEADLINE_MS, nowMs()))
  }

  fun postLinkEstablished(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.linkEstablished(peerKeyFor(deviceId)))
  }

  fun postLinkReleased(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.linkReleased(peerKeyFor(deviceId)))
  }

  fun postPeerLoss(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.peerLoss(peerKeyFor(deviceId), nowMs()))
  }

  fun postDisconnect(deviceId: String, lease: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.disconnect(peerKeyFor(deviceId), lease, nowMs()))
  }

  fun postDiscoveryBegin(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.discoveryBegin(peerKeyFor(deviceId)))
  }

  fun postDiscoveryComplete(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.discoveryComplete(peerKeyFor(deviceId)))
  }

  fun postDiscoveryFail(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.discoveryFail(peerKeyFor(deviceId)))
  }

  fun postServicesChanged(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.servicesChanged(peerKeyFor(deviceId)))
  }

  fun postAdapterReset(): UbmGattCentralBridge.PostResult? {
    scanOp.set(null)
    return post(GattCentralWire.adapterReset(nowMs()))
  }

  // -- lifecycle ---------------------------------------------------------------

  /**
   * Destroy path: final worker drain, unconditional `release` (real destroy
   * transition), executor shutdown, then native session close
   * (release-then-close). Blocks for a running drain — never call from a
   * binder thread.
   */
  fun release(): List<GattObservation> {
    val observations = bridge.releaseOnDestroy()
    if (isOpen) {
      try {
        jni.close(handle)
      } catch (_: Throwable) {
      } finally {
        handle = -1
      }
    } else {
      bridge.shutdown()
    }
    return observations
  }

  /** Abandon path when the owner is going away without destroy. */
  fun shutdown() {
    bridge.shutdown()
    if (isOpen) {
      try {
        jni.close(handle)
      } catch (_: Throwable) {
      } finally {
        handle = -1
      }
    }
  }

  // -- JNI seam (injectable for HOST-JVM tests) ----------------------------------

  interface CoreJni {
    fun open(revision: String): Long
    fun revision(): String
    fun enqueue(handle: Long, wire: String): Int
    fun drain(handle: Long): String
    fun depth(handle: Long): Int
    fun close(handle: Long)
  }

  object RealCoreJni : CoreJni {
    override fun open(revision: String): Long = EchoBridge.nativeOpen(revision)
    override fun revision(): String = EchoBridge.nativeRevision()
    override fun enqueue(handle: Long, wire: String): Int = GattBridge.nativeEnqueueGattEvent(handle, wire)
    override fun drain(handle: Long): String = GattBridge.nativeDrainGattEvents(handle)
    override fun depth(handle: Long): Int = GattBridge.nativeGattQueueDepth(handle)
    override fun close(handle: Long) = EchoBridge.nativeClose(handle)
  }
}

/**
 * Extracts the driving op id from a `central.scan-start` drain observation
 * (first `"op":"..."` value — the same convention as the JNI round-trip
 * harness `opOf`). Null when the line carries no attributable op.
 */
fun scanOpFrom(raw: String): String? {
  val key = "\"op\":\""
  var index = raw.indexOf(key)
  if (index < 0) return null
  index += key.length
  val end = raw.indexOf('"', index)
  if (end < 0) return null
  return raw.substring(index, end).ifEmpty { null }
}
