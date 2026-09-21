// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/MobileCorePort.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.ubm.core.MobileCoreBridge

/**
 * The JNI surface of the process-owned Rust mobile host
 * ([MobileCoreBridge]) as a seam. Production is [JniMobileCorePort]; JVM
 * tests substitute a recording fake, so nothing here loads the cdylib.
 * Status codes are the bridge's `STATUS_*` constants.
 */
interface MobileCorePort {
  fun buildIdentityJson(): String
  fun contractRevision(): String
  fun wireRevision(): String

  fun installHost(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, adapterLabel: String)
  fun hostInstalled(): Boolean
  /** [backgroundScope] names the module instance whose foreground-service leases the session shares. */
  fun openSession(owner: String, expectedWireRevision: String, backgroundScope: String): String
  /** Releases every foreground-service lease of [backgroundScope]; the cleanup record JSON. */
  fun releaseBackgroundScope(backgroundScope: String): String
  fun invoke(sessionId: Long, op: String, argsJson: String, callback: MobileCoreBridge.InvokeCallback)
  fun drain(sessionId: Long, maxItems: Int, maxBytes: Int): String

  fun completeUnit(requestId: Long): Int
  /** A descriptor read's value. */
  fun completeBytes(requestId: Long, value: ByteArray): Int
  /** A characteristic read's value and what the platform says it is (`read-response` | `read-or-notification`). */
  fun completeRead(requestId: Long, value: ByteArray, provenance: String): Int
  fun completeAdapter(requestId: Long, availability: String, authorization: String, power: String, safeReason: String?): Int
  fun completeDiscovered(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray): Int
  fun completeNotifyEnabled(requestId: Long, delivery: String): Int
  fun completeMtu(requestId: Long, mtu: Int): Int
  fun completeWriteLimits(requestId: Long, withResponse: Int, withoutResponse: Int): Int
  fun completeRssi(requestId: Long, rssi: Int): Int
  fun completeAccepted(requestId: Long, accepted: Boolean): Int
  fun completePhy(requestId: Long, tx: String, rx: String): Int
  fun completePhyRequest(requestId: Long, accepted: Boolean, tx: String?, rx: String?): Int
  fun completeSecurity(requestId: Long, security: SecurityFacts): Int
  fun completeBondedPeers(requestId: Long, peerIds: Array<String>, names: Array<String?>): Int
  fun completeLease(requestId: Long, leaseId: String): Int
  fun completeCompanionList(
    requestId: Long,
    associationIds: LongArray,
    peerIds: Array<String?>,
    displayNames: Array<String?>
  ): Int
  /**
   * [alreadyAssociated] reports an association the platform already held:
   * nothing new was created and the record is the existing association.
   */
  fun completeCompanion(
    requestId: Long,
    associationId: Long,
    peerId: String?,
    displayName: String?,
    alreadyAssociated: Boolean
  ): Int
  fun completeClosed(requestId: Long, failures: List<CloseFailure>): Int
  fun completeFailure(requestId: Long, failure: RadioFailure): Int

  fun ingestAdvertisement(advertisement: AdvertisementFacts): Int
  fun ingestConnection(peerId: String, connected: Boolean, status: Int?): Int
  fun ingestServicesChanged(peerId: String): Int
  fun ingestNotification(instance: CharacteristicInstance, epoch: Long, value: ByteArray): Int
  fun ingestAdapterState(state: AdapterFacts): Int
  fun ingestScanFailed(detail: String): Int
  fun ingestRestored(peers: List<PresenceRestoredPeer>): Int
  fun ingestSecurity(peerId: String, security: SecurityFacts): Int
  fun ingestDropped(ingressClass: String, detail: String): Int
}

/** One characteristic scope whose close-time release failed. */
data class CloseFailure(val instance: CharacteristicInstance, val detail: String)

/** Production port: every call is the matching [MobileCoreBridge] native. */
object JniMobileCorePort : MobileCorePort {
  private fun optional(value: Int?): Int = value ?: MobileCoreBridge.ABSENT_INT

