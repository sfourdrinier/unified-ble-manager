// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.annotation.SuppressLint
import android.Manifest
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattService
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
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference

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
internal data class OwnedAndroidNotificationPath(
  val serviceUuid: UUID,
  val serviceOccurrence: Int,
  val characteristicUuid: UUID,
  val characteristicOccurrence: Int
)

internal data class OrderedAndroidSourceOpen(
  val source: OrderedNotificationSource<BluetoothGattCharacteristic>,
  val group: OrderedNotificationSource<BluetoothGattCharacteristic>.Group,
  val members: Map<OwnedAndroidNotificationPath, Pair<BluetoothGattCharacteristic, OrderedNotificationSource.Membership>>
)

internal data class AndroidNativeGraphIdentity(val owner: String, val attachment: Long, val revision: ULong)
internal data class AndroidOrderedAdmission(
  val graph: AndroidNativeGraphIdentity,
  val source: OrderedNotificationSource<BluetoothGattCharacteristic>,
  val group: OrderedNotificationSource<BluetoothGattCharacteristic>.Group,
  val characteristic: BluetoothGattCharacteristic,
  val membership: OrderedNotificationSource.Membership
)
internal data class AndroidOrderedMarkerAdmission(
  val graph: AndroidNativeGraphIdentity,
  val source: OrderedNotificationSource<BluetoothGattCharacteristic>,
  val group: OrderedNotificationSource<BluetoothGattCharacteristic>.Group
)
internal enum class OrderedLocalCleanupStatus { RELEASED, PENDING, UNKNOWN }
internal class OwnedAndroidNotificationObserver(
  val owner: Any,
  val event: (String, Long, BluetoothGattCharacteristic, Long?, OwnedAndroidNotificationPath?, NotificationOwnershipFifo.Event) -> Unit,
  val stopped: (String, Long, BluetoothGattCharacteristic, Long?, OwnedAndroidNotificationPath?, NotificationOwnershipFifo.Stop) -> Unit
)
internal class OrderedEnableNotApplied(val original: Throwable) : IllegalStateException("ordered enable was not applied", original)
internal class OrderedAdmissionOverflow : IllegalStateException("reentrant callback capacity exhausted")
private data class LocalOnlyCccdCleanup(val operation: String, val epoch: Long?, val attachment: Long)

internal class OwnedAndroidSubscriptionOwnership<K>(
  private val onEvent: (String, Long, K, Long?, OwnedAndroidNotificationPath?, NotificationOwnershipFifo.Event) -> Unit = { _, _, _, _, _, _ -> },
  private val onStopped: (String, Long, K, Long?, OwnedAndroidNotificationPath?, NotificationOwnershipFifo.Stop) -> Unit = { _, _, _, _, _, _ -> }
) {
  private class Registration(
    val device: String,
    val generation: Long,
    val sourceEpoch: Long?,
    val subscriptionType: String?,
    val ordered: Boolean,
    val owner: Any?,
    val fifo: NotificationOwnershipFifo,
    val claim: NotificationOwnershipFifo.Claim
  ) {
    @Volatile var active = false
    val retainUntilRelease = AtomicBoolean(false)
  }

  private val registrations = ConcurrentHashMap<K, Registration>()

  /** Never replace an existing claim; the caller must share or reject it. */
  fun arm(deviceKey: String, generation: Long, key: K, sourceEpoch: Long? = null, sourcePath: OwnedAndroidNotificationPath? = null, subscriptionType: String? = null, ordered: Boolean = false,
          eventOwner: ((String, Long, K, Long?, OwnedAndroidNotificationPath?, NotificationOwnershipFifo.Event) -> Unit)? = null,
          stopOwner: ((String, Long, K, Long?, OwnedAndroidNotificationPath?, NotificationOwnershipFifo.Stop) -> Unit)? = null,
          owner: Any? = null): Boolean {
    val device = deviceKey.uppercase()
    val deliverEvent = eventOwner ?: onEvent
    val deliverStop = stopOwner ?: onStopped
    lateinit var registration: Registration
    val fifo = NotificationOwnershipFifo(
      ANDROID_ENABLEMENT_STAGING_ITEM_LIMIT,
      ANDROID_ENABLEMENT_STAGING_BYTE_LIMIT.toLong(),
      1,
      { reason -> deliverStop(device, generation, key, sourceEpoch, sourcePath, reason) }
    ) { event ->
      try {
        if (event is NotificationOwnershipFifo.Event.Terminal && event.failure != null) {
          deliverStop(device, generation, key, sourceEpoch, sourcePath,
            NotificationOwnershipFifo.Stop.DeliveryUncertain(event.failure, 0, 0))
        } else deliverEvent(device, generation, key, sourceEpoch, sourcePath, event)
      } finally {
        if (event is NotificationOwnershipFifo.Event.Terminal && !registration.retainUntilRelease.get()) {
          registrations.remove(key, registration)
        }
      }
    }
    val admission = fifo.arm(generation)
    if (admission !is NotificationOwnershipFifo.Admission.Armed) return false
    registration = Registration(device, generation, sourceEpoch, subscriptionType, ordered, owner, fifo, admission.claim)
    return registrations.putIfAbsent(key, registration) == null
  }

  fun activate(deviceKey: String, generation: Long, key: K, owner: NotificationOwnershipFifo.Claim? = null): Boolean {
    val registration = matchingOwner(deviceKey, generation, key, owner) ?: return false
    val activated = registration.fifo.activate(registration.claim, generation)
    registration.active = activated && registration.fifo.stopReason() == null
    return registration.active
  }

  fun deactivate(deviceKey: String, generation: Long, key: K, owner: NotificationOwnershipFifo.Claim? = null) {
    abandon(deviceKey, generation, key, owner)
  }

  fun abandon(deviceKey: String, generation: Long, key: K, owner: NotificationOwnershipFifo.Claim? = null) {
    val registration = matchingOwner(deviceKey, generation, key, owner) ?: return
    registration.active = false
    if (registration.retainUntilRelease.getAndSet(false) || registration.fifo.stopReason() != null) {
      registrations.remove(key, registration)
    } else {
      registration.fifo.retireStagedAsLoss(registration.claim, generation)
    }
  }

  /** Fence ingress while retaining the exact claim until remote compensation completes. */
  fun quiesceForCompensation(deviceKey: String, generation: Long, key: K, owner: NotificationOwnershipFifo.Claim? = null) {
    val registration = matchingOwner(deviceKey, generation, key, owner) ?: return
    registration.retainUntilRelease.set(true)
    registration.active = false
    if (registration.fifo.stopReason() == null) {
      registration.fifo.retireStagedAsLoss(registration.claim, generation)
    }
  }

  /** Ordinary ingress terminates in FIFO order while its physical claim remains owned. */
  fun failOrdinaryUncertain(deviceKey: String, generation: Long, key: K, owner: NotificationOwnershipFifo.Claim, failure: Throwable) {
    val registration = matchingOwner(deviceKey, generation, key, owner) ?: return
    if (registration.ordered) return
    registration.retainUntilRelease.set(true)
    registration.active = false
    registration.fifo.retire(registration.claim, generation, failure)
  }

  /** Source epochs identify the exact physical enable; null is reserved for legacy callers. */
  fun ownsEpoch(deviceKey: String, generation: Long, key: K, epoch: Long): Boolean =
    matching(deviceKey, generation, key)?.sourceEpoch == epoch

  fun ownedByOther(deviceKey: String, generation: Long, key: K, owner: Any?): Boolean =
    matching(deviceKey, generation, key)?.let { it.owner !== owner && (it.owner != null || owner != null) } ?: false

  fun ownerToken(deviceKey: String, generation: Long, key: K): NotificationOwnershipFifo.Claim? =
    matching(deviceKey, generation, key)?.claim

  fun eventOwner(deviceKey: String, generation: Long, key: K, claim: NotificationOwnershipFifo.Claim?): Any? =
    matchingOwner(deviceKey, generation, key, claim)?.owner

  fun subscriptionType(deviceKey: String, generation: Long, key: K, owner: NotificationOwnershipFifo.Claim): String? =
    matchingOwner(deviceKey, generation, key, owner)?.subscriptionType

  fun ownsToken(deviceKey: String, generation: Long, key: K, token: NotificationOwnershipFifo.Claim): Boolean =
    matching(deviceKey, generation, key)?.claim === token

  fun isActive(deviceKey: String, generation: Long, key: K): Boolean =
    matching(deviceKey, generation, key)?.active == true

  fun isArmed(deviceKey: String, generation: Long, key: K): Boolean =
    matching(deviceKey, generation, key)?.active == false

  /** Ownership check and staging are one FIFO operation. */
  fun offer(deviceKey: String, generation: Long, key: K, value: ByteArray): Boolean {
    val registration = matching(deviceKey, generation, key) ?: return false
    return registration.fifo.offer(registration.claim, generation, value)
  }

  private fun matching(deviceKey: String, generation: Long, key: K): Registration? =
    registrations[key]?.takeIf { it.device == deviceKey.uppercase() && it.generation == generation }

  private fun matchingOwner(deviceKey: String, generation: Long, key: K, owner: NotificationOwnershipFifo.Claim?): Registration? =
    matching(deviceKey, generation, key)?.takeIf { owner == null || it.claim === owner }

  fun invalidateForDatabaseChange(deviceKey: String, generation: Long): List<K> =
    invalidateForDatabaseChangeClaims(deviceKey, generation).map { it.first }

  fun invalidateForDatabaseChangeClaims(
    deviceKey: String, generation: Long, onError: (Throwable) -> Unit = { throw it }
  ): List<Triple<K, NotificationOwnershipFifo.Claim, Any?>> {
    val device = deviceKey.uppercase()
    val matches = registrations.entries.filter { it.value.device == device && it.value.generation == generation }
      .map { Triple(it.key, it.value.claim, it.value.owner) }
    var held: Throwable? = null
    matches.forEach { (key, owner, _) ->
      try { abandon(device, generation, key, owner) }
      catch (error: Throwable) { held = mergeErrors(held, error) }
    }
    held?.let(onError)
    return matches
  }

  fun clearDevice(deviceKey: String) {
    val device = deviceKey.uppercase()
    registrations.entries.filter { it.value.device == device }.forEach {
      abandon(device, it.value.generation, it.key)
    }
  }

  fun clear() {
    registrations.entries.toList().forEach { abandon(it.value.device, it.value.generation, it.key) }
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
  val isLinkLoss: Boolean = gattStatus == ANDROID_GATT_LINK_LOSS_STATUS,
  /** False when Android refused the request synchronously, before anything reached the peer. */
  val submitted: Boolean = true
) : IllegalStateException(
  when {
    gattStatus != null -> "$operation status=$gattStatus"
    isLinkLoss -> "$operation failed because the GATT link is unavailable"
    else -> "$operation failed"
  }
)

/**
 * A GATT request was proven not submitted, such as a queue admission refusal
 * or a false return from the legacy `BluetoothGatt` boolean API.
 */
internal class AndroidGattNotSubmitted(message: String, cause: Throwable? = null) : IllegalStateException(message, cause)

/**
 * The GATT link went down (peer or app disconnect, a failed connect, or a
 * close that never saw DISCONNECTED) while this operation was pending or
 * queued (finding 132). Legacy's dispatcher failed every pending command with
 * `connectionLost`. [submitted] is false for work still queued behind the
 * in-flight operation; [gattStatus] is the disconnect callback's status.
 */
internal class AndroidGattLinkLost(
  reason: String,
  val gattStatus: Int?,
  val submitted: Boolean
) : IllegalStateException(reason)

internal const val ANDROID_GATT_LINK_LOSS_STATUS = 19
/** Pending work failed by an adapter loss stays a platform failure (legacy). */
internal const val ADAPTER_UNAVAILABLE_REASON = "adapter unavailable"
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
  val pairingPossible: Boolean?,
  val encryption: String = "unsupported"
)

