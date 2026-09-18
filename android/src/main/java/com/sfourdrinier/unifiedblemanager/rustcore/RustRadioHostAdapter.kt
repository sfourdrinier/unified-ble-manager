// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustRadioHostAdapter.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceControlException
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattLinkLost
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattNotSubmitted
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattOperationFailure
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executor
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

/**
 * The Android platform radio of the process-owned Rust mobile host
 * (`MobileCoreBridge.RadioHost`, `docs/MOBILE_RUST_WIRE.md`).
 *
 * Rust asks, this adapter drives the OS through [AndroidRadioPort] and
 * answers every request exactly once with one `nativeComplete*`. OS facts
 * enter Rust through `nativeIngest*`. No decision about the BLE contract is
 * taken here: the adapter translates, and reports what the OS did.
 *
 * Threading: `RadioHost` methods are called from Rust executor threads and
 * return at once; work runs on [radioExecutor] (GATT/scan) or
 * [serviceExecutor] (foreground-service promotion and the companion
 * chooser, which may block). Driver callbacks arrive on the main/binder
 * threads and only touch concurrent state.
 *
 * Every non-zero status the host returns (late or mismatched completion,
 * dropped ingress, host closed, no host) is counted in [statusCounts] and
 * logged — never discarded silently.
 */
