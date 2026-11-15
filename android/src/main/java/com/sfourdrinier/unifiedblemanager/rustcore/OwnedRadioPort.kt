// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/OwnedRadioPort.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.os.Build
import com.sfourdrinier.unifiedblemanager.radio.GattConnectAttempt
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.NotificationOwnershipFifo
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidNotificationObserver
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidSecurityState
import com.sfourdrinier.unifiedblemanager.radio.OwnedRadioAdapterProtocolState
import com.sfourdrinier.unifiedblemanager.radio.resolveUuidOccurrence
import java.util.UUID

/** Preserve typed failure identity while keeping arbitrary exception text off the wire. */
internal fun classifyAndroidNotificationStop(reason: NotificationOwnershipFifo.Stop): RadioFailure = when (reason) {
  is NotificationOwnershipFifo.Stop.DeliveryUncertain -> {
    val classified = RustRadioHostAdapter.classify(reason.cause)
    classified.copy(
      detail = "Android notification ingress delivery failed",
      nativeCode = classified.nativeCode ?: "NOTIFICATION_DELIVERY_UNCERTAIN"
    )
  }
  is NotificationOwnershipFifo.Stop.ControlCapacity -> RadioFailure(
    RadioFailureKind.PLATFORM, null, "Android notification ingress control capacity exhausted",
    nativeCode = "NOTIFICATION_CONTROL_CAPACITY"
  )
}

/**
 * Production [AndroidRadioPort]: the process's one [OwnedAndroidGattRadio]
 * (the same OS driver the legacy dispatcher used). Translation only — the
 * driver keeps GATT serialization, generation fencing, CCCD arbitration and
 * teardown ownership.
 */
