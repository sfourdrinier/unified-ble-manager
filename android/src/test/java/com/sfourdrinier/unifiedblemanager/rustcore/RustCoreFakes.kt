// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreFakes.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.Executor

/** Records every call the adapter/session layer makes into Rust, in order. */
class FakeCore : MobileCorePort {
  val calls = mutableListOf<String>()
  var status = MobileCoreBridge.STATUS_DELIVERED
  var ingressStatus = MobileCoreBridge.STATUS_ACCEPTED
  var installed = false
  var installCount = 0
  var installedRadio: MobileCoreBridge.RadioHost? = null
  var installedWake: MobileCoreBridge.WakeListener? = null
  var openRecord: (String) -> String = { owner -> "{\"sessionId\":7,\"owner\":\"$owner\"}" }
  var openFailure: RuntimeException? = null
  var invokeFailure: RuntimeException? = null
  val invokes = mutableListOf<Triple<Long, String, String>>()
  val callbacks = mutableListOf<MobileCoreBridge.InvokeCallback>()
  var drainAnswer = "{\"more\":false,\"records\":[]}"

  private fun record(entry: String): Int {
    calls.add(entry)
    return status
  }

  private fun ingress(entry: String): Int {
    calls.add(entry)
    return ingressStatus
  }

  override fun buildIdentityJson() = "{\"schema\":\"ubm-native-build-identity/1\"}"
  override fun contractRevision() = "C-UBM.test"
  override fun wireRevision() = "ubm-mobile-wire/1"

  override fun installHost(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, adapterLabel: String) {
    installCount++
    installed = true
    installedRadio = radio
    installedWake = wake
  }

  override fun hostInstalled() = installed

  val openScopes = mutableListOf<String>()
  val releasedScopes = mutableListOf<String>()
  var scopeRelease = "{\"failures\":[],\"state\":\"released\"}"

  override fun openSession(owner: String, expectedWireRevision: String, backgroundScope: String): String {
    openFailure?.let { throw it }
    openScopes.add(backgroundScope)
    return openRecord(owner)
  }

  override fun releaseBackgroundScope(backgroundScope: String): String {
    releasedScopes.add(backgroundScope)
    return scopeRelease
  }

  override fun invoke(sessionId: Long, op: String, argsJson: String, callback: MobileCoreBridge.InvokeCallback) {
    invokeFailure?.let { throw it }
    invokes.add(Triple(sessionId, op, argsJson))
    callbacks.add(callback)
  }

  override fun drain(sessionId: Long, maxItems: Int, maxBytes: Int): String {
    calls.add("drain:$sessionId:$maxItems:$maxBytes")
    return drainAnswer
  }

  override fun completeUnit(requestId: Long) = record("unit:$requestId")
  override fun completeBytes(requestId: Long, value: ByteArray) = record("bytes:$requestId:${value.toList()}")
  override fun completeRead(requestId: Long, value: ByteArray, provenance: String) =
    record("read:$requestId:${value.toList()}:$provenance")
  override fun completeAdapter(requestId: Long, availability: String, authorization: String, power: String, safeReason: String?) =
    record("adapter:$requestId:$availability/$authorization/$power/$safeReason")

  override fun completeDiscovered(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray) =
    record("discovered:$requestId:" + levels.indices.joinToString(",") { "${levels[it]}/${uuids[it]}/${occurrences[it]}/${properties[it]}" })

  override fun completeNotifyEnabled(requestId: Long, delivery: String) = record("notify:$requestId:$delivery")
  override fun completeMtu(requestId: Long, mtu: Int) = record("mtu:$requestId:$mtu")
  override fun completeWriteLimits(requestId: Long, withResponse: Int, withoutResponse: Int) =
    record("writeLimits:$requestId:$withResponse:$withoutResponse")
  override fun completeRssi(requestId: Long, rssi: Int) = record("rssi:$requestId:$rssi")
  override fun completeAccepted(requestId: Long, accepted: Boolean) = record("accepted:$requestId:$accepted")
  override fun completePhy(requestId: Long, tx: String, rx: String) = record("phy:$requestId:$tx/$rx")
  override fun completePhyRequest(requestId: Long, accepted: Boolean, tx: String?, rx: String?) =
    record("phy-request:$requestId:$accepted:$tx/$rx")

  override fun completeSecurity(requestId: Long, security: SecurityFacts) = record("security:$requestId:$security")
  override fun completeBondedPeers(requestId: Long, peerIds: Array<String>, names: Array<String?>) =
    record("bonded:$requestId:${peerIds.toList()}:${names.toList()}")