/** Immutable native-only projection of one system-bonded Android peer. */
internal data class ConnectedPeerSnapshot(val nativePeerId: String, val displayName: String?)

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
  val rawRecord: ByteArray?,
  val cachedName: String?,
  val captureTimestampMs: Long?
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
  private val scheduleDelayed: ((Long, () -> Unit) -> Boolean)?,
  private val securitySdkInt: Int,
  private val securityFullSdkInt: Int,
  private val scanSdkInt: Int,
  private val peerLifecycleCapacity: Int
) {
  constructor(context: Context) : this(context, DEFAULT_PEER_LIFECYCLE_CAPACITY)

  /** Maximum deferred lifecycle requests retained per peer during a native open. */
  constructor(context: Context, peerLifecycleCapacity: Int) : this(
    context,
    Handler(Looper.getMainLooper()),
    null,
    null,
    Build.VERSION.SDK_INT,
    if (Build.VERSION.SDK_INT >= 36) Build.VERSION.SDK_INT_FULL else 0,
    Build.VERSION.SDK_INT,
    peerLifecycleCapacity
  )

  internal constructor(
    context: Context,
    post: ((() -> Unit) -> Boolean),
    scheduleDelayed: (Long, () -> Unit) -> Boolean,
    securitySdkInt: Int = Build.VERSION.SDK_INT,
    securityFullSdkInt: Int = if (Build.VERSION.SDK_INT >= 36) Build.VERSION.SDK_INT_FULL else 0,
    scanSdkInt: Int = Build.VERSION.SDK_INT,
    peerLifecycleCapacity: Int = DEFAULT_PEER_LIFECYCLE_CAPACITY
  ) : this(context, null, post, scheduleDelayed, securitySdkInt, securityFullSdkInt, scanSdkInt, peerLifecycleCapacity)

  init { require(peerLifecycleCapacity > 0) { "peer lifecycle capacity must be positive" } }

  private val bluetoothManager: BluetoothManager by lazy {
    context.getSystemService(Context.BLUETOOTH_SERVICE) as BluetoothManager
  }
  private val adapter: BluetoothAdapter? by lazy { bluetoothManager.adapter }

  private class ScanSession(val scanner: BluetoothLeScanner, val callback: ScanCallback, val identity: Any?) {
    @Volatile var delivering = true
  }
  private val scanLock = Any()
  private val legacyScanOwner = Any()
  private val scanSessions = mutableMapOf<Any, ScanSession>()
  @Volatile private var adapterStateReceiver: BroadcastReceiver? = null
  private var adapterReceiverAdmissionFailed = false
  @Volatile private var adapterReceiverActive = false
  private var bondStateReceiver: BroadcastReceiver? = null
  private var bondReceiverAdmissionFailed = false
  private var securityReceiverActive = false
  private val receiverCallbackLock = Any()
  private val receiverRetirementLock = Any()
  private var receiverCallbacks = 0
  private var receiversRetiring = false
  internal class ReceiverOwner {
    internal var retired = false
    internal var callbacks = 0
  }
  private class ReceiverObservers(
    val adapter: (String) -> Unit,
    val security: (String, OwnedAndroidSecurityState) -> Unit,
    val securityFailure: (String?, Throwable) -> Unit,
    val platformFact: (String, String, Long) -> Unit
  )
  private val receiverOwners = linkedMapOf<ReceiverOwner, ReceiverObservers>()
  private class ConnectionRoute(
    val peer: String,
    val owner: ReceiverOwner,
    val connection: (String, Boolean, Int, GattConnectAttempt?) -> Unit,
    val services: (String) -> Unit,
    val orderedLinkFence: (String, GattConnectAttempt?) -> Unit
  )
  // A route is admitted with the connect, survives a queued reconnect, and is
  // withdrawn before its exact native generation is closed.
  private val connectionRoutes = mutableMapOf<GattConnectAttempt, ConnectionRoute>()

  internal fun connectOwned(deviceId: String, autoConnect: Boolean, phyMask: Int,
    attempt: GattConnectAttempt, owner: ReceiverOwner,
    connection: (String, Boolean, Int, GattConnectAttempt?) -> Unit, services: (String) -> Unit,
    orderedLinkFence: (String, GattConnectAttempt?) -> Unit = { _, _ -> }) {
    val route = ConnectionRoute(deviceId.uppercase(Locale.ROOT), owner, connection, services, orderedLinkFence)
    synchronized(receiverRetirementLock) {
      check(!owner.retired) { "connection owner is retiring" }
      check(!connectionRoutes.containsKey(attempt)) { "connect attempt is already owned" }
      connectionRoutes[attempt] = route
    }
    try { connect(deviceId, autoConnect, phyMask, attempt) }
    catch (error: Throwable) {
      synchronized(receiverRetirementLock) { if (connectionRoutes[attempt] === route) connectionRoutes.remove(attempt) }
      throw error
    }
  }

  private fun withdrawConnectionRoute(attempt: GattConnectAttempt?) {
    if (attempt != null) synchronized(receiverRetirementLock) { connectionRoutes.remove(attempt) }
  }

  private fun <T> deliverConnectionRoute(peer: String, attempt: GattConnectAttempt?, deliver: (ConnectionRoute) -> T) {
    if (attempt == null) return
    val route = synchronized(receiverRetirementLock) {
      connectionRoutes[attempt]?.takeIf { it.peer == peer.uppercase(Locale.ROOT) }?.also { it.owner.callbacks++ }
    } ?: return
    try { deliver(route) } finally { synchronized(receiverRetirementLock) { route.owner.callbacks-- } }
  }

  internal fun dispatchOwnedConnection(peer: String, connected: Boolean, status: Int, attempt: GattConnectAttempt?) {
    if (attempt != null) {
      deliverConnectionRoute(peer, attempt) { it.connection(peer, connected, status, attempt) }
      return
    }
    // A GATT that predates this owner has no attempt route. Its fact is
    // retained as externally observable uncertainty before any route decision.
    // A sole same-peer replacement receives the null attempt only, so it can
    // report the foreign generation but cannot settle, claim, or retire it.
    val foreignRoutes = synchronized(receiverRetirementLock) {
      connectionRoutes.values.filter { it.peer == peer.uppercase(Locale.ROOT) && !it.owner.retired }
    }
    val detail = when (foreignRoutes.size) {
      0 -> "Android GATT connection outcome for $peer has no owned attempt (connected=$connected status=$status)"
      1 -> "Android GATT connection outcome for $peer belongs to a foreign generation (connected=$connected status=$status)"
      else -> "Android GATT connection outcome for $peer is ambiguous across ${foreignRoutes.size} owned attempts (connected=$connected status=$status)"
    }
    dispatchReceiverObservers { it.platformFact("connection-outcome", detail, 1) }
    val foreignRoute = synchronized(receiverRetirementLock) {
      foreignRoutes.singleOrNull()?.takeIf { !it.owner.retired }
        ?.also { it.owner.callbacks++ }
    } ?: return
    try {
      foreignRoute.connection(peer, connected, status, null)
    } finally {
      synchronized(receiverRetirementLock) { foreignRoute.owner.callbacks-- }
    }
  }

  private fun dispatchOwnedServices(peer: String, attempt: GattConnectAttempt?) {
    deliverConnectionRoute(peer, attempt) { it.services(peer) }
  }

  private fun dispatchOwnedLinkFence(peer: String, attempt: GattConnectAttempt?) {
    deliverConnectionRoute(peer, attempt) { it.orderedLinkFence(peer, attempt) }
  }

  private fun retainDeferredRoute(gatt: BluetoothGatt): () -> Unit {
    val owner = synchronized(receiverRetirementLock) {
      gattGenerationByInstance[gatt]?.let { gattConnectAttempts[it] }
        ?.let { connectionRoutes[it]?.owner }?.also { it.callbacks++ }
    }
    return { if (owner != null) synchronized(receiverRetirementLock) { owner.callbacks-- } }
  }

  private fun settleQueuedConnects() {
    pendingReconnect.entries.toList().forEachSettled { (key, pending) ->
      if (pendingReconnect.remove(key, pending)) {
        try { dispatchConnectionState(key, false, BluetoothGatt.GATT_FAILURE, pending.attempt) }
        finally { withdrawConnectionRoute(pending.attempt) }
      }
    }
  }
  private var receiverDispatchInstalled = false
  private var receiverCleanupOwner: ReceiverOwner? = null

  internal data class ReceiverRetirement(val callbacks: Int, val failures: List<OwnedRadioTeardownFailure>)

  private fun beginReceiverCallback(receiver: BroadcastReceiver, adapterReceiver: Boolean): Boolean = synchronized(receiverCallbackLock) {
    val active = if (adapterReceiver) adapterReceiverActive && adapterStateReceiver === receiver
      else securityReceiverActive && bondStateReceiver === receiver
    if (active) receiverCallbacks++
    active
  }

  private fun endReceiverCallback() = synchronized(receiverCallbackLock) { receiverCallbacks-- }

  /** Register a port before acquiring shared receiver resources. */
  internal fun registerReceiverOwner(
    owner: ReceiverOwner,
    adapter: (String) -> Unit,
    security: (String, OwnedAndroidSecurityState) -> Unit,
    securityFailure: (String?, Throwable) -> Unit,
    platformFact: (String, String, Long) -> Unit,
    installCallbacks: () -> Unit
  ) = synchronized(receiverRetirementLock) {
    check(!owner.retired && !receiversRetiring) { "receiver owner is retiring" }
    receiverOwners[owner] = ReceiverObservers(adapter, security, securityFailure, platformFact)
    if (!receiverDispatchInstalled) {
      onAdapterState = { state -> dispatchReceiverObservers { it.adapter(state) } }
      onSecurityState = { device, state -> dispatchReceiverObservers { it.security(device, state) } }
      onSecurityFailure = { device, error -> dispatchReceiverObservers { it.securityFailure(device, error) } }
      receiverDispatchInstalled = true
    }
    installCallbacks()
    registerAdapterStateReceiver()
    registerBondStateReceiver()
  }

  /** A callback is admitted to each exact owner independently and joined on retirement. */
  private fun dispatchReceiverObservers(deliver: (ReceiverObservers) -> Unit) {
    val owners = synchronized(receiverRetirementLock) { receiverOwners.keys.toList() }
    var failure: Throwable? = null
    owners.forEach { owner ->
      val observer = synchronized(receiverRetirementLock) {
        receiverOwners[owner]?.also { owner.callbacks++ }
      } ?: return@forEach
      try { deliver(observer) } catch (error: Throwable) {
        if (failure == null) failure = error else failure?.addSuppressed(error)
      } finally {
        synchronized(receiverRetirementLock) { owner.callbacks-- }
      }
    }
    failure?.let { throw it }
  }

  /** Count observer execution through return; this does not fence new callback admission. */
  internal fun <T> withReceiverOwnerCallback(owner: ReceiverOwner, deliver: () -> T): T {
    synchronized(receiverRetirementLock) { owner.callbacks++ }
    return try { deliver() } finally {
      synchronized(receiverRetirementLock) { owner.callbacks-- }
    }
  }

  /** Retiring one port must not unregister another port's shared receivers. */
  internal fun retireReceiverOwner(owner: ReceiverOwner): ReceiverRetirement = synchronized(receiverRetirementLock) {
    if (!owner.retired) {
      owner.retired = true
      val wasRegistered = receiverOwners.remove(owner) != null
      if (wasRegistered && receiverOwners.isEmpty()) receiverCleanupOwner = owner
    }
    // Existing routes remain deliverable until their exact generation is closed.
    val routeDebt = connectionRoutes.values.count { it.owner === owner }
    if (receiverCleanupOwner === owner) {
      val result = retireReceivers()
      ReceiverRetirement(maxOf(result.callbacks, owner.callbacks + routeDebt), result.failures)
    } else ReceiverRetirement(owner.callbacks + routeDebt, emptyList())
  }

  /** Unregistration fences new callbacks; already admitted callbacks remain owned until return. */
  internal fun retireReceivers(): ReceiverRetirement = synchronized(receiverRetirementLock) {
    receiversRetiring = true
    val failures = listOfNotNull(unregisterAdapterStateReceiver(), unregisterBondStateReceiver())
    ReceiverRetirement(synchronized(receiverCallbackLock) { receiverCallbacks }, failures)
  }
  private val connectedEncryptionGenerations = ConcurrentHashMap<String, Long>()
  private val encryptionApi by lazy {
    AndroidEncryptionApi(securityFullSdkInt, BluetoothDevice::class.java)
  }

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
    val done: () -> Unit,
    val retainAfterCancellation: Boolean = false
  )

  private data class PendingNotificationClaim(
    val generation: Long,
    val characteristic: BluetoothGattCharacteristic,
    val owner: NotificationOwnershipFifo.Claim
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
    val subscriptionCharacteristic: BluetoothGattCharacteristic? = null,
    val subscriptionOwner: NotificationOwnershipFifo.Claim? = null,
    val compensatingDisable: (() -> Unit)? = null,
    val compensatingEnable: (() -> Unit)? = null,
    val onLateSuccess: (() -> Unit)? = null,
    val retainOnFailure: Boolean = false,
    val onDeliveryUncertain: ((Throwable) -> Unit)? = null
  )

  private val exactReadPending = ConcurrentHashMap<BluetoothGattCharacteristic, ExactBytePending>()
  private val exactWritePending = ConcurrentHashMap<BluetoothGattCharacteristic, ExactBytePending>()
  private val exactWriteValues = ConcurrentHashMap<BluetoothGattCharacteristic, ByteArray>()
  // The callback owner itself is the fence, including after an ambiguous throw.
  // Removing it on its exact callback allows the attribute to be used again.
  private fun hasAmbiguousMarkerWrite(deviceId: String, gatt: BluetoothGatt, characteristic: BluetoothGattCharacteristic): Boolean =
    exactWritePending[characteristic]?.let { it.deviceKeyUpper == deviceId.uppercase() && it.gatt === gatt } == true
  private val exactCccdPending = ConcurrentHashMap<BluetoothGattDescriptor, ExactUnitPending>()
  private val descriptorWriteOwnershipLock = Any()
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
  // Every outstanding callback consumes one slot, including old database graphs.
  // Callbacks release slots; at the limit new operations fail without touching Android.
  private val callbackOwnerLock = Any()
  // A retiring attachment keeps its callback debt until close succeeds. Android
  // callbacks do not carry operation IDs, and close itself can fail.
  private val retiringGatts = ConcurrentHashMap<String, GattTeardownOwner>()
  private fun fenceRetiringGatt(key: String, gatt: BluetoothGatt, generation: Long) {
    synchronized(callbackOwnerLock) {
      if (isCurrentGatt(key, gatt, generation)) retiringGatts[key] = GattTeardownOwner(gatt, generation)
    }
  }
  internal var callbackOwnerCapacityForTest = 64
  private fun registerCallbackOwner(token: GattSerialQueue.GattOperationToken, gatt: BluetoothGatt, register: () -> Boolean): Boolean =
    token.registerOwner {
      synchronized(callbackOwnerLock) {
        if (retiringGatts.values.any { it.gatt === gatt }) return@synchronized false
        val count = exactReadPending.values.count { it.gatt === gatt } +
          exactWritePending.values.count { it.gatt === gatt } +
          exactDescriptorReadPending.values.count { it.gatt === gatt } +
          exactDescriptorWritePending.values.count { it.gatt === gatt } +
          exactCccdPending.values.count { it.gatt === gatt }
        count < callbackOwnerCapacityForTest && register()
      }
    }
  /** Serializes native local registration changes with publication of replacement claims. */
  private val notificationControlLock = Any()
  private val orderedSourceLock = Any()
  /** Installed by the one owner port before any ordered source can be opened. */
  private var orderedCallbackAdmission: (() -> AutoCloseable?)? = null
  internal fun setOrderedCallbackAdmission(admit: () -> AutoCloseable?) = synchronized(orderedSourceLock) {
    // A replacement owner installs its own exact admission edge; callbacks
    // from the displaced owner are already rejected by graph/source identity.
    orderedCallbackAdmission = admit
  }
  internal fun admissionLocksHeldByCurrentThread(): Boolean = Thread.holdsLock(orderedSourceLock) || Thread.holdsLock(notificationControlLock)
  internal fun probeAdmissionLocksForTest() = synchronized(orderedSourceLock) { synchronized(notificationControlLock) { Unit } }
  private class DeferredCallback(val action: () -> Unit, val bytes: Int, val control: Boolean, val wake: Boolean,
                                 val lifecycle: OrderedNotificationSource.Reason? = null, val teardown: Boolean = false)
  internal data class GraphPermitKey(val authority: ULong, val peer: String, val connection: String, val graph: AndroidNativeGraphIdentity, val id: ULong)
  internal data class GraphPermitRequest(val authority: ULong, val request: ULong, val peer: String, val connection: String, val graph: AndroidNativeGraphIdentity)
  internal sealed interface GraphReservationStatus {
    data object Pending : GraphReservationStatus
    data object NotCreated : GraphReservationStatus
    data class Issued(val permit: ULong) : GraphReservationStatus
  }
  private class GraphReservation(val request: GraphPermitRequest, val status: GraphReservationStatus, var fenced: Boolean = false)
  internal enum class GraphPermitResult { ACQUIRED, BUSY, STALE, COMMITTED, INVALIDATED, RELEASED, FORGOTTEN }
  internal enum class GraphPermitInvalidation { SERVICE_CHANGED, CONNECTION_LOST, OVERFLOW, TEARDOWN }
  private class GraphPermit(val owner: GraphOwner, val key: GraphPermitKey) {
    var committed = false
    var invalidation: GraphPermitInvalidation? = null
    fun invalidate(cause: GraphPermitInvalidation) {
      if (!committed && invalidation == null) invalidation = cause
    }
  }
  private class GraphPermitReceipt(val key: GraphPermitKey, val outcome: GraphPermitResult, var released: Boolean = false)
  internal sealed interface GraphAuthorityOpenStatus {
    data object Pending : GraphAuthorityOpenStatus
    data object NotCreated : GraphAuthorityOpenStatus
    data class Opened(val authority: ULong) : GraphAuthorityOpenStatus
    data class Closed(val authority: ULong) : GraphAuthorityOpenStatus
  }
  private class AuthorityTokenReceipt(val token: ULong, val reservation: GraphReservationStatus, var fenced: Boolean = false)
  private class AuthorityOpenReceipt(val id: ULong, val token: ULong, var status: GraphAuthorityOpenStatus = GraphAuthorityOpenStatus.Pending)
  private class GraphAuthority(val id: ULong, val openReceipt: ULong) {
    var closed = false
    val receipts = mutableMapOf<ULong, GraphPermitReceipt>()
    val retired = mutableSetOf<ULong>()
    var frontier = 0uL
    var nextPermit = 1uL
    var permitAvailable = true
    val reservations = mutableMapOf<ULong, GraphReservation>()
    val reservationRetired = mutableSetOf<ULong>()
    var reservationFrontier = 0uL
  }
  /** One identity owns one receipt namespace, even when ports share a radio. */
  internal class GraphOwner internal constructor(val incarnation: UUID, internal val radioIdentity: Any) {
    internal val calls = AtomicInteger()
  }
  private val graphRadioIdentity = Any()
  private class GraphOwnerStore {
    val graphAuthorities = mutableMapOf<ULong, GraphAuthority>()
    val authorityTokenReceipts = mutableMapOf<ULong, AuthorityTokenReceipt>()
    val authorityOpenReceipts = mutableMapOf<ULong, AuthorityOpenReceipt>()
    val retiredAuthorityOpenReceipts = mutableSetOf<ULong>()
    var authorityOpenFrontier = 0uL
    var nextAuthorityOpenReceipt = 1uL
    var authorityOpenReceiptAvailable = true
    val retiredGraphAuthorities = mutableSetOf<ULong>()
    var graphAuthorityFrontier = 0uL
    var nextGraphAuthority = 1uL
    var graphAuthorityAvailable = true
    var fenced = false
  }
  private val graphOwnerStores = ConcurrentHashMap<GraphOwner, GraphOwnerStore>()
  private val graphOwnerIncarnations = ConcurrentHashMap.newKeySet<UUID>()
  private val legacyGraphOwner = newGraphOwner(UUID.randomUUID())
  private val currentGraphOwner = ThreadLocal<GraphOwner>()

  internal fun newGraphOwner(incarnation: UUID): GraphOwner = synchronized(orderedSourceLock) {
    check(graphOwnerIncarnations.add(incarnation)) { "graph owner incarnation is already registered" }
    GraphOwner(incarnation, graphRadioIdentity).also { graphOwnerStores[it] = GraphOwnerStore() }
  }

  private fun graphStore(owner: GraphOwner): GraphOwnerStore {
    require(owner.radioIdentity === graphRadioIdentity) { "graph owner belongs to another radio" }
    return checkNotNull(graphOwnerStores[owner]) { "graph owner is unknown" }
  }

  /** Count before entering the graph monitor and keep the call charged through its return. */
  internal fun <T> withGraphOwner(owner: GraphOwner, action: () -> T): T {
    graphStore(owner)
    val previous = currentGraphOwner.get()
    owner.calls.incrementAndGet()
    currentGraphOwner.set(owner)
    try { return action() } finally {
      if (previous == null) currentGraphOwner.remove() else currentGraphOwner.set(previous)
      owner.calls.decrementAndGet()
    }
  }
  private fun activeGraphOwner() = currentGraphOwner.get() ?: legacyGraphOwner
  private fun activeGraphStore() = graphStore(activeGraphOwner())
  private val graphAuthorities
    get() = activeGraphStore().graphAuthorities
  private val authorityTokenReceipts
    get() = activeGraphStore().authorityTokenReceipts
  private val authorityOpenReceipts
    get() = activeGraphStore().authorityOpenReceipts
  private val retiredAuthorityOpenReceipts
    get() = activeGraphStore().retiredAuthorityOpenReceipts
  private var authorityOpenFrontier
    get() = activeGraphStore().authorityOpenFrontier
    set(value) { activeGraphStore().authorityOpenFrontier = value }
  private var nextAuthorityOpenReceipt
    get() = activeGraphStore().nextAuthorityOpenReceipt
    set(value) { activeGraphStore().nextAuthorityOpenReceipt = value }
  private var authorityOpenReceiptAvailable
    get() = activeGraphStore().authorityOpenReceiptAvailable
    set(value) { activeGraphStore().authorityOpenReceiptAvailable = value }
  private val retiredGraphAuthorities
    get() = activeGraphStore().retiredGraphAuthorities
  private var graphAuthorityFrontier
    get() = activeGraphStore().graphAuthorityFrontier
    set(value) { activeGraphStore().graphAuthorityFrontier = value }
  private var nextGraphAuthority
    get() = activeGraphStore().nextGraphAuthority
    set(value) { activeGraphStore().nextGraphAuthority = value }
  private var graphAuthorityAvailable
    get() = activeGraphStore().graphAuthorityAvailable
    set(value) { activeGraphStore().graphAuthorityAvailable = value }
  private var graphPermitShutdown = false
  private var graphPermitDestroyPending = false

  /**
   * A point-in-time observation of this radio's graph store. Counts are not
   * owner attribution or owner-wide quiescence; retired IDs are tombstones,
   * not live receipts. Calls waiting to enter the lock are not represented.
   */
  internal data class GraphStoreStatus(
    val authorities: Int, val authorityReservations: Int, val authorityOpenReceipts: Int,
    val permitReservations: Int, val permitReceipts: Int, val admissionGates: Int,
    val activePermits: Int, val deferredCallbacks: Int, val drainingGates: Int,
    val nativeGraphs: Int, val discoveryAdmissions: Int, val detachedSources: Int,
    val retiredAuthorities: Int, val retiredAuthorityOpens: Int,
    val retiredPermits: Int, val retiredPermitReservations: Int, val destroyPending: Boolean
  )

  internal fun graphStoreStatus(): GraphStoreStatus = synchronized(orderedSourceLock) {
    val stores = graphOwnerStores.values.toList()
    GraphStoreStatus(
      stores.sumOf { it.graphAuthorities.size }, stores.sumOf { it.authorityTokenReceipts.size },
      stores.sumOf { it.authorityOpenReceipts.size },
      stores.sumOf { store -> store.graphAuthorities.values.sumOf { it.reservations.size } },
      stores.sumOf { store -> store.graphAuthorities.values.sumOf { it.receipts.size } },
      admissionGates.size, admissionGates.values.count { it.graphPermit != null },
      admissionGates.values.sumOf { it.callbacks.size }, admissionGates.values.count { it.draining },
      nativeGraphs.size, nativeDiscoveryAdmissions.size, admissionGates.values.sumOf { it.detachedSources.size },
      stores.sumOf { it.retiredGraphAuthorities.size }, stores.sumOf { it.retiredAuthorityOpenReceipts.size },
      stores.sumOf { store -> store.graphAuthorities.values.sumOf { it.retired.size } },
      stores.sumOf { store -> store.graphAuthorities.values.sumOf { it.reservationRetired.size } },
      graphPermitDestroyPending
    )
  }

  internal data class GraphTokenReceiptSnapshot(val status: GraphReservationStatus, val fenced: Boolean)
  internal data class GraphOpenReceiptSnapshot(val token: ULong, val status: GraphAuthorityOpenStatus)
  internal data class GraphReservationSnapshot(val request: GraphPermitRequest, val status: GraphReservationStatus, val fenced: Boolean)
  internal data class GraphPermitReceiptSnapshot(val key: GraphPermitKey, val outcome: GraphPermitResult, val released: Boolean)
  internal data class GraphAuthoritySnapshot(
    val openReceipt: ULong, val closed: Boolean,
    val reservations: Map<ULong, GraphReservationSnapshot>, val receipts: Map<ULong, GraphPermitReceiptSnapshot>,
    val retiredPermits: Set<ULong>, val permitFrontier: ULong, val nextPermit: ULong, val permitAvailable: Boolean,
    val retiredReservations: Set<ULong>, val reservationFrontier: ULong
  )

  /** Owner-local observation, not a quiescence receipt. Non-permit gates and other stores remain separate. */
  internal data class GraphOwnerStatus(
    val incarnation: UUID, val fenced: Boolean, val callsInFlight: Int,
    val authorities: Int, val authorityReservations: Int, val authorityOpenReceipts: Int,
    val permitReservations: Int, val permitReceipts: Int, val admissionGates: Int,
    val activePermits: Int, val deferredCallbacks: Int, val drainingGates: Int,
    val discoveryWork: Int, val nativeGraphs: Int,
    val detachedSources: Int, val retiredAuthorities: Int, val retiredAuthorityOpens: Int,
    val retiredPermits: Int, val retiredPermitReservations: Int,
    val reservationReceipts: Map<ULong, GraphTokenReceiptSnapshot>,
    val openReceipts: Map<ULong, GraphOpenReceiptSnapshot>,
    val authorityStates: Map<ULong, GraphAuthoritySnapshot>,
    val retiredAuthorityIds: Set<ULong>, val retiredOpenReceiptIds: Set<ULong>,
    val authorityFrontier: ULong, val openReceiptFrontier: ULong,
    val nextAuthority: ULong, val authorityAvailable: Boolean,
    val nextOpenReceipt: ULong, val openReceiptAvailable: Boolean
  )

  /** A fenced observation is not a global quiescence receipt: other stores and later cleanup remain. */
  internal fun graphOwnerStatus(owner: GraphOwner): GraphOwnerStatus = withGraphOwner(owner) {
    synchronized(orderedSourceLock) {
      val store = graphStore(owner)
      val gates = admissionGates.values.filter { it.graphOwner === owner }
      GraphOwnerStatus(owner.incarnation, store.fenced, owner.calls.get() - 1,
        store.graphAuthorities.size, store.authorityTokenReceipts.size, store.authorityOpenReceipts.size,
        store.graphAuthorities.values.sumOf { it.reservations.size }, store.graphAuthorities.values.sumOf { it.receipts.size },
        gates.size, gates.count { it.graphPermit != null }, gates.sumOf { it.callbacks.size }, gates.count { it.draining },
        discoveryWork.count { it.owner === owner }, nativeGraphOwners.values.count { it === owner },
        gates.sumOf { it.detachedSources.size }, store.retiredGraphAuthorities.size, store.retiredAuthorityOpenReceipts.size,
        store.graphAuthorities.values.sumOf { it.retired.size }, store.graphAuthorities.values.sumOf { it.reservationRetired.size },
        store.authorityTokenReceipts.mapValues { (_, receipt) -> GraphTokenReceiptSnapshot(receipt.reservation, receipt.fenced) },
        store.authorityOpenReceipts.mapValues { (_, receipt) -> GraphOpenReceiptSnapshot(receipt.token, receipt.status) },
        store.graphAuthorities.mapValues { (_, authority) ->
          GraphAuthoritySnapshot(authority.openReceipt, authority.closed,
            authority.reservations.mapValues { (_, reservation) -> GraphReservationSnapshot(reservation.request, reservation.status, reservation.fenced) },
            authority.receipts.mapValues { (_, receipt) -> GraphPermitReceiptSnapshot(receipt.key, receipt.outcome, receipt.released) },
            authority.retired.toSet(), authority.frontier, authority.nextPermit, authority.permitAvailable,
            authority.reservationRetired.toSet(), authority.reservationFrontier)
        },
        store.retiredGraphAuthorities.toSet(), store.retiredAuthorityOpenReceipts.toSet(),
        store.graphAuthorityFrontier, store.authorityOpenFrontier, store.nextGraphAuthority, store.graphAuthorityAvailable,
        store.nextAuthorityOpenReceipt, store.authorityOpenReceiptAvailable)
    }
  }

  /** Stop new graph ownership while preserving exact receipts for their owner to retire. */
  internal fun fenceGraphOwner() = synchronized(orderedSourceLock) {
    val owner = activeGraphOwner()
    activeGraphStore().fenced = true
    admissionGates.values.mapNotNull { it.graphPermit }.filter { it.owner === owner }
      .forEach { it.invalidate(GraphPermitInvalidation.TEARDOWN) }
  }
  private class AdmissionGate {
    var graphOwner: GraphOwner? = null
    var graphPermit: GraphPermit? = null
    val callbacks = ArrayDeque<DeferredCallback>()
    val detachedSources = mutableSetOf<OrderedNotificationSource<BluetoothGattCharacteristic>>()
    val lifecycleControls = mutableSetOf<OrderedNotificationSource.Reason>()
    var teardownQueued = false
    var depth = 0
    var draining = false
    var ordinary = 0
    var controls = 0
    var wakes = 0
    var bytes = 0
    var notificationOverflow = 0L
    var controlOverflow = 0L
  }
  private val admissionGates = mutableMapOf<BluetoothGatt, AdmissionGate>()
  private val replayingAdmissionCallback = ThreadLocal<Boolean>()
  internal var admissionWakeCapacityForTest = 64
  internal var onAdmissionOverflow: ((String, Long, Long) -> Unit)? = null

  private fun enqueueAdmission(gate: AdmissionGate, bytes: Int, control: Boolean, wake: Boolean = false, action: () -> Unit): Boolean {
    if ((wake && gate.wakes >= admissionWakeCapacityForTest) || (control && !wake && gate.controls >= 64) ||
      (!control && gate.ordinary >= 64) || bytes > 262144 - gate.bytes) {
      if (control) gate.controlOverflow++ else gate.notificationOverflow++
      gate.graphPermit?.invalidate(GraphPermitInvalidation.OVERFLOW)
      return false
    }
    gate.callbacks.addLast(DeferredCallback(action, bytes, control, wake))
    if (wake) gate.wakes++ else if (control) gate.controls++ else gate.ordinary++
    gate.bytes += bytes
    return true
  }

  private fun <T> withNativeAdmission(gatt: BluetoothGatt, action: () -> T): T {
    var drain = false
    try {
      return synchronized(orderedSourceLock) {
        val gate = admissionGates.getOrPut(gatt) { AdmissionGate() }
        gate.depth++
        try { action() } finally {
          gate.depth--
          if (gate.depth == 0 && !gate.draining && (gate.callbacks.isNotEmpty() || gate.notificationOverflow != 0L || gate.controlOverflow != 0L)) {
            gate.draining = true
            drain = true
          } else if (gate.depth == 0 && !gate.draining) admissionGates.remove(gatt)
        }
      }
    } finally {
      if (drain) drainAdmission(gatt, throwOnOverflow = true)
    }
  }

  private fun drainAdmission(gatt: BluetoothGatt, throwOnOverflow: Boolean = false) {
    var overflowed = false
      while (true) {
        val entry = synchronized(orderedSourceLock) {
          val gate = admissionGates[gatt] ?: return
          // Loss must be attached before a queued wake can let the host drain
          // a terminal which preceded the overflow in this same held FIFO.
          val nextIsWake = gate.callbacks.peekFirst()?.wake == true
          if (nextIsWake && (gate.notificationOverflow != 0L || gate.controlOverflow != 0L)) {
            val notifications = gate.notificationOverflow
            val controls = gate.controlOverflow
            gate.notificationOverflow = 0
            gate.controlOverflow = 0
            return@synchronized Triple(null, notifications, controls)
          }
          val next = gate.callbacks.pollFirst()
          if (next != null) {
            if (next.teardown) gate.teardownQueued = false
            else if (next.lifecycle != null) gate.lifecycleControls.remove(next.lifecycle)
            else if (next.wake) gate.wakes-- else if (next.control) gate.controls-- else gate.ordinary--
            gate.bytes -= next.bytes
            Triple(next, 0L, 0L)
          } else {
            val notifications = gate.notificationOverflow
            val controls = gate.controlOverflow
            gate.notificationOverflow = 0
            gate.controlOverflow = 0
            if (notifications == 0L && controls == 0L) {
              gate.draining = false
              if (gate.depth == 0) admissionGates.remove(gatt)
            }
            Triple(null, notifications, controls)
          }
        }
        if (entry.first == null) {
          if (entry.second > 0 || entry.third > 0) {
            overflowed = true
            val id = gatt.device.address.uppercase()
            val sources = synchronized(orderedSourceLock) {
              val retained = admissionGates[gatt]?.detachedSources?.toMutableSet() ?: mutableSetOf()
              orderedSources[gatt]?.let { retained.add(it) }
              retained.toList()
            }
            var sourceError: Throwable? = null
            sources.forEach { source ->
              try { source.failDeferredIngress() } catch (error: Throwable) {
                if (sourceError == null) sourceError = error else sourceError?.addSuppressed(error)
              }
            }
            val pendingError = errorOf { failPendingForDevice(id, "reentrant callback capacity exhausted", linkLost = false) }
            val observerError = errorOf { onAdmissionOverflow?.invoke(id, entry.second, entry.third) }
            val failure = mergeErrors(sourceError, mergeErrors(pendingError, observerError))
            if (failure != null) reportOrderedWakeFailure("orderedSource.reentrantOverflow", failure)
          }
          if (entry.second == 0L && entry.third == 0L) {
            if (overflowed && throwOnOverflow) throw OrderedAdmissionOverflow()
            return
          }
          continue
        }
        try { entry.first?.action?.invoke() } catch (error: Throwable) {
          reportOrderedWakeFailure("orderedSource.deferredCallback", error)
        }
      }
  }

  private fun deferControl(gatt: BluetoothGatt, replayEntry: Boolean = true, bytes: Int = 0, action: () -> Unit): Boolean = synchronized(orderedSourceLock) {
    if (replayingAdmissionCallback.get() == true) {
      replayingAdmissionCallback.remove()
      return false
    }
    val gate = admissionGates[gatt] ?: return false
    val release = retainDeferredRoute(gatt)
    if (!enqueueAdmission(gate, bytes, true) {
      if (replayEntry) replayingAdmissionCallback.set(true)
      try { action() } finally { if (replayEntry) replayingAdmissionCallback.remove(); release() }
    }) release()
    true
  }

  private enum class PinnedOperation { NOT_PINNED, QUEUED, OVERFLOW }
  private fun deferPinnedOperation(gatt: BluetoothGatt, action: () -> Unit): PinnedOperation = synchronized(orderedSourceLock) {
    val gate = admissionGates[gatt]
    val permit = gate?.graphPermit
    if (permit == null) {
      // Publish teardown intent before releasing the source lock, so a later
      // acquire cannot validate the graph while teardown starts.
      removeNativeGraph(gatt)
      return PinnedOperation.NOT_PINNED
    }
    permit.invalidate(GraphPermitInvalidation.TEARDOWN)
    if (enqueueAdmission(gate, 0, true, action = action)) PinnedOperation.QUEUED else PinnedOperation.OVERFLOW
  }

  /** One reserved teardown per exact GATT survives ordinary-control saturation. */
  private fun deferPinnedTeardown(gatt: BluetoothGatt, action: () -> Unit): PinnedOperation = synchronized(orderedSourceLock) {
    val gate = admissionGates[gatt]
    val permit = gate?.graphPermit
    if (permit == null) {
      removeNativeGraph(gatt)
      return PinnedOperation.NOT_PINNED
    }
    permit.invalidate(GraphPermitInvalidation.TEARDOWN)
    if (!gate.teardownQueued) {
      gate.teardownQueued = true
      gate.callbacks.addLast(DeferredCallback(action, 0, control = true, wake = false, teardown = true))
    }
    PinnedOperation.QUEUED
  }

  private fun deferLifecycleControl(gatt: BluetoothGatt, reason: OrderedNotificationSource.Reason, action: () -> Unit): Boolean = synchronized(orderedSourceLock) {
    if (replayingAdmissionCallback.get() == true) {
      replayingAdmissionCallback.remove()
      removeNativeGraph(gatt)
      return false
    }
    val gate = admissionGates[gatt] ?: run { removeNativeGraph(gatt); return false }
    gate.graphPermit?.invalidate(if (reason == OrderedNotificationSource.Reason.CONNECTION_LOST) GraphPermitInvalidation.CONNECTION_LOST else GraphPermitInvalidation.SERVICE_CHANGED)
    if (gate.lifecycleControls.add(reason)) {
      val release = retainDeferredRoute(gatt)
      gate.callbacks.addLast(DeferredCallback({
        replayingAdmissionCallback.set(true)
        try { action() } finally { replayingAdmissionCallback.remove(); release() }
      }, 0, control = true, wake = false, lifecycle = reason))
    }
    if (gate.graphPermit != null) return true
    removeNativeGraph(gatt)
    nativeDiscoveryAdmissions[gatt]?.valid = false
    if (reason != OrderedNotificationSource.Reason.CONNECTION_LOST) {
      discovered.remove(gatt.device.address.uppercase())
    }
    orderedSources.remove(gatt)?.let { source ->
      gate.detachedSources.add(source)
      source.invalidate(reason) { wake -> enqueueAdmission(gate, 0, true, wake = true, action = wake) }
    }
    true
  }
  /** ForeignRadio fences its sole submission before retiring the process token. */
  internal fun reserveGraphAuthority(token: ULong): GraphReservationStatus = synchronized(orderedSourceLock) {
    authorityTokenReceipts[token]?.let { return it.reservation }
    check(!graphPermitShutdown && !activeGraphStore().fenced && authorityTokenReceipts.size < 64 && authorityOpenReceiptAvailable &&
      nextAuthorityOpenReceipt - authorityOpenFrontier <= 64uL) { "graph authority reservation capacity exhausted" }
    val receipt = nextAuthorityOpenReceipt
    if (receipt == ULong.MAX_VALUE) authorityOpenReceiptAvailable = false else nextAuthorityOpenReceipt++
    val status = GraphReservationStatus.Issued(receipt)
    authorityTokenReceipts[token] = AuthorityTokenReceipt(token, status)
    authorityOpenReceipts[receipt] = AuthorityOpenReceipt(receipt, token)
    status
  }

  internal fun statusGraphAuthorityReservation(token: ULong): GraphReservationStatus = synchronized(orderedSourceLock) {
    authorityTokenReceipts[token]?.reservation ?: GraphReservationStatus.Pending
  }

  internal fun fenceGraphAuthorityReservation(token: ULong): GraphReservationStatus = synchronized(orderedSourceLock) {
    val receipt = authorityTokenReceipts[token] ?: run {
      check(authorityTokenReceipts.size < 64) { "graph authority reservation capacity exhausted" }
      AuthorityTokenReceipt(token, GraphReservationStatus.NotCreated).also { authorityTokenReceipts[token] = it }
    }
    receipt.fenced = true
    receipt.reservation
  }

  internal fun forgetGraphAuthorityReservation(token: ULong) = synchronized(orderedSourceLock) {
    val tokenReceipt = authorityTokenReceipts[token] ?: return@synchronized
    check(tokenReceipt.fenced) { "graph authority reservation is not fenced" }
    val issued = tokenReceipt.reservation as? GraphReservationStatus.Issued
    if (issued != null) check(issued.permit !in authorityOpenReceipts) { "graph authority open receipt remains owned" }
    authorityTokenReceipts.remove(token)
  }

  internal fun openGraphAuthority(receiptId: ULong): GraphAuthorityOpenStatus = synchronized(orderedSourceLock) {
    val receipt = checkNotNull(authorityOpenReceipts[receiptId]) { "graph authority open receipt is unknown" }
    if (receipt.status != GraphAuthorityOpenStatus.Pending) return receipt.status
    check(!graphPermitShutdown && !activeGraphStore().fenced && graphAuthorityAvailable && nextGraphAuthority - graphAuthorityFrontier <= 64uL) { "graph authority capacity exhausted" }
    val id = nextGraphAuthority
    if (id == ULong.MAX_VALUE) graphAuthorityAvailable = false else nextGraphAuthority++
    graphAuthorities[id] = GraphAuthority(id, receiptId)
    GraphAuthorityOpenStatus.Opened(id).also { receipt.status = it }
  }

  internal fun statusGraphAuthorityOpen(receiptId: ULong): GraphAuthorityOpenStatus = synchronized(orderedSourceLock) {
    checkNotNull(authorityOpenReceipts[receiptId]) { "graph authority open receipt is unknown" }.status
  }

  internal fun fenceGraphAuthorityOpen(receiptId: ULong): GraphAuthorityOpenStatus = synchronized(orderedSourceLock) {
    val receipt = checkNotNull(authorityOpenReceipts[receiptId]) { "graph authority open receipt is unknown" }
    if (receipt.status == GraphAuthorityOpenStatus.Pending) receipt.status = GraphAuthorityOpenStatus.NotCreated
    receipt.status
  }

  internal fun forgetGraphAuthorityOpen(receiptId: ULong) = synchronized(orderedSourceLock) {
    if (receiptId <= authorityOpenFrontier || receiptId in retiredAuthorityOpenReceipts) return@synchronized
    val receipt = checkNotNull(authorityOpenReceipts[receiptId]) { "graph authority open receipt is unknown" }
    check(authorityTokenReceipts[receipt.token]?.fenced == true) { "graph authority reservation is not fenced" }
    when (val status = receipt.status) {
      GraphAuthorityOpenStatus.NotCreated -> Unit
      is GraphAuthorityOpenStatus.Closed -> check(status.authority !in graphAuthorities) { "graph authority remains owned" }
      else -> error("graph authority open receipt is not terminal")
    }
    authorityOpenReceipts.remove(receiptId)
    retiredAuthorityOpenReceipts.add(receiptId)
    while (authorityOpenFrontier != ULong.MAX_VALUE && retiredAuthorityOpenReceipts.remove(authorityOpenFrontier + 1uL)) authorityOpenFrontier++
  }

  internal fun closeGraphAuthority(id: ULong) = synchronized(orderedSourceLock) {
    val authority = checkNotNull(graphAuthorities[id]) { "graph authority is unknown" }
    check(authority.receipts.isEmpty() && authority.retired.isEmpty() && authority.reservations.isEmpty() && authority.reservationRetired.isEmpty()) { "graph permit retirement is pending" }
    authority.closed = true
    checkNotNull(authorityOpenReceipts[authority.openReceipt]).status = GraphAuthorityOpenStatus.Closed(id)
  }

  internal fun forgetGraphAuthority(id: ULong) = synchronized(orderedSourceLock) {
    if (id <= graphAuthorityFrontier || id in retiredGraphAuthorities) return@synchronized
    val authority = checkNotNull(graphAuthorities[id]) { "graph authority is unknown" }
    check(authority.closed && authority.receipts.isEmpty() && authority.retired.isEmpty() && authority.reservations.isEmpty() && authority.reservationRetired.isEmpty()) { "graph authority is still owned" }
    graphAuthorities.remove(id)
    retiredGraphAuthorities.add(id)
    while (graphAuthorityFrontier != ULong.MAX_VALUE && retiredGraphAuthorities.remove(graphAuthorityFrontier + 1uL)) graphAuthorityFrontier++
  }

  private fun graphReservationAuthority(request: GraphPermitRequest): GraphAuthority {
    val authority = checkNotNull(graphAuthorities[request.authority]) { "graph authority is unknown" }
    check(!authority.closed) { "graph authority is closed" }
    check(request.request > authority.reservationFrontier && request.request !in authority.reservationRetired &&
      request.request - authority.reservationFrontier <= 256uL) { "graph reservation request is retired or outside its window" }
    return authority
  }

  internal fun reserveGraphPermit(request: GraphPermitRequest): GraphReservationStatus = synchronized(orderedSourceLock) {
    val authority = graphReservationAuthority(request)
    authority.reservations[request.request]?.let {
      require(it.request == request) { "graph reservation identity mismatch" }
      return it.status
    }
    check(!graphPermitShutdown && !activeGraphStore().fenced) { "graph owner is retiring" }
    check(authority.permitAvailable && authority.nextPermit - authority.frontier <= 256uL) { "graph permit capacity exhausted" }
    val id = authority.nextPermit
    if (id == ULong.MAX_VALUE) authority.permitAvailable = false else authority.nextPermit++
    GraphReservationStatus.Issued(id).also { authority.reservations[request.request] = GraphReservation(request, it) }
  }

  internal fun statusGraphPermit(request: GraphPermitRequest): GraphReservationStatus = synchronized(orderedSourceLock) {
    val authority = graphReservationAuthority(request)
    authority.reservations[request.request]?.let {
      require(it.request == request) { "graph reservation identity mismatch" }
      return it.status
    }
    GraphReservationStatus.Pending
  }

  internal fun fenceGraphPermit(request: GraphPermitRequest): GraphReservationStatus = synchronized(orderedSourceLock) {
    val authority = graphReservationAuthority(request)
    val reservation = authority.reservations[request.request] ?: GraphReservation(request, GraphReservationStatus.NotCreated).also {
      authority.reservations[request.request] = it
    }
    require(reservation.request == request) { "graph reservation identity mismatch" }
    reservation.fenced = true
    reservation.status
  }

  internal fun forgetGraphPermitReservation(request: GraphPermitRequest) = synchronized(orderedSourceLock) {
    val authority = checkNotNull(graphAuthorities[request.authority]) { "graph authority is unknown" }
    if (request.request <= authority.reservationFrontier || request.request in authority.reservationRetired) return@synchronized
    val reservation = checkNotNull(authority.reservations[request.request]) { "graph reservation is unknown" }
    require(reservation.request == request) { "graph reservation identity mismatch" }
    check(reservation.fenced) { "graph reservation is not fenced" }
    val issued = reservation.status as? GraphReservationStatus.Issued
    if (issued != null) check(issued.permit <= authority.frontier || issued.permit in authority.retired) { "graph permit receipt remains owned" }
    authority.reservations.remove(request.request)
    authority.reservationRetired.add(request.request)
    while (authority.reservationFrontier != ULong.MAX_VALUE && authority.reservationRetired.remove(authority.reservationFrontier + 1uL)) authority.reservationFrontier++
  }

  /** Connection binding and native graph are checked in the same source admission transaction. */
  internal fun acquireGraphPermit(key: GraphPermitKey, validateConnection: () -> Boolean = { true }): GraphPermitResult = synchronized(orderedSourceLock) {
    val authority = graphAuthorities[key.authority] ?: return GraphPermitResult.STALE
    if (authority.closed) return GraphPermitResult.STALE
    val reservation = authority.reservations.values.firstOrNull { (it.status as? GraphReservationStatus.Issued)?.permit == key.id }
      ?: return GraphPermitResult.STALE
    if (reservation.request.peer != key.peer || reservation.request.connection != key.connection || reservation.request.graph != key.graph) return GraphPermitResult.STALE
    authority.receipts[key.id]?.let { receipt ->
      return if (receipt.key != key || receipt.released) GraphPermitResult.STALE else receipt.outcome
    }
    if (key.id <= authority.frontier || key.id in authority.retired || key.id - authority.frontier > 256uL) return GraphPermitResult.STALE
    val peer = key.peer.uppercase()
    val gatt = gatts[peer]
    val outcome = when {
      gatt == null || graphPermitShutdown || activeGraphStore().fenced -> GraphPermitResult.STALE
      admissionGates[gatt]?.graphPermit != null -> GraphPermitResult.BUSY
      !isCurrentGattCallback(gatt) || nativeGraphs[gatt] != key.graph || !validateConnection() -> GraphPermitResult.STALE
      // Validation may reenter owner retirement or invalidate the graph while this monitor is held.
      graphPermitShutdown || activeGraphStore().fenced || gatts[peer] !== gatt ||
        !isCurrentGattCallback(gatt) || nativeGraphs[gatt] != key.graph -> GraphPermitResult.STALE
      admissionGates.containsKey(gatt) -> GraphPermitResult.BUSY
      else -> GraphPermitResult.ACQUIRED
    }
    // Validation may synchronously retire this reservation or acquire it again.
    // Do not recreate retired evidence or replace a receipt published reentrantly.
    if (graphAuthorities[key.authority] !== authority || authority.closed ||
      authority.reservations[reservation.request.request] !== reservation ||
      key.id <= authority.frontier || key.id in authority.retired) return GraphPermitResult.STALE
    authority.receipts[key.id]?.let { receipt ->
      return if (receipt.key != key || receipt.released) GraphPermitResult.STALE else receipt.outcome
    }
    authority.receipts[key.id] = GraphPermitReceipt(key, outcome)
    if (outcome == GraphPermitResult.ACQUIRED) {
      admissionGates[checkNotNull(gatt)] = AdmissionGate().also { it.depth = 1; it.graphOwner = activeGraphOwner(); it.graphPermit = GraphPermit(activeGraphOwner(), key) }
    }
    outcome
  }

  internal fun commitGraphPermit(key: GraphPermitKey): GraphPermitResult = synchronized(orderedSourceLock) {
    val gatt = gatts[key.peer.uppercase()] ?: return GraphPermitResult.STALE
    val permit = admissionGates[gatt]?.graphPermit ?: return GraphPermitResult.STALE
    if (permit.owner !== activeGraphOwner() || permit.key != key) return GraphPermitResult.STALE
    if (permit.invalidation != null) return GraphPermitResult.INVALIDATED
    permit.committed = true
    GraphPermitResult.COMMITTED
  }

  internal fun graphPermitInvalidation(key: GraphPermitKey): GraphPermitInvalidation? = synchronized(orderedSourceLock) {
    val gatt = gatts[key.peer.uppercase()] ?: return null
    val permit = admissionGates[gatt]?.graphPermit ?: return null
    if (permit.owner === activeGraphOwner() && permit.key == key) permit.invalidation else null
  }

  internal fun releaseGraphPermit(key: GraphPermitKey): GraphPermitResult {
    var drainGatt: BluetoothGatt? = null
    var resumeDestroy = false
    val result = synchronized(orderedSourceLock) {
      val peer = key.peer.uppercase()
      val gatt = gatts[peer]
      val gate = gatt?.let { admissionGates[it] }
      val receipt = graphAuthorities[key.authority]?.receipts?.get(key.id) ?: return@synchronized GraphPermitResult.STALE
      if (receipt.key != key) return@synchronized GraphPermitResult.STALE
      if (receipt.released) return@synchronized GraphPermitResult.RELEASED
      val permit = gate?.graphPermit ?: return@synchronized GraphPermitResult.STALE
      if (permit.owner !== activeGraphOwner() || permit.key != key) return@synchronized GraphPermitResult.STALE
      receipt.released = true
      gate.graphPermit = null
      gate.depth--
      if (gate.depth == 0 && !gate.draining && (gate.callbacks.isNotEmpty() || gate.notificationOverflow != 0L || gate.controlOverflow != 0L)) {
        gate.draining = true
        drainGatt = gatt
      } else if (gate.depth == 0 && !gate.draining) admissionGates.remove(gatt)
      if (graphPermitDestroyPending && admissionGates.values.none { it.graphPermit != null }) {
        graphPermitDestroyPending = false
        resumeDestroy = true
      }
      GraphPermitResult.RELEASED
    }
    drainGatt?.let { drainAdmission(it) }
    if (resumeDestroy) destroy().failures.forEach { reportCleanupFailure(it) }
    return result
  }

  internal fun forgetGraphPermit(key: GraphPermitKey): GraphPermitResult = synchronized(orderedSourceLock) {
    val authority = graphAuthorities[key.authority] ?: return GraphPermitResult.STALE
    if (authority.closed) return GraphPermitResult.STALE
    if (key.id <= authority.frontier || key.id in authority.retired) return GraphPermitResult.FORGOTTEN
    if (key.id - authority.frontier > 256uL) return GraphPermitResult.STALE
    val reservation = authority.reservations.values.firstOrNull { (it.status as? GraphReservationStatus.Issued)?.permit == key.id }
      ?: return GraphPermitResult.STALE
    if (reservation.request.peer != key.peer || reservation.request.connection != key.connection || reservation.request.graph != key.graph) return GraphPermitResult.STALE
    val receipt = authority.receipts[key.id]
    if (receipt != null && receipt.key != key) return GraphPermitResult.STALE
    if (receipt != null && receipt.outcome == GraphPermitResult.ACQUIRED && !receipt.released) return GraphPermitResult.BUSY
    authority.receipts.remove(key.id)
    authority.retired.add(key.id)
    while (authority.frontier != ULong.MAX_VALUE && authority.retired.remove(authority.frontier + 1uL)) authority.frontier++
    GraphPermitResult.FORGOTTEN
  }

  private val orderedSources = mutableMapOf<BluetoothGatt, OrderedNotificationSource<BluetoothGattCharacteristic>>()
  private val nativeGraphOwner = UUID.randomUUID().toString()
  private val nativeGraphs = mutableMapOf<BluetoothGatt, AndroidNativeGraphIdentity>()
  private val nativeGraphOwners = mutableMapOf<BluetoothGatt, GraphOwner>()
  private class DiscoveryWork(val owner: GraphOwner) {
    var queued = true
    var gatt: BluetoothGatt? = null
    var valid = true
    var physicalRunning = false
    var callback: ((Result<ByteArray?>) -> Unit)? = null
  }
  private val discoveryWork = mutableSetOf<DiscoveryWork>()
  private val nativeDiscoveryAdmissions = mutableMapOf<BluetoothGatt, DiscoveryWork>()
  private fun finishDiscovery(work: DiscoveryWork) {
    if (!work.queued && work.gatt == null && !work.physicalRunning) discoveryWork.remove(work)
  }
  private fun removeNativeGraph(gatt: BluetoothGatt) {
    nativeGraphs.remove(gatt)
    nativeGraphOwners.remove(gatt)
  }
  private var nextNativeGraphRevision = 0uL
  private var nativeGraphRevisionAvailable = true
  private var nextOrderedSourceGeneration = 0uL
  private var orderedSourceGenerationAvailable = true
  internal var onOrderedSourceDetachedForTest: (() -> Unit)? = null

  private fun publishNativeGraph(gatt: BluetoothGatt, services: List<android.bluetooth.BluetoothGattService>, owner: GraphOwner = activeGraphOwner()) {
    check(nativeGraphRevisionAvailable) { "native graph revision exhausted" }
    val attachment = checkNotNull(gattGenerationByInstance[gatt])
    val revision = nextNativeGraphRevision
    if (revision == ULong.MAX_VALUE) nativeGraphRevisionAvailable = false else nextNativeGraphRevision++
    discovered[gatt.device.address.uppercase()] = services.toMutableList()
    nativeGraphs[gatt] = AndroidNativeGraphIdentity(nativeGraphOwner, attachment, revision)
    nativeGraphOwners[gatt] = owner
  }

  internal fun <T> withNativeGraph(deviceId: String, capture: (List<android.bluetooth.BluetoothGattService>) -> T): Pair<AndroidNativeGraphIdentity, T> = synchronized(orderedSourceLock) {
    val gatt = checkNotNull(gatts[deviceId.uppercase()])
    check(isCurrentGattCallback(gatt))
    val identity = checkNotNull(nativeGraphs[gatt]) { "native graph is unavailable" }
    val services = checkNotNull(discovered[deviceId.uppercase()])
    identity to capture(services.toList())
  }

  internal fun retryOrderedLocalCleanup(deviceId: String, path: OwnedAndroidNotificationPath, attachment: Long, epoch: Long): OrderedLocalCleanupStatus {
    val (characteristic, graph) = synchronized(orderedSourceLock) {
      val gatt = gatts[deviceId.uppercase()] ?: return OrderedLocalCleanupStatus.UNKNOWN
      if (gattGenerationByInstance[gatt] != attachment || !isCurrentGattCallback(gatt)) return OrderedLocalCleanupStatus.UNKNOWN
      val identity = nativeGraphs[gatt] ?: return OrderedLocalCleanupStatus.UNKNOWN
      val value = findChar(deviceId, path.serviceUuid, path.serviceOccurrence, path.characteristicUuid, path.characteristicOccurrence)
        ?: return OrderedLocalCleanupStatus.UNKNOWN
      value to identity
    }
    val marker = localOnlyCccdCompensations[characteristic] ?: return OrderedLocalCleanupStatus.UNKNOWN
    if (marker.epoch != epoch || marker.attachment != attachment) return OrderedLocalCleanupStatus.UNKNOWN
    val operation = marker.operation
    val cleanup = retryableCleanups[operation] ?: return OrderedLocalCleanupStatus.UNKNOWN
    val failure = cleanup()
    if (failure != null) return OrderedLocalCleanupStatus.PENDING
    retryableCleanups.remove(operation, cleanup)
    localOnlyCccdCompensations.remove(characteristic, marker)
    val sameGraph = synchronized(orderedSourceLock) {
      val gatt = gatts[deviceId.uppercase()]
      gatt != null && isCurrentGattCallback(gatt) && nativeGraphs[gatt] == graph
    }
    if (!sameGraph) return OrderedLocalCleanupStatus.UNKNOWN
    return OrderedLocalCleanupStatus.RELEASED
  }

  /** Installs one group before any of its physical notification enables. */
  internal fun openOrderedSourceGroup(
    gatt: BluetoothGatt, generation: Long, members: Set<BluetoothGattCharacteristic>,
    capacity: Int, byteCapacity: Int, wake: () -> Unit
  ): Pair<OrderedNotificationSource<BluetoothGattCharacteristic>, OrderedNotificationSource<BluetoothGattCharacteristic>.Group> =
    synchronized(orderedSourceLock) {
      val source = orderedSourceFor(gatt, generation, members)
      source to source.open(members, capacity, byteCapacity, wake)
    }

  internal fun openRegisteredOrderedSourceGroup(
    gatt: BluetoothGatt, generation: Long, members: Set<BluetoothGattCharacteristic>,
    capacity: Int, byteCapacity: Int, wake: () -> Unit
  ): Pair<OrderedNotificationSource<BluetoothGattCharacteristic>, Pair<OrderedNotificationSource<BluetoothGattCharacteristic>.Group, Map<BluetoothGattCharacteristic, OrderedNotificationSource.Membership>>> =
    synchronized(orderedSourceLock) {
      val source = orderedSourceFor(gatt, generation, members)
      source to source.openRegistered(members, capacity, byteCapacity, wake = wake)
    }

  /** Resolves full occurrence paths in the current database and installs them atomically. */
  internal var onOrderedOpenHandoffForTest: (() -> Unit)? = null
  internal fun openOrderedSourceForPeer(
    deviceId: String, paths: Set<OwnedAndroidNotificationPath>, capacity: Int, byteCapacity: Int,
    wakeFailureId: ULong? = null, onWakeFailure: ((ULong, ULong, Throwable) -> Unit)? = null,
    expectedGraph: AndroidNativeGraphIdentity? = null,
    onInstalled: ((OrderedNotificationSource<BluetoothGattCharacteristic>, OrderedNotificationSource<BluetoothGattCharacteristic>.Group,
                   Map<OwnedAndroidNotificationPath, BluetoothGattCharacteristic>, Map<BluetoothGattCharacteristic, OrderedNotificationSource.Membership>) -> Unit)? = null,
    wake: (ULong) -> Unit
  ): OrderedAndroidSourceOpen = synchronized(orderedSourceLock) {
    var handingOff = false
    val prepared = try {
      require(paths.isNotEmpty())
      val gatt = gatts[deviceId.uppercase()] ?: error("connection is not current")
      check((admissionGates[gatt]?.detachedSources?.size ?: 0) < 64) { "detached source recovery capacity exhausted" }
      val generation = gattGenerationByInstance[gatt] ?: error("connection generation is unavailable")
      if (expectedGraph != null && nativeGraphs[gatt] != expectedGraph) throw OrderedRegistrationStale()
      val resolved = paths.associateWith { path ->
        findChar(deviceId, path.serviceUuid, path.serviceOccurrence, path.characteristicUuid, path.characteristicOccurrence)
          ?: error("characteristic instance is not current")
      }
      require(resolved.values.toSet().size == resolved.size)
      val source = orderedSourceFor(gatt, generation, resolved.values.toSet())
      val opened = source.openRegistered(resolved.values.toSet(), capacity, byteCapacity, wakeFailureId, onWakeFailure,
        onRegistered = { group, memberships ->
          handingOff = true
          onInstalled?.invoke(source, group, resolved, memberships)
        }) { wake(source.sourceGeneration) }
      Triple(source, opened, resolved)
    } catch (error: Throwable) {
      if (error is OrderedRegistrationStale) throw error
      if (handingOff) throw error
      throw OrderedOpenNotCreated(error)
    }
    val (source, opened, resolved) = prepared
    val (group, memberships) = opened
    onOrderedOpenHandoffForTest?.invoke()
    val result = OrderedAndroidSourceOpen(source, group, resolved.mapValues { (_, characteristic) -> characteristic to memberships.getValue(characteristic) })
    result
  }

  private fun orderedSourceFor(gatt: BluetoothGatt, generation: Long, members: Set<BluetoothGattCharacteristic>): OrderedNotificationSource<BluetoothGattCharacteristic> {
    check(generation > 0 && isCurrentGattCallback(gatt) && gattGenerationByInstance[gatt] == generation)
    val services = discovered[gatt.device.address.uppercase()] ?: error("database is not discovered")
    require(members.all { member -> services.any { service -> service.characteristics.any { it === member } } })
    val source = orderedSources.getOrPut(gatt) {
      check(orderedSourceGenerationAvailable) { "ordered source generation exhausted" }
      val sourceGeneration = nextOrderedSourceGeneration
      if (sourceGeneration == ULong.MAX_VALUE) orderedSourceGenerationAvailable = false
      else nextOrderedSourceGeneration++
      OrderedNotificationSource(generation, sourceGeneration)
    }
    check(source.generation == generation)
    return source
  }

  private fun invalidateOrderedSource(gatt: BluetoothGatt, reason: OrderedNotificationSource.Reason) {
    val source = synchronized(orderedSourceLock) {
      removeNativeGraph(gatt)
      nativeDiscoveryAdmissions[gatt]?.valid = false
      if (reason == OrderedNotificationSource.Reason.DATABASE_CHANGED) {
        discovered.remove(gatt.device.address.uppercase())
      }
      orderedSources.remove(gatt)?.also { source -> admissionGates[gatt]?.detachedSources?.add(source) }
    }
    onOrderedSourceDetachedForTest?.invoke()
    source?.invalidate(reason) { wake ->
      val deferred = synchronized(orderedSourceLock) {
        val gate = admissionGates[gatt]
        if (gate == null) null else enqueueAdmission(gate, 0, true, wake = true, action = wake)
      }
      if (deferred == null) { wake(); true } else deferred
    }
  }

  private fun reportOrderedWakeFailure(operation: String, error: Throwable) {
    val failure = OwnedRadioTeardownFailure(operation, error)
    OwnedAndroidLog.e("$operation failed")
    errorOf { reportCleanupFailure(failure) }?.let { observerError ->
      OwnedAndroidLog.e("$operation failure observer failed: ${observerError.javaClass.simpleName}")
    }
  }
  private val activeNativeSubscriptionOwnership =
    OwnedAndroidSubscriptionOwnership<BluetoothGattCharacteristic>(
      onEvent = { device, _, characteristic, epoch, path, event ->
        when (event) {
          is NotificationOwnershipFifo.Event.Value -> {
            onNotification?.let { notify ->
              val serviceUuid = path?.serviceUuid ?: characteristic.service?.uuid
                ?: throw IllegalStateException("notification service is unavailable")
              notify(device, serviceUuid, characteristic.uuid, event.bytes.copyOf())
            }
            val owned = onOwnedProtocolNotification
            if (owned != null && epoch != null && path != null) {
              owned(device, path, epoch, event.bytes.copyOf())
            } else {
              val notify = onProtocolNotification
                ?: throw IllegalStateException("notification protocol owner is unavailable")
              val sourcePath = path ?: throw IllegalStateException("notification source path is unavailable")
              notify(device, characteristic, sourcePath, event.bytes.copyOf())
            }
          }
          is NotificationOwnershipFifo.Event.Loss -> {
            val notify = onProtocolNotificationLoss
              ?: throw IllegalStateException("notification loss owner is unavailable")
            val sourcePath = path ?: throw IllegalStateException("notification loss path is unavailable")
            notify(device, characteristic, sourcePath, event.items, event.bytes)
          }
          is NotificationOwnershipFifo.Event.Terminal -> Unit
        }
      },
      onStopped = { device, generation, characteristic, epoch, path, reason ->
        when (reason) {
          is NotificationOwnershipFifo.Stop.DeliveryUncertain ->
            OwnedAndroidLog.e("notification ingress delivery", IllegalStateException("delivery callback failed"))
          is NotificationOwnershipFifo.Stop.ControlCapacity ->
            OwnedAndroidLog.e("notification ingress control", IllegalStateException("control capacity exhausted"))
        }
        val notify = onProtocolNotificationFailure
          ?: throw IllegalStateException("notification failure owner is unavailable")
        notify(device, generation, characteristic, epoch, path, reason)
      }
    )

  /** Per-device connection lifecycle listeners (multi-device safe). */
  private val connectionListeners =
    ConcurrentHashMap<String, (deviceId: String, connected: Boolean, gattStatus: Int) -> Unit>()

  /** Per-device FIFO for outstanding GATT ops. */
  private val deviceQueues = ConcurrentHashMap<String, GattSerialQueue>()
  private val nextGattOperationId = AtomicLong(1L)

  /** The connectGatt parameters of one pending (re)connect: autoConnect and the PHY mask (0 = none). */
  private class PendingConnect(val autoConnect: Boolean, val phyMask: Int, val attempt: GattConnectAttempt?, val adapterEpoch: Long)

  /**
   * The connect request that opened each GATT generation, retained until that generation is
   * closed so every observation of it (connected, failed, lost) names the request it belongs to.
   */
  private val gattConnectAttempts = ConcurrentHashMap<Long, GattConnectAttempt>()

  /**
   * Connect parameters for a reconnect that must wait until the prior GATT reports
   * [BluetoothProfile.STATE_DISCONNECTED] before [BluetoothDevice.connectGatt] (R3-F003).
   */
  private val pendingReconnect = ConcurrentHashMap<String, PendingConnect>()

  // A native open is not owned by the GATT maps until connectGatt returns. Serialize the
  // peer's lifecycle through publication and reconciliation, including reentrant callbacks.
  // The lock protects admission only; neither native calls nor observers run under it.
  private val peerLifecycleLock = Any()
  private class PeerLifecycle {
    val owner: Thread = Thread.currentThread()
    val queue = ArrayDeque<() -> Unit>()
    var opening = false
    var adapterLost = false
  }
  private val peerLifecycleQueues = mutableMapOf<String, PeerLifecycle>()
  private var adapterLossEpoch = 0L

  private fun runPeerLifecycle(
    key: String, direct: () -> Unit, deferred: () -> Unit = direct,
    admissionRefused: ((Throwable) -> Unit)? = null
  ) {
    var refusal: Throwable? = null
    val reentrant = synchronized(peerLifecycleLock) {
      val lane = peerLifecycleQueues[key]
      if (lane != null) {
        if (lane.owner === Thread.currentThread() && !lane.opening) true
        else {
          if (lane.queue.size >= peerLifecycleCapacity) {
            val error = IllegalStateException("peer lifecycle deferred operation capacity exhausted")
            if (admissionRefused == null) throw error
            refusal = error
            false
          } else {
            lane.queue.addLast(deferred)
            return
          }
        }
      } else {
        peerLifecycleQueues[key] = PeerLifecycle()
        false
      }
    }
    // Only this pre-enqueue branch proves that the deferred closure cannot run.
    // Deliver the refusal outside the lifecycle lock, just like other callbacks.
    refusal?.let { admissionRefused?.invoke(it); return }
    if (reentrant) { direct(); return }
    var next: (() -> Unit)? = direct
    var error: Throwable? = null
    var first = true
    while (next != null) {
      val stepError = errorOf(next)
      if (first) error = stepError
      else if (stepError != null) {
        // The deferred caller has already returned. Never turn another request's failure
        // into a throw from the direct caller, which could orphan its admitted generation.
        OwnedAndroidLog.e("deferred peer lifecycle operation for $key", stepError)
      }
      first = false
      next = synchronized(peerLifecycleLock) {
        val queue = checkNotNull(peerLifecycleQueues[key]).queue
        if (queue.isEmpty()) {
          peerLifecycleQueues.remove(key)
          null
        } else queue.removeFirst()
      }
    }
    error?.let { throw it }
  }

  /**
   * One disconnect owner per GATT generation: every disconnect caller joins the
   * same close deadline and is completed exactly once by whichever path retires it.
   */
  private val disconnectOwners = AndroidGattDisconnectOwners()
  // Includes native close and the resulting waiter/observer callbacks, even
  // after the GATT has left the current-generation maps.
  private val gattTeardownsInFlight = AtomicInteger()

  /** Physical cleanup that failed after public cancellation or a remote rejection. */
  private class CleanupScope(val peer: String, val generation: Long, val owner: Any?) {
    fun matches(other: CleanupScope) = peer == other.peer && generation == other.generation && owner === other.owner
  }
  private class RetryableCleanup(val scope: CleanupScope, val cleanup: () -> OwnedRadioTeardownFailure?) {
    operator fun invoke() = cleanup()
  }
  private val retryableCleanups = ConcurrentHashMap<String, RetryableCleanup>()
  private val pendingCccdCompensations = ConcurrentHashMap<BluetoothGattCharacteristic, Set<String>>()
  private fun retainCccdCompensation(characteristic: BluetoothGattCharacteristic, operation: String) {
    pendingCccdCompensations.compute(characteristic) { _, existing -> (existing ?: emptySet()) + operation }
  }
  private fun releaseCccdCompensation(characteristic: BluetoothGattCharacteristic, operation: String) {
    pendingCccdCompensations.computeIfPresent(characteristic) { _, existing ->
      (existing - operation).takeIf { it.isNotEmpty() }
    }
  }
  private val localOnlyCccdCompensations = ConcurrentHashMap<BluetoothGattCharacteristic, LocalOnlyCccdCleanup>()

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

  /**
   * [onConnectionState] with the connect request the observed GATT generation was opened for
   * (null when it was opened without one). Delivered from the same single dispatch point, so a
   * consumer that keys a pending command by peer can tell a prior generation's loss from the
   * outcome of the connect it is waiting on.
   */
  internal var onConnectionOutcome:
    ((deviceId: String, connected: Boolean, gattStatus: Int, attempt: GattConnectAttempt?) -> Unit)? = null
  internal var onOrderedLinkFence: ((deviceId: String, attempt: GattConnectAttempt?) -> Unit)? = null
  var onNotification: ((deviceId: String, serviceUuid: UUID, charUuid: UUID, value: ByteArray) -> Unit)? = null
  /**
   * Protocol-v1 notification callback retaining the concrete native attribute
   * so a caller can derive exact UUID occurrence identity before crossing the
   * JavaScript boundary.
   */
  internal var onProtocolNotification: ((deviceId: String, characteristic: BluetoothGattCharacteristic, path: OwnedAndroidNotificationPath, value: ByteArray) -> Unit)? = null
  internal var onProtocolNotificationLoss: ((deviceId: String, characteristic: BluetoothGattCharacteristic, path: OwnedAndroidNotificationPath, items: Long, bytes: Long) -> Unit)? = null
  internal var onProtocolNotificationFailure: ((deviceId: String, generation: Long, characteristic: BluetoothGattCharacteristic, epoch: Long?, path: OwnedAndroidNotificationPath?, reason: NotificationOwnershipFifo.Stop) -> Unit)? = null
  internal var onOwnedProtocolNotification: ((deviceId: String, path: OwnedAndroidNotificationPath, epoch: Long, value: ByteArray) -> Unit)? = null
  /**
   * API 31+ [BluetoothGattCallback.onServiceChanged]: GATT DB out of sync;
   * apps should re-run discoverServices (Android docs).
   */
  var onServicesChanged: ((deviceId: String) -> Unit)? = null
  internal var onSecurityFailure: ((deviceId: String?, error: Throwable) -> Unit)? = null
  internal var onSecurityState: ((deviceId: String, state: OwnedAndroidSecurityState) -> Unit)? = null
  /** Runtime scan failures (permissions, internal errors) — not start exceptions. */
  var onScanFailed: ((errorCode: Int) -> Unit)? = null
  internal var onCleanupFailure: ((OwnedRadioTeardownFailure) -> Unit)? = null

  private data class GattTeardownOwner(
    val gatt: BluetoothGatt,
    val generation: Long
  )

  /**
   * One physical teardown of a GATT generation. [failure] is what the close did:
   * non-null means it failed and ownership is retained. [callbackError] is the
   * first error a waiter or diagnostic observer threw while the teardown was being
   * settled; it is held back so every cleanup and settlement step completes first.
   */
  private class GattTeardown(
    val failure: OwnedRadioTeardownFailure?,
    val callbackError: Throwable? = null,
    val inProgress: Boolean = false
  ) {
    fun withCallbackError(error: Throwable?): GattTeardown =
      GattTeardown(failure, mergeErrors(error, callbackError), inProgress)

    /** Rethrows the held-back [callbackError]; otherwise returns the close outcome. */
    fun orThrow(): OwnedRadioTeardownFailure? {
      callbackError?.let { throw it }
      return failure
    }

    /**
     * Runs [afterCleanClose] only when the close was clean, even if a waiter threw
     * while settling it, and then rethrows every error that occurred.
     */
    fun thenWhenClean(afterCleanClose: () -> Unit) {
      if (inProgress || failure != null) {
        orThrow()
        return
      }
      mergeErrors(callbackError, errorOf(afterCleanClose))?.let { throw it }
    }
  }

  /** The deadline scheduler refused the request (returned false) rather than throwing. */
  private class DeadlineRefused(key: String) :
    IllegalStateException("Android refused to schedule the GATT close deadline for $key")

  /** How a disconnect request relates to its GATT generation's owner. */
  private class GattDisconnectJoin(
    val kind: Kind,
    /** Error a superseded waiter threw, or the scheduler threw; surfaced after this call's own work. */
    val heldError: Throwable? = null,
    /** [Kind.FORCED_CLOSE]: the physical close that ran because no deadline could be armed. */
    val forcedClose: GattTeardown? = null
  ) {
    enum class Kind {
      /** This call created the owner: it alone requests the native disconnect. */
      STARTED,
      /** An owner already requested the disconnect; the waiter is settled by it. */
      JOINED,
      /** The generation's own teardown already ran: nothing was recorded. */
      REJECTED,
      /**
       * This call created the owner, but the generation was torn down by another path before a
       * refused deadline could close it: that path settled the waiter, so there is nothing to
       * close, request or complete here.
       */
      TORN_DOWN,
      /** The deadline could not be armed, so the generation was closed explicitly instead. */
      FORCED_CLOSE
    }
  }

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

  internal fun resolveDirectoryPeer(peerId: String): ConnectedPeerSnapshot? {
    val address = peerId.uppercase(java.util.Locale.ROOT)
    require(Regex("^[0-9A-F]{2}(:[0-9A-F]{2}){5}$").matches(address)) { "Invalid directory peer address" }
    connectedPeerSnapshots().firstOrNull { it.nativePeerId == address }?.let { return it }
    bondedPeerSnapshots().firstOrNull { it.nativePeerId == address }?.let { return ConnectedPeerSnapshot(it.nativePeerId, it.displayName) }
    val remoteDevice = adapter?.getRemoteDevice(address) ?: return null
    return ConnectedPeerSnapshot(address, remoteDevice.name?.takeIf { it.isNotEmpty() })
  }

  /** Current system GATT connections, including other applications' links. */
  internal fun connectedPeerSnapshots(): List<ConnectedPeerSnapshot> {
    bondedPeerAdapterReadiness(adapter != null, hasBluetoothConnectPermission(), adapter?.state ?: BluetoothAdapter.ERROR).getOrThrow()
    val peers = bluetoothManager.getConnectedDevices(BluetoothProfile.GATT)
    require(peers.size <= 4096) { "system-connected directory exceeds its bounded capacity" }
    return peers.map { device ->
      val address = device.address.uppercase(java.util.Locale.ROOT)
      require(Regex("^[0-9A-F]{2}(:[0-9A-F]{2}){5}$").matches(address)) { "Android supplied an invalid peer address" }
      ConnectedPeerSnapshot(address, device.name?.takeIf { it.isNotEmpty() })
    }.distinctBy { it.nativePeerId }.sortedBy { it.nativePeerId }
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
    synchronized(receiverRetirementLock) {
      check(!receiversRetiring) { "receiver owner is retiring" }
      if (adapterStateReceiver != null) {
        if (adapterReceiverActive) return
        unregisterAdapterStateReceiver()?.let { throw it.throwable }
      }
      val receiver =
        object : BroadcastReceiver() {
          override fun onReceive(ctx: Context?, intent: Intent?) {
            if (!beginReceiverCallback(this, true)) return
            try {
              if (intent?.action != BluetoothAdapter.ACTION_STATE_CHANGED) return
              val state = intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)
              runEvery(
                { handleAdapterStateTransition(state) },
                { onAdapterState?.invoke(mapAdapterState(state)) }
              )?.let { throw it }
            } finally { endReceiverCallback() }
          }
        }
      val filter = IntentFilter(BluetoothAdapter.ACTION_STATE_CHANGED)
      // System broadcast (ACTION_STATE_CHANGED) requires RECEIVER_EXPORTED on API 33+.
      synchronized(receiverCallbackLock) {
        adapterStateReceiver = receiver
        adapterReceiverAdmissionFailed = false
        adapterReceiverActive = true
      }
      try {
        if (Build.VERSION.SDK_INT >= 33) {
          context.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
        } else {
          @Suppress("UnspecifiedRegisterReceiverFlag")
          context.registerReceiver(receiver, filter)
        }
      } catch (error: Exception) {
        synchronized(receiverCallbackLock) { adapterReceiverActive = false }
        adapterReceiverAdmissionFailed = true
        throw error
      }
    }
  }

  private fun handleAdapterStateTransition(state: Int) {
    if (state == BluetoothAdapter.STATE_ON) {
      pendingGattTeardowns.keys.toList().forEachSettled { key -> retryGattTeardown(key).orThrow() }
      return
    }
    if (!requiresImmediateGattTeardownOnAdapterState(state)) return

    synchronized(peerLifecycleLock) {
      adapterLossEpoch++
      peerLifecycleQueues.values.filter { it.opening }.forEach { it.adapterLost = true }
    }
    runEvery({ settleQueuedConnects() }, { (gatts.keys + pendingGattTeardowns.keys).toSet().forEachSettled { key ->
      gatts[key]?.let { gatt -> gattGenerationByInstance[gatt]?.let { fenceRetiringGatt(key, gatt, it) } }
      val pendingError = errorOf { failPendingForDevice(key, "adapter unavailable") }
      val owner = pendingGattTeardowns[key]
      val gatt = gatts[key] ?: owner?.gatt ?: run {
        pendingError?.let { throw it }
        return@forEachSettled
      }
      val teardown = completeGattTeardown(key, gatt)
      mergeErrors(mergeErrors(pendingError, teardown.callbackError), teardown.failure?.throwable)?.let { throw it }
    } })?.let { throw it }
  }

  internal fun unregisterAdapterStateReceiver(): OwnedRadioTeardownFailure? {
    synchronized(receiverRetirementLock) {
      val receiver = adapterStateReceiver ?: return null
      synchronized(receiverCallbackLock) { adapterReceiverActive = false }
      try {
        context.unregisterReceiver(receiver)
      } catch (throwable: Exception) {
        if (adapterReceiverAdmissionFailed && throwable is IllegalArgumentException) {
          adapterStateReceiver = null
          adapterReceiverAdmissionFailed = false
          return null
        }
        OwnedAndroidLog.e("unregisterAdapterStateReceiver", throwable)
        // Keep the receiver owner intact so a later destroy attempt can retry the required release.
        return OwnedRadioTeardownFailure("unregisterAdapterStateReceiver", throwable)
      }
      adapterStateReceiver = null
      adapterReceiverAdmissionFailed = false
      return null
    }
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
    serviceUuids: Array<out String>?, scanMode: Int,
    callbackType: Int = ScanSettings.CALLBACK_TYPE_ALL_MATCHES, legacyScan: Boolean = true,
    allowDuplicates: Boolean = true, deviceAddresses: Array<out String> = emptyArray(),
    reportDelayMs: Long = 0, scanPhy: Int? = null
  ) = startLegacyScanSession(serviceUuids, scanMode, callbackType, legacyScan, allowDuplicates,
    deviceAddresses, reportDelayMs, scanPhy)

  internal fun startLegacyScanSession(
    serviceUuids: Array<out String>?,
    scanMode: Int,
    callbackType: Int = ScanSettings.CALLBACK_TYPE_ALL_MATCHES,
    legacyScan: Boolean = true,
    allowDuplicates: Boolean = true,
    deviceAddresses: Array<out String> = emptyArray(),
    reportDelayMs: Long = 0,
    scanPhy: Int? = null,
    onSessionAdvertisement: ((OwnedAndroidProtocolAdvertisement) -> Unit)? = null,
    onSessionFailure: ((Int, () -> Pair<Boolean, OwnedRadioTeardownFailure?>) -> Unit)? = null,
    identity: Any? = null
  ) = startOwnedScan(legacyScanOwner, serviceUuids, scanMode, callbackType, legacyScan, allowDuplicates,
    deviceAddresses, reportDelayMs, scanPhy, { advertisement ->
      onScanResult?.invoke(advertisement.deviceId, advertisement.name, advertisement.rssi,
        advertisement.connectable, advertisement.rawRecord?.copyOf())
      if (onSessionAdvertisement != null) onSessionAdvertisement(advertisement) else onProtocolScanResult?.invoke(advertisement)
    }, { errorCode, stop ->
      if (onSessionFailure != null) onSessionFailure(errorCode, stop) else onScanFailed?.invoke(errorCode)
    }, identity)

  internal fun startOwnedScan(
    owner: Any,
    serviceUuids: Array<out String>?,
    scanMode: Int,
    callbackType: Int,
    legacyScan: Boolean,
    allowDuplicates: Boolean,
    deviceAddresses: Array<out String>,
    reportDelayMs: Long,
    scanPhy: Int?,
    onAdvertisement: (OwnedAndroidProtocolAdvertisement) -> Unit,
    onFailure: (Int, () -> Pair<Boolean, OwnedRadioTeardownFailure?>) -> Unit,
    identity: Any? = null
  ) {
    // ScanSettings.Builder.setLegacy exists only on API 26+; below that the
    // platform can only run a legacy scan, so accepting legacyScan=false would
    // silently ignore the caller's extended-advertising request. Fail closed
    // before any scan state is touched.
    require(legacyScan || scanSdkInt >= 26) {
      "legacyScan=false requires ScanSettings.Builder.setLegacy (API 26+); this device cannot honour it"
    }
    require(reportDelayMs >= 0) { "scan report delay must be nonnegative" }
    require(scanPhy == null || (!legacyScan && scanSdkInt >= 26)) {
      "scan PHY selection requires a nonlegacy scan on API 26+"
    }
    val normalizedServiceUuids = normalizeScanServiceUuids(serviceUuids?.toList() ?: emptyList())
    val a = adapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    if (scanPhy == BluetoothDevice.PHY_LE_CODED) {
      require(scanSdkInt >= 26 && a.isLeCodedPhySupported) {
        "LE Coded scan PHY is unavailable on this Bluetooth adapter"
      }
    }
    val scanner = a.bluetoothLeScanner ?: throw IllegalStateException("LE scanner unavailable")
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
      .setReportDelay(reportDelayMs)
    // setLegacy is API 26+; default true matches 3.x / legacyScan:true docs.
    if (scanSdkInt >= 26) {
      builder.setLegacy(legacyScan)
      if (scanPhy != null) builder.setPhy(scanPhy)
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
    val seenDeviceIds = ConcurrentHashMap.newKeySet<String>()
    lateinit var session: ScanSession
    val cb = object : ScanCallback() {
      override fun onScanResult(callbackType: Int, result: AndroidScanResult) {
        deliverResult(result)
      }

      override fun onBatchScanResults(results: MutableList<AndroidScanResult>) {
        // Each packet follows the ordinary owned-payload ingress path. A batch
        // changes delivery scheduling, not the duplicate or quota policy.
        results.forEach { deliverResult(it) }
      }

      private fun deliverResult(result: AndroidScanResult) {
        if (!session.delivering) return
        val device = result.device ?: return
        val id = device.address
        if (!allowDuplicates && !seenDeviceIds.add(id.uppercase())) return
        val name = result.scanRecord?.deviceName
        val advertisement = protocolAdvertisement(result, id, name)
        onAdvertisement(advertisement)
      }

      override fun onScanFailed(errorCode: Int) {
        if (!session.delivering) return
        OwnedAndroidLog.e("scan failed code=$errorCode")
        onFailure(errorCode) { stopOwnedScanSession(owner, session, true) }
      }
    }
    session = ScanSession(scanner, cb, identity)
    synchronized(session) {
      synchronized(scanLock) {
        check(owner !in scanSessions) { "Android scan cleanup is still owned by a prior scan" }
        scanSessions[owner] = session
      }
      try {
        scanner.startScan(if (filters.isEmpty()) null else filters, settings, cb)
      } catch (error: Throwable) {
        session.delivering = false
        try {
          scanner.stopScan(cb)
          synchronized(scanLock) { scanSessions.remove(owner, session) }
        } catch (stopError: Throwable) {
          OwnedAndroidLog.e("startScan compensating stopScan", stopError)
          error.addSuppressed(stopError)
        }
        throw error
      }
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
      rawRecord = scanRecord?.bytes?.copyOf(),
      cachedName = if (scanSdkInt < Build.VERSION_CODES.S ||
        context.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) == PackageManager.PERMISSION_GRANTED
      ) result.device?.name else null,
      captureTimestampMs = result.timestampNanos.takeIf { it >= 0 }?.div(1_000_000L)
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

  internal fun stopScan(): OwnedRadioTeardownFailure? = stopOwnedScan(legacyScanOwner)

  internal fun stopOwnedScan(owner: Any): OwnedRadioTeardownFailure? {
    val session = synchronized(scanLock) { scanSessions[owner] } ?: return null
    return stopOwnedScanSession(owner, session, false).second
  }

  /** The callback's session identity prevents a late failure from stopping a replacement scan. */
  private fun stopOwnedScanSession(owner: Any, session: ScanSession, requireDelivery: Boolean): Pair<Boolean, OwnedRadioTeardownFailure?> {
    return synchronized(session) {
      if (synchronized(scanLock) { scanSessions[owner] !== session || requireDelivery && !session.delivering }) return@synchronized true to null
      try {
        session.scanner.stopScan(session.callback)
        session.delivering = false
        synchronized(scanLock) { scanSessions.remove(owner, session) }
        false to null
      } catch (t: Throwable) {
        OwnedAndroidLog.e("stopScan", t)
        false to OwnedRadioTeardownFailure("stopScan", t)
      }
    }
  }

  internal fun hasScanCleanupOwnership(): Boolean = synchronized(scanLock) { legacyScanOwner in scanSessions }

  internal fun stopLegacyScan(identity: Any): OwnedRadioTeardownFailure? {
    val session = synchronized(scanLock) { scanSessions[legacyScanOwner]?.takeIf { it.identity === identity } } ?: return null
    return stopOwnedScanSession(legacyScanOwner, session, false).second
  }

  internal fun hasLegacyScanCleanupOwnership(identity: Any): Boolean = synchronized(scanLock) {
    scanSessions[legacyScanOwner]?.identity === identity
  }

  internal fun securityState(deviceId: String): OwnedAndroidSecurityState {
    val bluetoothAdapter = adapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    if (!hasBluetoothConnectPermission()) throw SecurityException("BLUETOOTH_CONNECT is not granted")
    return securityStateOf(bluetoothAdapter.getRemoteDevice(deviceId.uppercase(Locale.ROOT)))
  }

  private fun securityStateOf(device: BluetoothDevice, observedBond: String? = null): OwnedAndroidSecurityState {
    val key = device.address.uppercase(Locale.ROOT)
    val encryption = if (encryptionApi.snapshotAvailable) {
      val generation = connectedEncryptionGenerations[key]
      val observed = encryptionApi.read(device)
      if (connectedEncryptionGenerations[key] == generation) observed else "unknown"
    } else if (securitySdkInt >= 36) {
      // Peer broadcasts carry no GATT generation and cannot establish this link's snapshot.
      "unknown"
    } else "unsupported"
    return OwnedAndroidSecurityState(
      bond = observedBond ?: when (device.bondState) {
        BluetoothDevice.BOND_BONDED -> "bonded"
        BluetoothDevice.BOND_BONDING -> "bonding"
        BluetoothDevice.BOND_NONE -> "notBonded"
        else -> "unknown"
      }, pairingPossible = true, encryption = encryption
    )
  }

  internal fun receiveEncryptionChange(device: BluetoothDevice, transport: Int, status: Int, enabled: Boolean?) {
    if (securitySdkInt < 36) return
    if (transport != BluetoothDevice.TRANSPORT_LE) return
    var admittedPeer: String? = null
    try {
      val key = device.address.uppercase(Locale.ROOT)
      admittedPeer = key
      if (gatts.containsKey(key) && !connectedEncryptionGenerations.containsKey(key)) return
      val observed = AndroidEncryptionApi.event(status, enabled)
      // The broadcast's own answer wins; querying the OS again cannot replace it.
      val bond = when (device.bondState) {
        BluetoothDevice.BOND_BONDED -> "bonded"
        BluetoothDevice.BOND_BONDING -> "bonding"
        BluetoothDevice.BOND_NONE -> "notBonded"
        else -> "unknown"
      }
      onSecurityState?.invoke(key, OwnedAndroidSecurityState(bond, true, observed))
    } catch (error: Exception) {
      onSecurityFailure?.invoke(admittedPeer, error)
    }
  }

  internal fun pair(
    deviceId: String,
    transport: String,
    callback: (String, OwnedAndroidSecurityState) -> Unit
  ): Long {
    val bluetoothAdapter = adapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    val device = bluetoothAdapter.getRemoteDevice(deviceId.uppercase(Locale.ROOT))
    if (isAlreadyPaired(device.bondState, device.type, transport)) {
      val state = securityStateOf(device, "bonded")
      if (!postNow { callback("alreadyPaired", state) }) {
        callback("alreadyPaired", state)
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
    synchronized(receiverRetirementLock) {
      check(!receiversRetiring) { "receiver owner is retiring" }
      if (bondStateReceiver != null) {
        if (securityReceiverActive) return
        unregisterBondStateReceiver()?.let { throw it.throwable }
      }
      val receiver = object : BroadcastReceiver() {
        override fun onReceive(ctx: Context?, intent: Intent?) {
          if (!beginReceiverCallback(this, false)) return
          try {
            if (securitySdkInt >= 36 && intent?.action == BluetoothDevice.ACTION_ENCRYPTION_CHANGE) {
              try {
              val device = IntentCompat.getParcelableExtra(intent, BluetoothDevice.EXTRA_DEVICE, BluetoothDevice::class.java)
              if (device == null) {
                onSecurityFailure?.invoke(null, IllegalArgumentException("Encryption Change omitted device"))
                return
              }
              if (!intent.hasExtra(BluetoothDevice.EXTRA_TRANSPORT)) {
                onSecurityFailure?.invoke(device.address, IllegalArgumentException("Encryption Change omitted transport"))
                return
              }
              receiveEncryptionChange(device, intent.getIntExtra(BluetoothDevice.EXTRA_TRANSPORT, BluetoothDevice.TRANSPORT_AUTO),
                intent.getIntExtra(BluetoothDevice.EXTRA_ENCRYPTION_STATUS, BluetoothDevice.ERROR),
                if (intent.hasExtra(BluetoothDevice.EXTRA_ENCRYPTION_ENABLED)) intent.getBooleanExtra(BluetoothDevice.EXTRA_ENCRYPTION_ENABLED, false) else null)
              } catch (error: Exception) { onSecurityFailure?.invoke(null, error) }
              return
            }
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
              var observationFailure: Exception? = null
              val observed = try { securityStateOf(device, state) } catch (error: Exception) {
                observationFailure = error
                OwnedAndroidSecurityState(state, pairingPossible, if (securitySdkInt >= 36) "unknown" else "unsupported")
              }
              onSecurityState?.invoke(deviceId, observed)
              observationFailure?.let { onSecurityFailure?.invoke(deviceId, it) }
              val callback = pendingBondPairs[deviceId.uppercase()]
              if (callback != null && state != "bonding") {
                pendingBondPairs.remove(deviceId.uppercase(), callback)
                callback(
                  when (state) {
                    "bonded" -> "paired"
                    "notBonded" -> "rejected"
                    else -> "unknown"
                  },
                  observed
                )
              }
            } catch (error: Exception) {
              onSecurityFailure?.invoke(null, error)
            }
          } finally { endReceiverCallback() }
        }
      }
      val filter = IntentFilter(BluetoothDevice.ACTION_BOND_STATE_CHANGED)
      if (securitySdkInt >= 36) filter.addAction(BluetoothDevice.ACTION_ENCRYPTION_CHANGE)
      synchronized(receiverCallbackLock) {
        bondStateReceiver = receiver
        bondReceiverAdmissionFailed = false
        securityReceiverActive = true
      }
      try {
        if (Build.VERSION.SDK_INT >= 33) context.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED)
        else {
          @Suppress("UnspecifiedRegisterReceiverFlag")
          context.registerReceiver(receiver, filter)
        }
      } catch (error: Exception) {
        synchronized(receiverCallbackLock) { securityReceiverActive = false }
        bondReceiverAdmissionFailed = true
        // Retain uncertain OS effects until release succeeds or Android confirms no registration.
        throw error
      }
    }
  }

  internal fun unregisterBondStateReceiver(): OwnedRadioTeardownFailure? {
    synchronized(receiverRetirementLock) {
      val receiver = bondStateReceiver ?: return null
      synchronized(receiverCallbackLock) { securityReceiverActive = false }
      return try {
        context.unregisterReceiver(receiver)
        bondStateReceiver = null
        bondReceiverAdmissionFailed = false
        null
      } catch (error: Exception) {
        if (bondReceiverAdmissionFailed && error is IllegalArgumentException) {
          // Context documents this exception when the receiver was never registered.
          bondStateReceiver = null
          bondReceiverAdmissionFailed = false
          return null
        }
        OwnedAndroidLog.e("unregisterBondStateReceiver", error)
        OwnedRadioTeardownFailure("unregisterBondStateReceiver", error)
      }
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

  /**
   * [phyMask]: `BluetoothDevice.PHY_LE_*_MASK` bits the link is established on
   * (`connectGatt(…, TRANSPORT_LE, phy)`, API 26+); 0 keeps the platform default.
   *
   * A connect that reports failure (throws) leaves no replacement queued: the protocol dispatcher
   * treats the throw as the command's terminal, so a link opened behind it would be live and
   * unowned. A close that failed stays retained and retried; only this connect's intent is dropped.
   *
   * [attempt] identifies this request: it is retained for the generation this connect opens and
   * delivered with that generation's observations ([onConnectionOutcome]), so an observation of a
   * prior generation can never be taken for this connect's outcome.
   */
  fun connect(deviceId: String, autoConnect: Boolean, phyMask: Int = 0) {
    connect(deviceId, autoConnect, phyMask, null)
  }

  internal fun connect(
    deviceId: String,
    autoConnect: Boolean,
    phyMask: Int,
    attempt: GattConnectAttempt?
  ) {
    val key = deviceId.uppercase(Locale.ROOT)
    val adapterEpoch = synchronized(peerLifecycleLock) { adapterLossEpoch }
    connectAtEpoch(deviceId, key, autoConnect, phyMask, attempt, adapterEpoch)
  }

  private fun connectAtEpoch(
    deviceId: String, key: String, autoConnect: Boolean, phyMask: Int, attempt: GattConnectAttempt?, adapterEpoch: Long
  ) {
    runPeerLifecycle(key,
      direct = { connectNow(deviceId, key, autoConnect, phyMask, attempt, adapterEpoch) },
      deferred = {
        try { connectNow(deviceId, key, autoConnect, phyMask, attempt, adapterEpoch) }
        catch (error: Throwable) {
          // A deferred caller already returned, so its own failure must be delivered rather
          // than mistaken for the currently executing caller's result.
          if (attempt == null || !attempt.observed.get()) {
            val settlementError = errorOf {
              try { dispatchConnectionState(deviceId, false, BluetoothGatt.GATT_FAILURE, attempt) }
              finally { withdrawConnectionRoute(attempt) }
            }
            mergeErrors(error, settlementError)
          }
          throw error
        }
      }
    )
  }

  private fun connectNow(
    deviceId: String, key: String, autoConnect: Boolean, phyMask: Int, attempt: GattConnectAttempt?, adapterEpoch: Long
  ) {
    check(synchronized(peerLifecycleLock) { adapterEpoch == adapterLossEpoch }) { "Bluetooth adapter became unavailable" }
    check(synchronized(orderedSourceLock) { !graphPermitShutdown }) { "radio shutdown is pending" }
    val pinned = gatts[key]?.let { gatt ->
      deferPinnedOperation(gatt) {
        try { connectAtEpoch(deviceId, key, autoConnect, phyMask, attempt, adapterEpoch) }
        catch (error: Throwable) {
          try { dispatchConnectionState(deviceId, false, BluetoothGatt.GATT_FAILURE, attempt) }
          finally { withdrawConnectionRoute(attempt) }
          throw error
        }
      }
    } ?: PinnedOperation.NOT_PINNED
    if (pinned == PinnedOperation.QUEUED) return
    check(pinned != PinnedOperation.OVERFLOW) { "graph permit deferred operation capacity exhausted" }
    check(!pendingGattTeardowns.containsKey(key)) {
      "Android GATT cleanup is still pending for $deviceId"
    }
    val prior = gatts[key]
    if (prior == null) {
      openGatt(deviceId, key, autoConnect, phyMask, attempt, adapterEpoch)
      return
    }
    gattGenerationByInstance[prior]?.let { fenceRetiringGatt(key, prior, it) }
    // R3-F003: never close() before STATE_DISCONNECTED — disconnect prior and reconnect
    // from the DISCONNECTED callback (or safety timeout). Immediate close→connectGatt is a
    // common cause of GATT status 133 / flaky reconnect.
    val pendingError = errorOf { failPendingForDevice(key, "reconnect") }
    clearCharCacheForDevice(key)
    deviceQueues.remove(key)?.clear()
    discovered.remove(key)
    val queued = PendingConnect(autoConnect, phyMask, attempt, adapterEpoch)
    val displaced = pendingReconnect.put(key, queued)
    val displacedError = displaced?.let { previous ->
      errorOf {
        try { dispatchConnectionState(deviceId, false, BluetoothGatt.GATT_FAILURE, previous.attempt) }
        finally { withdrawConnectionRoute(previous.attempt) }
      }
    }
    val held = mergeErrors(pendingError, displacedError)
    if (held != null) pendingReconnect.remove(key, queued)
    val reconnectError = errorOf { reconnectOverPrior(deviceId, key, prior, queued, held != null) }
    if (reconnectError != null) pendingReconnect.remove(key, queued)
    mergeErrors(held, reconnectError)?.let { throw it }
  }

  /**
   * Opens [queued] once the [prior] generation is gone. Every error leaves through [connect],
   * which withdraws [queued]; a route that already knows it will throw withdraws it first so no
   * teardown callback can open the replacement behind the failure.
   */
  private fun reconnectOverPrior(deviceId: String, key: String, prior: BluetoothGatt, queued: PendingConnect, reportDisconnectError: Boolean) {
    // A prior without a generation was already torn down by its own callback.
    val joined = gattGenerationByInstance[prior]
      ?.let { generation -> joinGattDisconnect(key, prior, generation, null) }
      ?: GattDisconnectJoin(GattDisconnectJoin.Kind.REJECTED)
    var held = joined.heldError
    when (joined.kind) {
      GattDisconnectJoin.Kind.REJECTED, GattDisconnectJoin.Kind.TORN_DOWN -> {
        // The prior generation's own teardown ran before this call could join it (or before a
        // refused deadline could close it), so there is no disconnect to request on a closed GATT.
        // If that teardown already opened the queued reconnect the entry is gone; otherwise
        // re-evaluate against what the radio holds now (a retained close failure is refused here).
        held?.let { throw it }
        if (pendingReconnect.remove(key, queued)) connectNow(deviceId, key, queued.autoConnect, queued.phyMask, queued.attempt, queued.adapterEpoch)
        return
      }
      GattDisconnectJoin.Kind.FORCED_CLOSE -> {
        // No deadline could be armed, so the prior generation was closed right here: there is no
        // native DISCONNECTED to wait for and no disconnect to request on a closed GATT.
        val closed = checkNotNull(joined.forcedClose)
        held = mergeErrors(held, closed.callbackError)
        closed.failure?.let { failure ->
          // The retained close is retried by its own owner; this connect's intent is not kept.
          throw IllegalStateException("Android GATT cleanup failed before reconnect", failure.throwable)
            .also { error -> held?.let { error.addSuppressed(it) } }
        }
        held?.let { throw it }
        if (pendingReconnect.remove(key, queued)) {
          openGatt(deviceId, key, queued.autoConnect, queued.phyMask, queued.attempt, queued.adapterEpoch)
        }
        return
      }
      GattDisconnectJoin.Kind.JOINED -> {
        // The existing owner already requested native disconnect. Keep this reconnect queued
        // for that owner's terminal result instead of issuing another native request.
        held?.let {
          pendingReconnect.remove(key, queued)
          throw it
        }
        return
      }
      GattDisconnectJoin.Kind.STARTED -> Unit
    }
    // A superseded waiter's error is surfaced after the disconnect is requested, but it already
    // decides this connect fails: withdraw first so the native DISCONNECTED cannot open the link.
    if (held != null) pendingReconnect.remove(key, queued)
    try {
      prior.disconnect()
    } catch (t: Throwable) {
      OwnedAndroidLog.e("connect disconnect prior gatt", t)
      val teardown = completeGattTeardown(key, prior)
      held = mergeErrors(held, teardown.callbackError)
      teardown.failure?.let { failure ->
        throw IllegalStateException("Android GATT cleanup failed before reconnect", failure.throwable)
          .also { error ->
            held?.let { error.addSuppressed(it) }
            error.addSuppressed(t)
          }
      }
      if (reportDisconnectError) held = mergeErrors(held, t)
      held?.let { throw it }
      // The prior's own native DISCONNECTED may have been delivered inside the refused
      // disconnect() and already opened the queued reconnect: consume it atomically, once.
      if (pendingReconnect.remove(key, queued)) {
        openGatt(deviceId, key, queued.autoConnect, queued.phyMask, queued.attempt, queued.adapterEpoch)
      }
      return
    }
    held?.let { throw it }
  }

  internal fun disconnect(
    deviceId: String,
    onComplete: (OwnedRadioTeardownFailure?) -> Unit = {}
  ): OwnedRadioTeardownFailure? = disconnectOwned(deviceId, null, onComplete)

  internal fun disconnectOwned(deviceId: String, owner: Any?, onComplete: (OwnedRadioTeardownFailure?) -> Unit): OwnedRadioTeardownFailure? {
    val key = deviceId.uppercase(Locale.ROOT)
    var result: OwnedRadioTeardownFailure? = null
    runPeerLifecycle(key, { result = disconnectNow(deviceId, key, onComplete, owner) })
    return result
  }

  internal fun cancelConnect(
    deviceId: String, attempt: GattConnectAttempt, onComplete: (OwnedRadioTeardownFailure?) -> Unit
  ) {
    val key = deviceId.uppercase(Locale.ROOT)
    val owner = synchronized(receiverRetirementLock) { connectionRoutes[attempt]?.owner }
    runPeerLifecycle(key, {
      val queued = pendingReconnect[key]
      if (queued?.attempt === attempt && pendingReconnect.remove(key, queued)) {
        val error = errorOf {
          try { dispatchConnectionState(deviceId, false, BluetoothGatt.GATT_FAILURE, attempt) }
          finally { withdrawConnectionRoute(attempt) }
        }
        val completionError = errorOf { onComplete(null) }
        mergeErrors(error, completionError)?.let { throw it }
      } else {
        val generation = gatts[key]?.let { gattGenerationByInstance[it] }
        if (generation != null && gattConnectAttempts[generation] === attempt) {
          val gatt = checkNotNull(gatts[key])
          when (deferPinnedOperation(gatt) { cancelConnect(deviceId, attempt, onComplete) }) {
            PinnedOperation.QUEUED -> Unit
            PinnedOperation.OVERFLOW -> {
              val cause = IllegalStateException("graph permit deferred operation capacity exhausted")
              onComplete(OwnedRadioTeardownFailure("graphPermit.cancelConnect",
                AndroidGattNotSubmitted(checkNotNull(cause.message), cause)))
            }
            PinnedOperation.NOT_PINNED -> disconnectAfterCancellingReconnect(deviceId, key, onComplete, owner)
          }
        } else onComplete(null)
      }
    }, admissionRefused = {
      onComplete(OwnedRadioTeardownFailure("peerLifecycle.cancelConnect", AndroidGattNotSubmitted(it.message ?: it.javaClass.simpleName, it)))
    })
  }

  private fun disconnectNow(
    deviceId: String, key: String, onComplete: (OwnedRadioTeardownFailure?) -> Unit, owner: Any?
  ): OwnedRadioTeardownFailure? {
    val pinned = gatts[key]?.let { gatt -> deferPinnedOperation(gatt) { disconnectOwned(deviceId, owner, onComplete) } }
      ?: PinnedOperation.NOT_PINNED
    if (pinned == PinnedOperation.QUEUED) return null
    if (pinned == PinnedOperation.OVERFLOW) {
      val failure = OwnedRadioTeardownFailure("graphPermit.disconnect", IllegalStateException("graph permit deferred operation capacity exhausted"))
      onComplete(failure)
      return failure
    }
    val cancelled = pendingReconnect.remove(key)
    val cancelledError = cancelled?.let { pending ->
      errorOf {
        try { dispatchConnectionState(deviceId, false, BluetoothGatt.GATT_FAILURE, pending.attempt) }
        finally { withdrawConnectionRoute(pending.attempt) }
      }
    }
    var result: OwnedRadioTeardownFailure? = null
    val disconnectError = errorOf { result = disconnectAfterCancellingReconnect(deviceId, key, onComplete, owner) }
    mergeErrors(cancelledError, disconnectError)?.let { throw it }
    return result
  }

  private fun disconnectAfterCancellingReconnect(
    deviceId: String, key: String, onComplete: (OwnedRadioTeardownFailure?) -> Unit, owner: Any? = null
  ): OwnedRadioTeardownFailure? {
    val scopeGeneration = gatts[key]?.let { gattGenerationByInstance[it] } ?: pendingGattTeardowns[key]?.generation
    gatts[key]?.let { gatt -> gattGenerationByInstance[gatt]?.let { fenceRetiringGatt(key, gatt, it) } }
    var heldError = errorOf { failPendingForDevice(key, "disconnected") }
    clearCharCacheForDevice(key)
    deviceQueues.remove(key)?.clear()
    val cleanupFailure = scopeGeneration?.let { generation ->
      combineCleanupFailures(retryCleanupLedger(CleanupScope(key, generation, owner)) { heldError = mergeErrors(heldError, it) })
    }
    val g = gatts[key]
    val generation = g?.let { gattGenerationByInstance[it] }
    if (g == null || generation == null) {
      discovered.remove(key)
      val teardown = retryGattTeardown(key)
      return deliverDisconnectResult(combineCleanupFailures(listOfNotNull(cleanupFailure, teardown.failure)), mergeErrors(heldError, teardown.callbackError), onComplete)
    }
    if (pendingGattTeardowns[key] != null) {
      val teardown = retryGattTeardown(key)
      return deliverDisconnectResult(combineCleanupFailures(listOfNotNull(cleanupFailure, teardown.failure)), mergeErrors(heldError, teardown.callbackError), onComplete)
    }
    if (requiresImmediateGattTeardownOnAdapterState(adapter?.state)) {
      val teardown = completeGattTeardown(key, g)
      return deliverDisconnectResult(combineCleanupFailures(listOfNotNull(cleanupFailure, teardown.failure)), mergeErrors(heldError, teardown.callbackError), onComplete)
    }
    // R3-F003: only disconnect(); close() waits for STATE_DISCONNECTED (or the one safety deadline).
    // Each caller retains its own cleanup result until the shared close settles.
    val joined = joinGattDisconnect(key, g, generation) { closeFailure ->
      onComplete(combineCleanupFailures(listOfNotNull(cleanupFailure, closeFailure)))
    }
    when (joined.kind) {
      GattDisconnectJoin.Kind.REJECTED -> {
        // The generation's own teardown (native DISCONNECTED, deadline, adapter loss) ran
        // between the read above and the join, so no owner exists to settle this caller.
        // Its outcome is that exact generation's: a retained close failure is retried and
        // reported; otherwise the close was clean. A GATT opened since is not this call's.
        val retained = pendingGattTeardowns[key]
        val teardown =
          if (retained != null && retained.gatt === g && retained.generation == generation) retryGattTeardown(key)
          else GattTeardown(null)
        return deliverDisconnectResult(combineCleanupFailures(listOfNotNull(cleanupFailure, teardown.failure)), mergeErrors(heldError, teardown.callbackError), onComplete)
      }
      GattDisconnectJoin.Kind.FORCED_CLOSE -> {
        // No deadline could be armed, so the generation was closed explicitly instead of
        // waiting for a callback nothing bounds. It is closed: no native disconnect follows.
        val teardown = checkNotNull(joined.forcedClose)
        return deliverDisconnectResult(
          combineCleanupFailures(listOfNotNull(cleanupFailure, teardown.failure)),
          mergeErrors(heldError, mergeErrors(joined.heldError, teardown.callbackError)),
          null
        )
      }
      GattDisconnectJoin.Kind.STARTED -> {
        try {
          g.disconnect()
        } catch (t: Throwable) {
          OwnedAndroidLog.e("disconnect", t)
          // No disconnect was accepted, so no DISCONNECTED will arrive: always close and settle
          // the owner. A ledger failure only wins the result; it never skips the physical close.
          val teardown = completeGattTeardown(key, g)
          return deliverDisconnectResult(
            combineCleanupFailures(listOfNotNull(cleanupFailure, teardown.failure)),
            mergeErrors(heldError, mergeErrors(joined.heldError, teardown.callbackError)),
            null
          )
        }
      }
      // TORN_DOWN: another path closed the generation and already settled the recorded waiter.
      GattDisconnectJoin.Kind.JOINED, GattDisconnectJoin.Kind.TORN_DOWN -> Unit
    }
    return deliverDisconnectResult(cleanupFailure, mergeErrors(heldError, joined.heldError), null)
  }

  /**
   * Hands this caller its own result, then rethrows an error a waiter or observer threw while
   * the teardown was settled: the caller is never starved by it and the error is never lost.
   */
  private fun deliverDisconnectResult(
    failure: OwnedRadioTeardownFailure?,
    heldError: Throwable?,
    onComplete: ((OwnedRadioTeardownFailure?) -> Unit)?
  ): OwnedRadioTeardownFailure? {
    val completionError = onComplete?.let { callback -> errorOf { callback(failure) } }
    mergeErrors(heldError, completionError)?.let { throw it }
    return failure
  }

  private fun openGatt(
    deviceId: String,
    key: String,
    autoConnect: Boolean,
    phyMask: Int,
    attempt: GattConnectAttempt?,
    adapterEpoch: Long,
    onAdmitted: () -> Unit = {}
  ) {
    val lane = synchronized(orderedSourceLock) {
      check(!graphPermitShutdown) { "radio shutdown is pending" }
      synchronized(peerLifecycleLock) {
        checkNotNull(peerLifecycleQueues[key]).also {
          check(it.owner === Thread.currentThread() && !it.opening)
          check(adapterEpoch == adapterLossEpoch) { "Bluetooth adapter became unavailable" }
          it.opening = true
          it.adapterLost = false
        }
      }
    }
    var held = errorOf {
      openGattInLane(deviceId, key, autoConnect, phyMask, attempt, onAdmitted)
    }
    val interruption = synchronized(orderedSourceLock) {
      synchronized(peerLifecycleLock) {
        val reason = when {
          graphPermitShutdown -> "radio shutdown interrupted native GATT open"
          lane.adapterLost -> "Bluetooth adapter became unavailable during native GATT open"
          else -> null
        }
        if (reason == null) lane.opening = false
        reason
      }
    }
    if (interruption != null) {
      held = mergeErrors(held, IllegalStateException(interruption))
      val gatt = gatts[key]
      if (gatt != null) {
        val closeError = errorOf {
          val teardown = completeGattTeardown(key, gatt)
          held = mergeErrors(held, teardown.callbackError)
          held = mergeErrors(held, teardown.failure?.throwable)
        }
        held = mergeErrors(held, closeError)
      }
      synchronized(peerLifecycleLock) { lane.opening = false }
    }
    held?.let { throw it }
  }

  private fun openGattInLane(
    deviceId: String,
    key: String,
    autoConnect: Boolean,
    phyMask: Int,
    attempt: GattConnectAttempt?,
    onAdmitted: () -> Unit
  ) {
    pendingReconnect.remove(key)
    connectedEncryptionGenerations.remove(key)
    val a = adapter ?: throw IllegalStateException("Bluetooth adapter unavailable")
    val device = try {
      // CDM MacAddress.toString() can be lowercase; key is the canonical adapter address.
      a.getRemoteDevice(key)
    } catch (throwable: Exception) {
      throw IllegalStateException("Android rejected Bluetooth device $deviceId", throwable)
    }
    val openingCallback = OpeningGattCallback(attempt)
    val gatt = try {
      val opened = when {
        phyMask == 0 -> device.connectGatt(context, autoConnect, openingCallback, BluetoothDevice.TRANSPORT_LE)
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.O ->
          device.connectGatt(context, autoConnect, openingCallback, BluetoothDevice.TRANSPORT_LE, phyMask)
        else -> throw UnsupportedOperationException("connectGatt with a PHY preference needs API 26")
      }
      opened ?: throw IllegalStateException("Android returned no BluetoothGatt for $deviceId")
    } catch (throwable: Throwable) {
      val cleanupError = errorOf {
        openingCallback.reject().forEachSettled { observed ->
          val generation = nextGattGeneration.getAndIncrement()
          val operation = "closeUnpublishedGatt:$key:$generation"
          val cleanup = {
            try { observed.close(); null }
            catch (error: Throwable) { OwnedRadioTeardownFailure(operation, error) }
          }
          val failure = cleanup()
          if (failure != null) {
            registerRetryableCleanup(operation, CleanupScope(key, generation, synchronized(receiverRetirementLock) { attempt?.let { connectionRoutes[it]?.owner } }), allowUnpublished = true, cleanup = cleanup)
            val observerError = errorOf { reportCleanupFailure(failure) }
            observerError?.let { failure.throwable.addSuppressed(it) }
            throw failure.throwable
          }
        }
      }
      if (cleanupError != null && cleanupError !== throwable) throwable.addSuppressed(cleanupError)
      if (throwable is Exception) throw IllegalStateException("Android could not open BluetoothGatt for $deviceId", throwable)
      throw throwable
    }
    val generation = nextGattGeneration.getAndIncrement()
    effectiveMtuByDevice.remove(key)
    attempt?.let { gattConnectAttempts[generation] = it }
    gatts[key] = gatt
    gattGenerations[key] = generation
    gattGenerationByInstance[gatt] = generation
    val interrupted = synchronized(orderedSourceLock) {
      graphPermitShutdown || synchronized(peerLifecycleLock) { checkNotNull(peerLifecycleQueues[key]).adapterLost }
    }
    if (interrupted) {
      openingCallback.reject()
      throw IllegalStateException("native GATT open was interrupted")
    }
    onAdmitted()
    try {
      openingCallback.publish(gatt)
    } catch (error: Throwable) {
      // The callback is now delivered on this opening call's stack. If an observer throws,
      // do not report a failed open while leaving its native generation live and unowned.
      var held: Throwable? = error
      // The owner may already have observed success even when a different observer threw.
      // Publish a terminal for this exact generation before withdrawing its route.
      held = mergeErrors(held, errorOf {
        dispatchConnectionState(deviceId, false, BluetoothGatt.GATT_FAILURE, attempt)
      })
      held = mergeErrors(held, errorOf {
        val teardown = completeGattTeardown(key, gatt, generation)
        held = mergeErrors(held, teardown.callbackError)
        held = mergeErrors(held, teardown.failure?.throwable)
      })
      throw checkNotNull(held)
    }
  }

  /** Preserve the other generation's waiters and refuse an unfenced native close. */
  private fun rejectMismatchedCloseOwner(key: String, generation: Long): GattTeardown {
    val failure = OwnedRadioTeardownFailure(
      "closeGattOwner:$key:generation=$generation",
      IllegalStateException("Native close cannot replace a different disconnect owner")
    )
    return GattTeardown(failure, errorOf { reportCleanupFailure(failure) })
  }


  /**
   * Force-close [gatt] and drop local state. Cancels any pending safety timeout
   * and retires the generation's disconnect owner, completing every waiter once
   * with the close result. Safe to call from DISCONNECTED callback or after a
   * failed disconnect.
   *
   * Never throws for a waiter or observer error: the physical close, the retained
   * ownership and every waiter's settlement all happen first, and the error comes
   * back in [GattTeardown.callbackError] for the caller to rethrow.
   */

  private fun completeGattTeardown(key: String, gatt: BluetoothGatt): GattTeardown {
    val generation = gattGenerationByInstance[gatt] ?: return GattTeardown(null)
    return completeGattTeardown(key, gatt, generation)
  }

  private fun completeGattTeardown(
    key: String,
    gatt: BluetoothGatt,
    generation: Long,
    closingMarked: Boolean = false
  ): GattTeardown {
    val pinDecision = synchronized(orderedSourceLock) {
      val decision = deferPinnedTeardown(gatt) {
        completeGattTeardown(key, gatt, generation, closingMarked).thenWhenClean {
          resumeQueuedReconnect(key, key, "reconnect after deferred teardown")
        }
      }
      if (decision == PinnedOperation.NOT_PINNED) removeNativeGraph(gatt)
      decision
    }
    when (pinDecision) {
      PinnedOperation.QUEUED -> return GattTeardown(null, inProgress = true)
      PinnedOperation.OVERFLOW -> return GattTeardown(OwnedRadioTeardownFailure("graphPermit.teardown", IllegalStateException("graph permit deferred operation capacity exhausted")))
      PinnedOperation.NOT_PINNED -> Unit
    }
    if (gatts[key] !== gatt || gattGenerations[key] != generation || gattGenerationByInstance[gatt] != generation) {
      return GattTeardown(null)
    }
    fenceRetiringGatt(key, gatt, generation)
    if (!closingMarked) {
      when (disconnectOwners.markClosing(key, gatt, generation)) {
        AndroidGattDisconnectOwners.ClosingMark.ALREADY_CLOSING ->
          return GattTeardown(null, inProgress = true)
        AndroidGattDisconnectOwners.ClosingMark.OWNER_MISMATCH ->
          return rejectMismatchedCloseOwner(key, generation)
        AndroidGattDisconnectOwners.ClosingMark.MARKED -> Unit
      }
    }
    gattTeardownsInFlight.incrementAndGet()
    try {
      val orderedWakeError = errorOf { invalidateOrderedSource(gatt, OrderedNotificationSource.Reason.CONNECTION_LOST) }
      cancelSafeClose(key)
      // No callback saved by a port can begin after native close starts.
      withdrawConnectionRoute(gattConnectAttempts[generation])
      try {
        gatt.close()
      } catch (throwable: Exception) {
        OwnedAndroidLog.e("completeGattTeardown close for $key", throwable)
        val failure = OwnedRadioTeardownFailure("closeGatt:$key:generation=$generation", throwable)
        pendingGattTeardowns[key] = GattTeardownOwner(gatt, generation)
        // Retire only after native close returns so reentrant joiners share this result.
        val waiters = disconnectOwners.retire(key, gatt, generation)
        // Waiters are settled before the diagnostic observer runs, and the observer runs even if
        // a waiter threw: neither can starve the other, and neither error is dropped.
        val settleError = settleDisconnectWaiters(waiters, failure)
        val observerError = errorOf { reportCleanupFailure(failure) }
        orderedWakeError?.let { reportOrderedWakeFailure("orderedSource.disconnectWake", it) }
        return GattTeardown(failure, mergeErrors(settleError, observerError))
      }
      pendingGattTeardowns.remove(key, GattTeardownOwner(gatt, generation))
      gatts.remove(key, gatt)
      synchronized(callbackOwnerLock) {
        gattGenerations.remove(key, generation)
        gattGenerationByInstance.remove(gatt, generation)
        retireClosedGenerationCleanups(key, generation)
      }
      retireExactCallbackOwners(gatt)
      retiringGatts[key]?.takeIf { it.gatt === gatt && it.generation == generation }?.let { retiringGatts.remove(key, it) }
      gattConnectAttempts.remove(generation)
      discovered.remove(key)
      effectiveMtuByDevice[key]?.let { state ->
        if (state.gattGeneration == generation) effectiveMtuByDevice.remove(key, state)
      }
      activeNativeSubscriptionOwnership.clearDevice(key)
      clearCharCacheForDevice(key)
      deviceQueues.remove(key)?.clear()
      // State is now closed, so any joiner from here on is rejected. Reentrant joiners
      // admitted during close are retired here and receive the same result exactly once.
      val waiters = disconnectOwners.retire(key, gatt, generation)
      val settleError = settleDisconnectWaiters(waiters, null)
      orderedWakeError?.let { reportOrderedWakeFailure("orderedSource.disconnectWake", it) }
      return GattTeardown(null, settleError)
    } finally {
      gattTeardownsInFlight.decrementAndGet()
    }
  }

  /**
   * Joins the disconnect owner of [gatt]'s [generation]. Returns true when this
   * call created the owner: that caller alone requests the native disconnect and
   * the single close deadline is armed here, so later callers share it instead of
   * moving it. A [waiter] receives exactly one terminal result, except when the
   * result is [GattDisconnectJoin.REJECTED]: the waiter was not recorded and the
   * caller settles it. The generation must still be current and not retained for
   * a failed close, checked atomically with admission.
   */
  private fun joinGattDisconnect(
    key: String,
    gatt: BluetoothGatt,
    generation: Long,
    waiter: ((OwnedRadioTeardownFailure?) -> Unit)?
  ): GattDisconnectJoin {
    val admission = disconnectOwners.join(key, gatt, generation, waiter) {
      isCurrentGatt(key, gatt, generation) && !pendingGattTeardowns.containsKey(key)
    }
    if (admission.rejected) return GattDisconnectJoin(GattDisconnectJoin.Kind.REJECTED)
    // A superseded waiter that throws is held, not thrown: the new owner still gets its
    // deadline (or explicit close) below, so it cannot be left without one.
    val supersededError =
      if (admission.superseded.isEmpty()) null
      else settleDisconnectWaiters(
        admission.superseded,
        OwnedRadioTeardownFailure(
          "disconnectSuperseded:$key",
          IllegalStateException("Android GATT generation was replaced before its disconnect completed")
        )
      )
    if (!admission.created) return GattDisconnectJoin(GattDisconnectJoin.Kind.JOINED, supersededError)
    val refusal = scheduleSafeClose(key, gatt, admission.token)
      ?: return GattDisconnectJoin(GattDisconnectJoin.Kind.STARTED, supersededError)
    // A scheduler that refused by returning false has no error of its own to surface.
    val schedulerError = refusal.takeUnless { it is DeadlineRefused }
    // The same fence the deadline body uses: if the generation was torn down (or replaced) between
    // the join and this refusal, its teardown retired the owner and settled the waiter, and the
    // replacement's connection loss and pending operations are not this owner's to publish.
    if (!disconnectOwners.isCurrent(key, admission.token) || !isCurrentGatt(key, gatt, generation)) {
      return GattDisconnectJoin(GattDisconnectJoin.Kind.TORN_DOWN, mergeErrors(supersededError, schedulerError))
    }
    // Nothing bounds the native DISCONNECTED callback, so the owner must not wait for it: close the
    // generation now. That retires the owner and settles its waiters with the real close outcome
    // (a failed close stays retained and reported, never a release).
    OwnedAndroidLog.e("GATT close deadline could not be armed for $key; closing without waiting", refusal)
    val closed = forceCloseWithoutDisconnectedCallback(key, gatt)
    return GattDisconnectJoin(
      GattDisconnectJoin.Kind.FORCED_CLOSE,
      mergeErrors(supersededError, schedulerError),
      closed
    )
  }

  /**
   * Completes every waiter once; one throwing waiter cannot starve the others.
   * Returns the first error (later ones suppressed) for the caller to rethrow once
   * all of its own cleanup is done.
   */
  private fun settleDisconnectWaiters(
    waiters: List<(OwnedRadioTeardownFailure?) -> Unit>,
    failure: OwnedRadioTeardownFailure?
  ): Throwable? {
    var held: Throwable? = null
    waiters.forEach { waiter -> held = mergeErrors(held, errorOf { waiter(failure) }) }
    return held
  }

  /**
   * Arms the one close deadline. Returns null when armed; otherwise why it could not
   * be: the scheduler's own error, or [DeadlineRefused] when it returned false. A
   * deadline that is not armed leaves nothing registered, and the caller must not wait.
   */
  private fun scheduleSafeClose(key: String, gatt: BluetoothGatt, token: Long): Throwable? {
    cancelSafeClose(key)
    val r =
      Runnable {
        // A deadline whose owner is gone (closed, replaced, discarded) must not act
        // on whichever GATT the key names now.
        if (disconnectOwners.isCurrent(key, token) && gatts[key] === gatt) {
          OwnedAndroidLog.i("GATT close safety timeout; cleanup callback absent, forced close proceeding for $key")
          // If a reconnect was queued, attempt it after a clean forced close, even when a
          // waiter or observer threw while the close was settled.
          forceCloseWithoutDisconnectedCallback(key, gatt)
            .thenWhenClean { resumeQueuedReconnect(key, key, "reconnect after close timeout") }
        }
      }
    closeTimeouts[key] = r
    val scheduled = try {
      scheduleDelayed?.invoke(GATT_CLOSE_TIMEOUT_MS) { r.run() }
        ?: mainHandler?.postDelayed(r, GATT_CLOSE_TIMEOUT_MS)
        ?: false
    } catch (throwable: Throwable) {
      closeTimeouts.remove(key, r)
      return throwable
    }
    if (scheduled) return null
    closeTimeouts.remove(key, r)
    return DeadlineRefused(key)
  }

  /**
   * Closes [gatt] without a native DISCONNECTED callback: publishes the connection loss,
   * fails pending operations and tears the generation down, running every step even if an
   * observer throws in an earlier one so the owner cannot be left without a terminal result.
   */
  private fun forceCloseWithoutDisconnectedCallback(key: String, gatt: BluetoothGatt): GattTeardown {
    when (deferPinnedTeardown(gatt) {
      forceCloseWithoutDisconnectedCallback(key, gatt).thenWhenClean {
        resumeQueuedReconnect(key, key, "reconnect after deferred close timeout")
      }
    }) {
      PinnedOperation.QUEUED -> return GattTeardown(null, inProgress = true)
      PinnedOperation.OVERFLOW -> return GattTeardown(OwnedRadioTeardownFailure("graphPermit.forceClose", IllegalStateException("graph permit deferred operation capacity exhausted")))
      PinnedOperation.NOT_PINNED -> Unit
    }
    val generation = gattGenerationByInstance[gatt] ?: return GattTeardown(null)
    fenceRetiringGatt(key, gatt, generation)
    when (disconnectOwners.markClosing(key, gatt, generation)) {
      AndroidGattDisconnectOwners.ClosingMark.ALREADY_CLOSING ->
        return GattTeardown(null, inProgress = true)
      AndroidGattDisconnectOwners.ClosingMark.OWNER_MISMATCH ->
        return rejectMismatchedCloseOwner(key, generation)
      AndroidGattDisconnectOwners.ClosingMark.MARKED -> Unit
    }
    val observerError = runEvery(
      {
        val attempt = gattGenerationByInstance[gatt]?.let { generation -> gattConnectAttempts[generation] }
        dispatchConnectionState(key, false, BluetoothGatt.GATT_FAILURE, attempt)
      },
      { failPendingForDevice(key, "disconnected timeout") }
    )
    val teardownFailure = completeGattTeardown(key, gatt, generation, closingMarked = true)
    return teardownFailure.withCallbackError(observerError)
  }

  /** Opens the reconnect that was queued behind a clean prior teardown, exactly once. */
  private fun resumeQueuedReconnect(deviceId: String, key: String, failureLabel: String) {
    runPeerLifecycle(key, { resumeQueuedReconnectNow(deviceId, key, failureLabel) })
  }

  private fun resumeQueuedReconnectNow(deviceId: String, key: String, failureLabel: String) {
    pendingReconnect.remove(key)?.let { pending ->
      var admitted = false
      try {
        openGatt(deviceId, key, pending.autoConnect, pending.phyMask, pending.attempt, pending.adapterEpoch) { admitted = true }
      } catch (t: Throwable) {
        if (admitted) throw t
        OwnedAndroidLog.e(failureLabel, t)
        // The replacement failed to open: this is its own connect's failure, not the prior's.
        try { dispatchConnectionState(deviceId, false, BluetoothGatt.GATT_FAILURE, pending.attempt) }
        finally { withdrawConnectionRoute(pending.attempt) }
      }
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
    scope: CleanupScope,
    allowUnpublished: Boolean = false,
    cleanup: () -> OwnedRadioTeardownFailure?
  ) {
    synchronized(callbackOwnerLock) {
      if (allowUnpublished || gattGenerations[scope.peer] == scope.generation) {
        retryableCleanups.putIfAbsent(key, RetryableCleanup(scope, cleanup))
      } else {
        retireCleanupMarkers(key, scope.generation)
      }
    }
  }

  /** A successful close releases this attachment's native obligations, across all owners. */
  private fun retireClosedGenerationCleanups(peer: String, generation: Long) {
    retryableCleanups.entries.toList().forEach { entry ->
      if (entry.value.scope.peer == peer && entry.value.scope.generation == generation &&
        retryableCleanups.remove(entry.key, entry.value)) {
        retireCleanupMarkers(entry.key, generation)
      }
    }
  }

  private fun retireCleanupMarkers(operation: String, generation: Long) {
    pendingCccdCompensations.entries.toList().forEach { (characteristic, operations) ->
      if (operation in operations) releaseCccdCompensation(characteristic, operation)
    }
    localOnlyCccdCompensations.entries.toList().forEach { (characteristic, marker) ->
      if (marker.operation == operation && marker.attachment == generation) {
        localOnlyCccdCompensations.remove(characteristic, marker)
      }
    }
  }

  private fun retryGattTeardown(key: String): GattTeardown {
    val owner = pendingGattTeardowns[key] ?: return GattTeardown(null)
    return completeGattTeardown(key, owner.gatt, owner.generation)
  }

  private fun combineCleanupFailures(failures: List<OwnedRadioTeardownFailure>): OwnedRadioTeardownFailure? {
    val first = failures.firstOrNull() ?: return null
    if (failures.size == 1) return first
    val combined = IllegalStateException(first.throwable.message, first.throwable)
    failures.drop(1).forEach { if (it.throwable !== first.throwable) combined.addSuppressed(it.throwable) }
    return OwnedRadioTeardownFailure(first.operation, combined)
  }

  private fun retryCleanupLedger(scope: CleanupScope? = null, onObserverError: (Throwable) -> Unit = { throw it }): List<OwnedRadioTeardownFailure> {
    val failures = mutableListOf<OwnedRadioTeardownFailure>()
    retryableCleanups.entries.toList().filter { scope == null || it.value.scope.matches(scope) }.forEach { entry ->
      val failure = try {
        entry.value()
      } catch (throwable: Throwable) {
        OwnedRadioTeardownFailure(entry.key, throwable)
      }
      if (failure == null) {
        retryableCleanups.remove(entry.key, entry.value)
      } else {
        failures.add(failure)
        errorOf { reportCleanupFailure(failure) }?.let(onObserverError)
      }
    }
    return failures
  }

  /**
   * Retries every retained cleanup without leaking Kotlin's default-argument
   * bridge into lifecycle callers.  It also gives the service-change retry
   * path one stable, zero-argument operation rather than asking it to
   * reconstruct a scope from a replacement GATT generation.
   */
  private fun retryCleanupLedger(): List<OwnedRadioTeardownFailure> = retryCleanupLedger(null)

  /**
   * Service discovery. [onDone] receives the failure itself, never just a
   * flag: a link that dropped while discovery was pending or queued is an
   * [AndroidGattLinkLost] (with the disconnect status), as for every other
   * operation, so the caller reports a link loss and not a discovery fault.
   */
  fun discover(deviceId: String, onDone: (Result<Unit>) -> Unit): Long {
    val key = "discover:${deviceId.uppercase()}"
    // Capture before queue admission: the pump and the platform callback can run
    // on different threads, and a shared radio has no implicit current owner.
    val work = synchronized(orderedSourceLock) {
      val owner = activeGraphOwner()
      check(!graphStore(owner).fenced) { "graph owner is retiring" }
      DiscoveryWork(owner).also { discoveryWork.add(it) }
    }
    return enqueue(
      deviceId,
      onCancelled = {
        synchronized(orderedSourceLock) {
          work.valid = false
          work.callback?.let { pending.remove(key, it) }
        }
        try { onDone(Result.failure(IllegalStateException("Android GATT service discovery was cancelled"))) }
        finally { synchronized(orderedSourceLock) { work.queued = false; finishDiscovery(work) } }
      },
      onStartFailure = { failure ->
        synchronized(orderedSourceLock) { work.valid = false; work.callback?.let { pending.remove(key, it) } }
        try { onDone(Result.failure(failure)) }
        finally { synchronized(orderedSourceLock) { work.queued = false; finishDiscovery(work) } }
      },
      settleOnClearBeforeOwner = true
    ) { token, done ->
      val gatt = gatts[deviceId.uppercase()]
      if (gatt == null) {
        try { completeExactUnitDirect(onDone, done, Result.failure(IllegalStateException("Android GATT service discovery has no GATT for $deviceId")), token) }
        finally { synchronized(orderedSourceLock) { work.queued = false; finishDiscovery(work) } }
        return@enqueue
      }
      val callback: (Result<ByteArray?>) -> Unit = { result ->
        try { if (token.markPubliclySettled()) onDone(result.map { }) }
        finally {
          try { done() } finally { synchronized(orderedSourceLock) { work.queued = false; finishDiscovery(work) } }
        }
      }
      val admitted = token.registerOwner { synchronized(orderedSourceLock) {
        check(isCurrentGattCallback(gatt))
        if (graphStore(work.owner).fenced || gatt in nativeDiscoveryAdmissions) false else {
          work.gatt = gatt
          work.callback = callback
          nativeDiscoveryAdmissions[gatt] = work
          pending[key] = callback
          true
        }
      } }
      if (!admitted) {
        try { completeExactUnitDirect(onDone, done, Result.failure(AndroidGattNotSubmitted("discovery owner fenced or previous callback remains pending")), token) }
        finally { synchronized(orderedSourceLock) { work.queued = false; finishDiscovery(work) } }
        return@enqueue
      }
      synchronized(orderedSourceLock) { work.physicalRunning = true }
      try {
        val invoked = token.invokePhysical(done) {
          if (!gatt.discoverServices()) {
            synchronized(orderedSourceLock) {
              nativeDiscoveryAdmissions.remove(gatt, work)
              work.gatt = null
              pending.remove(key, callback)
            }
            callback(Result.failure(IllegalStateException("BluetoothGatt.discoverServices() returned false")))
          }
        }
        if (!invoked) {
          synchronized(orderedSourceLock) {
            nativeDiscoveryAdmissions.remove(gatt, work)
            work.gatt = null
            pending.remove(key, callback)
            finishDiscovery(work)
          }
          done()
        }
      } catch (error: Throwable) {
        // A throwing platform invocation may have submitted: keep callback debt.
        synchronized(orderedSourceLock) { work.valid = false }
        throw error
      } finally {
        synchronized(orderedSourceLock) { work.physicalRunning = false; finishDiscovery(work) }
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
      onStartFailure = { error -> onResult(Result.failure(error)) },
      settleOnClearBeforeOwner = true
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
      if (!registerCallbackOwner(token, gatt) { exactReadPending.putIfAbsent(characteristic, pending) == null }) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact characteristic read is pending or callback owner capacity is exhausted")),
          token
        )
        return@enqueue
      }
      if (!token.invokePhysical(done) {
        if (!gatt.readCharacteristic(characteristic) && exactReadPending.remove(characteristic, pending)) {
          completeExactByte(pending, Result.failure(IllegalStateException("readCharacteristic failed to start")))
        }
      }) {
        if (exactReadPending.remove(characteristic, pending)) {
          completeExactByte(pending, Result.failure(token.beforePhysicalFailure()))
        } else done()
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
      if (hasAmbiguousMarkerWrite(deviceId, gatt, ch)) {
        completeExactByteDirect(onResult, done,
          Result.failure(AndroidGattNotSubmitted("characteristic write blocked by ambiguous marker write")), token)
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

  /** Marker admission and the Android write start share the source gate. */
  @Suppress("DEPRECATION")
  internal fun writeAfterOrderedMarker(
    deviceId: String, path: OwnedAndroidNotificationPath, value: ByteArray, withResponse: Boolean,
    markerId: ULong, admission: AndroidOrderedMarkerAdmission, transactions: OrderedMarkerTransactions,
    cancellation: OrderedMarkerCancellation, onResult: (OrderedMarkerTransactions.State) -> Unit
  ): Long {
    val published = AtomicBoolean(false)
    fun publish(state: OrderedMarkerTransactions.State) { if (published.compareAndSet(false, true)) onResult(state) }
    val bytes = value.copyOf()
    return enqueue(deviceId,
      onCancelled = { publish(transactions.cancel(markerId, OrderedMarkerCancelled())) },
      onStartFailure = { error -> transactions.driverSettled(markerId); publish(transactions.cancel(markerId, error)) },
      onRemovedBeforeStart = { transactions.driverSettled(markerId) }
    ) { token, done ->
      val key = deviceId.uppercase()
      val gatt = gatts[key]
      if (gatt == null) {
        transactions.driverSettled(markerId)
        publish(transactions.cancel(markerId, AndroidGattNotSubmitted("connection is unavailable")))
        done()
        return@enqueue
      }
      var pending: ExactBytePending? = null
      var target: BluetoothGattCharacteristic? = null
      val result = try {
        var admissionResult: OrderedMarkerTransactions.State? = null
        val invoked = token.invokePhysical(done) {
          admissionResult = withNativeAdmission(gatt) {
            transactions.admit(markerId) { markStarted ->
              if (token.isCancelled() || !isCurrentGattCallback(gatt) || nativeGraphs[gatt] != admission.graph || orderedSources[gatt] !== admission.source) {
                return@admit OrderedMarkerTransactions.State.NotAdmitted(AndroidGattNotSubmitted("marker write connection or graph is stale"))
              }
              val characteristic = findChar(deviceId, path.serviceUuid, path.serviceOccurrence, path.characteristicUuid, path.characteristicOccurrence)
                ?: return@admit OrderedMarkerTransactions.State.NotAdmitted(AndroidGattNotSubmitted("marker write characteristic is unavailable"))
              if (hasAmbiguousMarkerWrite(deviceId, gatt, characteristic)) {
                return@admit OrderedMarkerTransactions.State.NotAdmitted(
                  AndroidGattNotSubmitted("marker write blocked by ambiguous earlier marker write"))
              }
              target = characteristic
              val generation = gattGenerationByInstance[gatt]
                ?: return@admit OrderedMarkerTransactions.State.NotAdmitted(AndroidGattNotSubmitted("marker write attachment is unavailable"))
              val entry = ExactBytePending(key, gatt, generation, token, { outcome ->
                transactions.settleIfRetained(markerId, outcome)?.let { settled ->
                  transactions.driverSettled(markerId)
                  publish(settled)
                }
              }, done, retainAfterCancellation = true)
              if (!registerCallbackOwner(token, gatt) {
                  if (exactWritePending.putIfAbsent(characteristic, entry) != null) false
                  else { exactWriteValues[characteristic] = bytes.copyOf(); true }
                }) {
                return@admit OrderedMarkerTransactions.State.NotAdmitted(AndroidGattNotSubmitted("marker write is busy or callback owner capacity is exhausted"))
              }
              pending = entry
              val writeType = if (withResponse) BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT else BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
              val gate = checkNotNull(admissionGates[gatt])
              val marker = cancellation.admit { admission.source.markerAndRun(admission.group, markerId,
                { wake -> enqueueAdmission(gate, 0, true, wake = true, action = wake) }) { ordinal ->
                  characteristic.writeType = writeType
                  if (Build.VERSION.SDK_INT < 33) characteristic.value = bytes
                  markStarted(ordinal)
                  if (Build.VERSION.SDK_INT >= 33) gatt.writeCharacteristic(characteristic, bytes, writeType)
                  else if (gatt.writeCharacteristic(characteristic)) BluetoothGatt.GATT_SUCCESS else -1
                } } ?: return@admit OrderedMarkerTransactions.State.NotAdmitted(OrderedMarkerCancelled())
              when (marker) {
                is OrderedNotificationSource.MarkerAdmission.Rejected -> if (marker.terminalEvidence)
                  OrderedMarkerTransactions.State.Rejected(AndroidGattNotSubmitted("marker admission refused"), true)
                else OrderedMarkerTransactions.State.NotAdmitted(AndroidGattNotSubmitted("ordered source is no longer active"))
                is OrderedNotificationSource.MarkerAdmission.Admitted -> {
                  val failure = marker.effect.exceptionOrNull()
                  if (failure != null) {
                    OwnedAndroidLog.e("ordered marker GATT write", failure)
                    if (transactions.state(markerId) is OrderedMarkerTransactions.State.Initiating) OrderedMarkerTransactions.State.Started(marker.ordinal, failure)
                    else OrderedMarkerTransactions.State.AdmittedNotStarted(marker.ordinal, failure)
                  } else {
                    val status = marker.effect.getOrThrow()
                    if (acceptApi33WriteStatus(status)) OrderedMarkerTransactions.State.Started(marker.ordinal)
                    else OrderedMarkerTransactions.State.AdmittedNotStarted(marker.ordinal, AndroidGattOperationFailure("characteristic-write", status, submitted = false))
                  }
                }
              }
            }
          }
        }
        if (invoked) checkNotNull(admissionResult) else transactions.cancel(markerId, OrderedMarkerCancelled())
      } catch (error: Throwable) {
        OwnedAndroidLog.e("ordered marker admission", error)
        transactions.failAdmission(markerId, error)
      }
      if (result is OrderedMarkerTransactions.State.Started && result.uncertainty != null) {
        // The platform may never callback. Keep the original pending owner for
        // a late answer, but release this serial cell and report uncertainty now.
        transactions.driverSettled(markerId)
        try { publish(transactions.state(markerId) ?: result) } finally { done() }
      } else if (result !is OrderedMarkerTransactions.State.Started) {
        val characteristic = target
        val entry = pending
        if (characteristic != null && entry != null && exactWritePending.remove(characteristic, entry)) {
          exactWriteValues.remove(characteristic)
        }
        transactions.driverSettled(markerId)
        try { publish(result) } finally { done() }
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
      onStartFailure = { error -> onResult(Result.failure(error)) },
      settleOnClearBeforeOwner = true
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
      if (hasAmbiguousMarkerWrite(deviceId, gatt, characteristic)) {
        completeExactByteDirect(onResult, done,
          Result.failure(AndroidGattNotSubmitted("exact characteristic write blocked by ambiguous marker write")), token)
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
      if (!registerCallbackOwner(token, gatt) {
          if (exactWritePending.putIfAbsent(characteristic, pending) != null) false
          else { exactWriteValues[characteristic] = value.copyOf(); true }
        }) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact characteristic write is pending or callback owner capacity is exhausted")),
          token
        )
        return@enqueue
      }
      // The owner and its independent payload were published together above.
      if (!token.invokePhysical(done) {
        if (Build.VERSION.SDK_INT >= 33) {
          val status = gatt.writeCharacteristic(characteristic, value, writeType)
          if (!acceptApi33WriteStatus(status)) {
            val observedPayload = exactWriteValues[characteristic]
            if (exactWritePending.remove(characteristic, pending)) {
              removeAndroidWritePayloadIfSame(exactWriteValues, characteristic, observedPayload)
              completeExactByte(
                pending,
                Result.failure(AndroidGattOperationFailure("characteristic-write", status, submitted = false))
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
                Result.failure(AndroidGattNotSubmitted("writeCharacteristic failed to start"))
              )
            }
          }
        }
      }) {
        if (exactWritePending.remove(characteristic, pending)) {
          exactWriteValues.remove(characteristic)
          completeExactByte(pending, Result.failure(token.beforePhysicalFailure()))
        } else done()
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
      if (!withNativeAdmission(gatt) { gatt.setCharacteristicNotification(ch, enable) }) {
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
        val status = withNativeAdmission(gatt) { gatt.writeDescriptor(cccd, payload) }
        if (status != BluetoothGatt.GATT_SUCCESS) {
          pendingDesc.remove(key)
          onResult(Result.failure(IllegalStateException("writeDescriptor failed to start status=$status")))
          done()
        }
      } else {
        @Suppress("DEPRECATION")
        cccd.value = payload
        @Suppress("DEPRECATION")
        val started = withNativeAdmission(gatt) { gatt.writeDescriptor(cccd) }
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
    sourceEpoch: Long? = null,
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
      sourceEpoch,
      null,
      null,
      null,
      false,
      null,
      null,
      onResult
    )
  }

  internal fun setNotifyExactOwned(
    deviceId: String, serviceUuid: UUID, serviceOccurrence: Int, characteristicUuid: UUID,
    characteristicOccurrence: Int, enable: Boolean, subscriptionType: String?, sourceEpoch: Long?,
    observer: OwnedAndroidNotificationObserver, onResult: (Result<Unit>) -> Unit
  ): Long = setNotifyTarget(deviceId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence,
    enable, subscriptionType, sourceEpoch, null, null, null, false, null, observer, onResult)

  internal fun setNotifyExactRetained(
    deviceId: String, serviceUuid: UUID, serviceOccurrence: Int, characteristicUuid: UUID,
    characteristicOccurrence: Int, enable: Boolean, subscriptionType: String?, sourceEpoch: Long,
    admission: AndroidOrderedAdmission?, observer: OwnedAndroidNotificationObserver? = null, onResult: (Result<Unit>) -> Unit
  ): Long = setNotifyTarget(deviceId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence,
    enable, subscriptionType, sourceEpoch, null, null, null, true, admission, observer, onResult)

  private fun setNotifyTarget(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    enable: Boolean,
    subscriptionType: String?,
    sourceEpoch: Long?,
    expectedGatt: BluetoothGatt?,
    expectedGeneration: Long?,
    expectedOwner: NotificationOwnershipFifo.Claim?,
    retainOnFailure: Boolean,
    admission: AndroidOrderedAdmission?,
    observer: OwnedAndroidNotificationObserver?,
    onResult: (Result<Unit>) -> Unit
  ): Long {
    val claim = AtomicReference<PendingNotificationClaim?>(null)
    val restoreCancelledDisable = AtomicReference<(() -> Unit)?>(null)
    val setupGuard = Any()
    fun preEffect(error: Throwable): Throwable = if (enable && retainOnFailure) OrderedEnableNotApplied(error) else error
    return enqueue(
      deviceId,
      onCancelled = {
        val cleanupError = try {
          restoreCancelledDisable.get()?.invoke()
          synchronized(setupGuard) {
            claim.get()?.let { (generation, characteristic, owner) ->
              activeNativeSubscriptionOwnership.quiesceForCompensation(deviceId, generation, characteristic, owner)
            }
          }
          null
        } catch (throwable: Throwable) {
          OwnedAndroidLog.e("cancel notification ingress", IllegalStateException("ingress cleanup failed"))
          throwable
        }
        onResult(Result.failure(IllegalStateException("exact notification operation cancelled", cleanupError)))
      },
      onStartFailure = { error -> onResult(Result.failure(preEffect(error))) },
      settleOnClearBeforeOwner = true,
      clearBeforeOwnerUsesCancellation = true
    ) { token, done ->
      if (expectedGatt != null && expectedGeneration != null &&
        !isCurrentGatt(deviceId.uppercase(), expectedGatt, expectedGeneration)
      ) {
        val result = if (gattGenerationByInstance[expectedGatt] != expectedGeneration) Result.success(Unit)
        else Result.failure(IllegalStateException("notification cleanup attachment is still owned"))
        completeExactUnitDirect(onResult, done, result, token)
        return@enqueue
      }
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
          Result.failure(preEffect(IllegalStateException("exact characteristic not found"))),
          token
        )
        return@enqueue
      }
      val gattGeneration = gattGenerations[deviceId.uppercase()] ?: run {
        completeExactUnitDirect(
          onResult, done,
          Result.failure(preEffect(IllegalStateException("exact notification GATT generation is unavailable"))), token
        )
        return@enqueue
      }
      fun validAdmission(): Boolean = admission == null ||
        (nativeGraphs[gatt] == admission.graph && characteristic === admission.characteristic &&
          admission.source.hasPhysicalClaim(admission.group, characteristic, admission.membership))
      fun <T : Any> withValidAdmission(effect: () -> T): T? {
        if (admission == null) return effect()
        if (nativeGraphs[gatt] != admission.graph || characteristic !== admission.characteristic) return null
        return admission.source.withPhysicalClaim(admission.group, characteristic, admission.membership, effect)
      }
      if (enable && admission != null && !synchronized(orderedSourceLock) { validAdmission() }) {
        completeExactUnitDirect(onResult, done, Result.failure(OrderedEnableNotApplied(OrderedRegistrationStale())), token)
        return@enqueue
      }
      if (expectedOwner != null &&
        !activeNativeSubscriptionOwnership.ownsToken(deviceId, gattGeneration, characteristic, expectedOwner)
      ) {
        val result = if (enable) Result.failure(IllegalStateException("notification restoration owner is unavailable"))
        else Result.success(Unit)
        completeExactUnitDirect(onResult, done, result, token)
        return@enqueue
      }
      if (!enable && expectedOwner == null &&
        activeNativeSubscriptionOwnership.ownedByOther(deviceId, gattGeneration, characteristic, observer?.owner)
      ) {
        completeExactUnitDirect(onResult, done, Result.failure(IllegalStateException("notification belongs to another owner")), token)
        return@enqueue
      }
      if (!enable && sourceEpoch != null &&
        !activeNativeSubscriptionOwnership.ownsEpoch(deviceId, gattGeneration, characteristic, sourceEpoch)
      ) {
        val result = if (retainOnFailure) Result.failure(IllegalStateException("ordered physical owner is unavailable")) else Result.success(Unit)
        completeExactUnitDirect(onResult, done, result, token)
        return@enqueue
      }
      if (expectedGatt == null) {
        pendingCccdCompensations[characteristic]?.forEach { operation ->
          val cleanup = retryableCleanups[operation]
          // A replacement registration has no access to the old claim token.
          // It may nevertheless retry only the cleanup retained for this exact
          // characteristic's current peer/generation. Requiring the new
          // observer owner here strands a failed legacy rollback forever and
          // makes the physical path unavailable even after Android accepts the
          // cleanup. The compensation map is keyed by the characteristic, so
          // this cannot retry another path's owner.
          if (cleanup != null && cleanup.scope.peer == deviceId.uppercase() &&
            cleanup.scope.generation == gattGeneration && cleanup() == null) {
            releaseCccdCompensation(characteristic, operation)
            retryableCleanups.remove(operation, cleanup)
          }
        }
        if (pendingCccdCompensations.containsKey(characteristic)) {
          completeExactUnitDirect(
            onResult, done,
            Result.failure(preEffect(IllegalStateException("previous notification cleanup is pending"))), token
          )
          return@enqueue
        }
      }
      val cccdForOwnership = characteristic.getDescriptor(CCCD_UUID)
      if (cccdForOwnership != null && exactDescriptorWritePending.containsKey(cccdForOwnership)) {
        completeExactUnitDirect(onResult, done,
          Result.failure(preEffect(IllegalStateException("descriptor write callback is still pending"))), token)
        return@enqueue
      }
      val payload = resolveCccdPayload(enable, subscriptionType, characteristic.properties)
      if (payload == null) {
        completeExactUnitDirect(
          onResult,
          done,
          Result.failure(preEffect(
            IllegalStateException(
              "characteristic supports neither notify nor indicate (subscriptionType=$subscriptionType)"
            )
          )),
          token
        )
        return@enqueue
      }
      val rollbackOperation = "cccdRollback:${deviceId.uppercase()}:generation=$gattGeneration:${nextGattOperationId.getAndIncrement()}"
      var armed = true
      var cancelledBeforeStart = false
      if (enable && expectedOwner == null) {
        synchronized(setupGuard) {
          armed = synchronized(notificationControlLock) {
            activeNativeSubscriptionOwnership.arm(
              deviceId, gattGeneration, characteristic, sourceEpoch,
              OwnedAndroidNotificationPath(serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence),
              if (payload.contentEquals(ANDROID_CCCD_ENABLE_INDICATION)) "indication" else "notification",
              retainOnFailure, observer?.event, observer?.stopped, observer?.owner
            )
          }
          if (armed) {
            val owner = activeNativeSubscriptionOwnership.ownerToken(deviceId, gattGeneration, characteristic)
              ?: throw IllegalStateException("notification claim disappeared during admission")
            claim.set(PendingNotificationClaim(gattGeneration, characteristic, owner))
            if (token.isPubliclySettled()) {
              activeNativeSubscriptionOwnership.abandon(deviceId, gattGeneration, characteristic, owner)
              cancelledBeforeStart = true
            }
          }
        }
      }
      if (!armed) {
        completeExactUnitDirect(
          onResult, done,
          Result.failure(preEffect(IllegalStateException("exact notification registration is already owned"))), token
        )
        return@enqueue
      }
      if (cancelledBeforeStart) {
        done()
        return@enqueue
      }
      val physicalOwner = if (enable) expectedOwner ?: claim.get()?.owner else
        activeNativeSubscriptionOwnership.ownerToken(deviceId, gattGeneration, characteristic)
      var orderedSnapshot: Pair<OrderedNotificationSource<BluetoothGattCharacteristic>,
        Map<OrderedNotificationSource<BluetoothGattCharacteristic>.Group, OrderedNotificationSource.Membership>>? = null
      fun reportDeliveryUncertain(failure: Throwable) {
        val owner = physicalOwner ?: return
        val snapshot = orderedSnapshot
        if (snapshot != null && snapshot.second.isNotEmpty()) {
          try {
            withNativeAdmission(gatt) {
              if (orderedSources[gatt] === snapshot.first &&
                activeNativeSubscriptionOwnership.ownsToken(deviceId, gattGeneration, characteristic, owner)) {
                val gate = checkNotNull(admissionGates[gatt])
                snapshot.first.failCurrentMembers(characteristic, snapshot.second, failure) { wake ->
                  enqueueAdmission(gate, 0, true, wake = true) {
                    try { wake() } catch (error: Throwable) { reportOrderedWakeFailure("orderedSource.failureWake", error) }
                  }
                }
              }
            }
          } catch (error: Throwable) { reportOrderedWakeFailure("orderedSource.failureAdmission", error) }
        }
        activeNativeSubscriptionOwnership.failOrdinaryUncertain(deviceId, gattGeneration, characteristic, owner, failure)
      }
      val localAccepted = try {
        withNativeAdmission(gatt) { synchronized(callbackOwnerLock) { synchronized(notificationControlLock) {
          if (!isCurrentGatt(deviceId.uppercase(), gatt, gattGeneration) ||
            retiringGatts[deviceId.uppercase()]?.let { it.gatt === gatt && it.generation == gattGeneration } == true ||
            (!enable && sourceEpoch != null &&
              !activeNativeSubscriptionOwnership.ownsEpoch(deviceId, gattGeneration, characteristic, sourceEpoch)) ||
            (physicalOwner != null &&
              !activeNativeSubscriptionOwnership.ownsToken(deviceId, gattGeneration, characteristic, physicalOwner))
          ) null else if (enable) withValidAdmission { gatt.setCharacteristicNotification(characteristic, true) }
          else {
            orderedSources[gatt]?.takeIf { it.generation == gattGeneration }?.let { source ->
              orderedSnapshot = source to source.activeMemberships(characteristic)
            }
            gatt.setCharacteristicNotification(characteristic, false)
          }
        } } }
      } catch (throwable: Throwable) {
        if (expectedGatt != null) {
          completeExactUnitDirect(onResult, done, Result.failure(throwable), token)
          return@enqueue
        }
        if (!enable && physicalOwner != null) {
          try { reportDeliveryUncertain(throwable) } catch (reportError: Throwable) { throwable.addSuppressed(reportError) }
        }
        val cleanup = rollbackNotifyRegistration(deviceId.uppercase(), gatt, gattGeneration, characteristic, enable, rollbackOperation, physicalOwner)
        if (cleanup != null && enable) {
          localOnlyCccdCompensations[characteristic] = LocalOnlyCccdCleanup(cleanup.operation, sourceEpoch, gattGeneration)
          retainLocalNotificationCleanup(deviceId, gattGeneration, characteristic, physicalOwner, cleanup) {
            rollbackNotifyRegistration(deviceId.uppercase(), gatt, gattGeneration, characteristic, enable, rollbackOperation, physicalOwner)
          }
        } else {
          if (cleanup != null) {
            retainDisableRegistrationRestore(deviceId, gatt, gattGeneration, characteristic, physicalOwner, cleanup) {
              rollbackNotifyRegistration(deviceId.uppercase(), gatt, gattGeneration, characteristic, enable, rollbackOperation, physicalOwner)
            }
          }
          if (enable && expectedOwner == null && physicalOwner != null) activeNativeSubscriptionOwnership.abandon(deviceId, gattGeneration, characteristic, physicalOwner)
        }
        val terminal = if (enable && retainOnFailure && cleanup == null) OrderedEnableNotApplied(throwable) else throwable
        completeExactUnitDirect(onResult, done, Result.failure(terminal), token)
        return@enqueue
      }
      if (localAccepted == null) {
        if (enable && admission != null && physicalOwner != null) activeNativeSubscriptionOwnership.abandon(deviceId, gattGeneration, characteristic, physicalOwner)
        val retired = synchronized(callbackOwnerLock) {
          !isCurrentGatt(deviceId.uppercase(), gatt, gattGeneration) ||
            retiringGatts[deviceId.uppercase()]?.let { it.gatt === gatt && it.generation == gattGeneration } == true
        }
        completeExactUnitDirect(
          onResult, done,
          if (retired) Result.failure(preEffect(IllegalStateException("notification GATT generation is retiring")))
          else if (enable && admission != null) Result.failure(OrderedEnableNotApplied(OrderedRegistrationStale()))
          else if (enable || retainOnFailure) Result.failure(IllegalStateException("notification claim was invalidated before registration"))
          else Result.success(Unit), token
        )
        return@enqueue
      }
      if (!enable && localAccepted && physicalOwner != null) {
        val once = AtomicBoolean(false)
        restoreCancelledDisable.set {
          if (once.compareAndSet(false, true)) token.completeAfterInvocation {
            try {
              reportDeliveryUncertain(IllegalStateException("notification delivery may have been interrupted by an abandoned disable"))
            } finally {
              val cleanup = rollbackNotifyRegistration(deviceId.uppercase(), gatt, gattGeneration, characteristic, false, rollbackOperation, physicalOwner)
              if (cleanup != null) retainDisableRegistrationRestore(deviceId, gatt, gattGeneration, characteristic, physicalOwner, cleanup) {
                rollbackNotifyRegistration(deviceId.uppercase(), gatt, gattGeneration, characteristic, false, rollbackOperation, physicalOwner)
              }
            }
          }
        }
      }
      if (token.isPubliclySettled()) {
        if (enable && expectedOwner == null && physicalOwner != null) {
          val cleanup = rollbackNotifyRegistration(deviceId.uppercase(), gatt, gattGeneration, characteristic, true, rollbackOperation, physicalOwner)
          if (cleanup == null) activeNativeSubscriptionOwnership.abandon(deviceId, gattGeneration, characteristic, physicalOwner)
          else {
            localOnlyCccdCompensations[characteristic] = LocalOnlyCccdCleanup(cleanup.operation, sourceEpoch, gattGeneration)
            retainLocalNotificationCleanup(deviceId, gattGeneration, characteristic, physicalOwner, cleanup) {
              rollbackNotifyRegistration(deviceId.uppercase(), gatt, gattGeneration, characteristic, true, rollbackOperation, physicalOwner)
            }
          }
        } else if (!enable && physicalOwner != null) {
          restoreCancelledDisable.get()?.invoke()
        }
        done()
        return@enqueue
      }
      if (!localAccepted) {
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
          subscriptionEnabled = if (enable && expectedOwner != null) null else enable,
          subscriptionCharacteristic = characteristic,
          subscriptionOwner = physicalOwner,
          retainOnFailure = retainOnFailure
        )
        if (!token.registerOwner { exactRegistrationPending.putIfAbsent(characteristic, pending) == null }) {
          if (enable && expectedOwner == null && physicalOwner != null) activeNativeSubscriptionOwnership.abandon(deviceId, gattGeneration, characteristic, physicalOwner)
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
      val rollbackRegistration = {
          rollbackNotifyRegistration(
            deviceId.uppercase(),
            gatt,
            gattGeneration,
            characteristic,
            enable,
            rollbackOperation,
            physicalOwner
          )
      }
      val compensateRemote: () -> Unit = {
        physicalOwner?.let { owner -> scheduleCompensatingDisable(
          deviceId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence,
          gatt, gattGeneration, characteristic, rollbackOperation, owner
        ) }
        Unit
      }
      val restoreRemote: () -> Unit = {
        physicalOwner?.let { owner -> scheduleCompensatingEnable(
          deviceId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence,
          gatt, gattGeneration, characteristic, owner,
          activeNativeSubscriptionOwnership.subscriptionType(deviceId, gattGeneration, characteristic, owner)
        ) }
        Unit
      }
      val cccd = characteristic.getDescriptor(CCCD_UUID)
      if (cccd == null) {
        completeExactUnit(
          ExactUnitPending(
            deviceId.uppercase(), gatt, gattGeneration, token, onResult, done,
            if (expectedGatt == null) rollbackRegistration else null,
            subscriptionEnabled = if (enable && expectedOwner != null) null else enable, subscriptionCharacteristic = characteristic, subscriptionOwner = physicalOwner,
            retainOnFailure = retainOnFailure
          ),
          Result.success(Unit)
        )
        return@enqueue
      }
      val pending = ExactUnitPending(
        deviceId.uppercase(),
        gatt,
        gattGeneration,
        token,
        onResult,
        done,
        if (expectedGatt == null) rollbackRegistration else null,
        subscriptionEnabled = if (enable && expectedOwner != null) null else enable,
        subscriptionCharacteristic = characteristic,
        subscriptionOwner = physicalOwner,
        compensatingDisable = if (enable && expectedOwner == null) compensateRemote else null,
        compensatingEnable = if (!enable && expectedGatt == null && physicalOwner != null) restoreRemote else null,
        onLateSuccess = if (enable && expectedGatt != null && expectedOwner != null) ({ onResult(Result.success(Unit)) }) else null,
        retainOnFailure = retainOnFailure,
        onDeliveryUncertain = if (!enable && expectedGatt == null && physicalOwner != null) ::reportDeliveryUncertain else null
      )
      val cccdBusy = !registerCallbackOwner(token, gatt) {
        synchronized(descriptorWriteOwnershipLock) {
          !exactDescriptorWritePending.containsKey(cccd) && exactCccdPending.putIfAbsent(cccd, pending) == null
        }
      }
      if (cccdBusy) {
        completeExactUnit(pending, Result.failure(IllegalStateException("exact CCCD write is pending or callback owner capacity is exhausted")))
        return@enqueue
      }
      try {
        if (!token.invokePhysical(done) {
          if (Build.VERSION.SDK_INT >= 33) {
            val status = withNativeAdmission(gatt) { synchronized(callbackOwnerLock) {
              if (!isCurrentGatt(deviceId.uppercase(), gatt, gattGeneration) ||
                retiringGatts[deviceId.uppercase()]?.let { it.gatt === gatt && it.generation == gattGeneration } == true) null
              else if (enable) withValidAdmission { gatt.writeDescriptor(cccd, payload) } else gatt.writeDescriptor(cccd, payload)
            } }
            if (status == null) {
              if (exactCccdPending.remove(cccd, pending)) completeExactUnit(pending, Result.failure(OrderedRegistrationStale()))
            } else if (status != BluetoothGatt.GATT_SUCCESS) {
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
            val started = withNativeAdmission(gatt) { synchronized(callbackOwnerLock) {
              if (!isCurrentGatt(deviceId.uppercase(), gatt, gattGeneration) ||
                retiringGatts[deviceId.uppercase()]?.let { it.gatt === gatt && it.generation == gattGeneration } == true) null
              else if (enable) withValidAdmission { gatt.writeDescriptor(cccd) } else gatt.writeDescriptor(cccd)
            } }
            if (started == null) {
              if (exactCccdPending.remove(cccd, pending)) completeExactUnit(pending, Result.failure(OrderedRegistrationStale()))
            } else if (!started) {
              if (exactCccdPending.remove(cccd, pending)) {
                completeExactUnit(
                  pending,
                  Result.failure(AndroidCccdSubmissionFailure(null))
                )
              }
            }
          }
        }) {
          if (exactCccdPending.remove(cccd, pending)) {
            completeExactUnit(pending, Result.failure(token.beforePhysicalFailure()))
          } else done()
        }
      } catch (throwable: Throwable) {
        // SecurityException is Android's synchronous permission refusal: no
        // descriptor transaction was accepted, so retaining callback debt
        // would fence this characteristic forever.  Complete the exact claim
        // now; completeExactUnit retains and retries any local rollback.
        if (throwable is SecurityException) {
          if (exactCccdPending.remove(cccd, pending)) {
            completeExactUnit(pending, Result.failure(throwable))
          }
          return@enqueue
        }
        // Other throws may follow acceptance by Android. Keep the shared
        // descriptor fence and the physical claim until the actual callback
        // can drive rollback or remote compensation.
        if (exactCccdPending[cccd] === pending) {
          if (!enable && expectedGatt == null) {
            try { reportDeliveryUncertain(throwable) } catch (reportError: Throwable) { throwable.addSuppressed(reportError) }
          }
          try { restoreCancelledDisable.get()?.invoke() } catch (cleanupError: Throwable) {
            throwable.addSuppressed(cleanupError)
          }
          settleCccdAwaitingCallback(pending, Result.failure(throwable))
        } else {
          OwnedAndroidLog.e("notification descriptor submission after settlement", throwable)
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
        val status = withNativeAdmission(gatt) { gatt.writeDescriptor(desc, value) }
        if (status != BluetoothGatt.GATT_SUCCESS) {
          pendingDesc.remove(key)
          onResult(Result.failure(IllegalStateException("writeDescriptor failed to start status=$status")))
          done()
        }
      } else {
        @Suppress("DEPRECATION")
        desc.value = value
        @Suppress("DEPRECATION")
        val started = withNativeAdmission(gatt) { gatt.writeDescriptor(desc) }
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
      onStartFailure = { error -> onResult(Result.failure(error)) },
      settleOnClearBeforeOwner = true
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
      if (!registerCallbackOwner(token, gatt) { exactDescriptorReadPending.putIfAbsent(descriptor, pending) == null }) {
        completeExactByteDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact descriptor read is pending or callback owner capacity is exhausted")),
          token
        )
        return@enqueue
      }
      if (!token.invokePhysical(done) {
        if (!gatt.readDescriptor(descriptor) && exactDescriptorReadPending.remove(descriptor, pending)) {
          completeExactByte(pending, Result.failure(IllegalStateException("readDescriptor failed to start")))
        }
      }) {
        if (exactDescriptorReadPending.remove(descriptor, pending)) {
          completeExactByte(pending, Result.failure(token.beforePhysicalFailure()))
        } else done()
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
      onStartFailure = { error -> onResult(Result.failure(error)) },
      settleOnClearBeforeOwner = true
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
      val descriptorBusy = !registerCallbackOwner(token, gatt) {
        synchronized(descriptorWriteOwnershipLock) {
          !exactCccdPending.containsKey(descriptor) && exactDescriptorWritePending.putIfAbsent(descriptor, pending) == null
        }
      }
      if (descriptorBusy) {
        completeExactUnitDirect(
          onResult,
          done,
          Result.failure(IllegalStateException("exact descriptor write is pending or callback owner capacity is exhausted")),
          token
        )
        return@enqueue
      }
      if (!token.invokePhysical(done) {
        if (Build.VERSION.SDK_INT >= 33) {
          val status = withNativeAdmission(gatt) { gatt.writeDescriptor(descriptor, value) }
          if (status != BluetoothGatt.GATT_SUCCESS) {
            if (exactDescriptorWritePending.remove(descriptor, pending)) {
              completeExactUnit(
                pending,
                Result.failure(AndroidGattNotSubmitted("writeDescriptor failed to start status=$status"))
              )
            }
          }
        } else {
          @Suppress("DEPRECATION")
          descriptor.value = value.copyOf()
          @Suppress("DEPRECATION")
          val started = withNativeAdmission(gatt) { gatt.writeDescriptor(descriptor) }
          if (!started) {
            if (exactDescriptorWritePending.remove(descriptor, pending)) {
              completeExactUnit(pending, Result.failure(AndroidGattNotSubmitted("writeDescriptor failed to start")))
            }
          }
        }
      }) {
        if (exactDescriptorWritePending.remove(descriptor, pending)) {
          completeExactUnit(pending, Result.failure(token.beforePhysicalFailure()))
        } else done()
      }
    }
  }

  private val subrateApi by lazy {
    AndroidSubrateApi(securityFullSdkInt, BluetoothGatt::class.java)
  }

  fun subrateAvailable(): Boolean = subrateApi.available

  /** A status is the request's own OS answer, never a measured subrate factor. */
  fun requestSubrate(deviceId: String, mode: String, onResult: (Result<Int>) -> Unit): Long = enqueue(
    deviceId,
    onCancelled = { onResult(Result.failure(IllegalStateException("subrate request cancelled"))) },
    onStartFailure = { error -> onResult(Result.failure(error)) }
  ) { token, done ->
    val result = runCatching {
      val gatt = gatts[deviceId.uppercase()] ?: throw AndroidGattLinkLost("subrate request has no active GATT", null, false)
      subrateApi.request(gatt, mode)
    }
    try {
      if (token.markPubliclySettled()) onResult(result)
    } finally { done() }
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
    val (graphWorkPending, nativeOpenPending) = synchronized(orderedSourceLock) {
      graphPermitShutdown = true
      val active = admissionGates.values.mapNotNull { it.graphPermit }
      if (active.isNotEmpty()) {
        graphPermitDestroyPending = true
        active.forEach { it.invalidate(GraphPermitInvalidation.TEARDOWN) }
      }
      // A released permit can still be draining callbacks outside the lock.
      // The gate remains owned until the last callback and overflow relay return.
      val openPending = synchronized(peerLifecycleLock) { peerLifecycleQueues.isNotEmpty() }
      admissionGates.isNotEmpty() to openPending
    }
    if (graphWorkPending || nativeOpenPending) return OwnedRadioTeardownResult(buildList {
      if (graphWorkPending) add(OwnedRadioTeardownFailure("graphPermit.destroy", IllegalStateException("graph permit or deferred callback is pending")))
      if (nativeOpenPending) add(OwnedRadioTeardownFailure("nativeGattOpen.destroy", IllegalStateException("peer lifecycle work is pending")))
    })
    val failures = mutableListOf<OwnedRadioTeardownFailure>()
    stopScan()?.let { failure -> failures.add(failure) }
    unregisterAdapterStateReceiver()?.let { failure -> failures.add(failure) }
    unregisterBondStateReceiver()?.let { failure -> failures.add(failure) }
    pendingBondPairs.clear()
    var callbackError = errorOf { settleQueuedConnects() }
    failures.addAll(retryCleanupLedger { callbackError = mergeErrors(callbackError, it) })
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
    gatts.forEach { (key, gatt) -> gattGenerationByInstance[gatt]?.let { fenceRetiringGatt(key, gatt, it) } }
    pendingDeviceKeys.forEach { key ->
      callbackError = mergeErrors(callbackError, errorOf { failPendingForDevice(key, "radio destroyed", linkLost = false) })
    }
    // failPendingForDevice may create retryable CCCD rollback ownership; include
    // that cleanup in this destroy receipt rather than reducing it to a log.
    failures.addAll(retryCleanupLedger { callbackError = mergeErrors(callbackError, it) })
    // Close each exact generation through its disconnect owner. This fences its
    // deadline and settles every joined waiter with the actual close result.
    (gatts.keys + pendingGattTeardowns.keys).toSet().forEach { key ->
      val owner = pendingGattTeardowns[key]
      val gatt = gatts[key] ?: owner?.gatt ?: return@forEach
      val generation = gattGenerations[key] ?: owner?.generation ?: return@forEach
      val teardown = completeGattTeardown(key, gatt, generation)
      teardown.failure?.let { failure -> failures.add(failure) }
      if (teardown.inProgress) failures.add(OwnedRadioTeardownFailure(
        "closeGatt:$key:generation=$generation", IllegalStateException("GATT close is still in progress")
      ))
      // One generation's observer error must not stop the remaining generations from closing.
      callbackError = mergeErrors(callbackError, teardown.callbackError)
    }
    if (gattTeardownsInFlight.get() != 0) failures.add(OwnedRadioTeardownFailure(
      "closeGatt.callbacks", IllegalStateException("GATT teardown callbacks are still in progress")
    ))
    closeTimeouts.values.forEach { runnable -> mainHandler?.removeCallbacks(runnable) }
    closeTimeouts.clear()
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
      connectedEncryptionGenerations.clear()
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
    // Preserve physical cleanup failures even when an observer error prevents returning the receipt.
    callbackError?.let { error ->
      failures.forEach { failure -> mergeErrors(error, failure.throwable) }
      throw error
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
    synchronized(orderedSourceLock) { publishNativeGraph(gatt, services) }
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
    onStartFailure: (Throwable) -> Unit = GattSerialQueue.NO_START_FAILURE,
    onRemovedBeforeStart: () -> Unit = {},
    settleOnClearBeforeOwner: Boolean = false,
    clearBeforeOwnerUsesCancellation: Boolean = false,
    op: (token: GattSerialQueue.GattOperationToken, done: () -> Unit) -> Unit
  ): Long {
    val key = deviceId.uppercase()
    return deviceQueues.getOrPut(key) {
      GattSerialQueue(
        handler = mainHandler,
        post = post,
        idProvider = { nextGattOperationId.getAndIncrement() }
      )
    }.submitCancellable({ token, done ->
      if (retiringGatts.containsKey(key)) {
        try {
          if (token.markPubliclySettled()) onStartFailure(IllegalStateException("Android GATT teardown is pending"))
        } finally { done() }
      } else op(token, done)
    }, onCancelled, onStartFailure, onRemovedBeforeStart, settleOnClearBeforeOwner, clearBeforeOwnerUsesCancellation)
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
    var callbackError: Throwable? = null
    activeNativeSubscriptionOwnership
      .invalidateForDatabaseChangeClaims(deviceKeyUpper, gattGeneration) { callbackError = mergeErrors(callbackError, it) }
      .forEach { (characteristic, owner, eventOwner) ->
        val operation = "serviceChangedDisable:$deviceKeyUpper:generation=$gattGeneration:${nextGattOperationId.getAndIncrement()}"
        fun disable(): OwnedRadioTeardownFailure? = withNativeAdmission(gatt) { synchronized(callbackOwnerLock) { synchronized(notificationControlLock) {
          val current = activeNativeSubscriptionOwnership.ownerToken(deviceKeyUpper, gattGeneration, characteristic)
          if (retiringGatts[deviceKeyUpper]?.let { it.gatt === gatt && it.generation == gattGeneration } == true) {
            OwnedRadioTeardownFailure(operation, IllegalStateException("notification GATT generation is retiring"))
          } else if (!isCurrentGatt(deviceKeyUpper, gatt, gattGeneration) || (current != null && current !== owner)) {
            null
          } else try {
            if (gatt.setCharacteristicNotification(characteristic, false)) null
            else OwnedRadioTeardownFailure(operation, IllegalStateException("setCharacteristicNotification invalidation was rejected"))
          } catch (throwable: Throwable) {
            OwnedRadioTeardownFailure(operation, throwable)
          }
        } } }
        disable()?.let { failure ->
          registerRetryableCleanup(failure.operation, CleanupScope(deviceKeyUpper, gattGeneration, eventOwner)) {
            disable()?.let { retry ->
              val preserved = IllegalStateException(failure.throwable.message, failure.throwable)
              if (retry.throwable !== failure.throwable) preserved.addSuppressed(retry.throwable)
              OwnedRadioTeardownFailure(failure.operation, preserved)
            }
          }
          callbackError = mergeErrors(callbackError, errorOf { reportCleanupFailure(failure) })
        }
      }
    callbackError?.let { throw it }
  }

  /**
   * Fails every operation pending for one device. By default the link went
   * down (disconnect, failed connect, close timeout, reconnect), so each
   * failure is an [AndroidGattLinkLost] carrying [gattStatus], as legacy's
   * dispatcher reported `connectionLost` (finding 132). Adapter loss, a
   * database change and destroy pass `linkLost = false`: those stay plain
   * platform failures.
   */
  private fun failPendingForDevice(
    deviceKeyUpper: String,
    reason: String,
    gattStatus: Int? = null,
    linkLost: Boolean = reason != ADAPTER_UNAVAILABLE_REASON
  ) {
    fun failure(submitted: Boolean): Throwable =
      if (linkLost) AndroidGattLinkLost(reason, gattStatus, submitted) else IllegalStateException(reason)
    var held: Throwable? = null
    fun settle(action: () -> Unit) { held = mergeErrors(held, errorOf(action)) }
    effectiveMtuByDevice.remove(deviceKeyUpper)
    settle { deviceQueues.remove(deviceKeyUpper)?.clear(failure(submitted = false)) }
    val failBytes = Result.failure<ByteArray?>(failure(submitted = true))
    val failInt = Result.failure<Int>(failure(submitted = true))
    val failUnit = Result.failure<Unit>(failure(submitted = true))
    // A peer link-loss callback can win the race with an asynchronous GATT
    // callback. Keep that exact Android status on pending CCCD work so the
    // protocol boundary reports connectionLost instead of flattening the race
    // to platformFailure.
    val retainCallbackDebt = retiringGatts.containsKey(deviceKeyUpper) || reason == "GATT database changed" || reason == "reentrant callback capacity exhausted"
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
        settle { pending.remove(key)?.invoke(failBytes) }
      }
    settle { pendingMtu.remove("mtu:$deviceKeyUpper")?.invoke(failInt) }
    settle { pendingRssi.remove("rssi:$deviceKeyUpper")?.invoke(failInt) }
    settle { pendingPhyReads.remove("phyRead:$deviceKeyUpper")?.invoke(
      Result.failure(failure(submitted = true))
    ) }
    settle { pendingPhyRequests.remove("phyRequest:$deviceKeyUpper")?.invoke(
      Result.failure(failure(submitted = true))
    ) }
    pendingDesc.keys
      .filter { key ->
        keyBelongsToDevice(key, "cccd", deviceKeyUpper) ||
          keyBelongsToDevice(key, "descWrite", deviceKeyUpper)
      }
      .toList()
      .forEach { key ->
        settle { pendingDesc.remove(key)?.invoke(if (key.startsWith("cccd:")) failCccd else failUnit) }
      }
    pendingDescRead.keys
      .filter { key -> keyBelongsToDevice(key, "descRead", deviceKeyUpper) }
      .toList()
      .forEach { key -> settle { pendingDescRead.remove(key)?.invoke(failBytes) } }
    // Remove only the owner observed by this teardown. A queued replacement
    // may reuse the same UUID key while the old disconnect callback drains.
    pendingWriteRegistry.entriesForDevice(deviceKeyUpper).forEach { (key, owner) ->
      if (pendingWriteRegistry.remove(key, owner)) {
        writeTerminalArbiter.claim(key)
        settle { owner.callback(failBytes) }
      }
    }
    exactReadPending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (retainCallbackDebt || exactReadPending.remove(entry.key, entry.value)) {
          settle { completeExactByte(entry.value, failBytes) }
        }
      }
    exactWritePending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        val observedPayload = exactWriteValues[entry.key]
        if (retainCallbackDebt || exactWritePending.remove(entry.key, entry.value)) {
          if (!retainCallbackDebt) removeAndroidWritePayloadIfSame(exactWriteValues, entry.key, observedPayload)
          exactWriteTerminalArbiter.claim(entry.key)
          settle { completeExactByte(entry.value, failBytes) }
        }
      }
    exactCccdPending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (retainCallbackDebt || exactCccdPending.remove(entry.key, entry.value)) {
          exactCccdTerminalArbiter.claim(entry.key)
          val result = if (entry.value.subscriptionEnabled != null) failCccd else failUnit
          settle {
            if (retainCallbackDebt) settleCccdAwaitingCallback(entry.value, result)
            else completeExactUnit(entry.value, result)
          }
        }
      }
    exactRegistrationPending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (exactRegistrationPending.remove(entry.key, entry.value)) {
          exactRegistrationTerminalArbiter.claim(entry.key)
          settle { completeExactUnit(
            entry.value,
            if (gattStatus == ANDROID_GATT_LINK_LOSS_STATUS) failCccd else failUnit
          ) }
        }
      }
    exactDescriptorReadPending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (retainCallbackDebt || exactDescriptorReadPending.remove(entry.key, entry.value)) {
          settle { completeExactByte(entry.value, failBytes) }
        }
      }
    exactDescriptorWritePending.entries
      .filter { it.value.deviceKeyUpper == deviceKeyUpper }
      .forEach { entry ->
        if (retainCallbackDebt || exactDescriptorWritePending.remove(entry.key, entry.value)) {
          settle { completeExactUnit(entry.value, failUnit) }
        }
      }
    held?.let { throw it }
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
    return isCurrentGatt(key, gatt, generation) &&
      retiringGatts[key]?.let { it.gatt !== gatt || it.generation != generation } != false &&
      !disconnectOwners.isClosing(key, gatt, generation)
  }

  /** Native close succeeded: no callback from this attachment can own new work. */
  private fun retireExactCallbackOwners(gatt: BluetoothGatt) {
    synchronized(callbackOwnerLock) {
      exactReadPending.entries.filter { it.value.gatt === gatt }.forEach { exactReadPending.remove(it.key, it.value) }
      exactWritePending.entries.filter { it.value.gatt === gatt }.forEach {
        if (exactWritePending.remove(it.key, it.value)) {
          exactWriteTerminalArbiter.claim(it.key)
          exactWriteValues.remove(it.key)
        }
      }
      exactCccdPending.entries.filter { it.value.gatt === gatt }.forEach {
        if (exactCccdPending.remove(it.key, it.value)) exactCccdTerminalArbiter.claim(it.key)
      }
      exactDescriptorReadPending.entries.filter { it.value.gatt === gatt }.forEach { exactDescriptorReadPending.remove(it.key, it.value) }
      exactDescriptorWritePending.entries.filter { it.value.gatt === gatt }.forEach { exactDescriptorWritePending.remove(it.key, it.value) }
    }
  }

  private fun completeExactByte(
    pending: ExactBytePending,
    result: Result<ByteArray?>
  ) {
    if (!pending.token.markPubliclySettled() && !pending.retainAfterCancellation) {
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
      if (token == null || token.markPubliclySettled()) callback(result)
    } catch (throwable: Throwable) {
      OwnedAndroidLog.e("protocol exact byte callback", throwable)
    } finally {
      done()
    }
  }

  /** Publish uncertainty while preserving the claim for a later physical result. */
  private fun settleCccdAwaitingCallback(pending: ExactUnitPending, result: Result<Unit>) {
    if (pending.subscriptionEnabled == true && pending.subscriptionCharacteristic != null && pending.subscriptionOwner != null) {
      activeNativeSubscriptionOwnership.quiesceForCompensation(
        pending.deviceKeyUpper, pending.gattGeneration, pending.subscriptionCharacteristic, pending.subscriptionOwner)
    }
    completeExactUnitDirect(pending.callback, pending.done, result, pending.token)
  }

  private fun completeExactUnit(
    pending: ExactUnitPending,
    result: Result<Unit>
  ) {
    val publish = pending.token.markPubliclySettled()
    if (!publish && result.isSuccess && pending.onLateSuccess != null) {
      try { pending.onLateSuccess.invoke() } finally { pending.done() }
      return
    }
    if (!publish && result.isSuccess && pending.compensatingEnable != null) {
      try { pending.compensatingEnable.invoke() } finally { pending.done() }
      return
    }
    if (!publish && result.isSuccess && pending.compensatingDisable != null) {
      pending.subscriptionCharacteristic?.let { characteristic ->
        pending.subscriptionOwner?.let { owner ->
          activeNativeSubscriptionOwnership.quiesceForCompensation(pending.deviceKeyUpper, pending.gattGeneration, characteristic, owner)
        }
      }
      pending.compensatingDisable.invoke()
      pending.done()
      return
    }
    val effectiveResult = if (
      result.isSuccess && pending.subscriptionEnabled == true &&
      (pending.subscriptionCharacteristic == null || pending.subscriptionOwner == null ||
        !activeNativeSubscriptionOwnership.ownsToken(
          pending.deviceKeyUpper, pending.gattGeneration, pending.subscriptionCharacteristic, pending.subscriptionOwner
        ))
    ) Result.failure(IllegalStateException("notification claim was invalidated before activation")) else result
    val shouldRollback = !publish || effectiveResult.isFailure
    effectiveResult.exceptionOrNull()?.let { failure ->
      try { pending.onDeliveryUncertain?.invoke(failure) }
      catch (error: Throwable) { reportOrderedWakeFailure("notification.failureDelivery", error) }
    }
    val rollbackFailure = if (shouldRollback) {
      try {
        pending.physicalCleanup?.invoke()
      } catch (throwable: Throwable) {
        OwnedRadioTeardownFailure("cccdRollback:${pending.deviceKeyUpper}:generation=${pending.gattGeneration}:${nextGattOperationId.getAndIncrement()}", throwable)
      }
    } else {
      null
    }
    if (rollbackFailure != null) {
      val characteristic = pending.subscriptionCharacteristic
      if (pending.subscriptionEnabled == true && characteristic != null) {
        retainLocalNotificationCleanup(pending.deviceKeyUpper, pending.gattGeneration, characteristic, pending.subscriptionOwner, rollbackFailure,
          abandonOnSuccess = !pending.retainOnFailure) {
          pending.physicalCleanup?.invoke()
        }
      } else {
        if (pending.subscriptionEnabled == false && characteristic != null) {
          retainDisableRegistrationRestore(pending.deviceKeyUpper, pending.gatt, pending.gattGeneration, characteristic,
            pending.subscriptionOwner, rollbackFailure) { pending.physicalCleanup?.invoke() }
        } else {
          registerRetryableCleanup(rollbackFailure.operation, CleanupScope(pending.deviceKeyUpper, pending.gattGeneration, null)) { pending.physicalCleanup?.invoke() }
          reportCleanupFailure(rollbackFailure)
        }
      }
    }
    if (!publish) {
      pending.subscriptionCharacteristic?.takeIf { pending.subscriptionEnabled == true && rollbackFailure == null && pending.subscriptionOwner != null }?.let { characteristic ->
        activeNativeSubscriptionOwnership.abandon(
          pending.deviceKeyUpper,
          pending.gattGeneration,
          characteristic,
          pending.subscriptionOwner
        )
      }
      pending.done()
      return
    }
    val staleBeforeDispatch = effectiveResult.exceptionOrNull() as? OrderedRegistrationStale
    val terminalResult = if (pending.retainOnFailure && staleBeforeDispatch != null && rollbackFailure == null)
      Result.failure(OrderedEnableNotApplied(staleBeforeDispatch)) else androidGattTerminalResult(effectiveResult, rollbackFailure)
    val characteristic = pending.subscriptionCharacteristic
    if (terminalResult.isFailure && rollbackFailure == null && pending.subscriptionEnabled == true && characteristic != null && pending.subscriptionOwner != null) {
      if (pending.retainOnFailure && staleBeforeDispatch == null) activeNativeSubscriptionOwnership.quiesceForCompensation(pending.deviceKeyUpper, pending.gattGeneration, characteristic, pending.subscriptionOwner)
      else activeNativeSubscriptionOwnership.abandon(pending.deviceKeyUpper, pending.gattGeneration, characteristic, pending.subscriptionOwner)
    }
    try {
      pending.callback(terminalResult)
    } catch (throwable: Throwable) {
      OwnedAndroidLog.e("protocol exact unit callback", throwable)
    } finally {
      if (terminalResult.isSuccess && characteristic != null && pending.subscriptionOwner != null) {
        if (pending.subscriptionEnabled == true) {
          activeNativeSubscriptionOwnership.activate(pending.deviceKeyUpper, pending.gattGeneration, characteristic, pending.subscriptionOwner)
        } else if (pending.subscriptionEnabled == false) {
          activeNativeSubscriptionOwnership.deactivate(pending.deviceKeyUpper, pending.gattGeneration, characteristic, pending.subscriptionOwner)
        }
      }
      pending.done()
    }
  }

  /** A local re-registration failure fences later operations without retiring the retained owner. */
  private fun retainDisableRegistrationRestore(
    deviceId: String,
    gatt: BluetoothGatt,
    generation: Long,
    characteristic: BluetoothGattCharacteristic,
    owner: NotificationOwnershipFifo.Claim?,
    failure: OwnedRadioTeardownFailure,
    restore: () -> OwnedRadioTeardownFailure?
  ) {
    retainCccdCompensation(characteristic, failure.operation)
    registerRetryableCleanup(failure.operation, CleanupScope(deviceId.uppercase(), generation, activeNativeSubscriptionOwnership.eventOwner(deviceId, generation, characteristic, owner))) {
      val result = when {
        gattGenerationByInstance[gatt] != generation -> null
        !isCurrentGatt(deviceId.uppercase(), gatt, generation) || owner == null ||
          !activeNativeSubscriptionOwnership.ownsToken(deviceId, generation, characteristic, owner) -> failure
        else -> try { restore() } catch (throwable: Throwable) { OwnedRadioTeardownFailure(failure.operation, throwable) }
      }
      if (result == null) releaseCccdCompensation(characteristic, failure.operation)
      result
    }
    try {
      reportCleanupFailure(failure)
    } finally {
      if (owner != null) activeNativeSubscriptionOwnership.failOrdinaryUncertain(
        deviceId, generation, characteristic, owner,
        IllegalStateException("notification local registration restoration failed", failure.throwable)
      )
    }
  }

  /** Restore the retained physical subscription after an abandoned disable succeeds. */
  private fun scheduleCompensatingEnable(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    gatt: BluetoothGatt,
    generation: Long,
    characteristic: BluetoothGattCharacteristic,
    owner: NotificationOwnershipFifo.Claim,
    subscriptionType: String?
  ) {
    val key = deviceId.uppercase()
    val operation = "cccdRestore:$key:generation=$generation:${nextGattOperationId.getAndIncrement()}"
    val inFlight = AtomicBoolean(false)
    val autonomousRetries = AtomicLong(0)
    val failure = OwnedRadioTeardownFailure(operation, IllegalStateException("CCCD restoration has not completed"))
    fun complete() {
      releaseCccdCompensation(characteristic, operation)
      retryableCleanups.remove(operation)
    }
    lateinit var attempt: () -> OwnedRadioTeardownFailure?
    fun scheduleRetry() {
      if (autonomousRetries.getAndIncrement() >= 2) return
      val scheduled = try {
        scheduleDeferred(250) {
          if (pendingCccdCompensations[characteristic]?.contains(operation) == true) attempt()
        }
      } catch (throwable: Throwable) {
        reportCleanupFailure(OwnedRadioTeardownFailure(operation, throwable))
        false
      }
      if (!scheduled) reportCleanupFailure(OwnedRadioTeardownFailure(operation, IllegalStateException("CCCD restoration retry could not be scheduled")))
    }
    attempt = fun(): OwnedRadioTeardownFailure? {
      // Only the disappearance of this exact attachment releases its cleanup.
      if (gattGenerationByInstance[gatt] != generation) {
        complete()
        return null
      }
      if (!isCurrentGatt(key, gatt, generation) ||
        !activeNativeSubscriptionOwnership.ownsToken(key, generation, characteristic, owner)) {
        reportCleanupFailure(failure)
        return failure
      }
      val descriptor = characteristic.getDescriptor(CCCD_UUID)
      if (descriptor != null && exactCccdPending.containsKey(descriptor)) return failure
      if (inFlight.compareAndSet(false, true)) {
        setNotifyTarget(
          deviceId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence,
          true, subscriptionType, null, gatt, generation, owner, false, null, null
        ) { result ->
          inFlight.set(false)
          if (result.isSuccess && isCurrentGatt(key, gatt, generation) &&
            activeNativeSubscriptionOwnership.ownsToken(key, generation, characteristic, owner)) complete()
          else {
            try {
              reportCleanupFailure(OwnedRadioTeardownFailure(operation,
                result.exceptionOrNull() ?: IllegalStateException("CCCD restoration owner is no longer current")))
            } finally {
              scheduleRetry()
            }
          }
        }
      }
      return failure
    }
    retainCccdCompensation(characteristic, operation)
    registerRetryableCleanup(operation, CleanupScope(deviceId.uppercase(), generation, activeNativeSubscriptionOwnership.eventOwner(deviceId, generation, characteristic, owner))) { attempt() }
    if (attempt() == null) complete()
  }

  /** A failed local rollback remains fenced until its exact retry has completed. */
  private fun retainLocalNotificationCleanup(
    deviceId: String,
    generation: Long,
    characteristic: BluetoothGattCharacteristic,
    owner: NotificationOwnershipFifo.Claim?,
    failure: OwnedRadioTeardownFailure,
    abandonOnSuccess: Boolean = true,
    cleanup: () -> OwnedRadioTeardownFailure?
  ) {
    if (owner == null) {
      reportCleanupFailure(failure)
      return
    }
    activeNativeSubscriptionOwnership.quiesceForCompensation(deviceId, generation, characteristic, owner)
    retainCccdCompensation(characteristic, failure.operation)
    val lock = Any()
    var completed = false
    registerRetryableCleanup(failure.operation, CleanupScope(deviceId.uppercase(), generation, activeNativeSubscriptionOwnership.eventOwner(deviceId, generation, characteristic, owner))) {
      synchronized(lock) {
        if (completed) null else if (!activeNativeSubscriptionOwnership.ownsToken(deviceId, generation, characteristic, owner)) {
          completed = true
          releaseCccdCompensation(characteristic, failure.operation)
          null
        } else {
          val retryFailure = try { cleanup() } catch (throwable: Throwable) {
            OwnedRadioTeardownFailure(failure.operation, throwable)
          }
          if (retryFailure == null) {
            completed = true
            releaseCccdCompensation(characteristic, failure.operation)
            localOnlyCccdCompensations[characteristic]?.let { marker ->
              if (marker.operation == failure.operation) localOnlyCccdCompensations.remove(characteristic, marker)
            }
            if (abandonOnSuccess) activeNativeSubscriptionOwnership.abandon(deviceId, generation, characteristic, owner)
          }
          retryFailure
        }
      }
    }
    reportCleanupFailure(failure)
  }

  /** Keeps a cancelled successful CCCD write owned until an asynchronous disable succeeds. */
  private fun scheduleCompensatingDisable(
    deviceId: String,
    serviceUuid: UUID,
    serviceOccurrence: Int,
    characteristicUuid: UUID,
    characteristicOccurrence: Int,
    gatt: BluetoothGatt,
    generation: Long,
    characteristic: BluetoothGattCharacteristic,
    rollbackOperation: String,
    owner: NotificationOwnershipFifo.Claim
  ) {
    val operation = "cccdCompensation:${deviceId.uppercase()}:generation=$generation:${nextGattOperationId.getAndIncrement()}"
    val inFlight = AtomicBoolean(false)
    val failure = OwnedRadioTeardownFailure(operation, IllegalStateException("CCCD compensation has not completed"))
    fun attempt(): OwnedRadioTeardownFailure? {
      if (!isCurrentGatt(deviceId.uppercase(), gatt, generation) ||
        !activeNativeSubscriptionOwnership.ownsToken(deviceId, generation, characteristic, owner)
      ) {
        releaseCccdCompensation(characteristic, operation)
        retryableCleanups.remove(rollbackOperation)
        return null
      }
      if (inFlight.compareAndSet(false, true)) {
        setNotifyTarget(
          deviceId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence,
          false, null, null, gatt, generation, owner
          , false, null, null
        ) { result ->
          inFlight.set(false)
          if (result.isSuccess) {
            if (activeNativeSubscriptionOwnership.ownsToken(deviceId, generation, characteristic, owner)) {
              activeNativeSubscriptionOwnership.abandon(deviceId, generation, characteristic, owner)
            }
            releaseCccdCompensation(characteristic, operation)
            retryableCleanups.remove(rollbackOperation)
            retryableCleanups.remove(operation)
          } else {
            reportCleanupFailure(OwnedRadioTeardownFailure(operation, result.exceptionOrNull()
              ?: IllegalStateException("CCCD compensation failed")))
          }
        }
      }
      return failure
    }
    retainCccdCompensation(characteristic, operation)
    registerRetryableCleanup(operation, CleanupScope(deviceId.uppercase(), generation, activeNativeSubscriptionOwnership.eventOwner(deviceId, generation, characteristic, owner))) { attempt() }
    if (attempt() == null) {
      releaseCccdCompensation(characteristic, operation)
      retryableCleanups.remove(operation)
    }
  }

  private fun rollbackNotifyRegistration(
    deviceKeyUpper: String,
    gatt: BluetoothGatt,
    generation: Long,
    characteristic: BluetoothGattCharacteristic,
    enabled: Boolean,
    operation: String,
    owner: NotificationOwnershipFifo.Claim?
  ): OwnedRadioTeardownFailure? = withNativeAdmission(gatt) { synchronized(callbackOwnerLock) { synchronized(notificationControlLock) {
    if (owner == null || !activeNativeSubscriptionOwnership.ownsToken(deviceKeyUpper, generation, characteristic, owner)) {
      return@synchronized null
    }
    if (retiringGatts[deviceKeyUpper]?.let { it.gatt === gatt && it.generation == generation } == true) {
      return@synchronized OwnedRadioTeardownFailure(operation, IllegalStateException("notification GATT generation is retiring"))
    }
    if (!isCurrentGatt(deviceKeyUpper, gatt, generation)) {
      // The old GATT is no longer owned by this attachment; its local
      // registration died with that exact generation and needs no retry.
      return@synchronized null
    }
    try {
      if (gatt.setCharacteristicNotification(characteristic, !enabled)) {
        null
      } else {
        OwnedRadioTeardownFailure(
          operation,
          AndroidNotificationRollbackRejected()
        )
      }
    } catch (throwable: Throwable) {
      OwnedRadioTeardownFailure(operation, throwable)
    }
  } } }

  private fun deliverNotification(
    gatt: BluetoothGatt,
    characteristic: BluetoothGattCharacteristic,
    value: ByteArray
  ) {
    if (!isCurrentGattCallback(gatt)) return
    val deviceKeyUpper = gatt.device.address.uppercase()
    val gattGeneration = gattGenerationByInstance[gatt] ?: return
    val drain = synchronized(orderedSourceLock) {
      val pinned = admissionGates[gatt]
      if (pinned?.graphPermit != null) {
        val owned = value.copyOf()
        enqueueAdmission(pinned, owned.size, false) { deliverNotification(gatt, characteristic, owned) }
        return@synchronized false
      }
      var startDrain = false
      val gate = admissionGates.getOrPut(gatt) {
        startDrain = true
        AdmissionGate().also { it.draining = true }
      }
      val ownedValue = value.copyOf()
      val source = orderedSources[gatt]
      if (source?.generation == gattGeneration) {
        // This gate is acquired before source.admit, not merely before the
        // later Rust relay. A fence can therefore never be observed idle and
        // then admit a native callback into the source store.
        val admission = orderedCallbackAdmission
        val lease = admission?.invoke()
        if (admission != null && lease == null) {
          OwnedAndroidLog.e("ordered notification rejected after owner retirement")
        } else try {
          source.admit(characteristic, ownedValue) { wake ->
            enqueueAdmission(gate, 0, true, wake = true) {
              try { wake() } catch (error: Throwable) { reportOrderedWakeFailure("orderedSource.valueWake", error) }
            }
          }
        } finally {
          lease?.close()
        }
      }
      enqueueAdmission(gate, ownedValue.size, false) {
        if (isCurrentGatt(gatt.device.address.uppercase(), gatt, gattGeneration)) {
          activeNativeSubscriptionOwnership.offer(deviceKeyUpper, gattGeneration, characteristic, ownedValue)
        }
      }
      startDrain
    }
    if (drain) drainAdmission(gatt)
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
      if (token == null || token.markPubliclySettled()) callback(result)
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

  private fun dispatchConnectionState(
    id: String,
    connected: Boolean,
    gattStatus: Int,
    attempt: GattConnectAttempt?
  ) {
    attempt?.observed?.set(true)
    // Observers are independent: diagnostics cannot prevent the attempt owner settling.
    runEvery(
      { connectionListeners[id.uppercase()]?.invoke(id, connected, gattStatus) },
      { onConnectionState?.invoke(id, connected, gattStatus) },
      { onConnectionOutcome?.invoke(id, connected, gattStatus, attempt) },
      { dispatchOwnedConnection(id, connected, gattStatus, attempt) }
    )?.let { throw it }
  }

  private val gattCallback = GattCallback()

  // Each native open gets a distinct callback, so a synchronous observation can never be
  // attributed to another concurrent attempt. Drain only after its returned GATT is published.
  private inner class OpeningGattCallback(private val attempt: GattConnectAttempt?) : GattCallback() {
    private inner class Event(val gatt: BluetoothGatt, val status: Int?, val state: Int?, val release: () -> Unit)
    private val lock = Any()
    private val pending = ArrayDeque<Event>()
    private var published: BluetoothGatt? = null
    private var rejected = false
    private var draining = false

    // Hold the admitting owner across the pre-publication wait as well as observer return.
    private fun retainOwner(): () -> Unit {
      val owner = synchronized(receiverRetirementLock) {
        attempt?.let { connectionRoutes[it]?.owner }?.also { it.callbacks++ }
      }
      return { if (owner != null) synchronized(receiverRetirementLock) { owner.callbacks-- } }
    }

    override fun onConnectionStateChange(gatt: BluetoothGatt, status: Int, newState: Int) {
      accept(gatt, status, newState)
    }

    override fun onServiceChanged(gatt: BluetoothGatt) {
      accept(gatt, null, null)
    }

    private fun deliver(event: Event) {
      if (event.status == null) super.onServiceChanged(event.gatt)
      else super.onConnectionStateChange(event.gatt, event.status, checkNotNull(event.state))
    }

    private fun accept(gatt: BluetoothGatt, status: Int?, newState: Int?) {
      val release = retainOwner()
      var queued = false
      val deliver = synchronized(lock) {
        when {
          rejected -> false
          published == null || draining -> {
            pending.addLast(Event(gatt, status, newState, release))
            queued = true
            false
          }
          else -> published === gatt
        }
      }
      if (queued) return
      try { if (deliver) deliver(Event(gatt, status, newState, release)) }
      finally { release() }
    }

    fun reject(): List<BluetoothGatt> {
      val abandoned = synchronized(lock) {
        rejected = true
        pending.toList().also { pending.clear() }
      }
      abandoned.forEach { it.release() }
      val seen = java.util.IdentityHashMap<BluetoothGatt, Boolean>()
      return abandoned.map { it.gatt }.filter { seen.put(it, true) == null }
    }

    fun publish(gatt: BluetoothGatt) {
      synchronized(lock) { published = gatt; draining = true }
      var error: Throwable? = null
      while (true) {
        val event = synchronized(lock) {
          if (pending.isEmpty()) { draining = false; null } else pending.removeFirst()
        } ?: break
        try {
          if (event.gatt === gatt) {
            error = mergeErrors(error, errorOf { deliver(event) })
          }
        } finally { event.release() }
      }
      error?.let { throw it }
    }
  }

  private open inner class GattCallback : BluetoothGattCallback() {
    override fun onConnectionStateChange(gatt: BluetoothGatt, status: Int, newState: Int) {
      if (newState == BluetoothProfile.STATE_DISCONNECTED || (newState == BluetoothProfile.STATE_CONNECTED && status != BluetoothGatt.GATT_SUCCESS)) {
        if (deferLifecycleControl(gatt, OrderedNotificationSource.Reason.CONNECTION_LOST) { onConnectionStateChange(gatt, status, newState) }) return
      } else if (deferControl(gatt) { onConnectionStateChange(gatt, status, newState) }) return
      val id = gatt.device.address
      val key = id.uppercase()
      val generation = gattGenerationByInstance[gatt] ?: return
      // Lifecycle callbacks must still finish a retiring generation.
      if (!isCurrentGatt(key, gatt, generation) || disconnectOwners.isClosing(key, gatt, generation)) return
      if (newState == BluetoothProfile.STATE_DISCONNECTED || (newState == BluetoothProfile.STATE_CONNECTED && status != BluetoothGatt.GATT_SUCCESS)) {
        fenceRetiringGatt(key, gatt, generation)
      } else if (!isCurrentGattCallback(gatt)) return
      val attempt = gattConnectAttempts[generation]
      if (newState == BluetoothProfile.STATE_CONNECTED) {
        if (status == BluetoothGatt.GATT_SUCCESS) {
          connectedEncryptionGenerations[key] = generation
          dispatchConnectionState(id, true, status, attempt)
        } else {
          // Non-success while "connected" is a failed connect — surface status and tear down.
          runEvery(
            { dispatchConnectionState(id, false, status, attempt) },
            { failPendingForDevice(key, "connect failed status=$status") },
            {
              completeGattTeardown(key, gatt, generation)
                .thenWhenClean { resumeQueuedReconnect(id, key, "reconnect after failed CONNECTED") }
            }
          )?.let { throw it }
        }
      } else if (newState == BluetoothProfile.STATE_DISCONNECTED) {
        connectedEncryptionGenerations.remove(key, generation)
        runEvery(
          { onOrderedLinkFence?.invoke(id, attempt) },
          { dispatchOwnedLinkFence(id, attempt) },
          {
            try {
              onSecurityState?.invoke(id, OwnedAndroidSecurityState(
                when (gatt.device.bondState) {
                  BluetoothDevice.BOND_BONDED -> "bonded"
                  BluetoothDevice.BOND_BONDING -> "bonding"
                  BluetoothDevice.BOND_NONE -> "notBonded"
                  else -> "unknown"
                }, hasBluetoothConnectPermission(), if (securitySdkInt >= 36) "unknown" else "unsupported"))
            } catch (error: Exception) { onSecurityFailure?.invoke(id, error) }
          },
          // Pass native status: a failed connection is not a clean disconnect.
          { dispatchConnectionState(id, false, status, attempt) },
          { failPendingForDevice(key, "disconnected status=$status", status) },
          {
            // A throwing observer or waiter cannot skip physical cleanup or a queued reconnect.
            completeGattTeardown(key, gatt, generation)
              .thenWhenClean { resumeQueuedReconnect(id, key, "reconnect after DISCONNECTED") }
          }
        )?.let { throw it }
      }
    }

    override fun onDescriptorWrite(
      gatt: BluetoothGatt,
      descriptor: BluetoothGattDescriptor,
      status: Int
    ) {
      if (deferControl(gatt) { onDescriptorWrite(gatt, descriptor, status) }) return
      if (!isCurrentGattCallback(gatt)) return
      exactDescriptorWritePending[descriptor]?.let { pending ->
        if (pending.gatt !== gatt || !isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) return
        if (!exactDescriptorWritePending.remove(descriptor, pending)) return
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
        if (pending.gatt !== gatt || !isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) return
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
              if (matchedKey.startsWith("cccd:")) "cccd-write" else "descriptor-write",
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
      @Suppress("DEPRECATION")
      val snapshot = descriptor.value?.copyOf()
      if (deferControl(gatt, replayEntry = false, bytes = snapshot?.size ?: 0) { handleDescriptorRead(gatt, descriptor, status, snapshot) }) return
      handleDescriptorRead(gatt, descriptor, status, snapshot)
    }

    override fun onDescriptorRead(
      gatt: BluetoothGatt,
      descriptor: BluetoothGattDescriptor,
      status: Int,
      value: ByteArray
    ) {
      val snapshot = value.copyOf()
      if (deferControl(gatt, replayEntry = false, bytes = snapshot.size) { handleDescriptorRead(gatt, descriptor, status, snapshot) }) return
      handleDescriptorRead(gatt, descriptor, status, snapshot)
    }

    private fun handleDescriptorRead(gatt: BluetoothGatt, descriptor: BluetoothGattDescriptor, status: Int, value: ByteArray?) {
      if (!isCurrentGattCallback(gatt)) return
      exactDescriptorReadPending[descriptor]?.let { pending ->
        if (pending.gatt !== gatt || !isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) return
        if (!exactDescriptorReadPending.remove(descriptor, pending)) return
        if (status == BluetoothGatt.GATT_SUCCESS) {
          completeExactByte(pending, Result.success(value?.copyOf()))
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
      val services = if (status == BluetoothGatt.GATT_SUCCESS) gatt.services.toList() else null
      if (deferControl(gatt, replayEntry = false) { handleServicesDiscovered(gatt, status, services) }) return
      handleServicesDiscovered(gatt, status, services)
    }

    private fun handleServicesDiscovered(gatt: BluetoothGatt, status: Int, services: List<BluetoothGattService>?) {
      val id = gatt.device.address.uppercase()
      val (work, callback) = synchronized(orderedSourceLock) {
        val work = nativeDiscoveryAdmissions[gatt] ?: return
        val callback = if (!isCurrentGattCallback(gatt) || !work.valid) null else {
          if (status == BluetoothGatt.GATT_SUCCESS) {
            clearCharCacheForDevice(id)
            publishNativeGraph(gatt, checkNotNull(services), work.owner)
          }
          work.callback?.takeIf { pending.remove("discover:$id", it) }
        }
        // The physical callback has arrived, so the next request may use this
        // GATT; the exact work remains charged until this callback returns.
        nativeDiscoveryAdmissions.remove(gatt, work)
        work to callback
      }
      try {
        callback?.invoke(if (status == BluetoothGatt.GATT_SUCCESS) Result.success(null)
          else Result.failure(IllegalStateException("discover status=$status")))
      } finally {
        synchronized(orderedSourceLock) {
          nativeDiscoveryAdmissions.remove(gatt, work)
          work.gatt = null
          finishDiscovery(work)
        }
      }
    }

    @Deprecated("Deprecated in Java")
    override fun onCharacteristicRead(
      gatt: BluetoothGatt,
      characteristic: BluetoothGattCharacteristic,
      status: Int
    ) {
      @Suppress("DEPRECATION")
      val snapshot = characteristic.value?.copyOf()
      if (deferControl(gatt, replayEntry = false, bytes = snapshot?.size ?: 0) { handleCharacteristicRead(gatt, characteristic, snapshot, status) }) return
      handleCharacteristicRead(gatt, characteristic, snapshot, status)
    }

    override fun onCharacteristicRead(
      gatt: BluetoothGatt,
      characteristic: BluetoothGattCharacteristic,
      value: ByteArray,
      status: Int
    ) {
      val snapshot = value.copyOf()
      if (deferControl(gatt, replayEntry = false, bytes = snapshot.size) { handleCharacteristicRead(gatt, characteristic, snapshot, status) }) return
      handleCharacteristicRead(gatt, characteristic, snapshot, status)
    }

    private fun handleCharacteristicRead(gatt: BluetoothGatt, characteristic: BluetoothGattCharacteristic, value: ByteArray?, status: Int) {
      if (!isCurrentGattCallback(gatt)) return
      exactReadPending[characteristic]?.let { pending ->
        if (pending.gatt !== gatt || !isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) return
        if (!exactReadPending.remove(characteristic, pending)) return
        if (status == BluetoothGatt.GATT_SUCCESS) {
          completeExactByte(pending, Result.success(value?.copyOf()))
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
      if (deferControl(gatt) { onCharacteristicWrite(gatt, characteristic, status) }) return
      if (!isCurrentGattCallback(gatt)) return
      // A provisional callback failure must remain pending while lifecycle
      // evidence already in flight gets a chance to claim the terminal.
      if (exactWriteTerminalArbiter.isPending(characteristic)) return
      exactWritePending[characteristic]?.let { pending ->
        val observedPayload = exactWriteValues[characteristic]
        if (pending.gatt !== gatt || !isCurrentGatt(pending.deviceKeyUpper, gatt, pending.gattGeneration)) return
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
      if (deferControl(gatt) { onMtuChanged(gatt, mtu, status) }) return
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
      if (deferControl(gatt) { onReadRemoteRssi(gatt, rssi, status) }) return
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
      if (deferControl(gatt) { onPhyRead(gatt, txPhy, rxPhy, status) }) return
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
      if (deferControl(gatt) { onPhyUpdate(gatt, txPhy, rxPhy, status) }) return
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
      if (deferLifecycleControl(gatt, OrderedNotificationSource.Reason.DATABASE_CHANGED) { onServiceChanged(gatt) }) return
      val id = gatt.device.address
      val key = id.uppercase()
      val generation = gattGenerationByInstance[gatt] ?: return
      if (!isCurrentGatt(key, gatt, generation) || disconnectOwners.isClosing(key, gatt, generation)) return
      val pendingError = errorOf { failPendingForDevice(key, "GATT database changed", linkLost = false) }
      val orderedWakeError = errorOf { invalidateOrderedSource(gatt, OrderedNotificationSource.Reason.DATABASE_CHANGED) }
      val subscriptionError = errorOf { invalidateNativeSubscriptionsForDatabaseChange(key, gatt, generation) }
      clearCharCacheForDevice(key)
      val observerError = runEvery(
        { onServicesChanged?.invoke(id) },
        { dispatchOwnedServices(id, gattConnectAttempts[generation]) }
      )
      val wakeReportError = orderedWakeError?.let { errorOf { reportOrderedWakeFailure("orderedSource.serviceChangedWake", it) } }
      val callbackError = mergeErrors(pendingError, mergeErrors(subscriptionError, mergeErrors(observerError, wakeReportError)))
      if (callbackError != null) throw callbackError
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
      private val physicalLock = Any()
      private var physicalInvocation = false
      private var physicalReturned = false
      private var draining = false
      private var cancellationDispatching = false
      private var cancellationRelease: (() -> Unit)? = null
      private val deferredCompletions = ArrayDeque<() -> Unit>()
      // All queue completions for this token are the same idempotent finish.
      private var deferredQueueCompletion: (() -> Unit)? = null
      private var ownerRegistered = false
      private var clearReason: Throwable? = null

      internal fun registerOwner(register: () -> Boolean): Boolean = synchronized(physicalLock) {
        if (cancelled.get()) false else register().also { if (it) ownerRegistered = true }
      }

      internal fun cancelForClear(reason: Throwable, settleIfUnowned: Boolean): Boolean = synchronized(physicalLock) {
        clearReason = reason
        cancelled.set(true)
        settleIfUnowned && !ownerRegistered && markPubliclySettled()
      }

      internal fun beforePhysicalFailure(): Throwable = synchronized(physicalLock) {
        clearReason ?: IllegalStateException("GATT operation cancelled before submission")
      }

      /** Retain callback ownership but let unrelated queued work run after cancellation.
       * Never release the queue while the platform invocation itself is still running. */
      fun invokePhysical(done: () -> Unit, action: () -> Unit): Boolean {
        synchronized(physicalLock) {
          if (cancelled.get()) return false
          physicalInvocation = true
          physicalReturned = false
          cancellationRelease = done
        }
        var failure: Throwable? = null
        try { action() } catch (error: Throwable) { failure = error }
        synchronized(physicalLock) { physicalReturned = true }
        try { drainAfterInvocation() } catch (error: Throwable) {
          val prior = failure
          if (prior == null) failure = error else if (prior !== error) prior.addSuppressed(error)
        }
        failure?.let { throw it }
        return true
      }

      private fun drainAfterInvocation() {
        val shouldDrain = synchronized(physicalLock) {
          if (draining || cancellationDispatching || (physicalInvocation && !physicalReturned)) false
          else { draining = true; true }
        }
        if (!shouldDrain) return
        var failure: Throwable? = null
        fun runPreservingFailure(effect: () -> Unit) {
          try { effect() } catch (error: Throwable) {
            val prior = failure
            if (prior == null) failure = error else if (prior !== error) prior.addSuppressed(error)
          }
        }
        // Cancellation may still be scheduling cleanup. Queue completion is
        // separate so every cleanup runs before a subsequent operation starts.
        var finish: (() -> Unit)? = null
        var release: (() -> Unit)? = null
        while (true) {
          val completion = synchronized(physicalLock) {
            if (cancellationDispatching) { draining = false; null }
            else if (deferredCompletions.isNotEmpty()) deferredCompletions.removeFirst()
            else {
              physicalInvocation = false
              draining = false
              finish = deferredQueueCompletion.also { deferredQueueCompletion = null }
              if (cancelled.get()) release = cancellationRelease.also { cancellationRelease = null }
              null
            }
          } ?: break
          runPreservingFailure(completion)
        }
        finish?.let { runPreservingFailure(it) }
        release?.let { runPreservingFailure(it) }
        failure?.let { throw it }
      }

      internal fun completeAfterInvocation(completion: () -> Unit) {
        val runNow = synchronized(physicalLock) {
          if (physicalInvocation) { deferredCompletions.addLast(completion); false } else true
        }
        if (runNow) completion()
      }

      internal fun completeQueueAfterInvocation(completion: () -> Unit) {
        val runNow = synchronized(physicalLock) {
          if (physicalInvocation || cancellationDispatching) { deferredQueueCompletion = completion; false } else true
        }
        if (runNow) completion()
      }

      fun isCancelled(): Boolean = cancelled.get()

      fun isPubliclySettled(): Boolean = publiclySettled.get()

      internal fun cancel() = synchronized(physicalLock) { cancelled.set(true) }

      internal fun cancelAndSettle(): Boolean = synchronized(physicalLock) {
        val won = markPubliclySettled()
        cancelled.set(true)
        if (won) cancellationDispatching = true
        won
      }

      internal fun cancellationDispatched() {
        synchronized(physicalLock) { cancellationDispatching = false }
        drainAfterInvocation()
      }

      internal fun releaseCancelled() {
        val release = synchronized(physicalLock) {
          if (!physicalInvocation) cancellationRelease.also { cancellationRelease = null } else null
        }
        release?.invoke()
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
      val removedBeforeStart: () -> Unit,
      val startFailure: (Throwable) -> Unit,
      val settleOnClearBeforeOwner: Boolean,
      val clearBeforeOwnerUsesCancellation: Boolean,
      val startFailureDelivered: AtomicBoolean = AtomicBoolean(false)
    )

    private data class RunningOperation(val operation: QueuedOperation, val done: () -> Unit)

    private val lock = Any()
    private val queue = ArrayDeque<QueuedOperation>()
    private val busy = AtomicBoolean(false)
    private val nextOperationId = AtomicLong(1L)
    private var running: RunningOperation? = null

    fun submit(op: (done: () -> Unit) -> Unit, onCancelled: () -> Unit): Long =
      submitCancellable({ _, done -> op(done) }, onCancelled, NO_START_FAILURE)

    fun submitCancellable(
      op: (token: GattOperationToken, done: () -> Unit) -> Unit,
      onCancelled: () -> Unit,
      onStartFailure: (Throwable) -> Unit = NO_START_FAILURE,
      onRemovedBeforeStart: () -> Unit = {},
      settleOnClearBeforeOwner: Boolean = false,
      clearBeforeOwnerUsesCancellation: Boolean = false
    ): Long {
      val operation = QueuedOperation(
        idProvider?.invoke() ?: nextOperationId.getAndIncrement(),
        GattOperationToken(),
        op,
        onCancelled,
        onRemovedBeforeStart,
        onStartFailure,
        settleOnClearBeforeOwner,
        clearBeforeOwnerUsesCancellation
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
      var activeCancellationWon = false
      synchronized(lock) {
        val queuedOperation = queue.firstOrNull { operation -> operation.id == operationId }
        if (queuedOperation != null) {
          queued = queuedOperation
          queue.remove(queuedOperation)
        } else if (running?.operation?.id == operationId) {
          active = running
          activeCancellationWon = active?.operation?.token?.cancelAndSettle() == true
        }
      }
      queued?.let {
        it.removedBeforeStart()
        invokeCancellation(it, IllegalStateException("GATT operation was cancelled"))
        return true
      }
      active?.let {
        try {
          if (activeCancellationWon) invokeCancellation(it.operation, IllegalStateException("GATT operation was cancelled"), settled = true)
        } finally {
          try {
            if (activeCancellationWon) it.operation.token.cancellationDispatched()
          } catch (error: Throwable) {
            OwnedAndroidLog.e("GattSerialQueue deferred cancellation", error)
          } finally {
            it.operation.token.releaseCancelled()
          }
        }
        return true
      }
      return false
    }

    fun clear(reason: Throwable = IllegalStateException("GATT operation queue was cancelled")) {
      val cancelled: List<QueuedOperation>
      val active: RunningOperation?
      var settleActive = false
      synchronized(lock) {
        cancelled = queue.toList()
        queue.clear()
        active = running
        active?.let { settleActive = it.operation.token.cancelForClear(reason, it.operation.settleOnClearBeforeOwner) }
        if (active == null) busy.set(false)
      }
      var held: Throwable? = errorOf {
        active?.let {
          if (settleActive) {
            if (it.operation.clearBeforeOwnerUsesCancellation) invokeCancellation(it.operation, reason, settled = true)
            else invokeStartFailure(it.operation, reason, settled = true)
          }
        }
      }
      cancelled.forEach { operation ->
        held = mergeErrors(held, runEvery(
          { operation.removedBeforeStart() },
          {
            if (reason is AndroidGattLinkLost && operation.startFailure !== NO_START_FAILURE) {
              // Never started: its caller learns the link loss itself (132).
              if (operation.token.markPubliclySettled()) invokeStartFailure(operation, reason, settled = true)
            } else {
              invokeCancellation(operation, reason)
            }
          }
        ))
      }
      // Registered owners are settled by failPendingForDevice. Registration
      // and cancellation share the token lock, so the scan cannot miss an
      // owner whose active cell was left responsible for settlement.
      held?.let { throw it }
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
        val finish: () -> Unit = {
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
        val done: () -> Unit = { next.token.completeQueueAfterInvocation(finish) }
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

    private fun invokeCancellation(operation: QueuedOperation, reason: Throwable, settled: Boolean = false) {
      if (!settled && !operation.token.markPubliclySettled()) return
      try {
        operation.cancel.invoke()
      } catch (throwable: Throwable) {
        OwnedAndroidLog.e("GattSerialQueue cancellation ($reason)", throwable)
      }
    }

    private fun invokeStartFailure(operation: QueuedOperation, error: Throwable, settled: Boolean = false) {
      if (!operation.startFailureDelivered.compareAndSet(false, true)) return
      if (!settled && !operation.token.markPubliclySettled()) return
      try {
        operation.startFailure.invoke(error)
      } catch (throwable: Throwable) {
        OwnedAndroidLog.e("GattSerialQueue start failure callback", throwable)
      }
    }

    private fun schedule(task: () -> Unit): Boolean = post?.invoke(task) ?: handler?.post(task) ?: false

    companion object {
      /** An operation that did not ask to hear why it never started. */
      val NO_START_FAILURE: (Throwable) -> Unit = {}
    }
  }

  companion object {
    private const val DEFAULT_PEER_LIFECYCLE_CAPACITY = 64
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

/**
 * Identity of one connect request: an opaque, typed ownership token the caller hands to
 * [OwnedAndroidGattRadio.connect] before any callback can run. The radio retains it for the GATT
 * generation that request opens and delivers it with every observation of that generation, so a
 * consumer correlates an outcome to its request by identity, never by peer address.
 */
internal class GattConnectAttempt {
  internal val observed = AtomicBoolean(false)
}
