// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.annotation.SuppressLint
import android.Manifest
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanRecord
import android.bluetooth.le.ScanResult as AndroidScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.ParcelUuid
import androidx.core.content.IntentCompat
import com.sfourdrinier.unifiedblemanager.protocol.UnifiedBleProtocolAndroidDispatcher
import java.lang.reflect.InvocationTargetException
import java.util.ArrayDeque
import java.util.Locale
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

internal data class OwnedRadioTeardownFailure(
  val operation: String,
  val throwable: Throwable
)

internal data class OwnedRadioTeardownResult(
  val failures: List<OwnedRadioTeardownFailure>
) {
  val isSuccessful: Boolean
    get() = failures.isEmpty()
}

/** Identity-bearing owner for the legacy UUID-keyed characteristic-write path. */
internal class AndroidPendingWriteOwner<Key>(
  val deviceKeyUpper: String,
  val attribute: Key,
  val value: ByteArray,
  val callback: (Result<ByteArray?>) -> Unit
)

/**
 * Keeps replacement writes isolated when their legacy string key is reused.
 * Teardown must remove the exact owner it observed, never a newer operation.
 */
internal class AndroidPendingWriteRegistry<Key> {
  private val entries = ConcurrentHashMap<String, AndroidPendingWriteOwner<Key>>()

  fun putIfAbsent(key: String, owner: AndroidPendingWriteOwner<Key>): Boolean =
    entries.putIfAbsent(key, owner) == null

  fun get(key: String): AndroidPendingWriteOwner<Key>? = entries[key]

  fun remove(key: String, owner: AndroidPendingWriteOwner<Key>): Boolean =
    entries.remove(key, owner)

  /** Completes only the currently registered attribute, rejecting stale callbacks. */
  fun complete(
    key: String,
    attribute: Key,
    result: Result<ByteArray?>
  ): Boolean {
    val owner = entries[key] ?: return false
    if (owner.attribute !== attribute) return false
    if (!entries.remove(key, owner)) return false
    owner.callback(result)
    return true
  }

  fun remove(key: String): AndroidPendingWriteOwner<Key>? = entries.remove(key)

  fun entriesForDevice(deviceKeyUpper: String): List<Pair<String, AndroidPendingWriteOwner<Key>>> =
    entries.entries
      .filter { entry -> entry.value.deviceKeyUpper == deviceKeyUpper }
      .map { entry -> entry.key to entry.value }

  fun clear() {
    entries.clear()
  }
}

/** Removes an exact-write payload only when it is still the observed payload. */
internal fun <Key> removeAndroidWritePayloadIfSame(
  values: ConcurrentHashMap<Key, ByteArray>,
  key: Key,
  expected: ByteArray?
): Boolean = expected != null && values.remove(key, expected)

/**
 * Tracks exact native notification registrations by GATT generation.
 *
 * A Service Changed callback invalidates the database without replacing the
 * BluetoothGatt instance, so the GATT object identity alone cannot protect
 * notification delivery from stale characteristic ownership.
 */
internal class OwnedAndroidSubscriptionOwnership<K> {
  private data class Registration(
    val deviceKeyUpper: String,
    val gattGeneration: Long,
    val active: Boolean,
    val stagedValues: ArrayDeque<ByteArray> = ArrayDeque(),
    var stagedBytes: Int = 0,
    var overflowed: Boolean = false
  )

  private val registrations = ConcurrentHashMap<K, Registration>()

  fun arm(deviceKey: String, gattGeneration: Long, key: K) {
    registrations[key] = Registration(deviceKey.uppercase(), gattGeneration, active = false)
  }

  fun activate(deviceKey: String, gattGeneration: Long, key: K): List<ByteArray> {
    val deviceKeyUpper = deviceKey.uppercase()
    var staged: List<ByteArray> = emptyList()
    registrations.compute(key) { _, previous ->
      if (
        previous != null &&
        previous.deviceKeyUpper == deviceKeyUpper &&
        previous.gattGeneration == gattGeneration
      ) {
        staged = previous.stagedValues.toList()
      }
      Registration(deviceKeyUpper, gattGeneration, active = true)
    }
    return staged
  }

  fun deactivate(deviceKey: String, gattGeneration: Long, key: K) {
    abandon(deviceKey, gattGeneration, key)
  }

  fun abandon(deviceKey: String, gattGeneration: Long, key: K): List<ByteArray> {
    val registration = registrations[key] ?: return emptyList()
    if (registration.deviceKeyUpper != deviceKey.uppercase() ||
      registration.gattGeneration != gattGeneration
    ) {
      return emptyList()
    }
    if (!registrations.remove(key, registration)) return emptyList()
    return registration.stagedValues.toList()
  }

  fun isActive(deviceKey: String, gattGeneration: Long, key: K): Boolean {
    val registration = registrations[key] ?: return false
    return registration.active &&
      registration.deviceKeyUpper == deviceKey.uppercase() &&
      registration.gattGeneration == gattGeneration
  }

  fun isArmed(deviceKey: String, gattGeneration: Long, key: K): Boolean {
    val registration = registrations[key] ?: return false
    return !registration.active &&
      registration.deviceKeyUpper == deviceKey.uppercase() &&
      registration.gattGeneration == gattGeneration
  }

  fun stage(deviceKey: String, gattGeneration: Long, key: K, value: ByteArray): Boolean {
    var staged = false
    registrations.computeIfPresent(key) { _, registration ->
      if (
        !registration.active &&
        registration.deviceKeyUpper == deviceKey.uppercase() &&
        registration.gattGeneration == gattGeneration
      ) {
        if (
          !registration.overflowed &&
          registration.stagedValues.size < ANDROID_ENABLEMENT_STAGING_ITEM_LIMIT &&
          registration.stagedBytes + value.size <= ANDROID_ENABLEMENT_STAGING_BYTE_LIMIT
        ) {
          registration.stagedValues.addLast(value.copyOf())
          registration.stagedBytes += value.size
          staged = true
        } else {
          registration.overflowed = true
        }
      }
      registration
    }
    return staged
  }

  fun invalidateForDatabaseChange(deviceKey: String, gattGeneration: Long): List<K> {
    val deviceKeyUpper = deviceKey.uppercase()
    return registrations.entries
      .filter { entry ->
        entry.value.deviceKeyUpper == deviceKeyUpper &&
          entry.value.gattGeneration == gattGeneration
      }
      .toList()
      .mapNotNull { entry ->
        if (registrations.remove(entry.key, entry.value)) entry.key else null
      }
  }

  fun clearDevice(deviceKey: String) {
    val deviceKeyUpper = deviceKey.uppercase()
    registrations.entries
      .filter { entry -> entry.value.deviceKeyUpper == deviceKeyUpper }
      .toList()
      .forEach { entry -> registrations.remove(entry.key, entry.value) }
  }

  fun clear() {
    registrations.clear()
  }
}

/**
 * Preserves the Android GATT callback status when an asynchronous operation
 * terminates. Status 19 means that the peer terminated the link; keeping that
 * distinction typed prevents a GATT operation race from being flattened to a generic
 * platform failure before the later connection-state callback arrives.
 */
internal class AndroidGattOperationFailure(
  val operation: String,
  val gattStatus: Int?,
  val isLinkLoss: Boolean = gattStatus == ANDROID_GATT_LINK_LOSS_STATUS
) : IllegalStateException(
  when {
    gattStatus != null -> "$operation status=$gattStatus"
    isLinkLoss -> "$operation failed because the GATT link is unavailable"
    else -> "$operation failed"
  }
)

internal const val ANDROID_GATT_LINK_LOSS_STATUS = 19
internal const val ANDROID_PROPERTY_NOTIFY = 0x10
internal const val ANDROID_PROPERTY_INDICATE = 0x20
internal val ANDROID_CCCD_ENABLE_NOTIFICATION = byteArrayOf(0x01, 0x00)
internal val ANDROID_CCCD_ENABLE_INDICATION = byteArrayOf(0x02, 0x00)
internal val ANDROID_CCCD_DISABLE = byteArrayOf(0x00, 0x00)
internal const val ANDROID_ENABLEMENT_STAGING_ITEM_LIMIT = 16
internal const val ANDROID_ENABLEMENT_STAGING_BYTE_LIMIT = 64 * 1024

internal fun classifyAndroidGattOperationFailure(
  operation: String,
  gattStatus: Int
): AndroidGattOperationFailure = AndroidGattOperationFailure(operation, gattStatus)

/**
 * Local [BluetoothGatt.setCharacteristicNotification] rejection is not proven
 * peer-disconnect. Generation-matched [BluetoothGattCallback.onConnectionStateChange]
 * remains the physical-loss authority; status 19 on that callback is typed as
 * link loss. Do not consult [BluetoothManager.getConnectionState] here: that
 * query can stay "connected" after the GATT link is already gone.
 */
internal fun classifyAndroidNotificationRegistrationFailure(
  operation: String
): AndroidGattOperationFailure = AndroidGattOperationFailure(operation, null)

/**
 * Android may deliver an ordinary CCCD or characteristic-write callback
 * immediately before authoritative connection-loss evidence for the same GATT
 * generation. Keep only that provisional result pending long enough for the
 * lifecycle evidence already in flight to claim it. Descriptor writes and
 * already-typed link loss are never delayed.
 */
internal fun shouldAwaitAndroidCccdDisconnectEvidence(failure: Throwable): Boolean =
  failure is AndroidGattOperationFailure &&
    failure.operation == "cccd-write" &&
    !failure.isLinkLoss

internal fun shouldAwaitAndroidRegistrationDisconnectEvidence(failure: Throwable): Boolean =
  failure is AndroidGattOperationFailure &&
    failure.operation == "setCharacteristicNotification" &&
    !failure.isLinkLoss

internal fun shouldAwaitAndroidWriteDisconnectEvidence(failure: Throwable): Boolean =
  failure is AndroidGattOperationFailure &&
    failure.operation == "characteristic-write" &&
    !failure.isLinkLoss

private const val ANDROID_GATT_DISCONNECT_EVIDENCE_GRACE_MS = 250L

/**
 * Holds an asynchronous GATT callback failure briefly so a connection-state
 * callback already in flight can provide the authoritative terminal. The
 * first failure is retained and each key can be claimed only once.
 */
internal class AndroidGattTerminalArbiter<Key>(
  private val schedule: (delayMs: Long, action: () -> Unit) -> Boolean,
  private val onFallback: (key: Key, failure: AndroidGattOperationFailure) -> Unit
) {
  private data class DeferredFailure(
    val failure: AndroidGattOperationFailure
  )

  private val provisionalFailures = ConcurrentHashMap<Key, DeferredFailure>()

  fun defer(key: Key, failure: AndroidGattOperationFailure) {
    val deferred = DeferredFailure(failure)
    if (provisionalFailures.putIfAbsent(key, deferred) != null) return
    val fallback: () -> Unit = {
      if (provisionalFailures.remove(key, deferred)) onFallback(key, deferred.failure)
    }
    if (!schedule(ANDROID_GATT_DISCONNECT_EVIDENCE_GRACE_MS, fallback)) fallback()
  }

  fun claim(key: Key): AndroidGattOperationFailure? = provisionalFailures.remove(key)?.failure

  fun isPending(key: Key): Boolean = provisionalFailures.containsKey(key)

  fun clear() {
    provisionalFailures.clear()
  }
}

/**
 * Applies the characteristic-write callback policy at the radio boundary.
 * Returns true when the callback was deferred for lifecycle arbitration.
 */
internal fun <Key> AndroidGattTerminalArbiter<Key>.deferCharacteristicWriteFailure(
  key: Key,
  status: Int
): Boolean {
  val failure = classifyAndroidGattOperationFailure("characteristic-write", status)
  if (!shouldAwaitAndroidWriteDisconnectEvidence(failure)) return false
  defer(key, failure)
  return true
}

/** Kept as a descriptive alias for existing CCCD callers and tests. */
internal typealias AndroidCccdTerminalArbiter<Key> = AndroidGattTerminalArbiter<Key>

/** Descriptive alias for characteristic-write terminal arbitration. */
internal typealias AndroidWriteTerminalArbiter<Key> = AndroidGattTerminalArbiter<Key>

/** Synchronous API rejection after local CCCD registration, before Android starts ATT work. */
internal class AndroidCccdSubmissionFailure(
  val platformStatus: Int?
) : IllegalStateException(
  if (platformStatus == null) "writeDescriptor failed to start"
  else "writeDescriptor failed to start status=$platformStatus"
)

/** Android rejected removal of the exact local notification registration. */
internal class AndroidNotificationRollbackRejected :
  IllegalStateException("setCharacteristicNotification rollback was rejected")

internal fun androidGattTerminalResult(
  result: Result<Unit>,
  rollbackFailure: OwnedRadioTeardownFailure?
): Result<Unit> {
  // A rejected local-registration rollback is a cleanup obligation. It is not
  // generation-matched disconnect evidence, so it must not upgrade the primary
  // result into typed link loss. completeExactUnit already reports and retries
  // that cleanup separately.
  if (rollbackFailure == null || result.isFailure) return result
  return Result.failure(
    IllegalStateException(
      "CCCD operation failed; CCCD rollback failed: " +
        (rollbackFailure.throwable.message ?: "unknown error"),
      rollbackFailure.throwable
    )
  )
}

/** Runtime adapter state derived from Android hardware and granted permissions. */
internal data class OwnedRadioAdapterProtocolState(
  val availability: String,
  val authorization: String,
  val power: String,
  val safeReason: String?
)

internal data class OwnedAndroidSecurityState(
  val bond: String,
  val pairingPossible: Boolean?
)

/** Immutable native-only projection of one system-bonded Android peer. */
internal data class BondedPeerSnapshot(
  val nativePeerId: String,
  val displayName: String?
)

internal fun bondedPeerAdapterReadiness(
  adapterAvailable: Boolean,
  connectPermissionGranted: Boolean,
  adapterState: Int
): Result<Unit> {
  if (!adapterAvailable) {
    return Result.failure(IllegalStateException("Bluetooth adapter unavailable"))
  }
  if (!connectPermissionGranted) {
    return Result.failure(SecurityException("Android Bluetooth connect permission is required to enumerate bonded peers"))
  }
  return when (adapterState) {
    BluetoothAdapter.STATE_ON -> Result.success(Unit)
    BluetoothAdapter.STATE_OFF ->
      Result.failure(IllegalStateException("Bluetooth adapter is powered off"))
    BluetoothAdapter.STATE_TURNING_ON,
    BluetoothAdapter.STATE_TURNING_OFF ->
      Result.failure(IllegalStateException("Bluetooth adapter is resetting"))
    else -> Result.failure(IllegalStateException("Bluetooth adapter state is unavailable"))
  }
}

internal fun requiresImmediateGattTeardownOnAdapterState(state: Int?): Boolean =
  state == BluetoothAdapter.STATE_OFF ||
    state == BluetoothAdapter.STATE_TURNING_OFF ||
    state == BluetoothAdapter.STATE_TURNING_ON

/** Normalize the platform snapshot before it crosses the Android protocol boundary. */
internal fun normalizeBondedPeerSnapshots(
  peers: Iterable<BondedPeerSnapshot>
): List<BondedPeerSnapshot> {
  val normalized = peers.map { peer ->
    val nativePeerId = peer.nativePeerId.trim().uppercase(Locale.ROOT)
    require(nativePeerId.isNotEmpty()) { "Android bonded peer has an empty native identifier" }
    BondedPeerSnapshot(nativePeerId, peer.displayName?.takeIf { it.isNotEmpty() })
  }
  return normalized
    .groupBy { peer -> peer.nativePeerId }
    .values
    .map { duplicates ->
      duplicates.sortedWith(
        compareBy<BondedPeerSnapshot> { peer -> peer.displayName == null }
          .thenBy { peer -> peer.displayName ?: "" }
      ).first()
    }
    .sortedBy { peer -> peer.nativePeerId }
}

