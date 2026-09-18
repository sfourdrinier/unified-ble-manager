// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/OwnedRadioPort.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.os.Build
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidSecurityState
import com.sfourdrinier.unifiedblemanager.radio.OwnedRadioAdapterProtocolState
import com.sfourdrinier.unifiedblemanager.radio.resolveUuidOccurrence
import java.util.UUID

/**
 * Production [AndroidRadioPort]: the process's one [OwnedAndroidGattRadio]
 * (the same OS driver the legacy dispatcher used). Translation only — the
 * driver keeps GATT serialization, generation fencing, CCCD arbitration and
 * teardown ownership.
 */
class OwnedRadioPort(
  private val radio: OwnedAndroidGattRadio,
  private val log: (String) -> Unit
) : AndroidRadioPort {

  override fun setEvents(events: RadioPortEvents) {
    radio.onProtocolScanResult = { advertisement ->
      events.onAdvertisement(
        AdvertisementFacts(
          peerId = advertisement.deviceId,
          address = advertisement.deviceId,
          localName = advertisement.name,
          rssi = advertisement.rssi,
          txPower = advertisement.txPower,
          serviceUuids = advertisement.serviceUuids ?: emptyList(),
          manufacturerData = advertisement.manufacturerData?.map { it.companyIdentifier to it.value } ?: emptyList(),
          serviceData = advertisement.serviceData?.map { it.serviceUuid to it.value } ?: emptyList(),
          connectable = advertisement.connectable,
          solicitedServiceUuids = advertisement.solicitedServiceUuids,
          appearance = advertisement.appearance,
          rawRecord = advertisement.rawRecord?.copyOf()
        )
      )
    }
    radio.onConnectionState = { deviceId, connected, status -> events.onConnection(deviceId, connected, status) }
    radio.onServicesChanged = { deviceId -> events.onServicesChanged(deviceId) }
    radio.onProtocolNotification = { deviceId, characteristic, value ->
      val instance = instanceOf(deviceId, characteristic)
      if (instance == null) {
        events.onDropped(
          "notification",
          "value for ${characteristic.uuid} on $deviceId whose service is no longer in the database"
        )
      } else {
        events.onNotification(instance, value)
      }
    }
    radio.onSecurityState = { deviceId, state -> events.onSecurity(deviceId, securityFacts(state)) }
    radio.onScanFailed = { errorCode -> events.onScanFailed(errorCode) }
    radio.onAdapterState = { _ -> events.onAdapterState(adapterFacts(radio.currentProtocolAdapterState())) }
    radio.onCleanupFailure = { failure ->
      log("Android cleanup remains retryable operation=${failure.operation}: ${failure.throwable.message}")
    }
    radio.registerAdapterStateReceiver()
    radio.registerBondStateReceiver()
  }

  override fun adapterState(): AdapterFacts = adapterFacts(radio.currentProtocolAdapterState())

  private fun requireRadioReady() {
    val state = radio.currentProtocolAdapterState()
    val reason = state.safeReason
    when {
      state.availability != "available" ->
        throw RadioPortFailure(RadioFailureKind.ADAPTER_UNAVAILABLE, reason ?: "Bluetooth adapter is ${state.availability}")
      state.authorization == "restricted" ->
        throw RadioPortFailure(RadioFailureKind.PERMISSION_RESTRICTED, reason ?: "Bluetooth use is restricted")
      state.authorization == "not-determined" ->
        throw RadioPortFailure(RadioFailureKind.PERMISSION_NOT_DETERMINED, reason ?: "Bluetooth permission is not determined")
      state.authorization != "granted" ->
        throw RadioPortFailure(RadioFailureKind.PERMISSION_DENIED, reason ?: "Bluetooth is not authorized")
      state.power == "resetting" ->
        throw RadioPortFailure(RadioFailureKind.ADAPTER_RESETTING, "Bluetooth adapter is resetting")
      state.power == "off" ->
        throw RadioPortFailure(RadioFailureKind.ADAPTER_OFF, "Bluetooth adapter is off")
      state.power != "on" ->
        throw RadioPortFailure(RadioFailureKind.ADAPTER_UNAVAILABLE, reason ?: "Bluetooth adapter power is ${state.power}")
    }
  }

  override fun startScan(serviceUuids: List<String>, deviceAddresses: List<String>, mode: Int, callbackType: Int, legacy: Boolean) {
    requireRadioReady()
    try {
      radio.startScan(
        serviceUuids = serviceUuids.toTypedArray(),
        scanMode = mode,
        callbackType = callbackType,
        legacyScan = legacy,
        allowDuplicates = true,
        deviceAddresses = deviceAddresses.toTypedArray()
      )
    } catch (error: IllegalArgumentException) {
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, error.message ?: "scan settings unsupported", cause = error)
    }
  }

  override fun stopScan(): Throwable? = radio.stopScan()?.throwable

  override fun supportsConnectPhy(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O

  override fun connect(peerId: String, autoConnect: Boolean, phyMask: Int) {
    requireRadioReady()
    radio.connect(peerId, autoConnect, phyMask)
  }

  override fun disconnect(peerId: String, onComplete: (Throwable?) -> Unit) {
    radio.disconnect(peerId) { failure -> onComplete(failure?.throwable) }
  }

  override fun discover(peerId: String, onResult: (Result<List<GattServiceNode>>) -> Unit): Long =
    radio.discover(peerId) { result ->
      // The driver's own failure crosses as is: a link loss stays an
      // AndroidGattLinkLost, which the host adapter reports as NOT_CONNECTED.
      val failure = result.exceptionOrNull()
      if (failure != null) {
        onResult(Result.failure(failure))
      } else {
        onResult(Result.success(radio.services(peerId).map { service ->
          GattServiceNode(
            service.uuid.toString(),
            service.characteristics.map { characteristic ->
              GattCharacteristicNode(
                characteristic.uuid.toString(),
                characteristic.properties,
                characteristic.descriptors.map { descriptor -> GattDescriptorNode(descriptor.uuid.toString()) }
              )
            }
          )
        }))
      }
    }

  override fun characteristic(instance: CharacteristicInstance): CharacteristicFacts? {
    val target = resolve(instance) ?: return null
    return CharacteristicFacts(target.properties, target.getDescriptor(OwnedAndroidGattRadio.CCCD_UUID) != null)
  }

  override fun read(instance: CharacteristicInstance, onResult: (Result<ByteArray>) -> Unit): Long {
    val path = path(instance)
    return radio.readCharacteristicExact(instance.peerId, path.service, path.serviceOccurrence, path.characteristic, path.characteristicOccurrence) { result ->
      onResult(result.map { it ?: ByteArray(0) })
    }
  }

  override fun write(instance: CharacteristicInstance, value: ByteArray, withResponse: Boolean, onResult: (Result<Unit>) -> Unit): Long {
    val path = path(instance)
    return radio.writeCharacteristicExact(
      instance.peerId, path.service, path.serviceOccurrence, path.characteristic, path.characteristicOccurrence, value, withResponse
    ) { result -> onResult(result.map { }) }
  }

  override fun readDescriptor(descriptor: DescriptorInstance, onResult: (Result<ByteArray>) -> Unit): Long {
    val path = path(descriptor.characteristic)
    return radio.readDescriptorExact(
      descriptor.characteristic.peerId,
      path.service,
      path.serviceOccurrence,
      path.characteristic,
      path.characteristicOccurrence,
      uuid(descriptor.descriptorUuid),
      occurrence(descriptor.descriptorOccurrence)
    ) { result -> onResult(result.map { it ?: ByteArray(0) }) }
  }

  override fun writeDescriptor(descriptor: DescriptorInstance, value: ByteArray, onResult: (Result<Unit>) -> Unit): Long {
    val path = path(descriptor.characteristic)
    return radio.writeDescriptorExact(
      descriptor.characteristic.peerId,
      path.service,
      path.serviceOccurrence,
      path.characteristic,
      path.characteristicOccurrence,
      uuid(descriptor.descriptorUuid),
      occurrence(descriptor.descriptorOccurrence),
      value,
      onResult
    )
  }

  override fun setNotification(instance: CharacteristicInstance, enable: Boolean, mode: String?, onResult: (Result<Unit>) -> Unit): Long {
    val path = path(instance)
    return radio.setNotifyExact(
      instance.peerId,
      path.service,
      path.serviceOccurrence,
      path.characteristic,
      path.characteristicOccurrence,
      enable,
      subscriptionType = mode,
      onResult = onResult
    )
  }

  override fun readMtu(peerId: String, onResult: (Result<Int?>) -> Unit): Long = radio.readEffectiveMtu(peerId, onResult)

  override fun requestMtu(peerId: String, mtu: Int, onResult: (Result<Int>) -> Unit): Long = radio.requestMtu(peerId, mtu, onResult)

  override fun readRssi(peerId: String, onResult: (Result<Int>) -> Unit): Long = radio.readRemoteRssi(peerId, onResult)

  override fun requestConnectionPriority(peerId: String, priority: String, onResult: (Result<Boolean>) -> Unit): Long {
    val level = when (priority) {
      "low-power" -> BluetoothGatt.CONNECTION_PRIORITY_LOW_POWER
      "balanced" -> BluetoothGatt.CONNECTION_PRIORITY_BALANCED
      "high-throughput" -> BluetoothGatt.CONNECTION_PRIORITY_HIGH
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "connection priority '$priority' is not an Android priority")
    }
    return radio.requestConnectionPriority(peerId, level, onResult)
  }

  override fun readPhy(peerId: String, onResult: (Result<PhyFacts>) -> Unit): Long {
    requirePhyApi()
    return radio.readPhy(peerId) { result -> onResult(result.map { PhyFacts(wirePhy(it.txPhy), wirePhy(it.rxPhy)) }) }
  }

  override fun requestPhy(peerId: String, tx: String?, rx: String?, onResult: (Result<PhyFacts?>) -> Unit): Long {
    requirePhyApi()
    return radio.requestPhy(
      peerId,
      OwnedAndroidGattRadio.phyMaskValue(driverPhy(tx)),
      OwnedAndroidGattRadio.phyMaskValue(driverPhy(rx))
    ) { result -> onResult(result.map { phy -> phy?.let { PhyFacts(wirePhy(it.txPhy), wirePhy(it.rxPhy)) } }) }
  }

  override fun securityState(peerId: String): SecurityFacts = securityFacts(radio.securityState(peerId))

  override fun createBond(peerId: String, transport: String, onResult: (Result<SecurityFacts>) -> Unit) {
    requireRadioReady()
    val driverTransport = when (transport) {
      "auto" -> "platformDefault"
      "le" -> "le"
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "pair transport '$transport' is not supported on Android")
    }
    radio.pair(peerId, driverTransport) { outcome, state ->
      when (outcome) {
        "paired", "alreadyPaired", "rejected" -> onResult(Result.success(securityFacts(state)))
        else -> onResult(
          Result.failure(RadioPortFailure(RadioFailureKind.PLATFORM, "Android did not report a recognized terminal bond state"))
        )
      }
    }
  }

  override fun bondedPeers(): List<BondedPeerFacts> {
    requireRadioReady()
    return radio.bondedPeerSnapshots().map { BondedPeerFacts(it.nativePeerId, it.displayName) }
  }

  override fun cancel(operationId: Long): Boolean = radio.cancelOperation(operationId)

  private data class DriverPath(
    val service: UUID,
    val serviceOccurrence: Int,
    val characteristic: UUID,
    val characteristicOccurrence: Int
  )

  private fun path(instance: CharacteristicInstance) = DriverPath(
    uuid(instance.serviceUuid),
    occurrence(instance.serviceOccurrence),
    uuid(instance.characteristicUuid),
    occurrence(instance.characteristicOccurrence)
  )

  private fun resolve(instance: CharacteristicInstance): BluetoothGattCharacteristic? {
    val path = path(instance)
    val service = resolveUuidOccurrence(radio.services(instance.peerId), path.service, path.serviceOccurrence) { it.uuid }
      ?: return null
    return resolveUuidOccurrence(service.characteristics, path.characteristic, path.characteristicOccurrence) { it.uuid }
  }

  private fun instanceOf(deviceId: String, characteristic: BluetoothGattCharacteristic): CharacteristicInstance? {
    val service = characteristic.service ?: return null
    val serviceOccurrence = radio.services(deviceId)
      .asSequence()
      .filter { candidate -> candidate.uuid == service.uuid }
      .takeWhile { candidate -> candidate !== service }
      .count()
    val characteristicOccurrence = service.characteristics
      .asSequence()
      .filter { candidate -> candidate.uuid == characteristic.uuid }
      .takeWhile { candidate -> candidate !== characteristic }
      .count()
    return CharacteristicInstance(
      deviceId,
      service.uuid.toString(),
      serviceOccurrence.toLong(),
      characteristic.uuid.toString(),
      characteristicOccurrence.toLong()
    )
  }

  private fun requirePhyApi() {
    if (android.os.Build.VERSION.SDK_INT < android.os.Build.VERSION_CODES.O) {
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "Android PHY requires API 26")
    }
  }

  companion object {
    private fun uuid(value: String): UUID = try {
      UUID.fromString(value)
    } catch (error: IllegalArgumentException) {
      throw RadioPortFailure(RadioFailureKind.PATH_STALE, "'$value' is not a canonical UUID", cause = error)
    }

    private fun occurrence(value: Long): Int {
      if (value < 0 || value > Int.MAX_VALUE) {
        throw RadioPortFailure(RadioFailureKind.PATH_STALE, "occurrence $value is outside the Android database")
      }
      return value.toInt()
    }

    internal fun adapterFacts(state: OwnedRadioAdapterProtocolState) =
      AdapterFacts(state.availability, state.authorization, state.power, state.safeReason)

    /**
     * Android reports the bond state only; link encryption, authentication and
     * Secure Connections have no public API, so they are `unsupported`.
     */
    internal fun securityFacts(state: OwnedAndroidSecurityState) = SecurityFacts(
      bond = when (state.bond) {
        "bonded" -> "bonded"
        "bonding" -> "bonding"
        "notBonded" -> "not-bonded"
        else -> "unknown"
      },
      encryption = "unsupported",
      authentication = "unsupported",
      secureConnections = "unsupported",
      pairingPossible = state.pairingPossible
    )

    internal fun wirePhy(driver: String): String = when (driver) {
      "le1m" -> "le-1m"
      "le2m" -> "le-2m"
      "leCoded" -> "le-coded"
      else -> throw RadioPortFailure(RadioFailureKind.PLATFORM, "Android reported unknown PHY '$driver'")
    }

    internal fun driverPhy(wire: String?): String? = when (wire) {
      null -> null
      "le-1m" -> "le1m"
      "le-2m" -> "le2m"
      "le-coded" -> "leCoded"
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "PHY '$wire' is not an Android LE PHY")
    }
  }
}