internal class OwnedRadioPort private constructor(
  private val radio: OwnedAndroidGattRadio,
  private val orderedPort: OwnedOrderedGroupPort,
  private val log: (String) -> Unit
) : AndroidRadioPort, ScanIdentityRadioPort, ScopedDiscoveryPort, OrderedPhysicalPort, OwnerReceiverRetirementPort, OwnerGraphFencePort, OwnerOrderedStoreSnapshotPort by orderedPort, OrderedGroupPort by orderedPort {
  constructor(radio: OwnedAndroidGattRadio, log: (String) -> Unit) : this(radio, OwnedOrderedGroupPort(radio), log)
  private val scanOwner = Any()
  private val receiverOwner = OwnedAndroidGattRadio.ReceiverOwner()
  private lateinit var scanEvents: RadioPortEvents
  private lateinit var notificationObserver: OwnedAndroidNotificationObserver
  private lateinit var connectionEvents: RadioPortEvents

  override fun retireReceivers(): OwnerReceiverRetirement {
    val result = radio.retireReceiverOwner(receiverOwner)
    return OwnerReceiverRetirement(result.callbacks, result.failures.map { it.throwable })
  }

  override fun fenceGraphOwner() = orderedPort.fenceGraphOwner()
  override fun graphOwnerStatus() = orderedPort.graphOwnerStatus()

  override fun setEvents(events: RadioPortEvents) = radio.registerReceiverOwner(
    receiverOwner,
    adapter = { events.onAdapterState(adapterFacts(radio.currentProtocolAdapterState())) },
    security = { deviceId, state -> events.onSecurity(deviceId, securityFacts(state)) },
    securityFailure = { deviceId, error -> events.onSecurityFailure(deviceId, error) },
    platformFact = { ingressClass, detail, count -> events.onDropped(ingressClass, detail, count) }
  ) {
    scanEvents = events
    connectionEvents = events
    notificationObserver = OwnedAndroidNotificationObserver(
      owner = receiverOwner,
      event = { device, _, _, epoch, path, event ->
        radio.withReceiverOwnerCallback(receiverOwner) {
          when (event) {
            is NotificationOwnershipFifo.Event.Value -> {
              val sourcePath = path ?: throw IllegalStateException("notification source path is unavailable")
              val sourceEpoch = epoch ?: throw IllegalStateException("notification source epoch is unavailable")
              events.onNotification(CharacteristicInstance(device, sourcePath.serviceUuid.toString(),
                sourcePath.serviceOccurrence.toLong(), sourcePath.characteristicUuid.toString(),
                sourcePath.characteristicOccurrence.toLong()), sourceEpoch, event.bytes.copyOf())
            }
            is NotificationOwnershipFifo.Event.Loss ->
              events.onDropped("notification", "Android notification ingress staging was full", event.items)
            is NotificationOwnershipFifo.Event.Terminal -> Unit
          }
        }
      },
      stopped = { device, _, _, epoch, path, reason ->
        radio.withReceiverOwnerCallback(receiverOwner) {
          val sourceEpoch = epoch ?: throw IllegalStateException("failed notification source has no enable epoch")
          val sourcePath = path ?: throw IllegalStateException("failed notification source has no characteristic path")
          events.onNotificationSourceFailed(CharacteristicInstance(device, sourcePath.serviceUuid.toString(),
            sourcePath.serviceOccurrence.toLong(), sourcePath.characteristicUuid.toString(),
            sourcePath.characteristicOccurrence.toLong()), sourceEpoch, classifyAndroidNotificationStop(reason))
        }
      }
    )
    radio.onAdmissionOverflow = { _, notifications, controls ->
      var failure: Throwable? = null
      if (notifications > 0) try { events.onDropped("notification", "Android reentrant callback staging overflow", notifications) }
      catch (error: Throwable) { failure = error }
      if (controls > 0) try { events.onDropped("control", "Android reentrant callback staging overflow", controls) }
      catch (error: Throwable) { if (failure == null) failure = error else failure?.addSuppressed(error) }
      failure?.let { throw it }
    }
    radio.onCleanupFailure = { failure ->
      log("Android cleanup remains retryable operation=${failure.operation}: ${failure.throwable.message}")
    }
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

  override fun startScan(serviceUuids: List<String>, deviceAddresses: List<String>, mode: Int, callbackType: Int,
                         legacy: Boolean, reportDelayMs: Long, phy: Int?) =
    startScanWithIdentity(serviceUuids, deviceAddresses, mode, callbackType, legacy, reportDelayMs, phy, java.util.UUID.randomUUID().toString())

  override fun startScanWithIdentity(serviceUuids: List<String>, deviceAddresses: List<String>, mode: Int, callbackType: Int,
                         legacy: Boolean, reportDelayMs: Long, phy: Int?, scanIdentity: String) {
    requireRadioReady()
    try {
      val events = scanEvents
      radio.startOwnedScan(
        owner = scanOwner,
        serviceUuids = serviceUuids.toTypedArray(),
        scanMode = mode,
        callbackType = callbackType,
        legacyScan = legacy,
        allowDuplicates = true,
        deviceAddresses = deviceAddresses.toTypedArray(),
        reportDelayMs = reportDelayMs,
        scanPhy = phy,
        onAdvertisement = { advertisement -> events.onAdvertisement(AdvertisementFacts(
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
          rawRecord = advertisement.rawRecord?.copyOf(),
          cachedName = advertisement.cachedName,
          captureTimestampMs = advertisement.captureTimestampMs,
          scanIdentity = scanIdentity
        )) },
        onFailure = { code, stop -> events.onScanFailed(code, scanIdentity) {
          val (stale, failure) = stop()
          RadioScanStop(stale, failure?.throwable)
        } }
      )
    } catch (error: IllegalArgumentException) {
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, error.message ?: "scan settings unsupported", cause = error)
    }
  }

  override fun stopScan(): Throwable? = radio.stopOwnedScan(scanOwner)?.throwable

  override fun supportsConnectPhy(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
  override fun supportsSubrate(): Boolean = radio.subrateAvailable()

  override fun requestSubrate(peerId: String, mode: String, onResult: (Result<Int>) -> Unit): Long =
    radio.requestSubrate(peerId, mode, onResult)

  override fun connect(peerId: String, autoConnect: Boolean, phyMask: Int, attempt: GattConnectAttempt) {
    requireRadioReady()
    val events = connectionEvents
    radio.connectOwned(peerId, autoConnect, phyMask, attempt, receiverOwner,
      { device, connected, status, ownerAttempt -> events.onConnection(device, connected, status, ownerAttempt) },
      { device -> events.onServicesChanged(device) },
      { device, ownerAttempt -> events.onOrderedLinkFence(device, ownerAttempt) })
  }

  override fun disconnect(peerId: String, onComplete: (Throwable?) -> Unit) {
    radio.disconnectOwned(peerId, receiverOwner) { failure -> onComplete(failure?.throwable) }
  }

  override fun cancelConnect(peerId: String, attempt: GattConnectAttempt, onComplete: (Throwable?) -> Unit) {
    radio.cancelConnect(peerId, attempt) { failure -> onComplete(failure?.throwable) }
  }

  override fun discover(peerId: String, onResult: (Result<List<GattServiceNode>>) -> Unit): Long =
    discoverScoped(peerId) { result -> onResult(result.map { it.services }) }

  override fun discoverScoped(peerId: String, onResult: (Result<ScopedGattGraph>) -> Unit): Long =
    orderedPort.withGraphOwner { radio.discover(peerId) { result ->
      // The driver's own failure crosses as is: a link loss stays an
      // AndroidGattLinkLost, which the host adapter reports as NOT_CONNECTED.
      val failure = result.exceptionOrNull()
      if (failure != null) {
        onResult(Result.failure(failure))
      } else {
        onResult(runCatching {
          val (identity, tree) = radio.withNativeGraph(peerId) { services -> services.map { service ->
          GattServiceNode(
            service.uuid.toString(),
            service.characteristics.map { characteristic ->
              GattCharacteristicNode(
                characteristic.uuid.toString(),
                characteristic.properties,
                characteristic.descriptors.map { descriptor -> GattDescriptorNode(descriptor.uuid.toString()) }
              )
            },
            primary = when (service.type) {
              android.bluetooth.BluetoothGattService.SERVICE_TYPE_PRIMARY -> true
              android.bluetooth.BluetoothGattService.SERVICE_TYPE_SECONDARY -> false
              else -> throw IllegalStateException("Android reported an unknown service type")
            },
            includedServices = service.includedServices.map { target ->
              val index = services.indexOfFirst { it === target }
              require(index >= 0) { "An included service is absent from the current graph" }
              GattIncludedServiceNode(target.uuid.toString(), services.take(index).count { it.uuid == target.uuid }.toLong())
            }
          )
          } }
          ScopedGattGraph(identity, tree)
        })
      }
    } }

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

  override fun setNotification(instance: CharacteristicInstance, enable: Boolean, mode: String?, epoch: Long?, onResult: (Result<Unit>) -> Unit): Long {
    val path = path(instance)
    return radio.setNotifyExactOwned(
      instance.peerId,
      path.service,
      path.serviceOccurrence,
      path.characteristic,
      path.characteristicOccurrence,
      enable,
      subscriptionType = mode,
      sourceEpoch = epoch,
      observer = notificationObserver,
      onResult = onResult
    )
  }

  override fun setOrderedNotification(instance: CharacteristicInstance, enable: Boolean, mode: String?, epoch: Long, admission: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedAdmission?,
                                      onResult: (Result<Unit>) -> Unit): Long {
    val path = path(instance)
    return radio.setNotifyExactRetained(instance.peerId, path.service, path.serviceOccurrence, path.characteristic,
      path.characteristicOccurrence, enable, mode, epoch, admission, notificationObserver, onResult)
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

  override fun securityState(peerId: String): SecurityFacts = try { securityFacts(radio.securityState(peerId)) }
  catch (error: SecurityException) {
    throw RadioPortFailure(RadioFailureKind.PERMISSION_DENIED, error.message ?: "Bluetooth security observation denied",
      cause = error, nativeCode = error.javaClass.name)
  }

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

  override fun resolvePeer(peerId: String): ConnectedPeerFacts? {
    requireRadioReady()
    return radio.resolveDirectoryPeer(peerId)?.let { ConnectedPeerFacts(it.nativePeerId, it.displayName) }
  }

  override fun connectedPeers(): List<ConnectedPeerFacts> {
    requireRadioReady()
    return radio.connectedPeerSnapshots().map { ConnectedPeerFacts(it.nativePeerId, it.displayName) }
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
     * Bond and runtime-available encryption observations are native facts.
     * Encryption never implies authentication or Secure Connections.
     */
    internal fun securityFacts(state: OwnedAndroidSecurityState) = SecurityFacts(
      bond = when (state.bond) {
        "bonded" -> "bonded"
        "bonding" -> "bonding"
        "notBonded" -> "not-bonded"
        else -> "unknown"
      },
      encryption = state.encryption,
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