internal data class OwnedAndroidPhy(
  val txPhy: String,
  val rxPhy: String
)

private data class EffectiveMtuState(
  val gattGeneration: Long,
  val mtu: Int
)

internal data class OwnedAndroidProtocolServiceData(
  val serviceUuid: String,
  val value: ByteArray
)

internal data class OwnedAndroidProtocolManufacturerData(
  val companyIdentifier: Int,
  val value: ByteArray
)

/**
 * Android fields observed from a single [AndroidScanResult]. Android's public
 * ScanRecord API exposes neither overflow service UUIDs nor an independent scan
 * response PDU, so those protocol fields are intentionally absent downstream.
 */
internal data class OwnedAndroidProtocolAdvertisement(
  val deviceId: String,
  val name: String?,
  val rssi: Int,
  val txPower: Int?,
  val connectable: Boolean?,
  val appearance: Int?,
  val serviceUuids: List<String>?,
  val solicitedServiceUuids: List<String>?,
  val serviceData: List<OwnedAndroidProtocolServiceData>?,
  val manufacturerData: List<OwnedAndroidProtocolManufacturerData>?,
  val rawRecord: ByteArray?
)

/**
 * Pure Android BluetoothGatt radio core — no RxAndroidBle / RxJava.
 * Protocol-owned GATT operations used by [UnifiedBleProtocolAndroidDispatcher].
 *
 * Serializes GATT requests per device (Android allows only one outstanding
 * request at a time). Connection listeners are registered per-device so multi-
 * device connects never overwrite each other.
 */