  override fun completeLease(requestId: Long, leaseId: String) = record("lease:$requestId:$leaseId")
  override fun completeCompanionList(
    requestId: Long,
    associationIds: LongArray,
    peerIds: Array<String?>,
    displayNames: Array<String?>
  ) = record("companion-list:$requestId:${associationIds.toList()}:${peerIds.toList()}:${displayNames.toList()}")
  override fun completeCompanion(
    requestId: Long,
    associationId: Long,
    peerId: String?,
    displayName: String?,
    alreadyAssociated: Boolean
  ) = record("companion:$requestId:$associationId:$peerId:$displayName:$alreadyAssociated")

  override fun completeClosed(requestId: Long, failures: List<CloseFailure>) =
    record("closed:$requestId:${failures.map { "${it.instance.characteristicUuid}=${it.detail}" }.sorted()}")

  val failures = mutableMapOf<Long, RadioFailure>()

  override fun completeFailure(requestId: Long, failure: RadioFailure) =
    failures.put(requestId, failure).let { record("failure:$requestId:${failure.kind.wire}:${failure.gattStatus}") }

  val advertisements = mutableListOf<AdvertisementFacts>()

  override fun ingestAdvertisement(advertisement: AdvertisementFacts) =
    advertisements.add(advertisement).let { ingress("adv:${advertisement.peerId}") }
  override fun ingestConnection(peerId: String, connected: Boolean, status: Int?) = ingress("link:$peerId:$connected:$status")
  override fun ingestServicesChanged(peerId: String) = ingress("services-changed:$peerId")
  override fun ingestNotification(instance: CharacteristicInstance, epoch: Long, value: ByteArray) =
    ingress("value:${instance.peerId}:${instance.characteristicUuid}#${instance.characteristicOccurrence}:$epoch:${value.toList()}")

  override fun ingestAdapterState(state: AdapterFacts) = ingress("adapter-state:${state.power}")
  override fun ingestScanFailed(detail: String) = ingress("scan-failed:$detail")
  override fun ingestSecurity(peerId: String, security: SecurityFacts) = ingress("security-changed:$peerId:${security.bond}")
  override fun ingestDropped(ingressClass: String, detail: String) = ingress("dropped:$ingressClass")
  override fun ingestRestored(peers: List<PresenceRestoredPeer>) =
    ingress(peers.joinToString(",") { "restored:${it.peerId}:${it.connected}" })
}

/** Scripted OS driver: records calls; tests answer callbacks explicitly. */
class FakeRadio : AndroidRadioPort {
  private var installedEvents: RadioPortEvents? = null
  val events: RadioPortEvents get() = installedEvents ?: throw AssertionError("adapter did not install events")
  val calls = mutableListOf<String>()
  var adapter = AdapterFacts("available", "granted", "on", null)
  var startScanFailure: Throwable? = null
  var stopScanFailure: Throwable? = null
  var connectFailure: Throwable? = null
  val characteristics = mutableMapOf<CharacteristicInstance, CharacteristicFacts>()
  val pending = mutableMapOf<String, (Result<Any?>) -> Unit>()
  var disconnectAnswer: Throwable? = null
  var cancellable = true
  var nextOperation = 100L
  val cancelled = mutableListOf<Long>()
  val operations = mutableMapOf<Long, String>()
  var services: List<GattServiceNode> = emptyList()
  var security = SecurityFacts("bonded", "unsupported", "unsupported", "unsupported", true)
  var bonded = listOf(BondedPeerFacts("AA:BB:CC:DD:EE:FF", "Polar H10"), BondedPeerFacts("11:22:33:44:55:66", null))

  override fun setEvents(events: RadioPortEvents) {
    installedEvents = events
  }

  override fun adapterState() = adapter

  override fun startScan(serviceUuids: List<String>, deviceAddresses: List<String>, mode: Int, callbackType: Int, legacy: Boolean) {
    calls.add("startScan:$serviceUuids:$deviceAddresses:$mode:$callbackType:$legacy")
    startScanFailure?.let { throw it }
  }

  override fun stopScan(): Throwable? {
    calls.add("stopScan")
    return stopScanFailure
  }

  var connectPhySupported = true

  override fun supportsConnectPhy(): Boolean = connectPhySupported

  override fun connect(peerId: String, autoConnect: Boolean, phyMask: Int) {
    calls.add(if (phyMask == 0) "connect:$peerId:$autoConnect" else "connect:$peerId:$autoConnect:phy=$phyMask")
    connectFailure?.let { throw it }
  }

  override fun disconnect(peerId: String, onComplete: (Throwable?) -> Unit) {
    calls.add("disconnect:$peerId")
    onComplete(disconnectAnswer)
  }