  private fun tristate(value: Boolean?): Int = when (value) {
    null -> -1
    true -> 1
    false -> 0
  }

  override fun buildIdentityJson(): String = MobileCoreBridge.nativeBuildIdentityJson()
  override fun contractRevision(): String = MobileCoreBridge.nativeContractRevision()
  override fun wireRevision(): String = MobileCoreBridge.nativeWireRevision()

  override fun installHost(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, adapterLabel: String) =
    MobileCoreBridge.nativeInstallHost(radio, wake, "android", owner, adapterLabel)

  override fun hostInstalled(): Boolean = MobileCoreBridge.nativeHostInstalled()
  override fun openSession(owner: String, expectedWireRevision: String, backgroundScope: String): String =
    MobileCoreBridge.nativeOpenSession(owner, expectedWireRevision, backgroundScope)

  override fun releaseBackgroundScope(backgroundScope: String): String =
    MobileCoreBridge.nativeReleaseBackgroundScope(backgroundScope)

  override fun invoke(sessionId: Long, op: String, argsJson: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeInvoke(sessionId, op, argsJson, callback)

  override fun drain(sessionId: Long, maxItems: Int, maxBytes: Int): String =
    MobileCoreBridge.nativeDrain(sessionId, maxItems, maxBytes)

  override fun completeUnit(requestId: Long): Int = MobileCoreBridge.nativeCompleteUnit(requestId)
  override fun completeBytes(requestId: Long, value: ByteArray): Int = MobileCoreBridge.nativeCompleteBytes(requestId, value)
  override fun completeRead(requestId: Long, value: ByteArray, provenance: String): Int =
    MobileCoreBridge.nativeCompleteRead(requestId, value, provenance)
  override fun completeAdapter(requestId: Long, availability: String, authorization: String, power: String, safeReason: String?): Int =
    MobileCoreBridge.nativeCompleteAdapter(requestId, availability, authorization, power, safeReason)

  override fun completeDiscovered(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray): Int =
    MobileCoreBridge.nativeCompleteDiscovered(requestId, levels, uuids, occurrences, properties)

  override fun completeNotifyEnabled(requestId: Long, delivery: String): Int =
    MobileCoreBridge.nativeCompleteNotifyEnabled(requestId, delivery)

  override fun completeMtu(requestId: Long, mtu: Int): Int = MobileCoreBridge.nativeCompleteMtu(requestId, mtu)
  override fun completeWriteLimits(requestId: Long, withResponse: Int, withoutResponse: Int): Int =
    MobileCoreBridge.nativeCompleteWriteLimits(requestId, withResponse, withoutResponse)
  override fun completeRssi(requestId: Long, rssi: Int): Int = MobileCoreBridge.nativeCompleteRssi(requestId, rssi)
  override fun completeAccepted(requestId: Long, accepted: Boolean): Int = MobileCoreBridge.nativeCompleteAccepted(requestId, accepted)
  override fun completePhy(requestId: Long, tx: String, rx: String): Int = MobileCoreBridge.nativeCompletePhy(requestId, tx, rx)
  override fun completePhyRequest(requestId: Long, accepted: Boolean, tx: String?, rx: String?): Int =
    MobileCoreBridge.nativeCompletePhyRequest(requestId, accepted, tx, rx)

  override fun completeSecurity(requestId: Long, security: SecurityFacts): Int =
    MobileCoreBridge.nativeCompleteSecurity(
      requestId,
      security.bond,
      security.encryption,
      security.authentication,
      security.secureConnections,
      tristate(security.pairingPossible)
    )

  override fun completeBondedPeers(requestId: Long, peerIds: Array<String>, names: Array<String?>): Int =
    MobileCoreBridge.nativeCompleteBondedPeers(requestId, peerIds, names)

  override fun completeLease(requestId: Long, leaseId: String): Int = MobileCoreBridge.nativeCompleteLease(requestId, leaseId)
  override fun completeCompanionList(
    requestId: Long,
    associationIds: LongArray,
    peerIds: Array<String?>,
    displayNames: Array<String?>
  ): Int = MobileCoreBridge.nativeCompleteCompanionList(requestId, associationIds, peerIds, displayNames)
  override fun completeCompanion(
    requestId: Long,
    associationId: Long,
    peerId: String?,
    displayName: String?,
    alreadyAssociated: Boolean
  ): Int =
    MobileCoreBridge.nativeCompleteCompanion(requestId, associationId, peerId, displayName, alreadyAssociated)

  override fun completeClosed(requestId: Long, failures: List<CloseFailure>): Int =
    MobileCoreBridge.nativeCompleteClosed(
      requestId,
      failures.map { it.instance.peerId }.toTypedArray(),
      failures.map { it.instance.serviceUuid }.toTypedArray(),
      failures.map { it.instance.serviceOccurrence }.toLongArray(),
      failures.map { it.instance.characteristicUuid }.toTypedArray(),
      failures.map { it.instance.characteristicOccurrence }.toLongArray(),
      failures.map { it.detail }.toTypedArray()
    )

  override fun completeFailure(requestId: Long, failure: RadioFailure): Int =
    MobileCoreBridge.nativeCompleteFailure(
      requestId,
      failure.kind.wire,
      optional(failure.gattStatus),
      failure.detail,
      failure.dispatched,
      failure.nativeCode
    )

  override fun ingestAdvertisement(advertisement: AdvertisementFacts): Int =
    MobileCoreBridge.nativeIngestAdvertisement(
      advertisement.peerId,
      advertisement.address,
      advertisement.localName,
      optional(advertisement.rssi),
      optional(advertisement.txPower),
      advertisement.serviceUuids.toTypedArray(),
      advertisement.manufacturerData.map { it.first }.toIntArray(),
      advertisement.manufacturerData.map { it.second }.toTypedArray(),
      advertisement.serviceData.map { it.first }.toTypedArray(),
      advertisement.serviceData.map { it.second }.toTypedArray(),
      tristate(advertisement.connectable),
      advertisement.solicitedServiceUuids?.toTypedArray(),
      null,
      optional(advertisement.appearance),
      advertisement.rawRecord
    )

  override fun ingestConnection(peerId: String, connected: Boolean, status: Int?): Int =
    MobileCoreBridge.nativeIngestConnection(peerId, connected, optional(status))

  override fun ingestServicesChanged(peerId: String): Int = MobileCoreBridge.nativeIngestServicesChanged(peerId)

  override fun ingestNotification(instance: CharacteristicInstance, epoch: Long, value: ByteArray): Int =
    MobileCoreBridge.nativeIngestNotification(
      instance.peerId,
      instance.serviceUuid,
      instance.serviceOccurrence,
      instance.characteristicUuid,
      instance.characteristicOccurrence,
      epoch,
      value
    )

  override fun ingestAdapterState(state: AdapterFacts): Int =
    MobileCoreBridge.nativeIngestAdapterState(state.availability, state.authorization, state.power, state.safeReason)

  override fun ingestScanFailed(detail: String): Int = MobileCoreBridge.nativeIngestScanFailed(detail)

  override fun ingestRestored(peers: List<PresenceRestoredPeer>): Int =
    MobileCoreBridge.nativeIngestRestored(
      peers.map { it.peerId }.toTypedArray(),
      peers.map { it.name }.toTypedArray(),
      peers.map { it.connected }.toBooleanArray()
    )

  override fun ingestSecurity(peerId: String, security: SecurityFacts): Int =
    MobileCoreBridge.nativeIngestSecurity(
      peerId,
      security.bond,
      security.encryption,
      security.authentication,
      security.secureConnections,
      tristate(security.pairingPossible)
    )

  override fun ingestDropped(ingressClass: String, detail: String): Int =
    MobileCoreBridge.nativeIngestDropped(ingressClass, detail)
}