@SuppressLint("MissingPermission")
class OwnedAndroidGattRadio private constructor(
  private val context: Context,
  private val mainHandler: Handler?,
  private val post: ((() -> Unit) -> Boolean)?,
  private val scheduleDelayed: ((Long, () -> Unit) -> Boolean)?
) {
  constructor(context: Context) : this(
    context,
    Handler(Looper.getMainLooper()),
    null,
    null
  )

  internal constructor(
    context: Context,
    post: ((() -> Unit) -> Boolean),
    scheduleDelayed: (Long, () -> Unit) -> Boolean
  ) : this(context, null, post, scheduleDelayed)

  private val bluetoothManager: BluetoothManager by lazy {
    context.getSystemService(Context.BLUETOOTH_SERVICE) as BluetoothManager
  }
  private val adapter: BluetoothAdapter? by lazy { bluetoothManager.adapter }

  private var scanner: BluetoothLeScanner? = null
  private var scanCallback: ScanCallback? = null
  private var adapterStateReceiver: BroadcastReceiver? = null
  private var bondStateReceiver: BroadcastReceiver? = null
  private val scanSeenDeviceIds = ConcurrentHashMap.newKeySet<String>()

  private val gatts = ConcurrentHashMap<String, BluetoothGatt>()
  private val gattGenerations = ConcurrentHashMap<String, Long>()
  private val gattGenerationByInstance = ConcurrentHashMap<BluetoothGatt, Long>()
  private val pendingGattTeardowns = ConcurrentHashMap<String, GattTeardownOwner>()
  private val nextGattGeneration = AtomicLong(1L)
  private val discovered = ConcurrentHashMap<String, MutableList<android.bluetooth.BluetoothGattService>>()
  private val charCache = ConcurrentHashMap<String, BluetoothGattCharacteristic>()
  private val pending = ConcurrentHashMap<String, (Result<ByteArray?>) -> Unit>()
  private val pendingMtu = ConcurrentHashMap<String, (Result<Int>) -> Unit>()
  private val effectiveMtuByDevice = ConcurrentHashMap<String, EffectiveMtuState>()
  private val pendingRssi = ConcurrentHashMap<String, (Result<Int>) -> Unit>()
  private val pendingPhyReads = ConcurrentHashMap<String, (Result<OwnedAndroidPhy>) -> Unit>()
  private val pendingPhyRequests = ConcurrentHashMap<String, (Result<OwnedAndroidPhy?>) -> Unit>()
  private val pendingDesc = ConcurrentHashMap<String, (Result<Unit>) -> Unit>()
  private val pendingDescRead = ConcurrentHashMap<String, (Result<ByteArray?>) -> Unit>()
  /** Identity-bearing owners for the legacy UUID-keyed write callback path. */
  private val pendingWriteRegistry = AndroidPendingWriteRegistry<BluetoothGattCharacteristic>()
  private val pendingBondPairs =
    ConcurrentHashMap<String, (String, OwnedAndroidSecurityState) -> Unit>()

  /**
   * Protocol-v1 operations are keyed by the concrete Android attribute object,
   * rather than UUID strings. UUIDs are not unique within a GATT database, so
   * this preserves the service and characteristic occurrence selected by the
   * canonical command path.
   */
  private data class ExactBytePending(
    val deviceKeyUpper: String,
    val gatt: BluetoothGatt,
    val gattGeneration: Long,
    val token: GattSerialQueue.GattOperationToken,
    val callback: (Result<ByteArray?>) -> Unit,
    val done: () -> Unit
  )

  private data class ExactUnitPending(
    val deviceKeyUpper: String,
    val gatt: BluetoothGatt,
    val gattGeneration: Long,
    val token: GattSerialQueue.GattOperationToken,
    val callback: (Result<Unit>) -> Unit,
    val done: () -> Unit,
    val physicalCleanup: (() -> OwnedRadioTeardownFailure?)? = null,
    val subscriptionEnabled: Boolean? = null,
    val subscriptionCharacteristic: BluetoothGattCharacteristic? = null
  )

  private val exactReadPending = ConcurrentHashMap<BluetoothGattCharacteristic, ExactBytePending>()
  private val exactWritePending = ConcurrentHashMap<BluetoothGattCharacteristic, ExactBytePending>()
  private val exactWriteValues = ConcurrentHashMap<BluetoothGattCharacteristic, ByteArray>()
  private val exactCccdPending = ConcurrentHashMap<BluetoothGattDescriptor, ExactUnitPending>()
  private val exactCccdTerminalArbiter = AndroidCccdTerminalArbiter<BluetoothGattDescriptor>(
    schedule = { delayMs, action -> scheduleDeferred(delayMs, action) },
    onFallback = fallback@ { descriptor, failure ->
      val pending = exactCccdPending.remove(descriptor) ?: return@fallback
      completeExactUnit(pending, Result.failure(failure))
    }
  )
  private val exactWriteTerminalArbiter = AndroidWriteTerminalArbiter<BluetoothGattCharacteristic>(
    schedule = { delayMs, action -> scheduleDeferred(delayMs, action) },
    onFallback = fallback@ { characteristic, failure ->
      val observedPayload = exactWriteValues[characteristic]
      val pending = exactWritePending.remove(characteristic) ?: return@fallback
      removeAndroidWritePayloadIfSame(exactWriteValues, characteristic, observedPayload)
      completeExactByte(pending, Result.failure(failure))
    }
  )
  private val writeTerminalArbiter = AndroidWriteTerminalArbiter<String>(
    schedule = { delayMs, action -> scheduleDeferred(delayMs, action) },
    onFallback = fallback@ { key, failure ->
      val owner = pendingWriteRegistry.remove(key) ?: return@fallback
      owner.callback(Result.failure(failure))
    }
  )
  private val exactRegistrationPending = ConcurrentHashMap<BluetoothGattCharacteristic, ExactUnitPending>()
  private val exactRegistrationTerminalArbiter = AndroidGattTerminalArbiter<BluetoothGattCharacteristic>(
    schedule = { delayMs, action -> scheduleDeferred(delayMs, action) },
    onFallback = fallback@ { characteristic, failure ->
      val pending = exactRegistrationPending.remove(characteristic) ?: return@fallback
      completeExactUnit(pending, Result.failure(failure))
    }
  )
  private val exactDescriptorReadPending = ConcurrentHashMap<BluetoothGattDescriptor, ExactBytePending>()
  private val exactDescriptorWritePending = ConcurrentHashMap<BluetoothGattDescriptor, ExactUnitPending>()
  private val activeNativeSubscriptionOwnership =
    OwnedAndroidSubscriptionOwnership<BluetoothGattCharacteristic>()

  /** Per-device connection lifecycle listeners (multi-device safe). */
  private val connectionListeners =
    ConcurrentHashMap<String, (deviceId: String, connected: Boolean, gattStatus: Int) -> Unit>()

  /** Per-device FIFO for outstanding GATT ops. */
  private val deviceQueues = ConcurrentHashMap<String, GattSerialQueue>()
  private val nextGattOperationId = AtomicLong(1L)

  /**
   * autoConnect flag for a reconnect that must wait until the prior GATT reports
   * [BluetoothProfile.STATE_DISCONNECTED] before [BluetoothDevice.connectGatt] (R3-F003).
   */
  private val pendingReconnect = ConcurrentHashMap<String, Boolean>()
  private val pendingDisconnectCallbacks = ConcurrentHashMap<String, (OwnedRadioTeardownFailure?) -> Unit>()

  /** Physical cleanup that failed after public cancellation or a remote rejection. */
  private val retryableCleanups = ConcurrentHashMap<String, () -> OwnedRadioTeardownFailure?>()

  /** Safety-timeout runnables that force-close a GATT if DISCONNECTED never arrives (R3-F003). */
  private val closeTimeouts = ConcurrentHashMap<String, Runnable>()

  var onAdapterState: ((String) -> Unit)? = null
  var onScanResult: ((deviceId: String, name: String?, rssi: Int, connectable: Boolean?, raw: ByteArray?) -> Unit)? = null
  /** Protocol-v1 scan callback with every field available from Android's public LE scanner APIs. */
  internal var onProtocolScanResult: ((OwnedAndroidProtocolAdvertisement) -> Unit)? = null
  /**
   * Optional global connection hook (logging). Prefer [registerConnectionListener]
   * for multi-device delivery — never overwrite a single global per connect.
   */
  var onConnectionState: ((deviceId: String, connected: Boolean, gattStatus: Int) -> Unit)? = null
  var onNotification: ((deviceId: String, serviceUuid: UUID, charUuid: UUID, value: ByteArray) -> Unit)? = null
  /**
   * Protocol-v1 notification callback retaining the concrete native attribute
   * so a caller can derive exact UUID occurrence identity before crossing the
   * JavaScript boundary.
   */
  var onProtocolNotification: ((deviceId: String, characteristic: BluetoothGattCharacteristic, value: ByteArray) -> Unit)? = null
  /**
   * API 31+ [BluetoothGattCallback.onServiceChanged]: GATT DB out of sync;
   * apps should re-run discoverServices (Android docs).
   */
  var onServicesChanged: ((deviceId: String) -> Unit)? = null
  internal var onSecurityState: ((deviceId: String, state: OwnedAndroidSecurityState) -> Unit)? = null
  /** Runtime scan failures (permissions, internal errors) — not start exceptions. */
  var onScanFailed: ((errorCode: Int) -> Unit)? = null
  internal var onCleanupFailure: ((OwnedRadioTeardownFailure) -> Unit)? = null

  private data class GattTeardownOwner(
    val gatt: BluetoothGatt,
    val generation: Long
  )

  fun currentState(): String = mapAdapterState(adapter?.state)

  internal fun currentProtocolAdapterState(): OwnedRadioAdapterProtocolState {
    val availableAdapter = adapter
      ?: return OwnedRadioAdapterProtocolState(
        availability = "unsupported",
        authorization = "unavailable",
        power = "unsupported",
        safeReason = "This device does not expose an Android Bluetooth adapter."
      )
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S &&
      (context.checkSelfPermission(Manifest.permission.BLUETOOTH_SCAN) != PackageManager.PERMISSION_GRANTED ||
        context.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) != PackageManager.PERMISSION_GRANTED)
    ) {
      return OwnedRadioAdapterProtocolState(
        availability = "available",
        authorization = "denied",
        power = "unknown",
        safeReason = "Bluetooth scan and connect permissions are not granted."
      )
    }
    val state = try {
      availableAdapter.state
    } catch (error: SecurityException) {
      OwnedAndroidLog.e("currentProtocolAdapterState", error)
      return OwnedRadioAdapterProtocolState(
        availability = "available",
        authorization = "denied",
        power = "unknown",
        safeReason = "Android denied access to the Bluetooth adapter state."
      )
    }
    return when (state) {
      BluetoothAdapter.STATE_ON -> OwnedRadioAdapterProtocolState("available", "granted", "on", null)
      BluetoothAdapter.STATE_OFF -> OwnedRadioAdapterProtocolState("available", "granted", "off", null)
      BluetoothAdapter.STATE_TURNING_ON, BluetoothAdapter.STATE_TURNING_OFF ->
        OwnedRadioAdapterProtocolState("available", "granted", "resetting", null)
      else -> OwnedRadioAdapterProtocolState(
        availability = "available",
        authorization = "granted",
        power = "unknown",
        safeReason = "Android reported an unrecognized Bluetooth adapter state."
      )
    }
  }

  /** Read the system bond table without acquiring or mutating any GATT ownership. */
  internal fun bondedPeerSnapshots(): List<BondedPeerSnapshot> {
    val availableAdapter = adapter
    bondedPeerAdapterReadiness(
      adapterAvailable = availableAdapter != null,
      connectPermissionGranted = hasBluetoothConnectPermission(),
      adapterState = availableAdapter?.state ?: BluetoothAdapter.ERROR
    ).getOrThrow()
    val accessibleAdapter = availableAdapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    return normalizeBondedPeerSnapshots(
      accessibleAdapter.getBondedDevices().map { device ->
        BondedPeerSnapshot(
          nativePeerId = device.address,
          displayName = device.name?.takeIf { name -> name.isNotEmpty() }
        )
      }
    )
  }

  /**
   * Register [BluetoothAdapter.ACTION_STATE_CHANGED] and emit [onAdapterState] on transitions.
   * Idempotent; pair with [unregisterAdapterStateReceiver] from destroyClient.
   */
  fun registerAdapterStateReceiver() {
    if (adapterStateReceiver != null) return
    val receiver =
      object : BroadcastReceiver() {
        override fun onReceive(ctx: Context?, intent: Intent?) {
          if (intent?.action != BluetoothAdapter.ACTION_STATE_CHANGED) return
          val state = intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)
          handleAdapterStateTransition(state)
          onAdapterState?.invoke(mapAdapterState(state))
        }
      }
    val filter = IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED)
    // System broadcast (ACTION_STATE_CHANGED) requires RECEIVER_EXPORTED on API 33+.
    if (Build.VERSION.SDK_INT >= 33) {
      context.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
    } else {
      @Suppress("UnspecifiedRegisterReceiverFlag")
      context.registerReceiver(receiver, filter)
    }
    adapterStateReceiver = receiver
  }

  private fun handleAdapterStateTransition(state: Int) {
    if (state == BluetoothAdapter.STATE_ON) {
      pendingGattTeardowns.keys.toList().forEach { key -> retryGattTeardown(key) }
      return
    }
    if (!requiresImmediateGattTeardownOnAdapterState(state)) return

    pendingReconnect.clear()
    (gatts.keys + pendingGattTeardowns.keys).toSet().forEach { key ->
      failPendingForDevice(key, "adapter unavailable")
      val owner = pendingGattTeardowns[key]
      val gatt = gatts[key] ?: owner?.gatt ?: return@forEach
      val teardownFailure = completeGattTeardown(key, gatt)
      pendingDisconnectCallbacks.remove(key)?.invoke(teardownFailure)
    }
  }

  internal fun unregisterAdapterStateReceiver(): OwnedRadioTeardownFailure? {
    val receiver = adapterStateReceiver ?: return null
    try {
      context.unregisterReceiver(receiver)
    } catch (throwable: Exception) {
      OwnedAndroidLog.e("unregisterAdapterStateReceiver", throwable)
      // Keep the receiver owner intact so a later destroy attempt can retry the required release.
      return OwnedRadioTeardownFailure("unregisterAdapterStateReceiver", throwable)
    }
    adapterStateReceiver = null
    return null
  }

  fun registerConnectionListener(
    deviceId: String,
    listener: (deviceId: String, connected: Boolean, gattStatus: Int) -> Unit
  ) {
    connectionListeners[deviceId.uppercase()] = listener
  }

  fun unregisterConnectionListener(deviceId: String) {
    connectionListeners.remove(deviceId.uppercase())
  }

  /**
   * @param callbackType [ScanSettings] callback type (ALL_MATCHES / FIRST_MATCH / MATCH_LOST).
   * @param legacyScan when true (3.x default), restrict to legacy advertising; false enables BT5
   *   advertising extensions on API 26+ via [ScanSettings.Builder.setLegacy].
   */
  fun startScan(
    serviceUuids: Array<out String>?,
    scanMode: Int,
    callbackType: Int = ScanSettings.CALLBACK_TYPE_ALL_MATCHES,
    legacyScan: Boolean = true,
    allowDuplicates: Boolean = true,
    deviceAddresses: Array<out String> = emptyArray()
  ) {
    // ScanSettings.Builder.setLegacy exists only on API 26+; below that the
    // platform can only run a legacy scan, so accepting legacyScan=false would
    // silently ignore the caller's extended-advertising request. Fail closed
    // before any scan state is touched.
    require(legacyScan || Build.VERSION.SDK_INT >= 26) {
      "legacyScan=false requires ScanSettings.Builder.setLegacy (API 26+); this device cannot honour it"
    }
    val normalizedServiceUuids = normalizeScanServiceUuids(serviceUuids?.toList() ?: emptyList())
    check(scanCallback == null) { "Android scan cleanup is still owned by a prior scan" }
    scanSeenDeviceIds.clear()
    val a = adapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    scanner = a.bluetoothLeScanner ?: throw IllegalStateException("LE scanner unavailable")
    val builder = ScanSettings.Builder()
      .setScanMode(
        when (scanMode) {
          -1 -> ScanSettings.SCAN_MODE_OPPORTUNISTIC
          0 -> ScanSettings.SCAN_MODE_LOW_POWER
          1 -> ScanSettings.SCAN_MODE_BALANCED
          2 -> ScanSettings.SCAN_MODE_LOW_LATENCY
          else -> ScanSettings.SCAN_MODE_LOW_LATENCY
        }
      )
      .setCallbackType(callbackType)
    // setLegacy is API 26+; default true matches 3.x / legacyScan:true docs.
    if (Build.VERSION.SDK_INT >= 26) {
      builder.setLegacy(legacyScan)
    }
    val settings = builder.build()
    val filters = mutableListOf<ScanFilter>()
    val normalizedAddresses = deviceAddresses.map { it.uppercase() }.distinct()
    if (normalizedAddresses.isEmpty()) {
      normalizedServiceUuids.forEach { uuid ->
        filters.add(ScanFilter.Builder().setServiceUuid(ParcelUuid.fromString(uuid)).build())
      }
    } else if (normalizedServiceUuids.isEmpty()) {
      normalizedAddresses.forEach { address ->
        filters.add(ScanFilter.Builder().setDeviceAddress(address).build())
      }
    } else {
      normalizedAddresses.forEach { address ->
        normalizedServiceUuids.forEach { uuid ->
          filters.add(
            ScanFilter.Builder()
              .setDeviceAddress(address)
              .setServiceUuid(ParcelUuid.fromString(uuid))
              .build()
          )
        }
      }
    }
    val cb = object : ScanCallback() {
      override fun onScanResult(callbackType: Int, result: AndroidScanResult) {
        if (scanCallback !== this) return
        val device = result.device ?: return
        val id = device.address
        if (!allowDuplicates && !scanSeenDeviceIds.add(id.uppercase())) return
        val name = result.scanRecord?.deviceName ?: device.name
        val advertisement = protocolAdvertisement(result, id, name)
        onScanResult?.invoke(
          advertisement.deviceId,
          advertisement.name,
          advertisement.rssi,
          advertisement.connectable,
          advertisement.rawRecord?.copyOf()
        )
        onProtocolScanResult?.invoke(advertisement)
      }

      override fun onScanFailed(errorCode: Int) {
        if (scanCallback !== this) return
        OwnedAndroidLog.e("scan failed code=$errorCode")
        onScanFailed?.invoke(errorCode)
      }
    }
    try {
      if (filters.isEmpty()) {
        scanner?.startScan(null, settings, cb)
      } else {
        scanner?.startScan(filters, settings, cb)
      }
      scanCallback = cb
    } catch (error: Throwable) {
      val cleanupFailure =
        try {
          scanner?.stopScan(cb)
          null
        } catch (stopError: Throwable) {
          OwnedAndroidLog.e("startScan compensating stopScan", stopError)
          stopError
        }
      if (cleanupFailure == null) {
        scanCallback = null
        scanner = null
        scanSeenDeviceIds.clear()
      } else {
        scanCallback = cb
      }
      throw error
    }
  }

  private fun protocolAdvertisement(
    result: AndroidScanResult,
    deviceId: String,
    name: String?
  ): OwnedAndroidProtocolAdvertisement {
    val scanRecord = result.scanRecord
    return OwnedAndroidProtocolAdvertisement(
      deviceId = deviceId,
      name = name,
      rssi = result.rssi,
      txPower = txPowerFrom(result, scanRecord),
      connectable = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) result.isConnectable else null,
      appearance = appearanceFrom(scanRecord),
      serviceUuids = scanRecord?.serviceUuids
        ?.map { uuid -> uuid.uuid.toString() }
        ?.takeIf { it.isNotEmpty() },
      solicitedServiceUuids = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
        scanRecord?.serviceSolicitationUuids
          ?.map { uuid -> uuid.uuid.toString() }
          ?.takeIf { it.isNotEmpty() }
      } else {
        null
      },
      serviceData = scanRecord?.serviceData
        ?.entries
        ?.map { (uuid, value) -> OwnedAndroidProtocolServiceData(uuid.uuid.toString(), value.copyOf()) }
        ?.sortedBy { entry -> entry.serviceUuid }
        ?.takeIf { it.isNotEmpty() },
      manufacturerData = manufacturerDataFrom(scanRecord),
      rawRecord = scanRecord?.bytes?.copyOf()
    )
  }

  private fun txPowerFrom(result: AndroidScanResult, scanRecord: ScanRecord?): Int? {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      return result.txPower.takeUnless { txPower -> txPower == AndroidScanResult.TX_POWER_NOT_PRESENT }
    }
    return scanRecord?.txPowerLevel?.takeUnless { txPower -> txPower == Int.MIN_VALUE }
  }

  private fun appearanceFrom(scanRecord: ScanRecord?): Int? {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU || scanRecord == null) return null
    val appearanceBytes = scanRecord.advertisingDataMap[ScanRecord.DATA_TYPE_APPEARANCE] ?: return null
    if (appearanceBytes.size != 2) return null
    return (appearanceBytes[0].toInt() and 0xFF) or ((appearanceBytes[1].toInt() and 0xFF) shl 8)
  }

  private fun manufacturerDataFrom(scanRecord: ScanRecord?): List<OwnedAndroidProtocolManufacturerData>? {
    val source = scanRecord?.manufacturerSpecificData ?: return null
    val entries = mutableListOf<OwnedAndroidProtocolManufacturerData>()
    for (index in 0 until source.size()) {
      val value = source.valueAt(index) ?: continue
      entries.add(OwnedAndroidProtocolManufacturerData(source.keyAt(index), value.copyOf()))
    }
    return entries.takeIf { it.isNotEmpty() }
  }

  internal fun stopScan(): OwnedRadioTeardownFailure? {
    val cb = scanCallback
    if (cb == null) {
      scanSeenDeviceIds.clear()
      return null
    }
    return try {
      scanner?.stopScan(cb)
      scanCallback = null
      scanSeenDeviceIds.clear()
      null
    } catch (t: Throwable) {
      OwnedAndroidLog.e("stopScan", t)
      // Retain the callback and seen-device state so the caller can retry cleanup.
      OwnedRadioTeardownFailure("stopScan", t)
    }
  }

  internal fun hasScanCleanupOwnership(): Boolean = scanCallback != null

  internal fun securityState(deviceId: String): OwnedAndroidSecurityState {
    val bluetoothAdapter = adapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    if (!hasBluetoothConnectPermission()) {
      return OwnedAndroidSecurityState(bond = "unknown", pairingPossible = null)
    }
    val device = bluetoothAdapter.getRemoteDevice(deviceId)
    return try {
      OwnedAndroidSecurityState(
        bond = when (device.bondState) {
          BluetoothDevice.BOND_BONDED -> "bonded"
          BluetoothDevice.BOND_BONDING -> "bonding"
          BluetoothDevice.BOND_NONE -> "notBonded"
          else -> "unknown"
        },
        pairingPossible = true
      )
    } catch (error: SecurityException) {
      OwnedAndroidLog.e("securityState", error)
      OwnedAndroidSecurityState(bond = "unknown", pairingPossible = null)
    }
  }

  internal fun pair(
    deviceId: String,
    transport: String,
    callback: (String, OwnedAndroidSecurityState) -> Unit
  ): Long {
    val bluetoothAdapter = adapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    val device = bluetoothAdapter.getRemoteDevice(deviceId)
    if (isAlreadyPaired(device.bondState, device.type, transport)) {
      if (!postNow { callback("alreadyPaired", OwnedAndroidSecurityState("bonded", true)) }) {
        callback("alreadyPaired", OwnedAndroidSecurityState("bonded", true))
      }
      return 0L
    }
    val key = device.address.uppercase()
    check(pendingBondPairs.putIfAbsent(key, callback) == null) { "Android pairing is already active for $deviceId" }
    try {
      if (!createBond(device, transport)) {
        pendingBondPairs.remove(key, callback)
        throw IllegalStateException("Android rejected the bond request")
      }
    } catch (error: Exception) {
      pendingBondPairs.remove(key, callback)
      throw error
    }
    return nextGattOperationId.getAndIncrement()
  }

  private fun createBond(device: BluetoothDevice, transport: String): Boolean = when (transport) {
    "platformDefault" -> device.createBond()
    "le" -> {
      val method = device.javaClass.getMethod("createBond", Int::class.javaPrimitiveType)
      val result = try {
        method.invoke(device, BluetoothDevice.TRANSPORT_LE)
      } catch (error: InvocationTargetException) {
        val cause = error.targetException
        when (cause) {
          is SecurityException -> throw cause
          is Exception -> throw cause
          else -> throw IllegalStateException("Android createBond(TRANSPORT_LE) failed", cause)
        }
      }
      result as? Boolean
        ?: throw IllegalStateException("Android createBond(TRANSPORT_LE) returned a non-boolean result")
    }
    else -> throw IllegalArgumentException("Unsupported Android pair transport '$transport'")
  }

  internal fun registerBondStateReceiver() {
    if (bondStateReceiver != null) return
    val receiver = object : BroadcastReceiver() {
      override fun onReceive(ctx: Context?, intent: Intent?) {
        try {
          if (intent?.action != BluetoothDevice.ACTION_BOND_STATE_CHANGED) return
          val device = IntentCompat.getParcelableExtra(
            intent,
            BluetoothDevice.EXTRA_DEVICE,
            BluetoothDevice::class.java
          ) ?: return
          val state = when (intent.getIntExtra(BluetoothDevice.EXTRA_BOND_STATE, BluetoothDevice.ERROR)) {
            BluetoothDevice.BOND_BONDED -> "bonded"
            BluetoothDevice.BOND_BONDING -> "bonding"
            BluetoothDevice.BOND_NONE -> "notBonded"
            else -> "unknown"
          }
          val deviceId = device.address
          val pairingPossible = hasBluetoothConnectPermission()
          onSecurityState?.invoke(
            deviceId,
            OwnedAndroidSecurityState(bond = state, pairingPossible = pairingPossible)
          )
          val callback = pendingBondPairs[deviceId.uppercase()]
          if (callback != null && state != "bonding") {
            pendingBondPairs.remove(deviceId.uppercase(), callback)
            callback(
              when (state) {
                "bonded" -> "paired"
                "notBonded" -> "rejected"
                else -> "unknown"
              },
              OwnedAndroidSecurityState(state, pairingPossible)
            )
          }
        } catch (error: SecurityException) {
          OwnedAndroidLog.e("bondStateReceiver", error)
        }
      }
    }
    val filter = IntentFilter(BluetoothDevice.ACTION_BOND_STATE_CHANGED)
    if (Build.VERSION.SDK_INT >= 33) context.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
    else {
      @Suppress("UnspecifiedRegisterReceiverFlag")
      context.registerReceiver(receiver, filter)
    }
    bondStateReceiver = receiver
  }

  internal fun unregisterBondStateReceiver(): OwnedRadioTeardownFailure? {
    val receiver = bondStateReceiver ?: return null
    return try {
      context.unregisterReceiver(receiver)
      bondStateReceiver = null
      null
    } catch (error: Exception) {
      OwnedAndroidLog.e("unregisterBondStateReceiver", error)
      OwnedRadioTeardownFailure("unregisterBondStateReceiver", error)
    }
  }

  /**
   * The public Android API that cancels an in-progress bond was added in API 37.
   * This project currently supports the API-36 compile/runtime boundary, so
   * removing this callback would only make the library forget ownership while
   * the OS continues the asynchronous createBond() ceremony.
   */
  internal fun clearPendingBondPair(deviceId: String): Boolean =
    throw UnsupportedOperationException(
      "Android bonding cancellation is unsupported before API 37 for $deviceId"
    )

  private fun hasBluetoothConnectPermission(): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.S ||
      context.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) == PackageManager.PERMISSION_GRANTED

  fun connect(deviceId: String, autoConnect: Boolean) {
    val key = deviceId.uppercase()
    check(!pendingGattTeardowns.containsKey(key)) {
      "Android GATT cleanup is still pending for $deviceId"
    }
    val prior = gatts[key]
    if (prior != null) {
      // R3-F003: never close() before STATE_DISCONNECTED — disconnect prior and reconnect
      // from the DISCONNECTED callback (or safety timeout). Immediate close→connectGatt is a
      // common cause of GATT status 133 / flaky reconnect.
      failPendingForDevice(key, "reconnect")
      clearCharCacheForDevice(key)
      deviceQueues.remove(key)?.clear()
      discovered.remove(key)
      pendingReconnect[key] = autoConnect
      scheduleSafeClose(key, prior)
      try {
        prior.disconnect()
      } catch (t: Throwable) {
        OwnedAndroidLog.e("connect disconnect prior gatt", t)
        val teardownFailure = completeGattTeardown(key, prior)
        if (teardownFailure != null) throw IllegalStateException(
          "Android GATT cleanup failed before reconnect",
          teardownFailure.throwable
        )
        openGatt(deviceId, key, autoConnect)
      }
      return
    }
    openGatt(deviceId, key, autoConnect)
  }

  internal fun disconnect(
    deviceId: String,
    onComplete: (OwnedRadioTeardownFailure?) -> Unit = {}
  ): OwnedRadioTeardownFailure? {
    val key = deviceId.uppercase()
    // Explicit user/app disconnect cancels any pending reconnect-after-teardown.
    pendingReconnect.remove(key)
    failPendingForDevice(key, "disconnected")
    clearCharCacheForDevice(key)
    deviceQueues.remove(key)?.clear()
    val cleanupFailure = retryCleanupLedger().firstOrNull()
    val g = gatts[key]
    if (g == null) {
      discovered.remove(key)
      val failure = cleanupFailure ?: retryGattTeardown(key)
      onComplete(failure)
      return failure
    }
    if (pendingGattTeardowns[key] != null) {
      val teardownFailure = retryGattTeardown(key)
      val failure = cleanupFailure ?: teardownFailure
      onComplete(failure)
      return failure
    }
    if (requiresImmediateGattTeardownOnAdapterState(adapter?.state)) {
      val teardownFailure = completeGattTeardown(key, g)
      val failure = cleanupFailure ?: teardownFailure
      onComplete(failure)
      return failure
    }
    // R3-F003: only disconnect(); close() waits for STATE_DISCONNECTED (or safety timeout).
    pendingDisconnectCallbacks[key] = onComplete
    scheduleSafeClose(key, g)
    try {
      g.disconnect()
    } catch (t: Throwable) {
      OwnedAndroidLog.e("disconnect", t)
      val failure = cleanupFailure ?: completeGattTeardown(key, g)
      pendingDisconnectCallbacks.remove(key)?.invoke(failure)
      return failure
    }
    if (cleanupFailure != null) {
      pendingDisconnectCallbacks.remove(key)?.invoke(cleanupFailure)
    }
    return cleanupFailure
  }

  private fun openGatt(deviceId: String, key: String, autoConnect: Boolean) {
    pendingReconnect.remove(key)
    val a = adapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    val device = try {
      a.getRemoteDevice(deviceId)
    } catch (throwable: Exception) {
      throw IllegalStateException("Android rejected Bluetooth device $deviceId", throwable)
    }
    val gatt = try {
      device.connectGatt(context, autoConnect, gattCallback, BluetoothDevice.TRANSPORT_LE)
        ?: throw IllegalStateException("Android returned no BluetoothGatt for $deviceId")
    } catch (throwable: Exception) {
      throw IllegalStateException("Android could not open BluetoothGatt for $deviceId", throwable)
    }
    val generation = nextGattGeneration.getAndIncrement()
    effectiveMtuByDevice.remove(key)
    gatts[key] = gatt
    gattGenerations[key] = generation
    gattGenerationByInstance[gatt] = generation
  }

  /**
   * Force-close [gatt] and drop local state. Cancels any pending safety timeout.
   * Safe to call from DISCONNECTED callback or after a failed disconnect.
   */
  private fun completeGattTeardown(key: String, gatt: BluetoothGatt): OwnedRadioTeardownFailure? {
    val generation = gattGenerationByInstance[gatt] ?: return null
    return completeGattTeardown(key, gatt, generation)
  }

  private fun completeGattTeardown(
    key: String,
    gatt: BluetoothGatt,
    generation: Long
  ): OwnedRadioTeardownFailure? {
    if (gatts[key] !== gatt || gattGenerations[key] != generation || gattGenerationByInstance[gatt] != generation) {
      return null
    }
    cancelSafeClose(key)
    try {
      gatt.close()
    } catch (throwable: Exception) {
      OwnedAndroidLog.e("completeGattTeardown close for $key", throwable)
      val failure = OwnedRadioTeardownFailure("closeGatt:$key:generation=$generation", throwable)
      pendingGattTeardowns[key] = GattTeardownOwner(gatt, generation)
      reportCleanupFailure(failure)
      return failure
    }
    pendingGattTeardowns.remove(key, GattTeardownOwner(gatt, generation))
    gatts.remove(key, gatt)
    gattGenerations.remove(key, generation)
    gattGenerationByInstance.remove(gatt, generation)
    discovered.remove(key)
    effectiveMtuByDevice[key]?.let { state ->
      if (state.gattGeneration == generation) effectiveMtuByDevice.remove(key, state)
    }
    activeNativeSubscriptionOwnership.clearDevice(key)
    clearCharCacheForDevice(key)
    deviceQueues.remove(key)?.clear()
    return null
  }

  private fun scheduleSafeClose(key: String, gatt: BluetoothGatt) {
    cancelSafeClose(key)
    val r =
      Runnable {
        if (gatts[key] === gatt) {
          OwnedAndroidLog.i("GATT close safety timeout; cleanup callback absent, forced close proceeding for $key")
          dispatchConnectionState(key, false, BluetoothGatt.GATT_FAILURE)
          failPendingForDevice(key, "disconnected timeout")
          val teardownFailure = completeGattTeardown(key, gatt)
          pendingDisconnectCallbacks.remove(key)?.invoke(teardownFailure)
          if (teardownFailure != null) return@Runnable
          // If a reconnect was queued, attempt it after forced teardown.
          pendingReconnect.remove(key)?.let { autoConnect ->
            try {
              // device address is the key uppercased; openGatt needs original or upper — both OK.
              openGatt(key, key, autoConnect)
            } catch (t: Throwable) {
              OwnedAndroidLog.e("reconnect after close timeout", t)
              dispatchConnectionState(key, false, BluetoothGatt.GATT_FAILURE)
            }
          }
        }
      }
    closeTimeouts[key] = r
    if (mainHandler?.postDelayed(r, GATT_CLOSE_TIMEOUT_MS) != true) {
      closeTimeouts.remove(key, r)
    }
  }

  private fun cancelSafeClose(key: String) {
    closeTimeouts.remove(key)?.let { runnable -> mainHandler?.removeCallbacks(runnable) }
  }

  private fun scheduleDeferred(delayMs: Long, action: () -> Unit): Boolean =
    scheduleDelayed?.invoke(delayMs, action) ?: mainHandler?.postDelayed(action, delayMs) ?: false

  private fun postNow(action: () -> Unit): Boolean =
    post?.invoke(action) ?: mainHandler?.post(action) ?: false

  internal fun reportCleanupFailure(failure: OwnedRadioTeardownFailure) {
    onCleanupFailure?.invoke(failure)
  }

  private fun registerRetryableCleanup(
    key: String,
    cleanup: () -> OwnedRadioTeardownFailure?
  ) {
    retryableCleanups.putIfAbsent(key, cleanup)
  }

  private fun retryGattTeardown(key: String): OwnedRadioTeardownFailure? {
    val owner = pendingGattTeardowns[key] ?: return null
    return completeGattTeardown(key, owner.gatt, owner.generation)
  }

  private fun retryCleanupLedger(): List<OwnedRadioTeardownFailure> {
    val failures = mutableListOf<OwnedRadioTeardownFailure>()
    retryableCleanups.entries.toList().forEach { entry ->
      val failure = try {
        entry.value()
      } catch (throwable: Throwable) {
        OwnedRadioTeardownFailure(entry.key, throwable)
      }
      if (failure == null) {
        retryableCleanups.remove(entry.key, entry.value)
      } else {
        failures.add(failure)
        reportCleanupFailure(failure)
      }
    }
    return failures
  }

  fun discover(deviceId: String, onDone: (Boolean) -> Unit): Long {
    return enqueue(
      deviceId,
      onCancelled = { onDone(false) },
      onStartFailure = { onDone(false) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      if (gatt == null) {
        if (!token.isPubliclySettled()) onDone(false)
        done()
        return@enqueue
      }
      val key = "discover:${deviceId.uppercase()}"
      pending[key] = { r ->
        if (!token.isPubliclySettled()) {
          onDone(r.isSuccess)
        }
        done()
      }
      if (!gatt.discoverServices()) {
        pending.remove(key)
        if (!token.isPubliclySettled()) onDone(false)
        done()
      }
    }
  }

  fun services(deviceId: String): List<android.bluetooth.BluetoothGattService> {
    return discovered[deviceId.uppercase()]?.toList()
      ?: gatts[deviceId.uppercase()]?.services
      ?: emptyList()
  }

  private fun readCharacteristic(
    deviceId: String,
    serviceUuid: UUID,
    charUuid: UUID,
    onResult: (Result<ByteArray?>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("characteristic read cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val ch = findChar(deviceId, serviceUuid, charUuid)
      if (gatt == null || ch == null) {
        onResult(Result.failure(IllegalStateException("characteristic not found")))
        done()
        return@enqueue
      }
      // Key includes serviceUuid so two services sharing a char UUID never collide (R2-F095).
      val key = pendingCharKey("read", deviceId, serviceUuid, charUuid)
      pending[key] = { r ->
        if (!token.isPubliclySettled()) {
          onResult(r)
        }
        done()
      }
      if (!gatt.readCharacteristic(ch)) {
        pending.remove(key)
        onResult(Result.failure(IllegalStateException("readCharacteristic failed to start")))
        done()
      }
    }
  }

  /** Reads an exact duplicate-safe service/characteristic occurrence. */
  fun readCharacteristicExact(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    onResult: (Result<ByteArray?>) -> Unit
  ): Long {
    return readCharacteristicTarget(
      deviceId,
      serviceUuid,
      serviceOccurrence,
      characteristicUuid,
      characteristicOccurrence,
      onResult
    )
  }

  private fun readCharacteristicTarget(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    onResult: (Result<ByteArray?>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("exact characteristic read cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val characteristic = findChar(
        deviceId,
        serviceUuid,
        serviceOccurrence,
        characteristicUuid,
        characteristicOccurrence
      )
      if (gatt == null || characteristic == null) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact characteristic not found")),
          token
        )
        return@enqueue
      }
      val pending = ExactBytePending(
        deviceId.uppercase(),
        gatt,
        gattGenerations[deviceId.uppercase()] ?: 0L,
        token,
        onResult,
        done
      )
      if (exactReadPending.putIfAbsent(characteristic, pending) != null) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact characteristic read is already pending")),
          token
        )
        return@enqueue
      }
      if (!gatt.readCharacteristic(characteristic)) {
        if (exactReadPending.remove(characteristic, pending)) {
          completeExactByte(
            pending,
            Result.failure(IllegalStateException("readCharacteristic failed to start"))
          )
        }
      }
    }
  }

  private fun writeCharacteristic(
    deviceId: String,
    serviceUuid: UUID,
    charUuid: UUID,
    value: ByteArray,
    withResponse: Boolean,
    onResult: (Result<ByteArray?>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("characteristic write cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val ch = findChar(deviceId, serviceUuid, charUuid)
      if (gatt == null || ch == null) {
        onResult(Result.failure(IllegalStateException("characteristic not found")))
        done()
        return@enqueue
      }
      val writeType =
        if (withResponse) BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
        else BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
      ch.writeType = writeType
      val key = pendingCharKey("write", deviceId, serviceUuid, charUuid)
      val owner = AndroidPendingWriteOwner(
        deviceKeyUpper = deviceId.uppercase(),
        attribute = ch,
        value = value.copyOf()
      ) { r ->
        if (!token.isPubliclySettled()) {
          onResult(r)
        }
        done()
      }
      if (!pendingWriteRegistry.putIfAbsent(key, owner)) {
        onResult(Result.failure(IllegalStateException("characteristic write is already pending")))
        done()
        return@enqueue
      }
      if (Build.VERSION.SDK_INT >= 33) {
        val status = gatt.writeCharacteristic(ch, value, writeType)
        if (!acceptApi33WriteStatus(status)) {
          if (pendingWriteRegistry.remove(key, owner)) {
            owner.callback(
              Result.failure(classifyAndroidGattOperationFailure("characteristic-write", status))
            )
          }
        }
      } else {
        @Suppress("DEPRECATION")
        ch.value = value
        @Suppress("DEPRECATION")
        val started = gatt.writeCharacteristic(ch)
        if (!started) {
          if (pendingWriteRegistry.remove(key, owner)) {
            owner.callback(Result.failure(IllegalStateException("writeCharacteristic failed to start")))
          }
        }
      }
    }
  }

  /** Writes an exact duplicate-safe service/characteristic occurrence. */
  fun writeCharacteristicExact(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    value: ByteArray,
    withResponse: Boolean,
    onResult: (Result<ByteArray?>) -> Unit
  ): Long {
    return writeCharacteristicTarget(
      deviceId,
      serviceUuid,
      serviceOccurrence,
      characteristicUuid,
      characteristicOccurrence,
      value,
      withResponse,
      onResult
    )
  }

  private fun writeCharacteristicTarget(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    value: ByteArray,
    withResponse: Boolean,
    onResult: (Result<ByteArray?>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("exact characteristic write cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val characteristic = findChar(
        deviceId,
        serviceUuid,
        serviceOccurrence,
        characteristicUuid,
        characteristicOccurrence
      )
      if (gatt == null || characteristic == null) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact characteristic not found")),
          token
        )
        return@enqueue
      }
      val writeType =
        if (withResponse) BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
        else BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
      characteristic.writeType = writeType
      val pending = ExactBytePending(
        deviceId.uppercase(),
        gatt,
        gattGenerations[deviceId.uppercase()] ?: 0L,
        token,
        onResult,
        done
      )
      if (exactWritePending.putIfAbsent(characteristic, pending) != null) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact characteristic write is already pending")),
          token
        )
        return@enqueue
      }
      // Retain an independent copy because the caller may reuse its buffer while Android writes.
      exactWriteValues[characteristic] = value.copyOf()
      if (Build.VERSION.SDK_INT >= 33) {
        val status = gatt.writeCharacteristic(characteristic, value, writeType)
        if (!acceptApi33WriteStatus(status)) {
          val observedPayload = exactWriteValues[characteristic]
          if (exactWritePending.remove(characteristic, pending)) {
            removeAndroidWritePayloadIfSame(exactWriteValues, characteristic, observedPayload)
            completeExactByte(
              pending,
              Result.failure(classifyAndroidGattOperationFailure("characteristic-write", status))
            )
          }
        }
      } else {
        @Suppress("DEPRECATION")
        characteristic.value = value
        @Suppress("DEPRECATION")
        val started = gatt.writeCharacteristic(characteristic)
        if (!started) {
          val observedPayload = exactWriteValues[characteristic]
          if (exactWritePending.remove(characteristic, pending)) {
            removeAndroidWritePayloadIfSame(exactWriteValues, characteristic, observedPayload)
            completeExactByte(
              pending,
              Result.failure(IllegalStateException("writeCharacteristic failed to start"))
            )
          }
        }
      }
    }
  }

  /**
   * Pure helper for API-33 write status handling (unit-testable without a radio).
   * @return true if write was accepted (pending should wait for callback); false if failed immediately.
   */
  fun acceptApi33WriteStatus(status: Int): Boolean {
    return status == BluetoothGatt.GATT_SUCCESS
  }

  fun requestMtu(deviceId: String, mtu: Int, onResult: (Result<Int>) -> Unit): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("MTU request cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      if (gatt == null) {
        if (!token.isPubliclySettled()) {
          onResult(Result.failure(IllegalStateException("Not connected to $deviceId")))
        }
        done()
        return@enqueue
      }
      val key = "mtu:${deviceId.uppercase()}"
      pendingMtu[key] = { r ->
        if (!token.isPubliclySettled()) {
          onResult(r)
        }
        done()
      }
      if (!gatt.requestMtu(mtu)) {
        pendingMtu.remove(key)
        if (!token.isPubliclySettled()) {
          onResult(Result.failure(IllegalStateException("requestMtu failed to start")))
        }
        done()
      }
    }
  }

  /**
   * Reads the MTU measured by the current GATT generation. Android exposes no
   * synchronous getter for the negotiated ATT MTU, so an unmeasured link is
   * reported as a successful null observation rather than a guessed default.
   */
  fun readEffectiveMtu(deviceId: String, onResult: (Result<Int?>) -> Unit): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("effective MTU read cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val key = deviceId.uppercase()
      val gatt = gatts[key]
      val generation = gattGenerations[key]
      if (gatt == null || generation == null) {
        if (token.markPubliclySettled()) {
          onResult(Result.failure(IllegalStateException("Not connected to $deviceId")))
        }
        done()
        return@enqueue
      }
      val state = effectiveMtuByDevice[key]
      val value = if (state?.gattGeneration == generation) state.mtu else null
      if (token.markPubliclySettled()) onResult(Result.success(value))
      done()
    }
  }

  fun readRemoteRssi(deviceId: String, onResult: (Result<Int>) -> Unit): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("RSSI read cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      if (gatt == null) {
        if (!token.isPubliclySettled()) {
          onResult(Result.failure(IllegalStateException("Not connected to $deviceId")))
        }
        done()
        return@enqueue
      }
      val key = "rssi:${deviceId.uppercase()}"
      pendingRssi[key] = { r ->
        if (!token.isPubliclySettled()) {
          onResult(r)
        }
        done()
      }
      if (!gatt.readRemoteRssi()) {
        pendingRssi.remove(key)
        if (!token.isPubliclySettled()) {
          onResult(Result.failure(IllegalStateException("readRemoteRssi failed to start")))
        }
        done()
      }
    }
  }

  internal fun readPhy(deviceId: String, onResult: (Result<OwnedAndroidPhy>) -> Unit): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("PHY read cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      if (gatt == null) {
        if (!token.isPubliclySettled()) onResult(Result.failure(IllegalStateException("Not connected to $deviceId")))
        done()
        return@enqueue
      }
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
        if (!token.isPubliclySettled()) onResult(Result.failure(IllegalStateException("Android PHY requires API 26")))
        done()
        return@enqueue
      }
      val key = "phyRead:${deviceId.uppercase()}"
      pendingPhyReads[key] = { result ->
        if (!token.isPubliclySettled()) onResult(result)
        done()
      }
      try {
        gatt.readPhy()
      } catch (error: Throwable) {
        pendingPhyReads.remove(key)
        if (!token.isPubliclySettled()) onResult(Result.failure(error))
        done()
      }
    }
  }

  internal fun requestPhy(
    deviceId: String,
    txPhy: Int,
    rxPhy: Int,
    onResult: (Result<OwnedAndroidPhy?>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("PHY request cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      if (gatt == null) {
        if (!token.isPubliclySettled()) onResult(Result.failure(IllegalStateException("Not connected to $deviceId")))
        done()
        return@enqueue
      }
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
        if (!token.isPubliclySettled()) onResult(Result.failure(IllegalStateException("Android PHY requires API 26")))
        done()
        return@enqueue
      }
      val key = "phyRequest:${deviceId.uppercase()}"
      pendingPhyRequests[key] = { result ->
        if (!token.isPubliclySettled()) onResult(result)
        done()
      }
      try {
        gatt.setPreferredPhy(txPhy, rxPhy, android.bluetooth.BluetoothDevice.PHY_OPTION_NO_PREFERRED)
      } catch (error: Throwable) {
        pendingPhyRequests.remove(key)
        if (!token.isPubliclySettled()) onResult(Result.failure(error))
        done()
      }
    }
  }

  /**
   * Enable/disable notifications or indications.
   * [subscriptionType]: "notification" | "indication" | null (auto: notify preferred, else indicate).
   */
  private fun setNotify(
    deviceId: String,
    serviceUuid: UUID,
    charUuid: UUID,
    enable: Boolean,
    subscriptionType: String? = null,
    onResult: (Result<Unit>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("notification operation cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val ch = findChar(deviceId, serviceUuid, charUuid)
      if (gatt == null || ch == null) {
        onResult(Result.failure(IllegalStateException("characteristic not found")))
        done()
        return@enqueue
      }
      val payload = resolveCccdPayload(enable, subscriptionType, ch.properties)
      if (payload == null) {
        onResult(
          Result.failure(
            IllegalStateException(
              "characteristic supports neither notify nor indicate (subscriptionType=$subscriptionType)"
            )
          )
        )
        done()
        return@enqueue
      }
      if (!gatt.setCharacteristicNotification(ch, enable)) {
        onResult(
          Result.failure(
            classifyAndroidNotificationRegistrationFailure(
              "setCharacteristicNotification"
            )
          )
        )
        done()
        return@enqueue
      }
      val cccd = ch.getDescriptor(CCCD_UUID)
      if (cccd == null) {
        // No CCCD: local notification registration is the best we can do.
        onResult(Result.success(Unit))
        done()
        return@enqueue
      }
      // Must wait for onDescriptorWrite — reporting success before CCCD is armed is a race.
      val key = pendingCharKey("cccd", deviceId, serviceUuid, charUuid)
      pendingDesc[key] = { r ->
        if (!token.isPubliclySettled()) {
          onResult(r)
        }
        done()
      }
      if (Build.VERSION.SDK_INT >= 33) {
        val status = gatt.writeDescriptor(cccd, payload)
        if (status != BluetoothGatt.GATT_SUCCESS) {
          pendingDesc.remove(key)
          onResult(Result.failure(IllegalStateException("writeDescriptor failed to start status=$status")))
          done()
        }
      } else {
        @Suppress("DEPRECATION")
        cccd.value = payload
        @Suppress("DEPRECATION")
        val started = gatt.writeDescriptor(cccd)
        if (!started) {
          pendingDesc.remove(key)
          onResult(Result.failure(IllegalStateException("writeDescriptor failed to start")))
          done()
        }
      }
    }
  }

  /** Enables or disables an exact duplicate-safe service/characteristic occurrence. */
  fun setNotifyExact(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    enable: Boolean,
    subscriptionType: String? = null,
    onResult: (Result<Unit>) -> Unit
  ): Long {
    return setNotifyTarget(
      deviceId,
      serviceUuid,
      serviceOccurrence,
      characteristicUuid,
      characteristicOccurrence,
      enable,
      subscriptionType,
      onResult
    )
  }

  private fun setNotifyTarget(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    enable: Boolean,
    subscriptionType: String?,
    onResult: (Result<Unit>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("exact notification operation cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val characteristic = findChar(
        deviceId,
        serviceUuid,
        serviceOccurrence,
        characteristicUuid,
        characteristicOccurrence
      )
      if (gatt == null || characteristic == null) {
        completeExactUnitDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact characteristic not found")),
          token
        )
        return@enqueue
      }
      val payload = resolveCccdPayload(enable, subscriptionType, characteristic.properties)
      if (payload == null) {
        completeExactUnitDirect(
          onResult,
          done,
          Result.failure(
            IllegalStateException(
              "characteristic supports neither notify nor indicate (subscriptionType=$subscriptionType)"
            )
          ),
          token
        )
        return@enqueue
      }
      if (!gatt.setCharacteristicNotification(characteristic, enable)) {
        val failure = classifyAndroidNotificationRegistrationFailure(
          "setCharacteristicNotification"
        )
        val registrationGeneration = gattGenerations[deviceId.uppercase()] ?: run {
          completeExactUnitDirect(onResult, done, Result.failure(failure), token)
          return@enqueue
        }
        val pending = ExactUnitPending(
          deviceId.uppercase(),
          gatt,
          registrationGeneration,
          token,
          onResult,
          done,
          subscriptionEnabled = enable,
          subscriptionCharacteristic = characteristic
        )
        if (exactRegistrationPending.putIfAbsent(characteristic, pending) != null) {
          completeExactUnitDirect(onResult, done, Result.failure(failure), token)
          return@enqueue
        }
        if (shouldAwaitAndroidRegistrationDisconnectEvidence(failure)) {
          exactRegistrationTerminalArbiter.defer(characteristic, failure)
        } else if (exactRegistrationPending.remove(characteristic, pending)) {
          completeExactUnit(pending, Result.failure(failure))
        }
        return@enqueue
      }
      val gattGeneration = gattGenerations[deviceId.uppercase()] ?: run {
        completeExactUnitDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact notification GATT generation is unavailable")),
          token
        )
        return@enqueue
      }
      val rollbackRegistration = {
        rollbackNotifyRegistration(
          deviceId.uppercase(),
          gatt,
          gattGeneration,
          characteristic,
          enable
        )
      }
      val cccd = characteristic.getDescriptor(CCCD_UUID)
      if (cccd == null) {
        // There is no remote CCCD to write, but the platform accepted the local registration.
        if (!token.isPubliclySettled()) {
          if (enable) {
            activeNativeSubscriptionOwnership.activate(
              deviceId.uppercase(),
              gattGeneration,
              characteristic
            )
          } else {
            activeNativeSubscriptionOwnership.deactivate(
              deviceId.uppercase(),
              gattGeneration,
              characteristic
            )
          }
        }
        if (token.isPubliclySettled()) {
          rollbackRegistration()?.let { failure ->
            registerRetryableCleanup(failure.operation) { rollbackRegistration() }
            reportCleanupFailure(failure)
          }
        }
        completeExactUnitDirect(onResult, done, Result.success(Unit), token)
        return@enqueue
      }
      val pending = ExactUnitPending(
        deviceId.uppercase(),
        gatt,
        gattGeneration,
        token,
        onResult,
        done,
        rollbackRegistration,
        subscriptionEnabled = enable,
        subscriptionCharacteristic = characteristic
      )
      if (exactCccdPending.putIfAbsent(cccd, pending) != null) {
        completeExactUnitDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact CCCD write is already pending")),
          token
        )
        return@enqueue
      }
      if (enable) {
        activeNativeSubscriptionOwnership.arm(
          deviceId.uppercase(),
          gattGeneration,
          characteristic
        )
      }
      if (Build.VERSION.SDK_INT >= 33) {
        val status = gatt.writeDescriptor(cccd, payload)
        if (status != BluetoothGatt.GATT_SUCCESS) {
          if (exactCccdPending.remove(cccd, pending)) {
            completeExactUnit(
              pending,
              Result.failure(AndroidCccdSubmissionFailure(status))
            )
          }
        }
      } else {
        @Suppress("DEPRECATION")
        cccd.value = payload
        @Suppress("DEPRECATION")
        val started = gatt.writeDescriptor(cccd)
        if (!started) {
          if (exactCccdPending.remove(cccd, pending)) {
            completeExactUnit(
              pending,
              Result.failure(AndroidCccdSubmissionFailure(null))
            )
          }
        }
      }
    }
  }

  private fun readDescriptor(
    deviceId: String,
    serviceUuid: UUID,
    charUuid: UUID,
    descUuid: UUID,
    onResult: (Result<ByteArray?>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("descriptor read cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val ch = findChar(deviceId, serviceUuid, charUuid)
      val desc = ch?.getDescriptor(descUuid)
      if (gatt == null || desc == null) {
        onResult(Result.failure(IllegalStateException("descriptor not found")))
        done()
        return@enqueue
      }
      val key = pendingDescKey("descRead", deviceId, serviceUuid, charUuid, descUuid)
      pendingDescRead[key] = { r ->
        if (!token.isPubliclySettled()) {
          onResult(r)
        }
        done()
      }
      if (!gatt.readDescriptor(desc)) {
        pendingDescRead.remove(key)
        onResult(Result.failure(IllegalStateException("readDescriptor failed to start")))
        done()
      }
    }
  }

  private fun writeDescriptor(
    deviceId: String,
    serviceUuid: UUID,
    charUuid: UUID,
    descUuid: UUID,
    value: ByteArray,
    onResult: (Result<Unit>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("descriptor write cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val ch = findChar(deviceId, serviceUuid, charUuid)
      val desc = ch?.getDescriptor(descUuid)
      if (gatt == null || desc == null) {
        onResult(Result.failure(IllegalStateException("descriptor not found")))
        done()
        return@enqueue
      }
      val key = pendingDescKey("descWrite", deviceId, serviceUuid, charUuid, descUuid)
      pendingDesc[key] = { r ->
        if (!token.isPubliclySettled()) {
          onResult(r)
        }
        done()
      }
      if (Build.VERSION.SDK_INT >= 33) {
        val status = gatt.writeDescriptor(desc, value)
        if (status != BluetoothGatt.GATT_SUCCESS) {
          pendingDesc.remove(key)
          onResult(Result.failure(IllegalStateException("writeDescriptor failed to start status=$status")))
          done()
        }
      } else {
        @Suppress("DEPRECATION")
        desc.value = value
        @Suppress("DEPRECATION")
        val started = gatt.writeDescriptor(desc)
        if (!started) {
          pendingDesc.remove(key)
          onResult(Result.failure(IllegalStateException("writeDescriptor failed to start")))
          done()
        }
      }
    }
  }

  /** Reads one descriptor selected by its full duplicate-safe canonical path. */
  fun readDescriptorExact(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    descriptorUuid: UUID,
    descriptorOccurrence: Int,
    onResult: (Result<ByteArray?>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("exact descriptor read cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val descriptor = findDescriptor(
        deviceId,
        serviceUuid,
        serviceOccurrence,
        characteristicUuid,
        characteristicOccurrence,
        descriptorUuid,
        descriptorOccurrence
      )
      if (gatt == null || descriptor == null) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact descriptor not found")),
          token
        )
        return@enqueue
      }
      val pending = ExactBytePending(
        deviceId.uppercase(),
        gatt,
        gattGenerations[deviceId.uppercase()] ?: 0L,
        token,
        onResult,
        done
      )
      if (exactDescriptorReadPending.putIfAbsent(descriptor, pending) != null) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact descriptor read is already pending")),
          token
        )
        return@enqueue
      }
      if (!gatt.readDescriptor(descriptor)) {
        if (exactDescriptorReadPending.remove(descriptor, pending)) {
          completeExactByte(pending, Result.failure(IllegalStateException("readDescriptor failed to start")))
        }
      }
    }
  }

  /** Writes one descriptor selected by its full duplicate-safe canonical path. */
  fun writeDescriptorExact(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    descriptorUuid: UUID,
    descriptorOccurrence: Int,
    value: ByteArray,
    onResult: (Result<Unit>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = { onResult(Result.failure(IllegalStateException("exact descriptor write cancelled"))) },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      val descriptor = findDescriptor(
        deviceId,
        serviceUuid,
        serviceOccurrence,
        characteristicUuid,
        characteristicOccurrence,
        descriptorUuid,
        descriptorOccurrence
      )
      if (gatt == null || descriptor == null) {
        completeExactUnitDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact descriptor not found")),
          token
        )
        return@enqueue
      }
      val pending = ExactUnitPending(
        deviceId.uppercase(),
        gatt,
        gattGenerations[deviceId.uppercase()] ?: 0L,
        token,
        onResult,
        done
      )
      if (exactDescriptorWritePending.putIfAbsent(descriptor, pending) != null) {
        completeExactUnitDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact descriptor write is already pending")),
          token
        )
        return@enqueue
      }
      if (Build.VERSION.SDK_INT >= 33) {
        val status = gatt.writeDescriptor(descriptor, value)
        if (status != BluetoothGatt.GATT_SUCCESS) {
          if (exactDescriptorWritePending.remove(descriptor, pending)) {
            completeExactUnit(
              pending,
              Result.failure(IllegalStateException("writeDescriptor failed to start status=$status"))
            )
          }
        }
      } else {
        @Suppress("DEPRECATION")
        descriptor.value = value.copyOf()
        @Suppress("DEPRECATION")
        val started = gatt.writeDescriptor(descriptor)
        if (!started) {
          if (exactDescriptorWritePending.remove(descriptor, pending)) {
            completeExactUnit(pending, Result.failure(IllegalStateException("writeDescriptor failed to start")))
          }
        }
      }
    }
  }

  /**
   * [BluetoothGatt.requestConnectionPriority] — returns false if not connected or call rejected.
   * Priority values match [BluetoothGatt.CONNECTION_PRIORITY_BALANCED]/HIGH/LOW_POWER (0/1/2).
   */
  fun requestConnectionPriority(
    deviceId: String,
    connectionPriority: Int,
    onResult: (Result<Boolean>) -> Unit
  ): Long {
    return enqueue(
      deviceId,
      onCancelled = {
        onResult(Result.failure(IllegalStateException("connection priority request cancelled")))
      },
      onStartFailure = { error -> onResult(Result.failure(error)) }
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      if (gatt == null) {
        if (token.markPubliclySettled()) onResult(Result.success(false))
        done()
        return@enqueue
      }
      val accepted = try {
        gatt.requestConnectionPriority(connectionPriority)
      } catch (t: Throwable) {
        OwnedAndroidLog.e("requestConnectionPriority", t)
        false
      }
      if (token.markPubliclySettled()) onResult(Result.success(accepted))
      done()
    }
  }

  internal fun destroy(): OwnedRadioTeardownResult {
    val failures = mutableListOf<OwnedRadioTeardownFailure>()
    stopScan()?.let { failure -> failures.add(failure) }
    unregisterAdapterStateReceiver()?.let { failure -> failures.add(failure) }
    unregisterBondStateReceiver()?.let { failure -> failures.add(failure) }
    pendingBondPairs.clear()
    pendingReconnect.clear()
    failures.addAll(retryCleanupLedger())
    val pendingDeviceKeys = mutableSetOf<String>()
    pendingDeviceKeys.addAll(gatts.keys)
    pendingDeviceKeys.addAll(pendingGattTeardowns.keys)
    pendingDeviceKeys.addAll(deviceQueues.keys)
    pendingDeviceKeys.addAll(exactReadPending.values.map { it.deviceKeyUpper })
    pendingDeviceKeys.addAll(exactWritePending.values.map { it.deviceKeyUpper })
    pendingDeviceKeys.addAll(exactCccdPending.values.map { it.deviceKeyUpper })
    pendingDeviceKeys.addAll(exactRegistrationPending.values.map { it.deviceKeyUpper })
    pendingDeviceKeys.addAll(exactDescriptorReadPending.values.map { it.deviceKeyUpper })
    pendingDeviceKeys.addAll(exactDescriptorWritePending.values.map { it.deviceKeyUpper })
    pendingDeviceKeys.forEach { key -> failPendingForDevice(key, "radio destroyed") }
    // failPendingForDevice may create retryable CCCD rollback ownership; include
    // that cleanup in this destroy receipt rather than reducing it to a log.
    failures.addAll(retryCleanupLedger())
    // Force-close immediately on destroy — no need to wait for DISCONNECTED callbacks.
    (gatts.keys + pendingGattTeardowns.keys).toSet().forEach { key ->
      val owner = pendingGattTeardowns[key]
      val gatt = gatts[key] ?: owner?.gatt ?: return@forEach
      val generation = gattGenerations[key] ?: owner?.generation ?: return@forEach
      completeGattTeardown(key, gatt, generation)?.let { failure -> failures.add(failure) }
    }
    closeTimeouts.values.forEach { runnable -> mainHandler?.removeCallbacks(runnable) }
    closeTimeouts.clear()
    pendingDisconnectCallbacks.clear()
    if (failures.isEmpty() && pendingGattTeardowns.isEmpty() && gatts.isEmpty()) {
      discovered.clear()
      gattGenerations.clear()
      gattGenerationByInstance.clear()
      charCache.clear()
      connectionListeners.clear()
      deviceQueues.clear()
      pending.clear()
      pendingMtu.clear()
      effectiveMtuByDevice.clear()
      pendingRssi.clear()
      pendingPhyReads.clear()
      pendingPhyRequests.clear()
      pendingDesc.clear()
      pendingDescRead.clear()
      pendingWriteRegistry.clear()
      exactReadPending.clear()
      exactWritePending.clear()
      exactWriteValues.clear()
      exactCccdPending.clear()
      exactCccdTerminalArbiter.clear()
      exactRegistrationPending.clear()
      exactRegistrationTerminalArbiter.clear()
      exactWriteTerminalArbiter.clear()
      writeTerminalArbiter.clear()
      exactDescriptorReadPending.clear()
      exactDescriptorWritePending.clear()
      activeNativeSubscriptionOwnership.clear()
    }
    return OwnedRadioTeardownResult(failures)
  }

  internal fun cancelOperation(operationId: Long): Boolean =
    deviceQueues.values.any { queue -> queue.cancel(operationId) }

  internal fun attachConnectedGatt(
    deviceId: String,
    gatt: BluetoothGatt,
    services: List<android.bluetooth.BluetoothGattService>
  ): Long {
    val key = deviceId.uppercase()
    val generation = nextGattGeneration.getAndIncrement()
    gatts[key] = gatt
    gattGenerations[key] = generation
    gattGenerationByInstance[gatt] = generation
    discovered[key] = services.toMutableList()
    return generation
  }

  internal fun nativeGattCallback(): BluetoothGattCallback = gattCallback

  internal fun isNativeSubscriptionActive(
    deviceId: String,
    gattGeneration: Long,
    characteristic: BluetoothGattCharacteristic
  ): Boolean = activeNativeSubscriptionOwnership.isActive(deviceId, gattGeneration, characteristic)

  private fun enqueue(
    deviceId: String,
    onCancelled: () -> Unit = {},
    onStartFailure: (Throwable) -> Unit = {},
    op: (token: GattSerialQueue.GattOperationToken, done: () -> Unit) -> Unit
  ): Long {
    val key = deviceId.uppercase()
    return deviceQueues.getOrPut(key) {
      GattSerialQueue(
        handler = mainHandler,
        post = post,
        idProvider = { nextGattOperationId.getAndIncrement() }
      )
    }.submitCancellable(op, onCancelled, onStartFailure)
  }

  private fun clearCharCacheForDevice(deviceKeyUpper: String) {
    val prefix = "$deviceKeyUpper:"
    charCache.keys.filter { it.startsWith(prefix) }.forEach { charCache.remove(it) }
  }

  private fun invalidateNativeSubscriptionsForDatabaseChange(
    deviceKeyUpper: String,
    gatt: BluetoothGatt,
    gattGeneration: Long
  ) {
    activeNativeSubscriptionOwnership
      .invalidateForDatabaseChange(deviceKeyUpper, gattGeneration)
      .forEach { characteristic ->
        fun disable(): OwnedRadioTeardownFailure? = try {
          if (gatt.setCharacteristicNotification(characteristic, false)) {
            null
          } else {
            OwnedRadioTeardownFailure(
              "serviceChangedDisable:$deviceKeyUpper:generation=$gattGeneration",
              IllegalStateException("setCharacteristicNotification invalidation was rejected")
            )
          }
        } catch (throwable: Throwable) {
          OwnedRadioTeardownFailure(
            "serviceChangedDisable:$deviceKeyUpper:generation=$gattGeneration",
            throwable
          )
        }

        disable()?.let { failure ->
          registerRetryableCleanup(failure.operation) { disable() }
          reportCleanupFailure(failure)
        }
      }
  }

  private fun failPendingForDevice(
    deviceKeyUpper: String,
    reason: String,
    gattStatus: Int? = null
  ) {
    effectiveMtuByDevice.remove(deviceKeyUpper)
    deviceQueues.remove(deviceKeyUpper)?.clear(IllegalStateException(reason))
    val failBytes = Result.failure<ByteArray?>(IllegalStateException(reason))
    val failInt = Result.failure<Int>(IllegalStateException(reason))
    val failUnit = Result.failure<Unit>(IllegalStateException(reason))
    // A peer link-loss callback can win the race with an asynchronous GATT
    // callback. Keep that exact Android status on pending CCCD work so the
    // protocol boundary reports connectionLost instead of flattening the race
    // to platformFailure.
    val failCccd: Result<Unit> = if (gattStatus == ANDROID_GATT_LINK_LOSS_STATUS) {
      Result.failure(classifyAndroidGattOperationFailure("cccd-write", gattStatus))
    } else {
      failUnit
    }
    pending.keys
      .filter { key ->
        keyBelongsToDevice(key, "discover", deviceKeyUpper) ||
          keyBelongsToDevice(key, "read", deviceKeyUpper)
      }
      .toList()
      .forEach { key ->
        pending.remove(key)?.invoke(failBytes)
      }
    pendingMtu.remove("mtu:$deviceKeyUpper")?.invoke(failInt)
    pendingRssi.remove("rssi:$deviceKeyUpper")?.invoke(failInt)
    pendingPhyReads.remove("phyRead:$deviceKeyUpper")?.invoke(
      Result.failure(IllegalStateException(reason))
    )
    pendingPhyRequests.remove("phyRequest:$deviceKeyUpper")?.invoke(
      Result.failure(IllegalStateException(reason))
    )
    pendingDesc.keys
      .filter { key ->
        keyBelongsToDevice(key, "cccd", deviceKeyUpper) ||
          keyBelongsToDevice(key, "descWrite", deviceKeyUpper)
      }
      .toList()
      .forEach { key ->
        pendingDesc.remove(key)?.invoke(if (key.startsWith("cccd:")) failCccd else failUnit)
      }
    pendingDescRead.keys
      .filter { key -> keyBelongsToDevice(key, "descRead", deviceKeyUpper) }
      .toList()
      .forEach { key -> pendingDescRead.remove(key)?.invoke(failBytes) }
    // Remove only the owner observed by this teardown. A queued replacement
    // may reuse the same UUID key while the old disconnect callback drains.
    pendingWriteRegistry.entriesForDevice(deviceKeyUpper).forEach { (key, owner) ->
      if (pendingWriteRegistry.remove(key, owner)) {
        writeTerminalArbiter.claim(key)
        owner.callback(failBytes)
      }
    }
    exactReadPending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (exactReadPending.remove(entry.key, entry.value)) {
          completeExactByte(entry.value, failBytes)
        }
      }
    exactWritePending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        val observedPayload = exactWriteValues[entry.key]
        if (exactWritePending.remove(entry.key, entry.value)) {
          removeAndroidWritePayloadIfSame(exactWriteValues, entry.key, observedPayload)
          exactWriteTerminalArbiter.claim(entry.key)
          completeExactByte(entry.value, failBytes)
        }
      }
    exactCccdPending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (exactCccdPending.remove(entry.key, entry.value)) {
          exactCccdTerminalArbiter.claim(entry.key)
          completeExactUnit(
            entry.value,
            if (entry.value.subscriptionEnabled != null) failCccd else failUnit
          )
        }
      }
    exactRegistrationPending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (exactRegistrationPending.remove(entry.key, entry.value)) {
          exactRegistrationTerminalArbiter.claim(entry.key)
          completeExactUnit(
            entry.value,
            if (gattStatus == ANDROID_GATT_LINK_LOSS_STATUS) failCccd else failUnit
          )
        }
      }
    exactDescriptorReadPending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (exactDescriptorReadPending.remove(entry.key, entry.value)) {
          completeExactByte(entry.value, failBytes)
        }
      }
    exactDescriptorWritePending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (exactDescriptorWritePending.remove(entry.key, entry.value)) {
          completeExactUnit(entry.value, failUnit)
        }
      }
  }

  private fun findChar(
    deviceId: String,
    serviceUuid: UUID,
    charUuid: UUID
  ): BluetoothGattCharacteristic? {
    val cacheKey = "${deviceId.uppercase()}:$serviceUuid:$charUuid"
    charCache[cacheKey]?.let { return it }
    val services = services(deviceId)
    for (s in services) {
      if (s.uuid == serviceUuid) {
        val c = s.getCharacteristic(charUuid)
        if (c != null) {
          charCache[cacheKey] = c
          return c
        }
      }
    }
    return null
  }

  private fun keyBelongsToDevice(key: String, operation: String, deviceKeyUpper: String): Boolean {
    val prefix = "$operation:$deviceKeyUpper"
    return key == prefix || key.startsWith("$prefix:")
  }

  private fun findChar(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int
  ): BluetoothGattCharacteristic? {
    if (serviceOccurrence < 0 || characteristicOccurrence < 0) return null
    val service = resolveUuidOccurrence(services(deviceId), serviceUuid, serviceOccurrence) { it.uuid }
      ?: return null
    return resolveUuidOccurrence(
      service.characteristics,
      characteristicUuid,
      characteristicOccurrence
    ) { it.uuid }
  }

  private fun findDescriptor(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    descriptorUuid: UUID,
    descriptorOccurrence: Int
  ): BluetoothGattDescriptor? {
    if (descriptorOccurrence < 0) return null
    val characteristic = findChar(
      deviceId,
      serviceUuid,
      serviceOccurrence,
      characteristicUuid,
      characteristicOccurrence
    ) ?: return null
    return resolveUuidOccurrence(
      characteristic.descriptors,
      descriptorUuid,
      descriptorOccurrence
    ) { it.uuid }
  }

  private fun isCurrentGatt(pendingDeviceKey: String, gatt: BluetoothGatt, generation: Long): Boolean =
    gatts[pendingDeviceKey] === gatt &&
      gattGenerations[pendingDeviceKey] == generation &&
      gattGenerationByInstance[gatt] == generation

  private fun isCurrentGattCallback(gatt: BluetoothGatt): Boolean {
    val key = gatt.device.address.uppercase()
    val generation = gattGenerationByInstance[gatt] ?: return false
    return isCurrentGatt(key, gatt, generation)
  }

  private fun completeExactByte(
    pending: ExactBytePending,
    result: Result<ByteArray?>
  ) {
    if (pending.token.isPubliclySettled()) {
      pending.done()
      return
    }
    try {
      pending.callback(result)
    } catch (throwable: Throwable) {
      OwnedAndroidLog.e("protocol exact byte callback", throwable)
    } finally {
      pending.done()
    }
  }

  private fun completeExactByteDirect(
    callback: (Result<ByteArray?>) -> Unit,
    done: () -> Unit,
    result: Result<ByteArray?>,
    token: GattSerialQueue.GattOperationToken? = null
  ) {
    try {
      if (token?.isPubliclySettled() != true) callback(result)
    } catch (throwable: Throwable) {
      OwnedAndroidLog.e("protocol exact byte callback", throwable)
    } finally {
      done()
    }
  }

  private fun completeExactUnit(
    pending: ExactUnitPending,
    result: Result<Unit>
  ) {
    val shouldRollback = pending.token.isPubliclySettled() || result.isFailure
    val rollbackFailure = if (shouldRollback) {
      try {
        pending.physicalCleanup?.invoke()
      } catch (throwable: Throwable) {
        OwnedRadioTeardownFailure("cccdRollback:${pending.deviceKeyUpper}", throwable)
      }
    } else {
      null
    }
    if (rollbackFailure != null) {
      registerRetryableCleanup(rollbackFailure.operation) {
        pending.physicalCleanup?.invoke()
      }
      reportCleanupFailure(rollbackFailure)
    }
    if (pending.token.isPubliclySettled()) {
      pending.subscriptionCharacteristic?.let { characteristic ->
        activeNativeSubscriptionOwnership.abandon(
          pending.deviceKeyUpper,
          pending.gattGeneration,
          characteristic
        )
      }
      pending.done()
      return
    }
    val terminalResult = androidGattTerminalResult(result, rollbackFailure)
    val stagedNotifications = if (terminalResult.isSuccess && !pending.token.isPubliclySettled()) {
      pending.subscriptionEnabled?.let { enabled ->
        pending.subscriptionCharacteristic?.let { characteristic ->
          if (enabled) {
            activeNativeSubscriptionOwnership.activate(
              pending.deviceKeyUpper,
              pending.gattGeneration,
              characteristic
            )
          } else {
            activeNativeSubscriptionOwnership.deactivate(
              pending.deviceKeyUpper,
              pending.gattGeneration,
              characteristic
            )
            emptyList()
          }
        }
      }
    } else {
      pending.subscriptionCharacteristic?.let { characteristic ->
        activeNativeSubscriptionOwnership.abandon(
          pending.deviceKeyUpper,
          pending.gattGeneration,
          characteristic
        )
      }
      emptyList()
    }
    try {
      pending.callback(terminalResult)
    } catch (throwable: Throwable) {
      OwnedAndroidLog.e("protocol exact unit callback", throwable)
    } finally {
      if (!stagedNotifications.isNullOrEmpty()) {
        pending.subscriptionCharacteristic?.let { characteristic ->
          val serviceUuid = characteristic.service?.uuid
          if (serviceUuid != null) {
            for (value in stagedNotifications) {
              onNotification?.invoke(
                pending.deviceKeyUpper,
                serviceUuid,
                characteristic.uuid,
                value
              )
              onProtocolNotification?.invoke(pending.deviceKeyUpper, characteristic, value.copyOf())
            }
          }
        }
      }
      pending.done()
    }
  }

  private fun rollbackNotifyRegistration(
    deviceKeyUpper: String,
    gatt: BluetoothGatt,
    generation: Long,
    characteristic: BluetoothGattCharacteristic,
    enabled: Boolean
  ): OwnedRadioTeardownFailure? {
    if (!isCurrentGatt(deviceKeyUpper, gatt, generation)) {
      // The old GATT is no longer owned by this attachment; its local
      // registration died with that exact generation and needs no retry.
      return null
    }
    return try {
      if (gatt.setCharacteristicNotification(characteristic, !enabled)) {
        null
      } else {
        OwnedRadioTeardownFailure(
          "cccdRollback:$deviceKeyUpper:generation=$generation",
          AndroidNotificationRollbackRejected()
        )
      }
    } catch (throwable: Throwable) {
      OwnedRadioTeardownFailure("cccdRollback:$deviceKeyUpper:generation=$generation", throwable)
    }
  }

  private fun deliverNotification(
    gatt: BluetoothGatt,
    characteristic: BluetoothGattCharacteristic,
    value: ByteArray
  ) {
    if (!isCurrentGattCallback(gatt)) return
    val deviceKeyUpper = gatt.device.address.uppercase()
    val gattGeneration = gattGenerationByInstance[gatt] ?: return
    val copied = value.copyOf()
    if (activeNativeSubscriptionOwnership.isActive(deviceKeyUpper, gattGeneration, characteristic)) {
      val serviceUuid = characteristic.service?.uuid ?: return
      onNotification?.invoke(gatt.device.address, serviceUuid, characteristic.uuid, copied)
      onProtocolNotification?.invoke(gatt.device.address, characteristic, copied.copyOf())
      return
    }
    activeNativeSubscriptionOwnership.stage(deviceKeyUpper, gattGeneration, characteristic, copied)
  }

  private fun deferExactCccdFailure(
    descriptor: BluetoothGattDescriptor,
    pending: ExactUnitPending,
    failure: AndroidGattOperationFailure
  ) {
    if (exactCccdPending[descriptor] === pending) {
      exactCccdTerminalArbiter.defer(descriptor, failure)
    }
  }

  private fun completeExactUnitDirect(
    callback: (Result<Unit>) -> Unit,
    done: () -> Unit,
    result: Result<Unit>,
    token: GattSerialQueue.GattOperationToken? = null
  ) {
    try {
      if (token?.isPubliclySettled() != true) callback(result)
    } catch (throwable: Throwable) {
      OwnedAndroidLog.e("protocol exact unit callback", throwable)
    } finally {
      done()
    }
  }

  /** Pending map key: op:DEVICE:serviceUuid:charUuid — service disambiguates shared char UUIDs. */
  private fun pendingCharKey(op: String, deviceId: String, serviceUuid: UUID, charUuid: UUID): String =
    "$op:${deviceId.uppercase()}:$serviceUuid:$charUuid"

  private fun pendingDescKey(
    op: String,
    deviceId: String,
    serviceUuid: UUID,
    charUuid: UUID,
    descUuid: UUID
  ): String = "$op:${deviceId.uppercase()}:$serviceUuid:$charUuid:$descUuid"

  private fun charPendingKeyFromGatt(op: String, gattDeviceIdUpper: String, characteristic: BluetoothGattCharacteristic): String? {
    val serviceUuid = characteristic.service?.uuid ?: return null
    return "$op:$gattDeviceIdUpper:$serviceUuid:${characteristic.uuid}"
  }

  private fun descPendingKeyFromGatt(
    op: String,
    gattDeviceIdUpper: String,
    descriptor: BluetoothGattDescriptor
  ): String? {
    val ch = descriptor.characteristic ?: return null
    val serviceUuid = ch.service?.uuid ?: return null
    return "$op:$gattDeviceIdUpper:$serviceUuid:${ch.uuid}:${descriptor.uuid}"
  }

  private fun dispatchConnectionState(id: String, connected: Boolean, gattStatus: Int) {
    connectionListeners[id.uppercase()]?.invoke(id, connected, gattStatus)
    onConnectionState?.invoke(id, connected, gattStatus)
  }

  private val gattCallback = object : BluetoothGattCallback() {
    override fun onConnectionStateChange(gatt: BluetoothGatt, status: Int, newState: Int) {
      if (!isCurrentGattCallback(gatt)) return
      val id = gatt.device.address
      val key = id.uppercase()
      val generation = gattGenerationByInstance[gatt] ?: return
      if (newState == BluetoothProfile.STATE_CONNECTED) {
        if (status == BluetoothGatt.GATT_SUCCESS) {
          dispatchConnectionState(id, true, status)
        } else {
          // Non-success while "connected" is a failed connect — surface status and tear down.
          dispatchConnectionState(id, false, status)
          failPendingForDevice(key, "connect failed status=$status")
          completeGattTeardown(key, gatt, generation)
        }
      } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
        // Always pass gatt status: status 133 etc. means failed connect, not clean disconnect.
        // R3-F003: close() only after STATE_DISCONNECTED (not from disconnect()/connect prior).
        dispatchConnectionState(id, false, status)
        failPendingForDevice(key, "disconnected status=$status", status)
        val teardownFailure = completeGattTeardown(key, gatt, generation)
        pendingDisconnectCallbacks.remove(key)?.invoke(teardownFailure)
        if (teardownFailure != null) return
        // Reconnect that waited for a clean prior teardown.
        pendingReconnect.remove(key)?.let { autoConnect ->
          try {
            openGatt(id, key, autoConnect)
          } catch (t: Throwable) {
            OwnedAndroidLog.e("reconnect after DISCONNECTED", t)
            dispatchConnectionState(id, false, BluetoothGatt.GATT_FAILURE)
          }
        }
      }
    }

    override fun onDescriptorWrite(
      gatt: BluetoothGatt,
      descriptor: BluetoothGattDescriptor,
      status: Int
    ) {
      if (!isCurrentGattCallback(gatt)) return
      exactDescriptorWritePending.remove(descriptor)?.let { pending ->
        if (!isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) {
          pending.done()
          return@let
        }
        if (status == BluetoothGatt.GATT_SUCCESS) {
          completeExactUnit(pending, Result.success(Unit))
        } else {
          completeExactUnit(
            pending,
            Result.failure(classifyAndroidGattOperationFailure("descriptor-write", status))
          )
        }
        return
      }
      exactCccdPending[descriptor]?.let { pending ->
        if (!isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) {
          if (exactCccdPending.remove(descriptor, pending)) {
            exactCccdTerminalArbiter.claim(descriptor)
            pending.done()
          }
          return@let
        }
        if (status == BluetoothGatt.GATT_SUCCESS) {
          exactCccdTerminalArbiter.claim(descriptor)
          if (exactCccdPending.remove(descriptor, pending)) {
            completeExactUnit(pending, Result.success(Unit))
          }
          return
        }
        // A later success on this generation may still claim a provisional
        // failure. Duplicate failures cannot replace the first deferred result.
        if (exactCccdTerminalArbiter.isPending(descriptor)) return
        val failure = classifyAndroidGattOperationFailure("cccd-write", status)
        if (shouldAwaitAndroidCccdDisconnectEvidence(failure)) {
          deferExactCccdFailure(descriptor, pending, failure)
        } else if (exactCccdPending.remove(descriptor, pending)) {
          exactCccdTerminalArbiter.claim(descriptor)
          completeExactUnit(pending, Result.failure(failure))
        }
        return
      }
      val id = gatt.device.address.uppercase()
      val ch = descriptor.characteristic ?: return
      val serviceUuid = ch.service?.uuid ?: return
      // Prefer specific descWrite key, then CCCD key from setNotify.
      val descKey = "descWrite:$id:$serviceUuid:${ch.uuid}:${descriptor.uuid}"
      val cccdKey = "cccd:$id:$serviceUuid:${ch.uuid}"
      val matchedKey =
        when {
          pendingDesc.containsKey(descKey) -> descKey
          pendingDesc.containsKey(cccdKey) -> cccdKey
          else -> null
      }
      val cb = matchedKey?.let { pendingDesc.remove(it) }
      if (status == BluetoothGatt.GATT_SUCCESS) {
        cb?.invoke(Result.success(Unit))
      } else {
        cb?.invoke(
          Result.failure(
            classifyAndroidGattOperationFailure(
              if (matchedKey?.startsWith("cccd:") == true) "cccd-write" else "descriptor-write",
              status
            )
          )
        )
      }
    }

    @Deprecated("Deprecated in Java")
    override fun onDescriptorRead(
      gatt: BluetoothGatt,
      descriptor: BluetoothGattDescriptor,
      status: Int
    ) {
      if (!isCurrentGattCallback(gatt)) return
      exactDescriptorReadPending.remove(descriptor)?.let { pending ->
        if (!isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) {
          pending.done()
          return@let
        }
        @Suppress("DEPRECATION")
        val value = descriptor.value?.copyOf()
        if (status == BluetoothGatt.GATT_SUCCESS) {
          completeExactByte(pending, Result.success(value))
        } else {
          completeExactByte(
            pending,
            Result.failure(IllegalStateException("onDescriptorRead status=$status"))
          )
        }
        return
      }
      val id = gatt.device.address.uppercase()
      val key = descPendingKeyFromGatt("descRead", id, descriptor) ?: return
      @Suppress("DEPRECATION")
      val value = descriptor.value
      if (status == BluetoothGatt.GATT_SUCCESS) {
        pendingDescRead.remove(key)?.invoke(Result.success(value))
      } else {
        pendingDescRead.remove(key)?.invoke(
          Result.failure(IllegalStateException("onDescriptorRead status=$status"))
        )
      }
    }

    override fun onDescriptorRead(
      gatt: BluetoothGatt,
      descriptor: BluetoothGattDescriptor,
      status: Int,
      value: ByteArray
    ) {
      if (!isCurrentGattCallback(gatt)) return
      exactDescriptorReadPending.remove(descriptor)?.let { pending ->
        if (!isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) {
          pending.done()
          return@let
        }
        if (status == BluetoothGatt.GATT_SUCCESS) {
          completeExactByte(pending, Result.success(value.copyOf()))
        } else {
          completeExactByte(
            pending,
            Result.failure(IllegalStateException("onDescriptorRead status=$status"))
          )
        }
        return
      }
      val id = gatt.device.address.uppercase()
      val key = descPendingKeyFromGatt("descRead", id, descriptor) ?: return
      if (status == BluetoothGatt.GATT_SUCCESS) {
        pendingDescRead.remove(key)?.invoke(Result.success(value))
      } else {
        pendingDescRead.remove(key)?.invoke(
          Result.failure(IllegalStateException("onDescriptorRead status=$status"))
        )
      }
    }

    override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
      val id = gatt.device.address.uppercase()
      if (!isCurrentGattCallback(gatt)) return
      if (status == BluetoothGatt.GATT_SUCCESS) {
        // Fresh GATT tree — drop stale characteristic handles.
        clearCharCacheForDevice(id)
        discovered[id] = gatt.services.toMutableList()
        pending.remove("discover:$id")?.invoke(Result.success(null))
      } else {
        pending.remove("discover:$id")?.invoke(Result.failure(IllegalStateException("discover status=$status")))
      }
    }

    @Deprecated("Deprecated in Java")
    override fun onCharacteristicRead(
      gatt: BluetoothGatt,
      characteristic: BluetoothGattCharacteristic,
      status: Int
    ) {
      if (!isCurrentGattCallback(gatt)) return
      exactReadPending.remove(characteristic)?.let { pending ->
        if (!isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) {
          pending.done()
          return@let
        }
        @Suppress("DEPRECATION")
        val value = characteristic.value?.copyOf()
        if (status == BluetoothGatt.GATT_SUCCESS) {
          completeExactByte(pending, Result.success(value))
        } else {
          completeExactByte(pending, Result.failure(IllegalStateException("read status=$status")))
        }
        return
      }
      val id = gatt.device.address.uppercase()
      val key = charPendingKeyFromGatt("read", id, characteristic) ?: return
      @Suppress("DEPRECATION")
      val value = characteristic.value
      if (status == BluetoothGatt.GATT_SUCCESS) {
        pending.remove(key)?.invoke(Result.success(value))
      } else {
        pending.remove(key)?.invoke(Result.failure(IllegalStateException("read status=$status")))
      }
    }

    override fun onCharacteristicRead(
      gatt: BluetoothGatt,
      characteristic: BluetoothGattCharacteristic,
      value: ByteArray,
      status: Int
    ) {
      if (!isCurrentGattCallback(gatt)) return
      exactReadPending.remove(characteristic)?.let { pending ->
        if (!isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) {
          pending.done()
          return@let
        }
        if (status == BluetoothGatt.GATT_SUCCESS) {
          completeExactByte(pending, Result.success(value.copyOf()))
        } else {
          completeExactByte(pending, Result.failure(IllegalStateException("read status=$status")))
        }
        return
      }
      val id = gatt.device.address.uppercase()
      val key = charPendingKeyFromGatt("read", id, characteristic) ?: return
      if (status == BluetoothGatt.GATT_SUCCESS) {
        pending.remove(key)?.invoke(Result.success(value))
      } else {
        pending.remove(key)?.invoke(Result.failure(IllegalStateException("read status=$status")))
      }
    }

    override fun onCharacteristicWrite(
      gatt: BluetoothGatt,
      characteristic: BluetoothGattCharacteristic,
      status: Int
    ) {
      if (!isCurrentGattCallback(gatt)) return
      // A provisional callback failure must remain pending while lifecycle
      // evidence already in flight gets a chance to claim the terminal.
      if (exactWriteTerminalArbiter.isPending(characteristic)) return
      exactWritePending[characteristic]?.let { pending ->
        val observedPayload = exactWriteValues[characteristic]
        if (!isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) {
          if (exactWritePending.remove(characteristic, pending)) {
            exactWriteTerminalArbiter.claim(characteristic)
            removeAndroidWritePayloadIfSame(exactWriteValues, characteristic, observedPayload)
            pending.done()
          }
          return@let
        }
        if (status == BluetoothGatt.GATT_SUCCESS) {
          if (exactWritePending.remove(characteristic, pending)) {
            removeAndroidWritePayloadIfSame(exactWriteValues, characteristic, observedPayload)
            completeExactByte(pending, Result.success(observedPayload?.copyOf()))
          }
        } else {
          if (exactWriteTerminalArbiter.deferCharacteristicWriteFailure(characteristic, status)) {
            return@let
          }
          val failure = classifyAndroidGattOperationFailure("characteristic-write", status)
          if (exactWritePending.remove(characteristic, pending)) {
            removeAndroidWritePayloadIfSame(exactWriteValues, characteristic, observedPayload)
            completeExactByte(pending, Result.failure(failure))
          }
        }
        return
      }
      val id = gatt.device.address.uppercase()
      val key = charPendingKeyFromGatt("write", id, characteristic) ?: return
      if (writeTerminalArbiter.isPending(key)) return
      if (status == BluetoothGatt.GATT_SUCCESS) {
        // The owner retains the API-33 payload and rejects stale callbacks
        // after a same-key replacement has taken ownership.
        val owner = pendingWriteRegistry.get(key) ?: return
        pendingWriteRegistry.complete(key, characteristic, Result.success(owner.value.copyOf()))
      } else {
        if (writeTerminalArbiter.deferCharacteristicWriteFailure(key, status)) return
        val failure = classifyAndroidGattOperationFailure("characteristic-write", status)
        pendingWriteRegistry.complete(key, characteristic, Result.failure(failure))
      }
    }

    @Deprecated("Deprecated in Java")
    override fun onCharacteristicChanged(
      gatt: BluetoothGatt,
      characteristic: BluetoothGattCharacteristic
    ) {
      @Suppress("DEPRECATION")
      val raw = characteristic.value ?: return
      deliverNotification(gatt, characteristic, raw)
    }

    override fun onCharacteristicChanged(
      gatt: BluetoothGatt,
      characteristic: BluetoothGattCharacteristic,
      value: ByteArray
    ) {
      deliverNotification(gatt, characteristic, value)
    }

    override fun onMtuChanged(gatt: BluetoothGatt, mtu: Int, status: Int) {
      val id = gatt.device.address.uppercase()
      if (!isCurrentGattCallback(gatt)) return
      val key = "mtu:$id"
      val generation = gattGenerationByInstance[gatt] ?: return
      if (status == BluetoothGatt.GATT_SUCCESS && mtu in 23..517) {
        effectiveMtuByDevice[id] = EffectiveMtuState(generation, mtu)
        pendingMtu.remove(key)?.invoke(Result.success(mtu))
      } else if (status == BluetoothGatt.GATT_SUCCESS) {
        pendingMtu.remove(key)?.invoke(Result.failure(IllegalStateException("onMtuChanged returned invalid MTU=$mtu")))
      } else {
        pendingMtu.remove(key)?.invoke(Result.failure(IllegalStateException("onMtuChanged status=$status")))
      }
    }

    override fun onReadRemoteRssi(gatt: BluetoothGatt, rssi: Int, status: Int) {
      val id = gatt.device.address.uppercase()
      if (!isCurrentGattCallback(gatt)) return
      val key = "rssi:$id"
      if (status == BluetoothGatt.GATT_SUCCESS) {
        pendingRssi.remove(key)?.invoke(Result.success(rssi))
      } else {
        pendingRssi.remove(key)?.invoke(Result.failure(IllegalStateException("onReadRemoteRssi status=$status")))
      }
    }

    override fun onPhyRead(gatt: BluetoothGatt, txPhy: Int, rxPhy: Int, status: Int) {
      if (!isCurrentGattCallback(gatt)) return
      val key = "phyRead:${gatt.device.address.uppercase()}"
      if (status != BluetoothGatt.GATT_SUCCESS) {
        pendingPhyReads.remove(key)?.invoke(Result.failure(IllegalStateException("onPhyRead status=$status")))
        return
      }
      val mappedTx = phyName(txPhy)
      val mappedRx = phyName(rxPhy)
      if (mappedTx === null || mappedRx === null) {
        pendingPhyReads.remove(key)?.invoke(Result.failure(IllegalStateException("onPhyRead returned unknown PHY")))
        return
      }
      pendingPhyReads.remove(key)?.invoke(Result.success(OwnedAndroidPhy(mappedTx, mappedRx)))
    }

    override fun onPhyUpdate(gatt: BluetoothGatt, txPhy: Int, rxPhy: Int, status: Int) {
      if (!isCurrentGattCallback(gatt)) return
      val key = "phyRequest:${gatt.device.address.uppercase()}"
      if (status != BluetoothGatt.GATT_SUCCESS) {
        pendingPhyRequests.remove(key)?.invoke(Result.failure(IllegalStateException("onPhyUpdate status=$status")))
        return
      }
      val mappedTx = phyName(txPhy)
      val mappedRx = phyName(rxPhy)
      if (mappedTx === null || mappedRx === null) {
        pendingPhyRequests.remove(key)?.invoke(Result.failure(IllegalStateException("onPhyUpdate returned unknown PHY")))
        return
      }
      pendingPhyRequests.remove(key)?.invoke(Result.success(OwnedAndroidPhy(mappedTx, mappedRx)))
    }

    // Android 12 (API 31)+ : ATT Service Changed indication → re-discover required.
    // https://developer.android.com/reference/android/bluetooth/BluetoothGattCallback#onServiceChanged
    override fun onServiceChanged(gatt: BluetoothGatt) {
      if (!isCurrentGattCallback(gatt)) return
      val id = gatt.device.address
      val key = id.uppercase()
      val generation = gattGenerationByInstance[gatt] ?: return
      failPendingForDevice(key, "GATT database changed")
      invalidateNativeSubscriptionsForDatabaseChange(key, gatt, generation)
      discovered.remove(key)
      clearCharCacheForDevice(key)
      onServicesChanged?.invoke(id)
    }
  }

  /**
   * Per-device FIFO: only one GATT request outstanding until [done] is invoked.
   * All pumps run on [handler] (main) so BluetoothGatt is never touched from mixed
   * binder/JS threads (R3-F024).
   */
  internal class GattSerialQueue(
    private val handler: Handler? = null,
    private val post: ((() -> Unit) -> Boolean)? = null,
    private val idProvider: (() -> Long)? = null
  ) {
    internal class GattOperationToken internal constructor() {
      private val cancelled = AtomicBoolean(false)
      private val publiclySettled = AtomicBoolean(false)

      fun isCancelled(): Boolean = cancelled.get()

      fun isPubliclySettled(): Boolean = publiclySettled.get()

      internal fun cancel() {
        cancelled.set(true)
      }

      internal fun markPubliclySettled(): Boolean {
        return publiclySettled.compareAndSet(false, true)
      }
    }

    private data class QueuedOperation(
      val id: Long,
      val token: GattOperationToken,
      val start: (token: GattOperationToken, done: () -> Unit) -> Unit,
      val cancel: () -> Unit,
      val startFailure: (Throwable) -> Unit,
      val startFailureDelivered: AtomicBoolean = AtomicBoolean(false)
    )

    private data class RunningOperation(val operation: QueuedOperation, val done: () -> Unit)

    private val lock = Any()
    private val queue = ArrayDeque<QueuedOperation>()
    private val busy = AtomicBoolean(false)
    private val nextOperationId = AtomicLong(1L)
    private var running: RunningOperation? = null

    fun submit(op: (done: () -> Unit) -> Unit, onCancelled: () -> Unit): Long =
      submitCancellable({ _, done -> op(done) }, onCancelled, {})

    fun submitCancellable(
      op: (token: GattOperationToken, done: () -> Unit) -> Unit,
      onCancelled: () -> Unit,
      onStartFailure: (Throwable) -> Unit = {}
    ): Long {
      val operation = QueuedOperation(
        idProvider?.invoke() ?: nextOperationId.getAndIncrement(),
        GattOperationToken(),
        op,
        onCancelled,
        onStartFailure
      )
      var startNow = false
      synchronized(lock) {
        queue.addLast(operation)
        if (busy.compareAndSet(false, true)) {
          startNow = true
        }
      }
      if (startNow) {
        // R3-F024: never pump() inline on the caller thread (binder/JS).
        if (!schedule { pump() }) {
          clear(IllegalStateException("GATT operation queue could not schedule its pump"))
        }
      }
      return operation.id
    }

    fun cancel(operationId: Long): Boolean {
      var queued: QueuedOperation? = null
      var active: RunningOperation? = null
      synchronized(lock) {
        val queuedOperation = queue.firstOrNull { operation -> operation.id == operationId }
        if (queuedOperation != null) {
          queued = queuedOperation
          queue.remove(queuedOperation)
        } else if (running?.operation?.id == operationId) {
          active = running
          active?.operation?.token?.cancel()
        }
      }
      queued?.let {
        invokeCancellation(it, IllegalStateException("GATT operation was cancelled"))
        return true
      }
      active?.let {
        invokeCancellation(it.operation, IllegalStateException("GATT operation was cancelled"))
        return true
      }
      return false
    }

    fun clear(reason: Throwable = IllegalStateException("GATT operation queue was cancelled")) {
      val cancelled: List<QueuedOperation>
      val active: RunningOperation?
      synchronized(lock) {
        cancelled = queue.toList()
        queue.clear()
        active = running
        active?.operation?.token?.cancel()
        if (active == null) busy.set(false)
      }
      cancelled.forEach { operation ->
        invokeCancellation(operation, reason)
      }
      // The pending callback is the physical-settlement owner for an
      // in-flight non-abortable Android request. failPendingForDevice()
      // completes that callback exactly once; invoking its public
      // cancellation callback here would double-deliver before it.
    }

    private fun pump() {
      val completed = AtomicBoolean(false)
      var operation: RunningOperation? = null
      synchronized(lock) {
        val next = queue.pollFirst()
        if (next == null) {
          running = null
          busy.set(false)
          return
        }
        val done: () -> Unit = {
          if (completed.compareAndSet(false, true)) {
            synchronized(lock) {
              if (running?.operation?.id == next.id) {
                running = null
              }
            }
            // Schedule next on main to avoid deep re-entrancy from binder callbacks.
            if (!schedule { pump() }) {
              clear(IllegalStateException("GATT operation queue could not schedule its next pump"))
            }
          }
        }
        operation = RunningOperation(next, done)
        // Publish the running operation before releasing the queue lock. A
        // concurrent cancellation must see this operation as active rather
        // than losing the dequeue-to-running transition.
        running = operation
      }
      val active = operation ?: return
      try {
        active.operation.start.invoke(active.operation.token, active.done)
      } catch (t: Throwable) {
        OwnedAndroidLog.e("GattSerialQueue op", t)
        invokeStartFailure(active.operation, t)
        active.done()
      }
    }

    private fun invokeCancellation(operation: QueuedOperation, reason: Throwable) {
      if (!operation.token.markPubliclySettled()) return
      try {
        operation.cancel.invoke()
      } catch (throwable: Throwable) {
        OwnedAndroidLog.e("GattSerialQueue cancellation ($reason)", throwable)
      }
    }

    private fun invokeStartFailure(operation: QueuedOperation, error: Throwable) {
      if (!operation.startFailureDelivered.compareAndSet(false, true)) return
      if (operation.token.isPubliclySettled()) return
      try {
        operation.startFailure.invoke(error)
      } catch (throwable: Throwable) {
        OwnedAndroidLog.e("GattSerialQueue start failure callback", throwable)
      }
    }

    private fun schedule(task: () -> Unit): Boolean = post?.invoke(task) ?: handler?.post(task) ?: false
  }

  companion object {
    /** Build marker for tests / evidence that owned radio is on the classpath. */
    const val RADIO_ID = "owned-android-gatt-v1"

    /** Safety close if onConnectionStateChange(DISCONNECTED) never arrives (R3-F003). */
    const val GATT_CLOSE_TIMEOUT_MS = 5_000L

    val CCCD_UUID: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

    internal fun isAlreadyPaired(bondState: Int, deviceType: Int, transport: String): Boolean {
      val bonded = bondState == BluetoothDevice.BOND_BONDED
      return when (transport) {
        "platformDefault" -> bonded
        // A generic bond on a dual-mode device may be BR/EDR-only. Android 36
        // exposes no public per-transport bond query, so only an LE-only device
        // is unambiguous; every other type retries explicit LE and fails closed.
        "le" -> bonded && deviceType == BluetoothDevice.DEVICE_TYPE_LE
        else -> throw IllegalArgumentException("Unsupported Android pair transport '$transport'")
      }
    }

    @JvmStatic
    internal fun normalizeScanServiceUuids(serviceUuids: List<String>): List<String> {
      return serviceUuids.map { value ->
        val lower = value.lowercase()
        val normalized = when {
          lower.matches(Regex("[0-9a-f]{4}")) -> "0000$lower-0000-1000-8000-00805f9b34fb"
          lower.matches(Regex("[0-9a-f]{8}")) -> "$lower-0000-1000-8000-00805f9b34fb"
          lower.matches(Regex("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")) -> lower
          else -> throw IllegalArgumentException("Android scan service UUID is malformed: $value")
        }
        UUID.fromString(normalized)
        normalized
      }
    }

    /**
     * Map BluetoothAdapter state ints to ble-plx PoweredOn/Off/Resetting strings.
     */
    @JvmStatic
    fun mapAdapterState(state: Int?): String {
      if (state == null) return "Unsupported"
      return when (state) {
        BluetoothAdapter.STATE_ON -> "PoweredOn"
        BluetoothAdapter.STATE_OFF -> "PoweredOff"
        BluetoothAdapter.STATE_TURNING_ON, BluetoothAdapter.STATE_TURNING_OFF -> "Resetting"
        else -> "Unknown"
      }
    }

    /**
     * Resolve CCCD enable/disable payload from subscriptionType + characteristic properties.
     * Returns null when the characteristic cannot be monitored for the requested mode.
     */
    @JvmStatic
    fun resolveCccdPayload(enable: Boolean, subscriptionType: String?, properties: Int): ByteArray? {
      if (!enable) {
        return ANDROID_CCCD_DISABLE.copyOf()
      }
      val notifiable = (properties and ANDROID_PROPERTY_NOTIFY) != 0
      val indicatable = (properties and ANDROID_PROPERTY_INDICATE) != 0
      return when {
        "notification".equals(subscriptionType, ignoreCase = true) && notifiable ->
          ANDROID_CCCD_ENABLE_NOTIFICATION.copyOf()
        "indication".equals(subscriptionType, ignoreCase = true) && indicatable ->
          ANDROID_CCCD_ENABLE_INDICATION.copyOf()
        subscriptionType == null || subscriptionType.isEmpty() ->
          when {
            notifiable -> ANDROID_CCCD_ENABLE_NOTIFICATION.copyOf()
            indicatable -> ANDROID_CCCD_ENABLE_INDICATION.copyOf()
            else -> null
          }
        else -> null
      }
    }

    @JvmStatic
    fun scanFailMessage(errorCode: Int): String = "scan failed code=$errorCode"

    @JvmStatic
        fun phyValue(value: String?): Int {
          return when (value) {
            null -> ScanSettings.PHY_LE_ALL_SUPPORTED
            "le1m" -> BluetoothDevice.PHY_LE_1M
            "le2m" -> BluetoothDevice.PHY_LE_2M
            "leCoded" -> BluetoothDevice.PHY_LE_CODED
            else -> throw IllegalArgumentException("Android PHY is unsupported")
          }
        }

        @JvmStatic
        fun phyMaskValue(value: String?): Int {
          return when (value) {
            null -> 0
            "le1m" -> BluetoothDevice.PHY_LE_1M_MASK
            "le2m" -> BluetoothDevice.PHY_LE_2M_MASK
            "leCoded" -> BluetoothDevice.PHY_LE_CODED_MASK
            else -> throw IllegalArgumentException("Android PHY is unsupported")
          }
        }

    private fun phyName(value: Int): String? = when (value) {
      BluetoothDevice.PHY_LE_1M -> "le1m"
      BluetoothDevice.PHY_LE_2M -> "le2m"
      BluetoothDevice.PHY_LE_CODED -> "leCoded"
      else -> null
    }
  }
}