  private fun <T> park(name: String, onResult: (Result<T>) -> Unit): Long {
    calls.add(name)
    @Suppress("UNCHECKED_CAST")
    pending[name] = { result -> onResult(result as Result<T>) }
    val id = nextOperation++
    operations[id] = name
    return id
  }

  /** Answers the parked driver call [name]. */
  fun answer(name: String, result: Result<Any?>) {
    val callback = pending.remove(name) ?: throw AssertionError("no parked call $name; parked=${pending.keys}")
    callback(result)
  }

  override fun discover(peerId: String, onResult: (Result<List<GattServiceNode>>) -> Unit): Long =
    park<List<GattServiceNode>>("discover:$peerId", onResult)

  override fun characteristic(instance: CharacteristicInstance): CharacteristicFacts? = characteristics[instance]

  override fun read(instance: CharacteristicInstance, onResult: (Result<ByteArray>) -> Unit): Long =
    park("read:${instance.characteristicUuid}", onResult)

  override fun write(instance: CharacteristicInstance, value: ByteArray, withResponse: Boolean, onResult: (Result<Unit>) -> Unit): Long =
    park("write:${instance.characteristicUuid}:${value.toList()}:$withResponse", onResult)

  override fun readDescriptor(descriptor: DescriptorInstance, onResult: (Result<ByteArray>) -> Unit): Long =
    park("readDescriptor:${descriptor.descriptorUuid}#${descriptor.descriptorOccurrence}", onResult)

  override fun writeDescriptor(descriptor: DescriptorInstance, value: ByteArray, onResult: (Result<Unit>) -> Unit): Long =
    park("writeDescriptor:${descriptor.descriptorUuid}:${value.toList()}", onResult)

  override fun setNotification(instance: CharacteristicInstance, enable: Boolean, mode: String?, onResult: (Result<Unit>) -> Unit): Long =
    park("notify:${instance.characteristicUuid}:$enable:$mode", onResult)

  override fun readMtu(peerId: String, onResult: (Result<Int?>) -> Unit): Long = park("readMtu:$peerId", onResult)
  override fun requestMtu(peerId: String, mtu: Int, onResult: (Result<Int>) -> Unit): Long = park("requestMtu:$peerId:$mtu", onResult)
  override fun readRssi(peerId: String, onResult: (Result<Int>) -> Unit): Long = park("readRssi:$peerId", onResult)
  override fun requestConnectionPriority(peerId: String, priority: String, onResult: (Result<Boolean>) -> Unit): Long =
    park("priority:$peerId:$priority", onResult)

  override fun readPhy(peerId: String, onResult: (Result<PhyFacts>) -> Unit): Long = park("readPhy:$peerId", onResult)
  override fun requestPhy(peerId: String, tx: String?, rx: String?, onResult: (Result<PhyFacts?>) -> Unit): Long =
    park("requestPhy:$peerId:$tx:$rx", onResult)

  override fun securityState(peerId: String): SecurityFacts {
    calls.add("securityState:$peerId")
    return security
  }

  override fun createBond(peerId: String, transport: String, onResult: (Result<SecurityFacts>) -> Unit) {
    park("createBond:$peerId:$transport", onResult)
  }

  override fun bondedPeers(): List<BondedPeerFacts> {
    calls.add("bondedPeers")
    return bonded
  }

  override fun cancel(operationId: Long): Boolean {
    cancelled.add(operationId)
    if (!cancellable) return false
    val name = operations[operationId] ?: return false
    pending.remove(name)?.invoke(Result.failure(IllegalStateException("GATT operation was cancelled")))
    return true
  }
}

class FakeBackground : BackgroundPort {
  val calls = mutableListOf<String>()
  var failure: RuntimeException? = null
  override fun acquire(kind: String, reason: String): String {
    calls.add("acquire:$kind:$reason")
    failure?.let { throw it }
    return "background-1"
  }

  override fun release(leaseId: String) {
    calls.add("release:$leaseId")
    failure?.let { throw it }
  }

  override fun update(leaseId: String, title: String, body: String?) {
    calls.add("update:$leaseId:$title:$body")
    failure?.let { throw it }
  }
}

/** Runs tasks only when [runAll] is called, so tests can interleave cancellation. */
class QueuedExecutor : Executor {
  private val tasks = ArrayDeque<Runnable>()
  override fun execute(command: Runnable) {
    tasks.addLast(command)
  }

  fun runAll() {
    while (tasks.isNotEmpty()) tasks.removeFirst().run()
  }

  fun size() = tasks.size
}

val DirectExecutor = Executor { it.run() }