class RustRadioHostAdapter(
  private val core: MobileCorePort,
  private val radio: AndroidRadioPort,
  private val background: BackgroundPort,
  private val companion: () -> CompanionPort?,
  private val radioExecutor: Executor,
  private val serviceExecutor: Executor,
  private val log: (String) -> Unit
) : MobileCoreBridge.RadioHost {

  private data class InstanceKey(
    val peer: String,
    val service: String,
    val serviceOccurrence: Long,
    val characteristic: String,
    val characteristicOccurrence: Long
  )

  private data class Enablement(val instance: CharacteristicInstance, val epoch: Long)

  private val inFlight = ConcurrentHashMap.newKeySet<Long>()
  private val cancelRequested = ConcurrentHashMap.newKeySet<Long>()
  private val started = ConcurrentHashMap.newKeySet<Long>()
  private val driverOperations = ConcurrentHashMap<Long, Long>()
  private val pendingConnects = ConcurrentHashMap<String, Long>()
  private val connectedPeers = ConcurrentHashMap.newKeySet<String>()
  private val enablements = ConcurrentHashMap<InstanceKey, Enablement>()
  private val counts = ConcurrentHashMap<String, AtomicLong>()

  init {
    radio.setEvents(Events())
  }

  /** Non-delivered host statuses and adapter-side suppressions, by label. */
  fun statusCounts(): Map<String, Long> = counts.mapValues { it.value.get() }

  // -- requests -----------------------------------------------------------

  override fun adapterState(requestId: Long) = perform(requestId) {
    val state = radio.adapterState()
    answer(requestId, "adapter") {
      core.completeAdapter(requestId, state.availability, state.authorization, state.power, state.safeReason)
    }
  }

  override fun startScan(
    requestId: Long,
    serviceUuids: Array<String>?,
    deviceAddresses: Array<String>?,
    scanMode: String?,
    callbackType: String?,
    legacy: Int
  ) = perform(requestId) {
    val mode = when (scanMode) {
      null, "low-latency" -> SCAN_MODE_LOW_LATENCY
      "low-power" -> SCAN_MODE_LOW_POWER
      "balanced" -> SCAN_MODE_BALANCED
      "opportunistic" -> SCAN_MODE_OPPORTUNISTIC
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "scan mode '$scanMode' is not an Android scan mode")
    }
    val callback = when (callbackType) {
      null, "all-matches" -> CALLBACK_TYPE_ALL_MATCHES
      "first-match" -> CALLBACK_TYPE_FIRST_MATCH
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "scan callback type '$callbackType' is not supported")
    }
    radio.startScan(
      serviceUuids?.toList() ?: emptyList(),
      deviceAddresses?.toList() ?: emptyList(),
      mode,
      callback,
      legacy != 0
    )
    answer(requestId, "unit") { core.completeUnit(requestId) }
  }

  override fun stopScan(requestId: Long) = perform(requestId) {
    val failure = radio.stopScan()
    if (failure == null) answer(requestId, "unit") { core.completeUnit(requestId) } else fail(requestId, failure)
  }

  override fun connect(requestId: Long, peerId: String, autoConnect: Boolean, preferredPhy: Array<String>) = perform(requestId) {
    val phyMask = connectPhyMask(preferredPhy)
    if (phyMask != 0) {
      // A PHY preference is honoured by establishing the link on it, or
      // refused before any effect; never connected without it.
      if (autoConnect) {
        throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "Android does not apply a connect PHY with autoConnect")
      }
      if (!radio.supportsConnectPhy()) {
        throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "connectGatt with a PHY preference needs API 26")
      }
    }
    val key = peerKey(peerId)
    if (connectedPeers.contains(key)) {
      if (phyMask != 0) {
        throw RadioPortFailure(
          RadioFailureKind.UNSUPPORTED,
          "the link to $peerId is already established; its PHYs were chosen then"
        )
      }
      // The OS already holds this link (a live link this owner opened):
      // answered from the live link, never by a second connectGatt.
      answer(requestId, "unit") { core.completeUnit(requestId) }
      return@perform
    }
    val prior = pendingConnects.putIfAbsent(key, requestId)
    if (prior != null) {
      throw RadioPortFailure(RadioFailureKind.BUSY, "a connect is already pending for $peerId")
    }
    try {
      radio.connect(peerId, autoConnect, phyMask)
    } catch (error: Throwable) {
      pendingConnects.remove(key, requestId)
      throw error
    }
  }

  override fun disconnect(requestId: Long, peerId: String) = perform(requestId) {
    radio.disconnect(peerId) { failure ->
      if (failure == null) answer(requestId, "unit") { core.completeUnit(requestId) } else fail(requestId, failure)
    }
  }

  override fun discover(requestId: Long, peerId: String) = perform(requestId) {
    requireConnected(peerId)
    track(requestId, radio.discover(peerId) { result ->
      result.fold(
        onSuccess = { services ->
          val tree = encodeDiscovery(services)
          answer(requestId, "discovered") {
            core.completeDiscovered(requestId, tree.levels, tree.uuids, tree.occurrences, tree.properties)
          }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun read(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long
  ) = perform(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.read(instance) { result ->
      result.fold(
        onSuccess = { value -> answer(requestId, "bytes") { core.completeBytes(requestId, value) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun write(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    value: ByteArray,
    withResponse: Boolean
  ) = perform(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.write(instance, value.copyOf(), withResponse) { result ->
      result.fold(
        onSuccess = { answer(requestId, "unit") { core.completeUnit(requestId) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun readDescriptor(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    descriptorUuid: String,
    descriptorOccurrence: Long
  ) = perform(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.readDescriptor(DescriptorInstance(instance, descriptorUuid, descriptorOccurrence)) { result ->
      result.fold(
        onSuccess = { value -> answer(requestId, "bytes") { core.completeBytes(requestId, value) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun writeDescriptor(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    descriptorUuid: String,
    descriptorOccurrence: Long,
    value: ByteArray
  ) = perform(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.writeDescriptor(DescriptorInstance(instance, descriptorUuid, descriptorOccurrence), value.copyOf()) { result ->
      result.fold(
        onSuccess = { answer(requestId, "unit") { core.completeUnit(requestId) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun enableNotifications(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    epoch: Long,
    requested: String?,
    preferred: String?
  ) = perform(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    val facts = requirePath(instance)
    val mode = resolveCccdMode(requested, preferred, facts.properties)
    val key = instanceKey(instance)
    // Stamped before the CCCD write: values Android stages while the
    // enable is in flight are delivered with this enable's epoch.
    enablements[key] = Enablement(instance, epoch)
    track(requestId, radio.setNotification(instance, true, mode) { result ->
      result.fold(
        onSuccess = {
          val delivery = if (facts.hasCccd) mode else DELIVERY_UNKNOWN
          answer(requestId, "notify-enabled") { core.completeNotifyEnabled(requestId, delivery) }
        },
        onFailure = { error ->
          enablements.remove(key, Enablement(instance, epoch))
          fail(requestId, error)
        }
      )
    })
  }

  override fun disableNotifications(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long
  ) = perform(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.setNotification(instance, false, null) { result ->
      result.fold(
        onSuccess = {
          enablements.remove(instanceKey(instance))
          answer(requestId, "unit") { core.completeUnit(requestId) }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun readMtu(requestId: Long, peerId: String) = perform(requestId) {
    requireConnected(peerId)
    track(requestId, radio.readMtu(peerId) { result ->
      result.fold(
        onSuccess = { mtu -> answer(requestId, "mtu") { core.completeMtu(requestId, mtu ?: 0) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  /**
   * Android's per-mode single-write limits. The stack performs a long
   * (prepared) write for a with-response request larger than one ATT
   * payload, up to the ATT maximum attribute value; a command must fit one
   * ATT payload of the MTU `onMtuChanged` reported, or of the ATT default
   * LE MTU before any exchange (the link's MTU until one completes).
   */
  override fun readWriteLimits(requestId: Long, peerId: String) = perform(requestId) {
    requireConnected(peerId)
    track(requestId, radio.readMtu(peerId) { result ->
      result.fold(
        onSuccess = { mtu ->
          val withoutResponse = minOf((mtu ?: ATT_DEFAULT_LE_MTU) - ATT_WRITE_HEADER_BYTES, ATT_MAX_ATTRIBUTE_VALUE)
          answer(requestId, "write-limits") {
            core.completeWriteLimits(requestId, ATT_MAX_ATTRIBUTE_VALUE, withoutResponse)
          }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun requestMtu(requestId: Long, peerId: String, mtu: Int) = perform(requestId) {
    requireConnected(peerId)
    track(requestId, radio.requestMtu(peerId, mtu) { result ->
      result.fold(
        onSuccess = { negotiated -> answer(requestId, "mtu") { core.completeMtu(requestId, negotiated) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun readRssi(requestId: Long, peerId: String) = perform(requestId) {
    requireConnected(peerId)
    track(requestId, radio.readRssi(peerId) { result ->
      result.fold(
        onSuccess = { rssi -> answer(requestId, "rssi") { core.completeRssi(requestId, rssi) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun requestConnectionPriority(requestId: Long, peerId: String, priority: String) = perform(requestId) {
    requireConnected(peerId)
    track(requestId, radio.requestConnectionPriority(peerId, priority) { result ->
      result.fold(
        onSuccess = { accepted -> answer(requestId, "accepted") { core.completeAccepted(requestId, accepted) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun readPhy(requestId: Long, peerId: String) = perform(requestId) {
    requireConnected(peerId)
    track(requestId, radio.readPhy(peerId) { result ->
      result.fold(
        onSuccess = { phy -> answer(requestId, "phy") { core.completePhy(requestId, phy.tx, phy.rx) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun requestPhy(requestId: Long, peerId: String, tx: String?, rx: String?) = perform(requestId) {
    requireConnected(peerId)
    track(requestId, radio.requestPhy(peerId, tx, rx) { result ->
      result.fold(
        onSuccess = { phy ->
          answer(requestId, "phy-request") { core.completePhyRequest(requestId, phy != null, phy?.tx, phy?.rx) }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun securityState(requestId: Long, peerId: String) = perform(requestId) {
    val state = radio.securityState(peerId)
    answer(requestId, "security") { core.completeSecurity(requestId, state) }
  }

  override fun createBond(requestId: Long, peerId: String, transport: String) = perform(requestId) {
    radio.createBond(peerId, transport) { result ->
      result.fold(
        onSuccess = { state -> answer(requestId, "security") { core.completeSecurity(requestId, state) } },
        onFailure = { error -> fail(requestId, error) }
      )
    }
  }

  override fun cancelBond(requestId: Long, peerId: String) = perform(requestId) {
    // BluetoothDevice.cancelBondProcess is hidden API and the public
    // cancellation arrives only after the compiled API-36 boundary; the
    // legacy route registered no cancel-pairing capability either.
    throw RadioPortFailure(
      RadioFailureKind.UNSUPPORTED,
      "Android exposes no public bonding cancellation before API 37 for $peerId"
    )
  }

  override fun bondedPeers(requestId: Long) = perform(requestId) {
    val peers = radio.bondedPeers()
    answer(requestId, "bonded-peers") {
      core.completeBondedPeers(requestId, peers.map { it.peerId }.toTypedArray(), peers.map { it.name }.toTypedArray())
    }
  }

  override fun acquireBackground(requestId: Long, kind: String, reason: String) = runService(requestId) {
    val leaseId = background.acquire(kind, reason)
    answer(requestId, "lease") { core.completeLease(requestId, leaseId) }
  }

  override fun releaseBackground(requestId: Long, leaseId: String) = runService(requestId) {
    background.release(leaseId)
    answer(requestId, "unit") { core.completeUnit(requestId) }
  }

  override fun updateBackgroundNotification(requestId: Long, leaseId: String, title: String, body: String?) =
    runService(requestId) {
      background.update(leaseId, title, body)
      answer(requestId, "unit") { core.completeUnit(requestId) }
    }

  override fun associateCompanion(requestId: Long, name: String?, serviceUuid: String?) = runService(requestId) {
    val chooser = companion()
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "no React Native activity host is attached to launch the Companion Device Manager chooser"
      )
    chooser.associate(name, serviceUuid) { result ->
      result.fold(
        onSuccess = { association ->
          answer(requestId, "companion") {
            core.completeCompanion(requestId, association.associationId, association.peerId, association.displayName)
          }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    }
  }

  override fun close(requestId: Long) = perform(requestId) {
    val live = enablements.values.toList()
    if (live.isEmpty()) {
      answer(requestId, "closed") { core.completeClosed(requestId, emptyList()) }
      return@perform
    }
    val failures = ConcurrentHashMap<InstanceKey, CloseFailure>()
    val remaining = AtomicInteger(live.size)
    fun settle() {
      if (remaining.decrementAndGet() == 0) {
        answer(requestId, "closed") { core.completeClosed(requestId, failures.values.toList()) }
      }
    }
    live.forEach { enablement ->
      val key = instanceKey(enablement.instance)
      try {
        radio.setNotification(enablement.instance, false, null) { result ->
          result.fold(
            onSuccess = { enablements.remove(key, enablement) },
            onFailure = { error -> failures[key] = CloseFailure(enablement.instance, detailOf(error)) }
          )
          settle()
        }
      } catch (error: Throwable) {
        failures[key] = CloseFailure(enablement.instance, detailOf(error))
        settle()
      }
    }
  }

  override fun cancel(requestId: Long) {
    if (!inFlight.contains(requestId)) {
      bump("cancel-after-answer")
      return
    }
    cancelRequested.add(requestId)
    if (!started.contains(requestId)) return
    val connectPeer = pendingConnects.entries.firstOrNull { it.value == requestId }?.key
    if (connectPeer != null && pendingConnects.remove(connectPeer, requestId)) {
      // Android cannot abort connectGatt in place: release the pending GATT
      // (legacy cancellation did the same) and report the cancellation.
      radio.disconnect(connectPeer) { failure ->
        if (failure != null) log("connect cancellation cleanup failed for $connectPeer: ${detailOf(failure)}")
      }
      fail(requestId, RadioPortFailure(RadioFailureKind.CANCELLED, "connect cancelled; pending GATT released"))
      return
    }
    val operation = driverOperations[requestId]
    if (operation == null || !radio.cancel(operation)) {
      // Nothing cancellable on the OS side (synchronous verb, bonding
      // ceremony, foreground-service promotion, chooser UI): the request
      // keeps running and answers truthfully; Rust counts that answer late.
      bump("cancel-not-cancellable")
    }
  }

  // -- plumbing -----------------------------------------------------------

  private fun perform(requestId: Long, body: () -> Unit) = submit(requestId, radioExecutor, body)

  private fun runService(requestId: Long, body: () -> Unit) = submit(requestId, serviceExecutor, body)

  private fun submit(requestId: Long, executor: Executor, body: () -> Unit) {
    inFlight.add(requestId)
    val task = Runnable {
      started.add(requestId)
      if (cancelRequested.contains(requestId)) {
        // Cancelled before the adapter touched the OS: no effect at all.
        fail(requestId, RadioPortFailure(RadioFailureKind.CANCELLED, "cancelled before dispatch"), dispatched = false)
        return@Runnable
      }
      try {
        body()
      } catch (error: Throwable) {
        // Thrown before the driver accepted the request: nothing reached the peer.
        fail(requestId, error, dispatched = false)
      }
    }
    try {
      executor.execute(task)
    } catch (error: RejectedExecutionException) {
      fail(
        requestId,
        RadioPortFailure(RadioFailureKind.PLATFORM, "Android radio executor refused the request", cause = error),
        dispatched = false
      )
    }
  }

  private fun track(requestId: Long, operationId: Long) {
    if (operationId != 0L && inFlight.contains(requestId)) driverOperations[requestId] = operationId
  }

  /** Delivers exactly one answer per request id; later answers are counted, not sent. */
  private fun answer(requestId: Long, shape: String, deliver: () -> Int) {
    if (!inFlight.remove(requestId)) {
      bump("suppressed-second-answer")
      log("second answer ($shape) for request $requestId suppressed")
      return
    }
    driverOperations.remove(requestId)
    cancelRequested.remove(requestId)
    started.remove(requestId)
    recordStatus("complete:$shape", deliver())
  }

  /** [dispatched] overrides the classification when the adapter knows the request never left. */
  private fun fail(requestId: Long, error: Throwable, dispatched: Boolean? = null) {
    val classified = classify(error)
    val failure = (
      if (cancelRequested.contains(requestId)) {
        RadioFailure(RadioFailureKind.CANCELLED, null, "cancelled: ${detailOf(error)}", classified.dispatched)
      } else {
        classified
      }
    ).let { if (dispatched == null) it else it.copy(dispatched = dispatched) }
    answer(requestId, "failure") { core.completeFailure(requestId, failure) }
  }

  private fun recordStatus(label: String, status: Int) {
    if (status == MobileCoreBridge.STATUS_DELIVERED) return
    val name = when (status) {
      MobileCoreBridge.STATUS_LATE -> "late"
      MobileCoreBridge.STATUS_MISMATCHED -> "mismatched"
      MobileCoreBridge.STATUS_DROPPED_CONTROL -> "dropped-control"
      MobileCoreBridge.STATUS_CLOSED -> "closed"
      MobileCoreBridge.STATUS_NO_HOST -> "no-host"
      else -> "status-$status"
    }
    bump("$label:$name")
    log("host answered $name to $label")
  }

  private fun recordIngress(label: String, status: Int) {
    if (status == MobileCoreBridge.STATUS_ACCEPTED) return
    val name = when (status) {
      MobileCoreBridge.STATUS_DROPPED_ADVERTISEMENT -> "dropped-advertisement"
      MobileCoreBridge.STATUS_DROPPED_NOTIFICATION -> "dropped-notification"
      MobileCoreBridge.STATUS_DROPPED_CONTROL -> "dropped-control"
      MobileCoreBridge.STATUS_CLOSED -> "closed"
      MobileCoreBridge.STATUS_NO_HOST -> "no-host"
      else -> "status-$status"
    }
    bump("ingest:$label:$name")
    // Queue-full drops are already counted and surfaced by Rust as
    // `ingress-drop` records; closed/no-host facts only this side can see.
    if (status == MobileCoreBridge.STATUS_CLOSED || status == MobileCoreBridge.STATUS_NO_HOST) {
      log("host refused $label ingress: $name")
    }
  }

  private fun bump(label: String) {
    counts.computeIfAbsent(label) { AtomicLong() }.incrementAndGet()
  }

  private fun requireConnected(peerId: String) {
    if (!connectedPeers.contains(peerKey(peerId))) {
      throw RadioPortFailure(RadioFailureKind.NOT_CONNECTED, "$peerId is not connected")
    }
  }

  private fun requirePath(instance: CharacteristicInstance): CharacteristicFacts {
    requireConnected(instance.peerId)
    return radio.characteristic(instance)
      ?: throw RadioPortFailure(
        RadioFailureKind.PATH_STALE,
        "${instance.serviceUuid}#${instance.serviceOccurrence}/${instance.characteristicUuid}#${instance.characteristicOccurrence} is not in the current database"
      )
  }

  private fun clearEnablementsFor(peerId: String) {
    val peer = peerKey(peerId)
    enablements.keys.filter { it.peer == peer }.forEach { enablements.remove(it) }
  }

  private inner class Events : RadioPortEvents {
    override fun onAdvertisement(advertisement: AdvertisementFacts) =
      recordIngress("advertisement", core.ingestAdvertisement(advertisement))

    override fun onConnection(peerId: String, connected: Boolean, gattStatus: Int) {
      val key = peerKey(peerId)
      val pending = pendingConnects.remove(key)
      if (connected && gattStatus == GATT_SUCCESS) {
        connectedPeers.add(key)
        if (pending != null) answer(pending, "unit") { core.completeUnit(pending) }
        recordIngress("connection", core.ingestConnection(peerId, true, gattStatus))
        return
      }
      val wasConnected = connectedPeers.remove(key)
      clearEnablementsFor(peerId)
      if (pending != null) {
        fail(
          pending,
          RadioPortFailure(
            if (gattStatus == GATT_SUCCESS) RadioFailureKind.PLATFORM else RadioFailureKind.GATT_STATUS,
            "Android GATT connection failed with status $gattStatus",
            gattStatus
          )
        )
      }
      if (wasConnected) {
        recordIngress("connection", core.ingestConnection(peerId, false, gattStatus))
      } else if (pending == null) {
        bump("disconnect-without-link")
        log("disconnect for $peerId without a live link or pending connect (status $gattStatus)")
      }
    }

    override fun onServicesChanged(peerId: String) {
      clearEnablementsFor(peerId)
      recordIngress("services-changed", core.ingestServicesChanged(peerId))
    }

    override fun onNotification(instance: CharacteristicInstance, value: ByteArray) {
      val enablement = enablements[instanceKey(instance)]
      if (enablement == null) {
        recordIngress(
          "dropped",
          core.ingestDropped(
            INGRESS_NOTIFICATION,
            "value for ${instance.characteristicUuid}#${instance.characteristicOccurrence} on ${instance.peerId} without a live enable"
          )
        )
        return
      }
      recordIngress("notification", core.ingestNotification(enablement.instance, enablement.epoch, value))
    }

    override fun onAdapterState(state: AdapterFacts) {
      recordIngress("adapter", core.ingestAdapterState(state))
      if (state.power == "on" && state.authorization == "granted") return
      // The driver force-closes every GATT on adapter loss without a
      // per-link callback: report each link's end and each pending connect.
      pendingConnects.entries.toList().forEach { entry ->
        if (pendingConnects.remove(entry.key, entry.value)) {
          fail(entry.value, adapterLossFailure(state))
        }
      }
      if (state.power == "on") return
      connectedPeers.toList().forEach { peer ->
        if (connectedPeers.remove(peer)) {
          clearEnablementsFor(peer)
          recordIngress("connection", core.ingestConnection(peer, false, null))
        }
      }
    }

    override fun onScanFailed(errorCode: Int) {
      val stopFailure = radio.stopScan()
      val detail = if (stopFailure == null) {
        "scan failed code=$errorCode"
      } else {
        "scan failed code=$errorCode; scan cleanup failed: ${detailOf(stopFailure)}"
      }
      recordIngress("scan-failed", core.ingestScanFailed(detail))
    }

    override fun onSecurity(peerId: String, security: SecurityFacts) =
      recordIngress("security", core.ingestSecurity(peerId, security))

    override fun onDropped(ingressClass: String, detail: String) =
      recordIngress("dropped", core.ingestDropped(ingressClass, detail))
  }

  private fun adapterLossFailure(state: AdapterFacts): RadioPortFailure = when {
    state.authorization == "restricted" ->
      RadioPortFailure(RadioFailureKind.PERMISSION_RESTRICTED, "Bluetooth use is restricted")
    state.authorization == "not-determined" ->
      RadioPortFailure(RadioFailureKind.PERMISSION_NOT_DETERMINED, "Bluetooth permission is not determined")
    state.authorization != "granted" ->
      RadioPortFailure(RadioFailureKind.PERMISSION_DENIED, state.safeReason ?: "Bluetooth permission was revoked")
    state.power == "resetting" -> RadioPortFailure(RadioFailureKind.ADAPTER_RESETTING, "Bluetooth adapter is resetting")
    state.power == "off" -> RadioPortFailure(RadioFailureKind.ADAPTER_OFF, "Bluetooth adapter is off")
    else -> RadioPortFailure(RadioFailureKind.ADAPTER_UNAVAILABLE, state.safeReason ?: "Bluetooth adapter is ${state.power}")
  }

  /** Pre-order discovery tree in the `nativeCompleteDiscovered` encoding. */
  internal class DiscoveryTree(
    val levels: IntArray,
    val uuids: Array<String>,
    val occurrences: LongArray,
    val properties: IntArray
  )

  companion object {
    const val SCAN_MODE_OPPORTUNISTIC = -1
    const val SCAN_MODE_LOW_POWER = 0
    const val SCAN_MODE_BALANCED = 1
    const val SCAN_MODE_LOW_LATENCY = 2
    const val CALLBACK_TYPE_ALL_MATCHES = 1
    const val CALLBACK_TYPE_FIRST_MATCH = 2
    const val GATT_SUCCESS = 0
    const val PROPERTY_NOTIFY = 0x10
    const val PROPERTY_INDICATE = 0x20
    const val DELIVERY_NOTIFICATION = "notification"
    const val DELIVERY_INDICATION = "indication"
    const val DELIVERY_UNKNOWN = "unknown"
    const val INGRESS_NOTIFICATION = "notification"

    /** Core spec Vol 3 Part F §3.2.8: the LE ATT_MTU before any exchange. */
    const val ATT_DEFAULT_LE_MTU = 23
    /** Core spec Vol 3 Part F §3.2.9: the longest attribute value. */
    const val ATT_MAX_ATTRIBUTE_VALUE = 512
    /** Opcode and handle of an ATT write request/command. */
    const val ATT_WRITE_HEADER_BYTES = 3

    /** `BluetoothDevice.PHY_LE_1M_MASK` / `PHY_LE_2M_MASK` / `PHY_LE_CODED_MASK`. */
    const val PHY_LE_1M_MASK = 1
    const val PHY_LE_2M_MASK = 2
    const val PHY_LE_CODED_MASK = 4

    /** The `connectGatt` PHY mask for wire PHY names; an unknown name is refused, never dropped. */
    internal fun connectPhyMask(preferredPhy: Array<String>): Int =
      preferredPhy.fold(0) { mask, name ->
        mask or when (name) {
          "le-1m" -> PHY_LE_1M_MASK
          "le-2m" -> PHY_LE_2M_MASK
          "le-coded" -> PHY_LE_CODED_MASK
          else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "'$name' is not an LE PHY")
        }
      }

    /** `BluetoothStatusCodes.ERROR_GATT_WRITE_REQUEST_BUSY` (API 33 write submission). */
    const val ANDROID_ERROR_GATT_WRITE_REQUEST_BUSY = 201

    private fun peerKey(peerId: String): String = peerId.uppercase()

    private fun instanceKey(instance: CharacteristicInstance) = InstanceKey(
      peerKey(instance.peerId),
      instance.serviceUuid.lowercase(),
      instance.serviceOccurrence,
      instance.characteristicUuid.lowercase(),
      instance.characteristicOccurrence
    )

    /**
     * The CCCD mode to write. A `requested` mode is hard: the characteristic
     * must carry that property or the enable is refused before any effect.
     * Otherwise `preferred` (default notification) wins when allowed, else
     * the other mode (legacy `resolveCccdPayload` rule).
     */
    @JvmStatic
    fun resolveCccdMode(requested: String?, preferred: String?, properties: Int): String {
      fun allows(mode: String) = when (mode) {
        DELIVERY_NOTIFICATION -> (properties and PROPERTY_NOTIFY) != 0
        DELIVERY_INDICATION -> (properties and PROPERTY_INDICATE) != 0
        else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "delivery mode '$mode' is not a CCCD mode")
      }
      if (requested != null) {
        if (allows(requested)) return requested
        throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "characteristic does not support $requested")
      }
      val first = preferred ?: DELIVERY_NOTIFICATION
      val other = if (first == DELIVERY_NOTIFICATION) DELIVERY_INDICATION else DELIVERY_NOTIFICATION
      if (allows(first)) return first
      if (allows(other)) return other
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "characteristic supports neither notify nor indicate")
    }

    @JvmStatic
    internal fun encodeDiscovery(services: List<GattServiceNode>): DiscoveryTree {
      val levels = ArrayList<Int>()
      val uuids = ArrayList<String>()
      val occurrences = ArrayList<Long>()
      val properties = ArrayList<Int>()
      fun emit(level: Int, uuid: String, occurrence: Long, bits: Int) {
        levels.add(level)
        uuids.add(uuid)
        occurrences.add(occurrence)
        properties.add(bits)
      }
      val serviceCounts = HashMap<String, Long>()
      for (service in services) {
        emit(0, service.uuid, nextOccurrence(serviceCounts, service.uuid), 0)
        val characteristicCounts = HashMap<String, Long>()
        for (characteristic in service.characteristics) {
          emit(1, characteristic.uuid, nextOccurrence(characteristicCounts, characteristic.uuid), characteristic.properties)
          val descriptorCounts = HashMap<String, Long>()
          for (descriptor in characteristic.descriptors) {
            emit(2, descriptor.uuid, nextOccurrence(descriptorCounts, descriptor.uuid), 0)
          }
        }
      }
      return DiscoveryTree(levels.toIntArray(), uuids.toTypedArray(), occurrences.toLongArray(), properties.toIntArray())
    }

    private fun nextOccurrence(counts: MutableMap<String, Long>, uuid: String): Long {
      val key = uuid.lowercase()
      val occurrence = counts[key] ?: 0L
      counts[key] = occurrence + 1
      return occurrence
    }

    private fun detailOf(error: Throwable): String = error.message ?: error.javaClass.simpleName

    /** Maps a driver failure onto the frozen failure kinds; Rust maps kinds to contract identities. */
    @JvmStatic
    fun classify(error: Throwable): RadioFailure = when (error) {
      is RadioPortFailure -> RadioFailure(error.kind, error.gattStatus, detailOf(error), nativeCode = error.nativeCode)
      // Legacy failed every command pending at a link loss with connectionLost (132).
      is AndroidGattLinkLost -> RadioFailure(RadioFailureKind.NOT_CONNECTED, error.gattStatus, detailOf(error), error.submitted)
      is AndroidGattOperationFailure -> when {
        error.isLinkLoss -> RadioFailure(RadioFailureKind.NOT_CONNECTED, error.gattStatus, detailOf(error), error.submitted)
        !error.submitted && error.gattStatus == ANDROID_ERROR_GATT_WRITE_REQUEST_BUSY ->
          RadioFailure(RadioFailureKind.BUSY, error.gattStatus, detailOf(error), dispatched = false)
        error.gattStatus != null -> RadioFailure(RadioFailureKind.GATT_STATUS, error.gattStatus, detailOf(error), error.submitted)
        else -> RadioFailure(RadioFailureKind.PLATFORM, null, detailOf(error), error.submitted)
      }
      is AndroidGattNotSubmitted -> RadioFailure(RadioFailureKind.PLATFORM, null, detailOf(error), dispatched = false)
      // A rejected argument (for example an oversize value) never leaves the stack.
      is IllegalArgumentException -> RadioFailure(RadioFailureKind.PLATFORM, null, detailOf(error), dispatched = false)
      is ForegroundServiceControlException -> classifyBackgroundFailure(error)
      is SecurityException -> RadioFailure(RadioFailureKind.PERMISSION_DENIED, null, detailOf(error))
      is UnsupportedOperationException -> RadioFailure(RadioFailureKind.UNSUPPORTED, null, detailOf(error))
      else -> RadioFailure(RadioFailureKind.PLATFORM, null, detailOf(error))
    }
  }
}
