// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/AndroidRadioPort.kt

package com.sfourdrinier.unifiedblemanager.rustcore

/**
 * The Android OS driver as the Rust radio adapter sees it: plain values and
 * callbacks, no `android.bluetooth` types. Production is [OwnedRadioPort]
 * over `OwnedAndroidGattRadio`; JVM tests script a fake. Every asynchronous
 * operation returns the driver's queue operation id (0 when the operation
 * is not queue-cancellable) and answers its callback exactly once.
 *
 * A failure the driver can classify is thrown or delivered as
 * [RadioPortFailure]; anything else is classified by
 * [RustRadioHostAdapter.classify].
 */
interface AndroidRadioPort {
  fun setEvents(events: RadioPortEvents)
  fun adapterState(): AdapterFacts

  /** Starts the one physical scan; throws when the OS refuses. `mode`/`callbackType` are `ScanSettings` values. */
  fun startScan(serviceUuids: List<String>, deviceAddresses: List<String>, mode: Int, callbackType: Int, legacy: Boolean)

  /** Stops the scan; the returned failure keeps scan ownership for a retry. */
  fun stopScan(): Throwable?

  /** Whether the OS can establish a link on caller-chosen PHYs (`connectGatt(…, phy)`, API 26+). */
  fun supportsConnectPhy(): Boolean

  /**
   * Opens the link; the outcome arrives through [RadioPortEvents.onConnection].
   * [phyMask] is `BluetoothDevice.PHY_LE_*_MASK` bits to establish the link on (0 = no
   * preference); the caller only passes one where [supportsConnectPhy] and never with [autoConnect].
   */
  fun connect(peerId: String, autoConnect: Boolean, phyMask: Int)

  /** Releases the link; [onComplete] receives the teardown failure, or null once the OS confirmed. */
  fun disconnect(peerId: String, onComplete: (Throwable?) -> Unit)

  fun discover(peerId: String, onResult: (Result<List<GattServiceNode>>) -> Unit): Long

  /** The characteristic at [instance], or null when the path is not in the current database. */
  fun characteristic(instance: CharacteristicInstance): CharacteristicFacts?

  fun read(instance: CharacteristicInstance, onResult: (Result<ByteArray>) -> Unit): Long
  fun write(instance: CharacteristicInstance, value: ByteArray, withResponse: Boolean, onResult: (Result<Unit>) -> Unit): Long
  fun readDescriptor(descriptor: DescriptorInstance, onResult: (Result<ByteArray>) -> Unit): Long
  fun writeDescriptor(descriptor: DescriptorInstance, value: ByteArray, onResult: (Result<Unit>) -> Unit): Long

  /** `mode` is the exact CCCD mode to write (`notification`/`indication`); null when disabling. */
  fun setNotification(instance: CharacteristicInstance, enable: Boolean, mode: String?, onResult: (Result<Unit>) -> Unit): Long

  fun readMtu(peerId: String, onResult: (Result<Int?>) -> Unit): Long
  fun requestMtu(peerId: String, mtu: Int, onResult: (Result<Int>) -> Unit): Long
  fun readRssi(peerId: String, onResult: (Result<Int>) -> Unit): Long
  fun requestConnectionPriority(peerId: String, priority: String, onResult: (Result<Boolean>) -> Unit): Long
  fun readPhy(peerId: String, onResult: (Result<PhyFacts>) -> Unit): Long

  /** `tx`/`rx` are wire PHY names or null (no preference). The observation is null when the OS reported none. */
  fun requestPhy(peerId: String, tx: String?, rx: String?, onResult: (Result<PhyFacts?>) -> Unit): Long

  fun securityState(peerId: String): SecurityFacts

  /** `transport` is `auto`/`le`; the callback receives the terminal bond state. */
  fun createBond(peerId: String, transport: String, onResult: (Result<SecurityFacts>) -> Unit)

  fun bondedPeers(): List<BondedPeerFacts>

  /** Cancels a queued/running driver operation; false when it is no longer cancellable. */
  fun cancel(operationId: Long): Boolean
}

