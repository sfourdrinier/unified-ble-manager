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
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
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
 * every post returns null — under R02 authority the owner then fails the
 * covered command loud with a `coreUnavailable` terminal (never silent
 * radio-only execution) and emits the [openFailure] as a diagnostic. A
 * missing core never bricks BLE and never passes silently.
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
 * reference it. The scan op id is minted by the core and tracked from
 * `central.scan-start` drain observations. The dispatcher retains that exact
 * identity until the platform's successful stop settles the core operation.
 *
 * Authority barriers (R02): each scan-start is tagged with its generation at
 * enqueue and the tag travels with the drained verdict. Reset advances the
 * generation so even a verdict published after a replacement cannot install
 * the old operation. Stops also bar admission to the retired operation.
 * [release]/[shutdown] latch a disabled barrier: afterwards every post is a
 * null shadow and every drain observation is dropped, so no callback can
 * observe the bridge after stop.
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
    const val CONNECT_ADMISSION_CAP = 256

    private val MAC_ADDRESS = Regex("^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$")
  }

  private val appContext = context.applicationContext
  private var handle: Long = -1
  private var openFailureValue: String? = null
  private val scanOp = AtomicReference<String?>(null)
  private val connectAdmissions = ConcurrentHashMap<String, ConnectAdmission>()
  private val connectReservations = ConcurrentHashMap<String, String>()
  private val connectFailures = ConcurrentHashMap<String, String>()
  private val connectAdmissionGuard = Any()
  private val connectAdmissionDiagnostic = AtomicReference<String?>(null)

  /**
   * [scanGuard] linearizes scan generation, admission and stop. A verdict is
   * accepted only for its original live generation. [disabled] is set once by
   * [release]/[shutdown] and needs no lock.
   */
  private val scanGuard = Any()
  private var scanStopped = false
  private var scanGeneration = 0L
  private var scanAdmissionOwner: String? = null
  private var scanAdmission: ScanAdmission = ScanAdmission.Missing
  private val stoppingScanOps = mutableSetOf<String>()
  // The historical protocol owns at most one scan. Keep the queued
  // platform settlement until its drain observation arrives, including when
  // executor scheduling failed or a drain itself failed.
  private val scanSettlements = mutableMapOf<String, Pair<String, Boolean?>>()
  private var scanSettlementSerial = 0L
  private val scanPlatformStarts = mutableMapOf<String, Pair<String, Boolean?>>()
  private var scanPlatformStartSerial = 0L
  private var resetSerial = 0L
  private var pendingReset: Pair<String, Boolean?>? = null
  private val disabled = AtomicBoolean(false)

  /** Why the session failed to open, null when open. Never silent. */
  val openFailure: String? get() = openFailureValue

  /** True only with a live, revision-checked native session. */
  val isOpen: Boolean get() = handle >= 0

  val bridge: UbmGattCentralBridge = UbmGattCentralBridge(
    enqueue = { wire -> jni.enqueue(handle, wire) },
    drain = { jni.drain(handle) },
    hasBlePermissions = { hasBlePermissions() },
    worker = worker
  ).also { bridge ->
    bridge.onCorrelatedObservations = { observations -> onDrained(observations) }
    bridge.onScanAdmission = { generation, observation ->
      synchronized(scanGuard) {
        if (generation == scanGeneration && !scanStopped) {
          val op = if (observation.ok) scanOpFrom(observation.raw) else null
          scanAdmission = when {
            op != null -> ScanAdmission.Admitted(op).also { scanOp.set(op) }
            !observation.ok -> ScanAdmission.Rejected(observation)
            else -> ScanAdmission.Missing
          }
        }
      }
    }
    bridge.onTaggedObservation = { tag, observation ->
      synchronized(scanGuard) {
        val start = scanPlatformStarts.entries.singleOrNull { it.value.first == tag }
        if (start != null) {
          start.setValue(tag to observation.ok)
        } else if (scanSettlements.entries.singleOrNull { it.value.first == tag } != null) {
          val settlement = scanSettlements.entries.single { it.value.first == tag }
          settlement.setValue(tag to observation.ok)
        } else if (pendingReset?.first == tag) {
          pendingReset = tag to observation.ok
          if (observation.ok) {
            scanGeneration++
            scanStopped = true
            scanOp.set(null)
            scanAdmission = ScanAdmission.Missing
            scanAdmissionOwner = null
            stoppingScanOps.clear()
            scanSettlements.clear()
            scanPlatformStarts.clear()
            synchronized(connectAdmissionGuard) {
              connectReservations.clear()
              connectAdmissions.clear()
              connectFailures.clear()
            }
          }
        }
      }
    }
  }

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

  private fun onDrained(observations: List<Pair<GattObservation, String?>>) {
    if (disabled.get()) return
    for ((observation, tag) in observations) {
      observation.connectAdmission?.let { admission ->
        val key = admissionKey(admission.peer, admission.lease)
        val accepted = synchronized(connectAdmissionGuard) {
          if (connectReservations[key] != admission.peer) {
            false
          } else {
            connectReservations.remove(key)
            connectAdmissions[key] = admission
            true
          }
        }
        if (!accepted) {
          connectAdmissionDiagnostic.set("connect-admission-stale|$key")
        }
      }
      if (!observation.ok && observation.event == "connect" && observation.connectPeer != null && observation.connectLease != null) {
        val key = admissionKey(observation.connectPeer, observation.connectLease)
        synchronized(connectAdmissionGuard) {
          if (connectReservations.remove(key) != null) {
            connectFailures[key] = observation.detail ?: "connect-admission-rejected"
          }
        }
      }
      if (!observation.ok) {
        if (tag?.startsWith("scan-settlement:") == true) {
          // This verdict is consumed by scanSettlementVerdict(op). Sending it
          // to the uncorrelated rejection journal could fail a later scan.
          android.util.Log.e("UbmGattCoreBinding", "Core rejected $tag: ${observation.code}|${observation.operation}")
          continue
        }
        try {
          onCoreRejection(observation)
        } catch (_: Throwable) {
        }
      }
    }
  }

  // -- posting --------------------------------------------------------------

  private fun post(wire: String): UbmGattCentralBridge.PostResult? {
    if (disabled.get()) return null
    if (!isOpen) return null
    return bridge.postEvent(wire)
  }

  private fun nowMs(): Long = clockMs()

  private fun admissionKey(peer: String, lease: String): String = "$peer\u0000$lease"

  /** Returns and consumes the one exact connect admission for this device/lease. */
  fun takeConnectAdmission(deviceId: String, lease: String): ConnectAdmissionResult {
    if (lease.isEmpty()) return ConnectAdmissionResult.Rejected("lease-empty")
    val key = admissionKey(peerKeyFor(deviceId), lease)
    return synchronized(connectAdmissionGuard) {
      connectAdmissions.remove(key)?.let { return@synchronized ConnectAdmissionResult.Admitted(it) }
      connectFailures.remove(key)?.let { return@synchronized ConnectAdmissionResult.Rejected(it) }
      ConnectAdmissionResult.Missing
    }
  }

  /** Withdraws every exact pending/settled admission during cancellation or compensation. */
  fun withdrawConnectAdmission(deviceId: String, lease: String): ConnectAdmissionResult {
    if (lease.isEmpty()) return ConnectAdmissionResult.Rejected("lease-empty")
    val key = admissionKey(peerKeyFor(deviceId), lease)
    return synchronized(connectAdmissionGuard) {
      val removed = connectReservations.remove(key) != null ||
        connectAdmissions.remove(key) != null ||
        connectFailures.remove(key) != null
      if (removed) ConnectAdmissionResult.Rejected("withdrawn") else ConnectAdmissionResult.Missing
    }
  }

  /** Diagnostic for a bounded admission rejection, retained until replaced. */
  fun connectAdmissionDiagnostic(): String? = connectAdmissionDiagnostic.get()

  /** Peer domain for one Android device id (matches the staged-driver convention). */
  fun peerDomainFor(deviceId: String): String =
    if (MAC_ADDRESS.matches(deviceId)) "public-address" else "platform-guid"

  /** Locally derived core peer key (`domain:value`; no drain round-trip). */
  fun peerKeyFor(deviceId: String): String = "${peerDomainFor(deviceId)}:$deviceId"

  sealed interface ScanAdmission {
    object Missing : ScanAdmission
    data class Admitted(val operation: String) : ScanAdmission
    data class Rejected(val observation: GattObservation) : ScanAdmission
  }

  fun postScanStart(serviceUuids: List<String>, allowDuplicates: Boolean): UbmGattCentralBridge.PostResult? =
    postScanStart(serviceUuids, allowDuplicates, null)

  fun postScanStart(serviceUuids: List<String>, allowDuplicates: Boolean, owner: String?): UbmGattCentralBridge.PostResult? =
    synchronized(scanGuard) {
      scanGeneration++
      scanAdmissionOwner = owner
      scanStopped = false
      scanAdmission = ScanAdmission.Missing
      if (disabled.get() || !isOpen) return@synchronized null
      val result = bridge.postEvent(GattCentralWire.scanStart(
        PROTOCOL_SCAN_OWNER,
        NO_PROTOCOL_DEADLINE_MS,
        nowMs(),
        serviceUuids,
        if (allowDuplicates) "all" else "first",
        "none"
      ), scanGeneration)
      result
    }

  fun currentScanAdmission(): ScanAdmission = synchronized(scanGuard) { scanAdmission }
  fun currentScanAdmission(owner: String): ScanAdmission = synchronized(scanGuard) {
    if (scanAdmissionOwner == owner) scanAdmission else ScanAdmission.Missing
  }

  /** The core-minted identity observed during the synchronous admission drain. */
  fun currentScanOperation(): String? = synchronized(scanGuard) { scanOp.get() }

  fun postScanPlatformStarted(op: String): UbmGattCentralBridge.PostResult? = synchronized(scanGuard) {
    if (scanOp.get() != op || scanStopped || scanPlatformStarts.containsKey(op) || disabled.get() || !isOpen) return@synchronized null
    val tag = "scan-started:${++scanPlatformStartSerial}"
    scanPlatformStarts[op] = tag to null
    val result = bridge.postTaggedEvent(GattCentralWire.scanPlatformStarted(op), tag)
    if (result !is UbmGattCentralBridge.PostResult.Queued && result !is UbmGattCentralBridge.PostResult.ScheduleFailed) {
      scanPlatformStarts.remove(op)
    }
    result
  }

  fun scanPlatformStartedVerdict(op: String): Boolean? = synchronized(scanGuard) { scanPlatformStarts[op]?.second }

  fun postScanStop(): UbmGattCentralBridge.PostResult? = postScanStop(currentScanOperation())

  fun postScanStop(op: String?): UbmGattCentralBridge.PostResult? {
    if (disabled.get()) return null
    if (op == null) return null
    return synchronized(scanGuard) {
      if (op in stoppingScanOps || scanOp.get() != op) return@synchronized null
      scanStopped = true
      val result = post(GattCentralWire.scanStop(op, nowMs()))
      if (result is UbmGattCentralBridge.PostResult.Queued || result is UbmGattCentralBridge.PostResult.ScheduleFailed) {
        scanOp.compareAndSet(op, null)
        stoppingScanOps.add(op)
      }
      result
    }
  }

  /** Settle only the operation whose platform stop actually succeeded. */
  fun postScanStopped(op: String?): UbmGattCentralBridge.PostResult? {
    if (op == null || disabled.get()) return null
    return synchronized(scanGuard) {
      if (op !in stoppingScanOps) return@synchronized null
      val existing = scanSettlements[op]
      if (existing != null && existing.second != false) return@synchronized null
      val tag = "scan-settlement:${++scanSettlementSerial}"
      scanSettlements[op] = tag to null
      val result = bridge.postTaggedEvent(GattCentralWire.scanPlatformEvent(op, "platform-stopped", nowMs()), tag)
      if (result !is UbmGattCentralBridge.PostResult.Queued && result !is UbmGattCentralBridge.PostResult.ScheduleFailed) {
        scanSettlements.remove(op)
      }
      result
    }
  }

  fun scanSettlementQueued(op: String): Boolean = synchronized(scanGuard) { scanSettlements[op]?.second != false && scanSettlements.containsKey(op) }
  fun scanSettlementVerdict(op: String): Boolean? = synchronized(scanGuard) { scanSettlements[op]?.second }
  fun forgetStoppedScan(op: String) = synchronized(scanGuard) {
    scanSettlements.remove(op)
    scanPlatformStarts.remove(op)
    stoppingScanOps.remove(op)
  }

  fun postPeerResolve(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.peerResolve(peerDomainFor(deviceId), deviceId))
  }

  /**
   * R02 authority ordering: the peer resolve is admitted first and its
   * refusal is returned WITHOUT posting the connect line, so a refused peer
   * never leaves an orphaned core connect op behind. Callers gate on the
   * single returned verdict (see `admitCoreCommand`).
   */
  fun postConnect(deviceId: String, lease: String): UbmGattCentralBridge.PostResult? {
    if (lease.isEmpty()) return UbmGattCentralBridge.PostResult.EnqueueFailed("connect-lease-empty")
    val peer = peerKeyFor(deviceId)
    val key = admissionKey(peer, lease)
    synchronized(connectAdmissionGuard) {
      if (connectReservations.size + connectAdmissions.size + connectFailures.size >= CONNECT_ADMISSION_CAP) {
        connectAdmissionDiagnostic.set("connect-admission-cap")
        return UbmGattCentralBridge.PostResult.EnqueueFailed("connect-admission-cap")
      }
      if (connectReservations.containsKey(key) || connectAdmissions.containsKey(key) || connectFailures.containsKey(key)) {
        connectAdmissionDiagnostic.set("connect-admission-duplicate|$key")
        return UbmGattCentralBridge.PostResult.EnqueueFailed("connect-admission-duplicate")
      }
      connectReservations[key] = peer
    }
    val resolved = postPeerResolve(deviceId)
    if (resolved !is UbmGattCentralBridge.PostResult.Queued) {
      if (resolved !is UbmGattCentralBridge.PostResult.ScheduleFailed) {
        synchronized(connectAdmissionGuard) { connectReservations.remove(key) }
      }
      return resolved
    }
    val result = post(GattCentralWire.connect(peer, lease, NO_PROTOCOL_DEADLINE_MS, nowMs()))
    if (result !is UbmGattCentralBridge.PostResult.Queued &&
      result !is UbmGattCentralBridge.PostResult.ScheduleFailed
    ) {
      synchronized(connectAdmissionGuard) { connectReservations.remove(key) }
    }
    return result
  }

  fun postLinkEstablished(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.linkEstablished(peerKeyFor(deviceId)))
  }

  fun postLinkEstablished(deviceId: String, generation: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.linkEstablishedScoped(peerKeyFor(deviceId), generation))
  }

  fun postLinkReleased(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.linkReleased(peerKeyFor(deviceId)))
  }

  fun postLinkReleased(deviceId: String, generation: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.linkReleasedScoped(peerKeyFor(deviceId), generation))
  }

  fun postPeerLoss(deviceId: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.peerLoss(peerKeyFor(deviceId), nowMs()))
  }

  fun postPeerLoss(deviceId: String, generation: String): UbmGattCentralBridge.PostResult? {
    return post(GattCentralWire.peerLossScoped(peerKeyFor(deviceId), generation, nowMs()))
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

  fun postAdapterReset(): UbmGattCentralBridge.PostResult? = synchronized(scanGuard) {
    if (disabled.get() || !isOpen || pendingReset != null) return null
    val tag = "adapter-reset:${++resetSerial}"
    pendingReset = tag to null
    val result = bridge.postTaggedEvent(GattCentralWire.adapterReset(nowMs()), tag)
    if (result !is UbmGattCentralBridge.PostResult.Queued && result !is UbmGattCentralBridge.PostResult.ScheduleFailed) {
      pendingReset = null
    } else {
      // A physical adapter loss fences old connect admissions even while the
      // scan identity remains owned until the reset verdict is published.
      synchronized(connectAdmissionGuard) {
        connectReservations.clear()
        connectAdmissions.clear()
        connectFailures.clear()
      }
    }
    result
  }

  fun adapterResetPending(): Boolean = synchronized(scanGuard) { pendingReset != null }
  fun adapterResetVerdict(): Boolean? = synchronized(scanGuard) { pendingReset?.second }
  fun forgetAdapterReset() = synchronized(scanGuard) { pendingReset = null }

  // -- lifecycle ---------------------------------------------------------------

  /**
   * Destroy path: final worker drain, unconditional `release` (real destroy
   * transition), executor shutdown, then native session close
   * (release-then-close). Blocks for a running drain — never call from a
   * binder thread.
   */
  fun release(): List<GattObservation> {
    val observations = bridge.releaseOnDestroy()
    // Latched after the joined final drain (which still forwards, as before):
    // only truly late callbacks are dropped from here on.
    disabled.set(true)
    synchronized(connectAdmissionGuard) {
      connectReservations.clear()
      connectAdmissions.clear()
      connectFailures.clear()
    }
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
    // Latched first: shutdown does not join a running drain, so an in-flight
    // worker callback must already be barred from the gone owner.
    disabled.set(true)
    synchronized(connectAdmissionGuard) {
      connectReservations.clear()
      connectAdmissions.clear()
      connectFailures.clear()
    }
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