/** Unsolicited OS facts from the driver. */
interface RadioPortEvents {
  fun onAdvertisement(advertisement: AdvertisementFacts)
  fun onConnection(peerId: String, connected: Boolean, gattStatus: Int)
  fun onServicesChanged(peerId: String)
  fun onNotification(instance: CharacteristicInstance, value: ByteArray)
  fun onAdapterState(state: AdapterFacts)
  fun onScanFailed(errorCode: Int)
  fun onSecurity(peerId: String, security: SecurityFacts)

  /** A platform fact the driver could not translate (never silently dropped). */
  fun onDropped(ingressClass: String, detail: String)
}

/** Failure kinds of the frozen radio interface (`docs/MOBILE_RUST_WIRE.md`). */
enum class RadioFailureKind(val wire: String) {
  NOT_CONNECTED("not-connected"),
  PEER_UNKNOWN("peer-unknown"),
  PATH_STALE("path-stale"),
  BUSY("busy"),
  PERMISSION_DENIED("permission-denied"),
  PERMISSION_RESTRICTED("permission-restricted"),
  PERMISSION_NOT_DETERMINED("permission-not-determined"),
  ADAPTER_OFF("adapter-off"),
  ADAPTER_UNAVAILABLE("adapter-unavailable"),
  ADAPTER_RESETTING("adapter-resetting"),
  GATT_STATUS("gatt-status"),
  CANCELLED("cancelled"),
  UNSUPPORTED("unsupported"),
  PLATFORM("platform")
}

/**
 * One failed request. [dispatched] is false only when the platform refused
 * before anything could reach the peer (a failed write then reports commit
 * `not-dispatched`); true whenever the request may have reached it.
 */
data class RadioFailure(
  val kind: RadioFailureKind,
  val gattStatus: Int?,
  val detail: String,
  val dispatched: Boolean = true,
  /** The platform's own named failure code, as legacy reported it (finding 133). */
  val nativeCode: String? = null
)

/** A failure the driver already classified; [nativeCode] is legacy's named code, if any. */
class RadioPortFailure(
  val kind: RadioFailureKind,
  detail: String,
  val gattStatus: Int? = null,
  cause: Throwable? = null,
  val nativeCode: String? = null
) : RuntimeException(detail, cause)

data class CharacteristicInstance(
  val peerId: String,
  val serviceUuid: String,
  val serviceOccurrence: Long,
  val characteristicUuid: String,
  val characteristicOccurrence: Long
)

data class DescriptorInstance(
  val characteristic: CharacteristicInstance,
  val descriptorUuid: String,
  val descriptorOccurrence: Long
)

/** `properties` are Android `BluetoothGattCharacteristic.PROPERTY_*` bits. */
data class CharacteristicFacts(val properties: Int, val hasCccd: Boolean)

data class GattDescriptorNode(val uuid: String)
data class GattCharacteristicNode(val uuid: String, val properties: Int, val descriptors: List<GattDescriptorNode>)
data class GattServiceNode(val uuid: String, val characteristics: List<GattCharacteristicNode>)

data class AdapterFacts(val availability: String, val authorization: String, val power: String, val safeReason: String?)

/** Wire strings of the frozen security vocabulary. */
data class SecurityFacts(
  val bond: String,
  val encryption: String,
  val authentication: String,
  val secureConnections: String,
  val pairingPossible: Boolean?
)

data class BondedPeerFacts(val peerId: String, val name: String?)

/** Wire PHY names (`le-1m`, `le-2m`, `le-coded`). */
data class PhyFacts(val tx: String, val rx: String)

data class AdvertisementFacts(
  val peerId: String,
  val address: String?,
  val localName: String?,
  val rssi: Int?,
  val txPower: Int?,
  val serviceUuids: List<String>,
  val manufacturerData: List<Pair<Int, ByteArray>>,
  val serviceData: List<Pair<String, ByteArray>>,
  val connectable: Boolean?,
  val solicitedServiceUuids: List<String>?,
  /** GAP Appearance (AD type 0x19); null when the advertisement did not carry it. */
  val appearance: Int? = null,
  /** The raw advertising record bytes (`ScanRecord.getBytes()`); null when not reported. */
  val rawRecord: ByteArray? = null
)
