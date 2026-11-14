// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/MobileCorePort.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.ubm.core.MobileCoreBridge

/** Exact-binding continuation controls. */
class JniBoundContinuationPort(private val binding: Long) {
  init { require(binding > 0) { "binding must be positive" } }

  fun configureRecordingDirectory(path: String) = MobileCoreBridge.nativeContinuationConfigureRecordingDirectoryForBinding(binding, path)
  fun describeBacklog(callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationDescribeBacklogForBinding(binding, callback)
  fun replacementFailure(declaration: String) = MobileCoreBridge.nativeContinuationDeclarationReplacementFailureForBinding(binding, declaration)
  fun execute(peer: String, declaration: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationExecuteForBinding(binding, peer, declaration, callback)
  fun reserveDeclaration(declaration: String) = MobileCoreBridge.nativeContinuationReserveDeclarationForBinding(binding, declaration)
  fun commitDeclaration(token: String) = MobileCoreBridge.nativeContinuationCommitDeclarationForBinding(binding, token)
  fun cancelDeclaration(token: String) = MobileCoreBridge.nativeContinuationCancelDeclarationForBinding(binding, token)
  fun seedDeclaration(declaration: String) = MobileCoreBridge.nativeContinuationSeedDeclarationForBinding(binding, declaration)
  fun prepareClaim(items: Int, bytes: Int, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationPrepareClaimForBinding(binding, items, bytes, callback)
  fun acknowledgeClaim(token: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationAcknowledgeClaimForBinding(binding, token, callback)
  fun prepareClaimVersioned(version: Int, items: Int, bytes: Int, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationPrepareClaimVersionedForBinding(binding, version, items, bytes, callback)
  fun acknowledgeClaimV1(token: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationAcknowledgeClaimV1ForBinding(binding, token, callback)
  fun acknowledgeClaimV2(token: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationAcknowledgeClaimV2ForBinding(binding, token, callback)
  fun readPeerFailureRecovery(record: String, offset: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationReadPeerFailureRecoveryForBinding(binding, record, offset, callback)
  fun acknowledgePeerFailureRecovery(record: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationAcknowledgePeerFailureRecoveryForBinding(binding, record, callback)
  fun retireClaimReceipt(token: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationRetireClaimReceiptForBinding(binding, token, callback)
  fun recordingControl(operation: String, id: String, token: String, items: Int, bytes: Int) = MobileCoreBridge.nativeContinuationRecordingControlForBinding(binding, operation, id, token, items, bytes)
  fun orderedVersion() = MobileCoreBridge.nativeOrderedRecordingVersionForBinding(binding)
  fun receipt(session: Long, operation: String, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeOrderedRecordingReceiptControlForBinding(binding, session, operation, id, callback)
  fun start(session: Long, handle: String, id: String, maxBytes: String, maxRecords: String, declaration: String, budgetMs: Long, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationStartOrderedForBinding(binding, session, handle, id, maxBytes, maxRecords, declaration, budgetMs, callback)
  fun stop(session: Long, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationStopOrderedForBinding(binding, session, id, callback)
  fun context(session: Long, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationOrderedContextForBinding(binding, session, id, callback)
  fun offline(operation: String, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationOrderedOfflineForBinding(binding, operation, id, callback)

  /**
   * Trusted process-host-only scan recovery bridge.  None of these handles is
   * a durable credential; it is scoped to [binding] and is revoked when that
   * JNI host retires.  This type is deliberately not exposed by the RN module.
   */
  fun scanFailureRecoveryAttach(recordingId: String) =
    MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(binding, recordingId)
  fun scanFailureRecoveryOwners(leaseId: String) =
    MobileCoreBridge.nativeScanFailureRecoveryOwnersForBinding(binding, leaseId)
  fun scanFailureRecoveryMember(ownerId: String, memberIndex: Int) =
    MobileCoreBridge.nativeScanFailureRecoveryMemberForBinding(binding, ownerId, memberIndex)
  fun scanFailureRecoveryHead(ownerId: String, memberIndex: Int) =
    MobileCoreBridge.nativeScanFailureRecoveryHeadForBinding(binding, ownerId, memberIndex)
  fun scanFailureRecoveryRead(headId: String, offset: String) =
    MobileCoreBridge.nativeScanFailureRecoveryReadForBinding(binding, headId, offset)
  /** One lifecycle-fenced page. JSI callers must use this rather than the
   * legacy metadata/bytes pair so teardown cannot split a page. */
  fun scanFailureRecoveryReadBytes(headId: String, offset: String): ByteArray =
    MobileCoreBridge.nativeScanFailureRecoveryReadBytesForBinding(binding, headId, offset)
  fun scanFailureRecoveryReadPage(headId: String, offset: String): MobileCoreBridge.ScanFailureRecoveryPage =
    MobileCoreBridge.nativeScanFailureRecoveryReadPageForBinding(binding, headId, offset)
  fun scanFailureRecoveryHeadControl(headId: String, operation: String) =
    MobileCoreBridge.nativeScanFailureRecoveryHeadControlForBinding(binding, headId, operation)
  /** Canonical raw durable-source metadata, fenced by this exact JNI binding. */
  fun rawInfo(ownerId: String): String =
    MobileCoreBridge.nativeScanFailureRecoveryRawInfoForBinding(binding, ownerId)
  /** Canonical raw member metadata, fenced by this exact JNI binding. */
  fun rawMember(ownerId: String, memberIndex: Int): String =
    MobileCoreBridge.nativeScanFailureRecoveryRawMemberForBinding(binding, ownerId, memberIndex)
  /**
   * One atomic, bounded raw token read. [offset] and [maxBytes] are canonical
   * unsigned-decimal strings; binary windows stay [ByteArray].
   */
  fun rawToken(
    ownerId: String,
    tokenIndex: Int,
    offset: String,
    maxBytes: String,
  ): MobileCoreBridge.ScanFailureRecoveryRawToken =
    MobileCoreBridge.nativeScanFailureRecoveryRawTokenForBinding(
      binding, ownerId, tokenIndex, offset, maxBytes)
  fun rawControl(ownerId: String, operation: String): String =
    MobileCoreBridge.nativeScanFailureRecoveryRawControlForBinding(binding, ownerId, operation)
  /** Revokes every opaque recovery alias for this exact JNI binding. */
  fun revokeScanFailureRecovery() =
    MobileCoreBridge.nativeScanFailureRecoveryRevokeForBinding(binding)
  fun scanFailureRecoveryPendingCleanup(leaseId: String) =
    MobileCoreBridge.nativeScanFailureRecoveryPendingCleanupForBinding(binding, leaseId)
  fun scanFailureRecoveryRetireCleanup(cleanupReceiptId: String) =
    MobileCoreBridge.nativeScanFailureRecoveryRetireCleanupForBinding(binding, cleanupReceiptId)
  /** Zero-argument recovery selection is owned by the native process host. */
  fun terminalEvidenceRecoveryOpenSelected() =
    MobileCoreBridge.nativeTerminalEvidenceRecoveryOpenSelectedForBinding(binding)
  fun terminalEvidenceRecoveryList(leaseId: String, maxItems: Int, maxBytes: Long) =
    MobileCoreBridge.nativeTerminalEvidenceRecoveryListForBinding(binding, leaseId, maxItems, maxBytes)
  fun terminalEvidenceRecoveryListWithStatus(leaseId: String, maxItems: Int, maxBytes: Long) =
    MobileCoreBridge.nativeTerminalEvidenceRecoveryListWithStatusForBinding(binding, leaseId, maxItems, maxBytes)
  fun terminalEvidenceRecoveryRead(handle: String, offset: String, maxBytes: Int) =
    MobileCoreBridge.nativeTerminalEvidenceRecoveryReadForBinding(binding, handle, offset, maxBytes)
  fun terminalEvidenceRecoveryAcknowledge(handle: String) =
    MobileCoreBridge.nativeTerminalEvidenceRecoveryAcknowledgeForBinding(binding, handle)

  /** Exact JNI binding identity used to fence a JSI recovery snapshot. */
  fun bindingIdentity(): Long = binding
}

/**
 * The JNI surface of the process-owned Rust mobile host
 * ([MobileCoreBridge]) as a seam. JVM
 * tests substitute a recording fake, so nothing here loads the cdylib.
 * Status codes are the bridge's `STATUS_*` constants.
 */
interface MobileCorePort {
  /** Retry an unpublished install's retained cleanup; true only when it is gone. */
  fun cleanupFailedInstall(): Boolean = true
  fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int =
    throw UnsupportedOperationException("owner retirement completion is unavailable")
  fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult): Int =
    throw UnsupportedOperationException("ordered completion is unavailable")
  fun completeOrderedEnableReleased(requestId: Long, failure: RadioFailure): Int =
    throw UnsupportedOperationException("ordered enable release is unavailable")
  fun ingestOrderedWake(generation: Long, group: Long): Int =
    throw UnsupportedOperationException("ordered wake is unavailable")
  fun ingestOrderedWakeFailure(generation: Long, group: Long): Int =
    throw UnsupportedOperationException("ordered wake failure signal is unavailable")
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
  fun negotiateScanEndVersion(sessionId: Long, offeredJson: String): String =
    throw UnsupportedOperationException("scan-end negotiation is unavailable")
  fun continuationConfigureRecordingDirectory(path: String): String
  fun orderedRecordingReceiptControl(sessionId: Long, operation: String, id: String, callback: MobileCoreBridge.InvokeCallback): Unit = throw UnsupportedOperationException("ordered recording is unavailable")
  fun orderedRecordingVersion(): Int = throw UnsupportedOperationException("ordered recording is unavailable")
  fun orderedRecordingStart(sessionId: Long, handle: String, id: String, maxBytes: String, maxRecords: String, declaration: String, budgetMs: Long, callback: MobileCoreBridge.InvokeCallback): Unit = throw UnsupportedOperationException("ordered recording is unavailable")
  fun orderedRecordingContext(sessionId: Long, id: String, callback: MobileCoreBridge.InvokeCallback): Unit = throw UnsupportedOperationException("ordered recording is unavailable")
  fun orderedRecordingStop(sessionId: Long, id: String, callback: MobileCoreBridge.InvokeCallback): Unit = throw UnsupportedOperationException("ordered recording is unavailable")
  fun orderedRecordingOffline(operation: String, id: String, callback: MobileCoreBridge.InvokeCallback): Unit = throw UnsupportedOperationException("ordered recording is unavailable")
  fun continuationRecordingControl(operation: String, id: String, token: String, maxItems: Int, maxBytes: Int): String
  fun continuationExecute(peerId: String, declarationJson: String, callback: MobileCoreBridge.InvokeCallback)
  fun continuationPrepareClaim(maxItems: Int, maxBytes: Int, callback: MobileCoreBridge.InvokeCallback)
  fun continuationAcknowledgeClaim(claimToken: String, callback: MobileCoreBridge.InvokeCallback)
  fun continuationPrepareClaimVersioned(version: Int, maxItems: Int, maxBytes: Int, callback: MobileCoreBridge.InvokeCallback): Unit =
    throw UnsupportedOperationException("versioned continuation claim is unavailable")
  fun continuationAcknowledgeClaimV1(claimToken: String, callback: MobileCoreBridge.InvokeCallback): Unit =
    throw UnsupportedOperationException("versioned continuation claim is unavailable")
  fun continuationAcknowledgeClaimV2(claimToken: String, callback: MobileCoreBridge.InvokeCallback): Unit =
    throw UnsupportedOperationException("versioned continuation claim is unavailable")
  fun continuationReadPeerFailureRecovery(recoveryRecord: String, offset: String, callback: MobileCoreBridge.InvokeCallback): Unit =
    throw UnsupportedOperationException("peer failure recovery is unavailable")
  fun continuationAcknowledgePeerFailureRecovery(recoveryRecord: String, callback: MobileCoreBridge.InvokeCallback): Unit =
    throw UnsupportedOperationException("peer failure recovery is unavailable")
  fun continuationRetireClaimReceipt(claimToken: String, callback: MobileCoreBridge.InvokeCallback): Unit =
    throw UnsupportedOperationException("versioned continuation claim is unavailable")
  fun continuationDescribeBacklog(callback: MobileCoreBridge.InvokeCallback)
  fun continuationDeclarationReplacementFailure(declarationJson: String): String?
  fun continuationReserveDeclaration(declarationJson: String): String
  fun continuationCommitDeclaration(token: String): String
  fun continuationCancelDeclaration(token: String): String
  fun continuationSeedDeclaration(declarationJson: String): String

  fun completeUnit(requestId: Long): Int
  /** Legacy implementations discard the v2 scan identity. */
  fun completeScanStarted(requestId: Long, scanIdentity: String): Int = completeUnit(requestId)
  /** A descriptor read's value. */
  fun completeBytes(requestId: Long, value: ByteArray): Int
  /** A characteristic read's value and what the platform says it is (`read-response` | `read-or-notification`). */
  fun completeRead(requestId: Long, value: ByteArray, provenance: String): Int
  fun completeAdapter(requestId: Long, availability: String, authorization: String, power: String, safeReason: String?): Int
  fun completeDiscovered(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray): Int
  fun completeDiscoveredScoped(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray,
                               owner: String, attachment: Long, revision: Long): Int =
    throw UnsupportedOperationException("scoped discovery completion is unavailable")
  fun completeNotifyEnabled(requestId: Long, delivery: String): Int
  fun completeMtu(requestId: Long, mtu: Int): Int
  fun completeWriteLimits(requestId: Long, withResponse: Int, withoutResponse: Int): Int
  fun completeRssi(requestId: Long, rssi: Int): Int
  fun completeAccepted(requestId: Long, accepted: Boolean): Int
  fun completeSubrateStatus(requestId: Long, status: Int): Int
  fun completePhy(requestId: Long, tx: String, rx: String): Int
  fun completePhyRequest(requestId: Long, accepted: Boolean, tx: String?, rx: String?): Int
  fun completeSecurity(requestId: Long, security: SecurityFacts): Int
  fun completeResolvedPeer(requestId: Long, peerId: String?, name: String?): Int
  fun completeConnectedPeers(requestId: Long, peerIds: Array<String>, names: Array<String?>): Int
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
  fun ingestConnectionForGeneration(peerId: String, expectedGeneration: String, connected: Boolean, status: Int?): Int
  fun ingestServicesChanged(peerId: String): Int
  fun ingestNotification(instance: CharacteristicInstance, epoch: Long, value: ByteArray): Int
  fun ingestNotificationSourceFailed(instance: CharacteristicInstance, epoch: Long, failure: RadioFailure): Int
  fun ingestAdapterState(state: AdapterFacts): Int
  fun ingestScanFailed(detail: String): Int
  fun ingestScanFailed(detail: String, scanIdentity: String): Int = ingestScanFailed(detail)
  fun ingestScanFailed(detail: String, scanIdentity: String, errorCode: Int): Int
  fun ingestRestored(peers: List<PresenceRestoredPeer>): Int
  fun ingestSecurity(peerId: String, security: SecurityFacts): Int
  fun ingestSecurityFailure(peerId: String?, failure: RadioFailure, encryptionStatus: Int?): Int
  fun ingestDropped(ingressClass: String, detail: String): Int
  fun ingestDroppedCount(ingressClass: String, detail: String, count: Long): Int
}

/** One characteristic scope whose close-time release failed. */
data class CloseFailure(val instance: CharacteristicInstance, val detail: String)

/** Production port: every call is the matching [MobileCoreBridge] native. */
object JniMobileCorePort : MobileCorePort {
  // Probe once: old cdylibs lack the symbol. Never invoke individual v2 methods
  // unless the peer explicitly advertises the complete extension.
  private val scanIdentityV2: Boolean by lazy {
    try { MobileCoreBridge.nativeScanIdentityWireVersion() == 2 } catch (_: UnsatisfiedLinkError) { false }
  }

  override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) =
    MobileCoreBridge.nativeCompleteOwnerRetirement(requestId, result)
  override fun completeOrderedEnableReleased(requestId: Long, failure: RadioFailure) =
    MobileCoreBridge.nativeCompleteOrderedEnableReleased(requestId, failure.kind.wire, failure.gattStatus ?: MobileCoreBridge.ABSENT_INT,
      failure.detail, failure.dispatched, failure.nativeCode)
  override fun completeDiscoveredScoped(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray,
                                        owner: String, attachment: Long, revision: Long) =
    MobileCoreBridge.nativeCompleteDiscoveredScoped(requestId, levels, uuids, occurrences, properties, owner, attachment, revision)
  override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult) = MobileCoreBridge.nativeCompleteOrdered(requestId, result)
  override fun ingestOrderedWake(generation: Long, group: Long) = MobileCoreBridge.nativeIngestOrderedWake(generation, group)
  override fun ingestOrderedWakeFailure(generation: Long, group: Long) = MobileCoreBridge.nativeIngestOrderedWakeFailure(generation, group)
  override fun continuationConfigureRecordingDirectory(path: String) = MobileCoreBridge.nativeContinuationConfigureRecordingDirectory(path)
  override fun orderedRecordingReceiptControl(sessionId: Long, operation: String, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeOrderedRecordingReceiptControl(sessionId, operation, id, callback)
  override fun orderedRecordingVersion() = MobileCoreBridge.nativeOrderedRecordingVersion()
  override fun orderedRecordingStart(sessionId: Long, handle: String, id: String, maxBytes: String, maxRecords: String, declaration: String, budgetMs: Long, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationStartOrdered(sessionId, handle, id, maxBytes, maxRecords, declaration, budgetMs, callback)
  override fun orderedRecordingContext(sessionId: Long, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationOrderedContext(sessionId, id, callback)
  override fun orderedRecordingStop(sessionId: Long, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationStopOrdered(sessionId, id, callback)
  override fun orderedRecordingOffline(operation: String, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationOrderedOffline(operation, id, callback)
  override fun continuationRecordingControl(operation: String, id: String, token: String, maxItems: Int, maxBytes: Int) =
    MobileCoreBridge.nativeContinuationRecordingControl(operation, id, token, maxItems, maxBytes)
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

  override fun negotiateScanEndVersion(sessionId: Long, offeredJson: String): String =
    MobileCoreBridge.nativeNegotiateScanEndVersionQualified(sessionId, offeredJson)

  override fun continuationExecute(peerId: String, declarationJson: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationExecute(peerId, declarationJson, callback)

  override fun continuationPrepareClaim(maxItems: Int, maxBytes: Int, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationPrepareClaim(maxItems, maxBytes, callback)

  override fun continuationAcknowledgeClaim(claimToken: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationAcknowledgeClaim(claimToken, callback)

  override fun continuationPrepareClaimVersioned(version: Int, maxItems: Int, maxBytes: Int, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationPrepareClaimVersioned(version, maxItems, maxBytes, callback)
  override fun continuationAcknowledgeClaimV1(claimToken: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationAcknowledgeClaimV1(claimToken, callback)
  override fun continuationRetireClaimReceipt(claimToken: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationRetireClaimReceipt(claimToken, callback)

  override fun continuationDescribeBacklog(callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationDescribeBacklog(callback)

  override fun continuationDeclarationReplacementFailure(declarationJson: String): String? =
    MobileCoreBridge.nativeContinuationDeclarationReplacementFailure(declarationJson)
  override fun continuationReserveDeclaration(declarationJson: String) = MobileCoreBridge.nativeContinuationReserveDeclaration(declarationJson)
  override fun continuationCommitDeclaration(token: String) = MobileCoreBridge.nativeContinuationCommitDeclaration(token)
  override fun continuationCancelDeclaration(token: String) = MobileCoreBridge.nativeContinuationCancelDeclaration(token)
  override fun continuationSeedDeclaration(declarationJson: String) = MobileCoreBridge.nativeContinuationSeedDeclaration(declarationJson)

  override fun completeUnit(requestId: Long): Int = MobileCoreBridge.nativeCompleteUnit(requestId)
  override fun completeScanStarted(requestId: Long, scanIdentity: String): Int =
    if (scanIdentityV2) MobileCoreBridge.nativeCompleteScanStartedV2(requestId, scanIdentity)
    else completeUnit(requestId)

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
  override fun completeSubrateStatus(requestId: Long, status: Int): Int = MobileCoreBridge.nativeCompleteSubrateStatus(requestId, status)
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

  override fun completeResolvedPeer(requestId: Long, peerId: String?, name: String?): Int = MobileCoreBridge.nativeCompleteResolvedPeer(requestId, peerId, name)
  override fun completeConnectedPeers(requestId: Long, peerIds: Array<String>, names: Array<String?>): Int =
    MobileCoreBridge.nativeCompleteConnectedPeers(requestId, peerIds, names)

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

  override fun ingestAdvertisement(advertisement: AdvertisementFacts): Int {
    val scanIdentity = advertisement.scanIdentity
    return if (scanIdentityV2 && scanIdentity != null) MobileCoreBridge.nativeIngestAdvertisementV2(
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
      advertisement.rawRecord,
      advertisement.cachedName,
      advertisement.captureTimestampMs ?: -1L,
      scanIdentity
    ) else MobileCoreBridge.nativeIngestAdvertisement(
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
      advertisement.rawRecord,
      advertisement.cachedName,
      advertisement.captureTimestampMs ?: -1L
    )
  }

  override fun ingestConnection(peerId: String, connected: Boolean, status: Int?): Int =
    MobileCoreBridge.nativeIngestConnection(peerId, connected, optional(status))

  override fun ingestConnectionForGeneration(peerId: String, expectedGeneration: String, connected: Boolean, status: Int?): Int =
    MobileCoreBridge.nativeIngestConnectionForGeneration(peerId, expectedGeneration, connected, optional(status))

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

  override fun ingestNotificationSourceFailed(instance: CharacteristicInstance, epoch: Long, failure: RadioFailure): Int =
    MobileCoreBridge.nativeIngestNotificationSourceFailed(
      instance.peerId, instance.serviceUuid, instance.serviceOccurrence,
      instance.characteristicUuid, instance.characteristicOccurrence, epoch,
      failure.kind.wire, optional(failure.gattStatus), failure.detail, failure.dispatched, failure.nativeCode
    )

  override fun ingestAdapterState(state: AdapterFacts): Int =
    MobileCoreBridge.nativeIngestAdapterState(state.availability, state.authorization, state.power, state.safeReason)

  override fun ingestScanFailed(detail: String): Int = MobileCoreBridge.nativeIngestScanFailed(detail)
  override fun ingestScanFailed(detail: String, scanIdentity: String): Int =
    if (scanIdentityV2) MobileCoreBridge.nativeIngestScanFailedV2(detail, scanIdentity)
    else ingestScanFailed(detail)
  override fun ingestScanFailed(detail: String, scanIdentity: String, errorCode: Int): Int =
    MobileCoreBridge.nativeIngestScanFailedV3(detail, scanIdentity, errorCode)


  override fun ingestRestored(peers: List<PresenceRestoredPeer>): Int =
    MobileCoreBridge.nativeIngestRestored(
      peers.map { it.peerId }.toTypedArray(),
      peers.map { it.name }.toTypedArray(),
      peers.map { it.connected }.toBooleanArray()
    )

  override fun ingestSecurityFailure(peerId: String?, failure: RadioFailure, encryptionStatus: Int?): Int =
    MobileCoreBridge.nativeIngestSecurityFailure(peerId, failure.kind.wire, encryptionStatus ?: MobileCoreBridge.ABSENT_INT, failure.detail)

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

  override fun ingestDroppedCount(ingressClass: String, detail: String, count: Long): Int =
    MobileCoreBridge.nativeIngestDroppedCount(ingressClass, detail, count)
}


/** Install operations are injectable for synthetic JVM ownership tests. */
internal interface BoundInstallApi {
  fun routeVersion(): Int
  fun install(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, label: String): Long
  fun find(radio: MobileCoreBridge.RadioHost): Long
  fun shutdown(binding: Long): String
}

private object NativeBoundInstallApi : BoundInstallApi {
  override fun routeVersion() = MobileCoreBridge.nativeBindingRouteVersion()
  override fun install(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, label: String) =
    MobileCoreBridge.nativeInstallHostForBinding(1, radio, wake, "android", owner, label)
  override fun find(radio: MobileCoreBridge.RadioHost) = MobileCoreBridge.nativeFindRetainedBinding(radio)
  override fun shutdown(binding: Long) = MobileCoreBridge.nativeShutdownHostForBinding(binding)
}

/** A single JNI host identity. The radio must call [acceptBindingRoute] during
 * install, before it sends any completion or observation. A port that has
 * received an identity is never reused, including after a failed install. */
class JniBoundMobileCorePort(
  private val offlineRecordingControl: (String, String, String, Int, Int) -> String =
    MobileCoreBridge::nativeContinuationRecordingControlOffline,
  private val offlineRecordingVersion: () -> Int = MobileCoreBridge::nativeOrderedRecordingVersionOffline
) : MobileCorePort {
  internal var installApi: BoundInstallApi = NativeBoundInstallApi
  private val lock = Any()
  @Volatile private var token = 0L
  @Volatile private var installed = false
  @Volatile private var attempted = false
  @Volatile private var nativeInstallStarted = false
  private var installing = false
  private var retainedRadio: MobileCoreBridge.RadioHost? = null

  fun acceptBindingRoute(version: Int, binding: Long): Boolean = synchronized(lock) {
    if (!installing || version != 1 || binding <= 0 || token != 0L) false
    else { token = binding; true }
  }

  private fun binding(): Long = token.takeIf { it > 0 }
    ?: throw IllegalStateException("binding has not been assigned")

  /** Native-process-only recovery capability.  Callers must already have
   * configured app-private recording storage; RN/Expo never receives this. */
  internal fun scanFailureRecoveryPort(): JniBoundContinuationPort {
    check(installed) { "scan failure recovery requires an installed binding" }
    return JniBoundContinuationPort(binding())
  }

  override fun installHost(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, adapterLabel: String) {
    synchronized(lock) {
      check(!attempted) { "a binding port cannot be reused" }
      attempted = true
      installing = true
      retainedRadio = radio
    }
    try {
      check(installApi.routeVersion() == 1) { "binding route version unavailable" }
      nativeInstallStarted = true
      val result = installApi.install(radio, wake, owner, adapterLabel)
      check(result > 0 && token == result) { "radio did not accept the exact binding" }
      installed = true
    } catch (failure: Throwable) {
      // JNI may have retained an unpublished host even when acceptance threw.
      val retained = if (nativeInstallStarted) installApi.find(radio) else 0L
      if (retained > 0) {
        check(token == 0L || token == retained) { "retained binding identity changed" }
        token = retained
      }
      throw failure
    } finally {
      synchronized(lock) { installing = false }
    }
  }

  override fun hostInstalled(): Boolean = installed

  override fun cleanupFailedInstall(): Boolean {
    if (token == 0L) {
      val radio = retainedRadio ?: return true
      if (!nativeInstallStarted) { retainedRadio = null; return true }
      val retained = installApi.find(radio)
      if (retained == 0L) { retainedRadio = null; return true }
      token = retained
    }
    shutdownHost()
    return retainedRadio == null
  }

  /** Returns the native cleanup receipt. Failed cleanup remains retryable. */
  fun shutdownHost(): String {
    val identity = binding()
    val receipt = installApi.shutdown(identity)
    val radio = retainedRadio
    val released = try { (RustCoreJson.parse(receipt) as? Map<*, *>)?.get("state") == "released" }
      catch (_: IllegalArgumentException) { false }
    if (radio != null && installApi.find(radio) == 0L &&
        released) {
      installed = false
      retainedRadio = null
    }
    return receipt
  }

  override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) =
    MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(binding(), requestId, result)
  override fun completeOrderedEnableReleased(requestId: Long, failure: RadioFailure) =
    MobileCoreBridge.nativeCompleteOrderedEnableReleasedForBinding(binding(), requestId, failure.kind.wire, failure.gattStatus ?: MobileCoreBridge.ABSENT_INT,
      failure.detail, failure.dispatched, failure.nativeCode)
  override fun completeDiscoveredScoped(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray,
                                        owner: String, attachment: Long, revision: Long) =
    MobileCoreBridge.nativeCompleteDiscoveredScopedForBinding(binding(), requestId, levels, uuids, occurrences, properties, owner, attachment, revision)
  override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult) = MobileCoreBridge.nativeCompleteOrderedForBinding(binding(), requestId, result)
  override fun ingestOrderedWake(generation: Long, group: Long) = MobileCoreBridge.nativeIngestOrderedWakeForBinding(binding(), generation, group)
  override fun ingestOrderedWakeFailure(generation: Long, group: Long) = MobileCoreBridge.nativeIngestOrderedWakeFailureForBinding(binding(), generation, group)
  override fun continuationConfigureRecordingDirectory(path: String) =
    MobileCoreBridge.nativeContinuationConfigureRecordingDirectory(path)
  override fun orderedRecordingReceiptControl(sessionId: Long, operation: String, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeOrderedRecordingReceiptControlForBinding(binding(), sessionId, operation, id, callback)
  override fun orderedRecordingVersion() =
    if (!installed) offlineRecordingVersion()
    else MobileCoreBridge.nativeOrderedRecordingVersionForBinding(binding())
  override fun orderedRecordingStart(sessionId: Long, handle: String, id: String, maxBytes: String, maxRecords: String, declaration: String, budgetMs: Long, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationStartOrderedForBinding(binding(), sessionId, handle, id, maxBytes, maxRecords, declaration, budgetMs, callback)
  override fun orderedRecordingContext(sessionId: Long, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationOrderedContextForBinding(binding(), sessionId, id, callback)
  override fun orderedRecordingStop(sessionId: Long, id: String, callback: MobileCoreBridge.InvokeCallback) = MobileCoreBridge.nativeContinuationStopOrderedForBinding(binding(), sessionId, id, callback)
  override fun orderedRecordingOffline(operation: String, id: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationOrderedOffline(operation, id, callback)
  override fun continuationRecordingControl(operation: String, id: String, token: String, maxItems: Int, maxBytes: Int) =
    if (!installed) offlineRecordingControl(operation, id, token, maxItems, maxBytes)
    else MobileCoreBridge.nativeContinuationRecordingControlForBinding(binding(), operation, id, token, maxItems, maxBytes)
  private fun optional(value: Int?): Int = value ?: MobileCoreBridge.ABSENT_INT

  private fun tristate(value: Boolean?): Int = when (value) {
    null -> -1
    true -> 1
    false -> 0
  }

  override fun buildIdentityJson(): String = MobileCoreBridge.nativeBuildIdentityJson()
  override fun contractRevision(): String = MobileCoreBridge.nativeContractRevision()
  override fun wireRevision(): String = MobileCoreBridge.nativeWireRevision()

  override fun openSession(owner: String, expectedWireRevision: String, backgroundScope: String) =
    MobileCoreBridge.nativeOpenSessionForBinding(binding(), owner, expectedWireRevision, backgroundScope)
  override fun releaseBackgroundScope(backgroundScope: String) =
    MobileCoreBridge.nativeReleaseBackgroundScopeForBinding(binding(), backgroundScope)
  override fun invoke(sessionId: Long, op: String, argsJson: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeInvokeForBinding(binding(), sessionId, op, argsJson, callback)
  override fun drain(sessionId: Long, maxItems: Int, maxBytes: Int) =
    MobileCoreBridge.nativeDrainForBinding(binding(), sessionId, maxItems, maxBytes)
  override fun negotiateScanEndVersion(sessionId: Long, offeredJson: String) =
    MobileCoreBridge.nativeNegotiateScanEndVersionQualifiedForBinding(binding(), sessionId, offeredJson)

  override fun continuationExecute(peerId: String, declarationJson: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationExecuteForBinding(binding(), peerId, declarationJson, callback)

  override fun continuationPrepareClaim(maxItems: Int, maxBytes: Int, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationPrepareClaimForBinding(binding(), maxItems, maxBytes, callback)

  override fun continuationAcknowledgeClaim(claimToken: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationAcknowledgeClaimForBinding(binding(), claimToken, callback)

  override fun continuationDescribeBacklog(callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationDescribeBacklogForBinding(binding(), callback)

  override fun continuationPrepareClaimVersioned(version: Int, maxItems: Int, maxBytes: Int, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationPrepareClaimVersionedForBinding(binding(), version, maxItems, maxBytes, callback)
  override fun continuationAcknowledgeClaimV1(claimToken: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationAcknowledgeClaimV1ForBinding(binding(), claimToken, callback)
  override fun continuationAcknowledgeClaimV2(claimToken: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationAcknowledgeClaimV2ForBinding(binding(), claimToken, callback)
  override fun continuationReadPeerFailureRecovery(recoveryRecord: String, offset: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationReadPeerFailureRecoveryForBinding(binding(), recoveryRecord, offset, callback)
  override fun continuationAcknowledgePeerFailureRecovery(recoveryRecord: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationAcknowledgePeerFailureRecoveryForBinding(binding(), recoveryRecord, callback)
  override fun continuationRetireClaimReceipt(claimToken: String, callback: MobileCoreBridge.InvokeCallback) =
    MobileCoreBridge.nativeContinuationRetireClaimReceiptForBinding(binding(), claimToken, callback)

  override fun continuationDeclarationReplacementFailure(declarationJson: String): String? =
    MobileCoreBridge.nativeContinuationDeclarationReplacementFailureForBinding(binding(), declarationJson)
  override fun continuationReserveDeclaration(declarationJson: String) = MobileCoreBridge.nativeContinuationReserveDeclarationForBinding(binding(), declarationJson)
  override fun continuationCommitDeclaration(token: String) = MobileCoreBridge.nativeContinuationCommitDeclarationForBinding(binding(), token)
  override fun continuationCancelDeclaration(token: String) = MobileCoreBridge.nativeContinuationCancelDeclarationForBinding(binding(), token)
  override fun continuationSeedDeclaration(declarationJson: String) = MobileCoreBridge.nativeContinuationSeedDeclarationForBinding(binding(), declarationJson)

  override fun completeUnit(requestId: Long): Int = MobileCoreBridge.nativeCompleteUnitForBinding(binding(), requestId)
  override fun completeScanStarted(requestId: Long, scanIdentity: String): Int =
    MobileCoreBridge.nativeCompleteScanStartedV2ForBinding(binding(), requestId, scanIdentity)

  override fun completeBytes(requestId: Long, value: ByteArray): Int = MobileCoreBridge.nativeCompleteBytesForBinding(binding(), requestId, value)
  override fun completeRead(requestId: Long, value: ByteArray, provenance: String): Int =
    MobileCoreBridge.nativeCompleteReadForBinding(binding(), requestId, value, provenance)
  override fun completeAdapter(requestId: Long, availability: String, authorization: String, power: String, safeReason: String?): Int =
    MobileCoreBridge.nativeCompleteAdapterForBinding(binding(), requestId, availability, authorization, power, safeReason)

  override fun completeDiscovered(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray): Int =
    MobileCoreBridge.nativeCompleteDiscoveredForBinding(binding(), requestId, levels, uuids, occurrences, properties)

  override fun completeNotifyEnabled(requestId: Long, delivery: String): Int =
    MobileCoreBridge.nativeCompleteNotifyEnabledForBinding(binding(), requestId, delivery)

  override fun completeMtu(requestId: Long, mtu: Int): Int = MobileCoreBridge.nativeCompleteMtuForBinding(binding(), requestId, mtu)
  override fun completeWriteLimits(requestId: Long, withResponse: Int, withoutResponse: Int): Int =
    MobileCoreBridge.nativeCompleteWriteLimitsForBinding(binding(), requestId, withResponse, withoutResponse)
  override fun completeRssi(requestId: Long, rssi: Int): Int = MobileCoreBridge.nativeCompleteRssiForBinding(binding(), requestId, rssi)
  override fun completeAccepted(requestId: Long, accepted: Boolean): Int = MobileCoreBridge.nativeCompleteAcceptedForBinding(binding(), requestId, accepted)
  override fun completeSubrateStatus(requestId: Long, status: Int): Int = MobileCoreBridge.nativeCompleteSubrateStatusForBinding(binding(), requestId, status)
  override fun completePhy(requestId: Long, tx: String, rx: String): Int = MobileCoreBridge.nativeCompletePhyForBinding(binding(), requestId, tx, rx)
  override fun completePhyRequest(requestId: Long, accepted: Boolean, tx: String?, rx: String?): Int =
    MobileCoreBridge.nativeCompletePhyRequestForBinding(binding(), requestId, accepted, tx, rx)

  override fun completeSecurity(requestId: Long, security: SecurityFacts): Int =
    MobileCoreBridge.nativeCompleteSecurityForBinding(binding(),
      requestId,
      security.bond,
      security.encryption,
      security.authentication,
      security.secureConnections,
      tristate(security.pairingPossible)
    )

  override fun completeResolvedPeer(requestId: Long, peerId: String?, name: String?): Int = MobileCoreBridge.nativeCompleteResolvedPeerForBinding(binding(), requestId, peerId, name)
  override fun completeConnectedPeers(requestId: Long, peerIds: Array<String>, names: Array<String?>): Int =
    MobileCoreBridge.nativeCompleteConnectedPeersForBinding(binding(), requestId, peerIds, names)

  override fun completeBondedPeers(requestId: Long, peerIds: Array<String>, names: Array<String?>): Int =
    MobileCoreBridge.nativeCompleteBondedPeersForBinding(binding(), requestId, peerIds, names)

  override fun completeLease(requestId: Long, leaseId: String): Int = MobileCoreBridge.nativeCompleteLeaseForBinding(binding(), requestId, leaseId)
  override fun completeCompanionList(
    requestId: Long,
    associationIds: LongArray,
    peerIds: Array<String?>,
    displayNames: Array<String?>
  ): Int = MobileCoreBridge.nativeCompleteCompanionListForBinding(binding(), requestId, associationIds, peerIds, displayNames)
  override fun completeCompanion(
    requestId: Long,
    associationId: Long,
    peerId: String?,
    displayName: String?,
    alreadyAssociated: Boolean
  ): Int =
    MobileCoreBridge.nativeCompleteCompanionForBinding(binding(), requestId, associationId, peerId, displayName, alreadyAssociated)

  override fun completeClosed(requestId: Long, failures: List<CloseFailure>): Int =
    MobileCoreBridge.nativeCompleteClosedForBinding(binding(),
      requestId,
      failures.map { it.instance.peerId }.toTypedArray(),
      failures.map { it.instance.serviceUuid }.toTypedArray(),
      failures.map { it.instance.serviceOccurrence }.toLongArray(),
      failures.map { it.instance.characteristicUuid }.toTypedArray(),
      failures.map { it.instance.characteristicOccurrence }.toLongArray(),
      failures.map { it.detail }.toTypedArray()
    )

  override fun completeFailure(requestId: Long, failure: RadioFailure): Int =
    MobileCoreBridge.nativeCompleteFailureForBinding(binding(),
      requestId,
      failure.kind.wire,
      optional(failure.gattStatus),
      failure.detail,
      failure.dispatched,
      failure.nativeCode
    )

  override fun ingestAdvertisement(advertisement: AdvertisementFacts): Int {
    val scanIdentity = advertisement.scanIdentity
    return if (scanIdentity != null) MobileCoreBridge.nativeIngestAdvertisementV2ForBinding(binding(),
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
      advertisement.rawRecord,
      advertisement.cachedName,
      advertisement.captureTimestampMs ?: -1L,
      scanIdentity
    ) else MobileCoreBridge.nativeIngestAdvertisementForBinding(binding(),
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
      advertisement.rawRecord,
      advertisement.cachedName,
      advertisement.captureTimestampMs ?: -1L
    )
  }

  override fun ingestConnection(peerId: String, connected: Boolean, status: Int?): Int =
    MobileCoreBridge.nativeIngestConnectionForBinding(binding(), peerId, connected, optional(status))

  override fun ingestConnectionForGeneration(peerId: String, expectedGeneration: String, connected: Boolean, status: Int?): Int =
    MobileCoreBridge.nativeIngestConnectionForGenerationForBinding(binding(), peerId, expectedGeneration, connected, optional(status))

  override fun ingestServicesChanged(peerId: String): Int = MobileCoreBridge.nativeIngestServicesChangedForBinding(binding(), peerId)

  override fun ingestNotification(instance: CharacteristicInstance, epoch: Long, value: ByteArray): Int =
    MobileCoreBridge.nativeIngestNotificationForBinding(binding(),
      instance.peerId,
      instance.serviceUuid,
      instance.serviceOccurrence,
      instance.characteristicUuid,
      instance.characteristicOccurrence,
      epoch,
      value
    )

  override fun ingestNotificationSourceFailed(instance: CharacteristicInstance, epoch: Long, failure: RadioFailure): Int =
    MobileCoreBridge.nativeIngestNotificationSourceFailedForBinding(binding(),
      instance.peerId, instance.serviceUuid, instance.serviceOccurrence,
      instance.characteristicUuid, instance.characteristicOccurrence, epoch,
      failure.kind.wire, optional(failure.gattStatus), failure.detail, failure.dispatched, failure.nativeCode
    )

  override fun ingestAdapterState(state: AdapterFacts): Int =
    MobileCoreBridge.nativeIngestAdapterStateForBinding(binding(), state.availability, state.authorization, state.power, state.safeReason)

  override fun ingestScanFailed(detail: String): Int = MobileCoreBridge.nativeIngestScanFailedForBinding(binding(), detail)
  override fun ingestScanFailed(detail: String, scanIdentity: String): Int =
    MobileCoreBridge.nativeIngestScanFailedV2ForBinding(binding(), detail, scanIdentity)
  override fun ingestScanFailed(detail: String, scanIdentity: String, errorCode: Int): Int =
    MobileCoreBridge.nativeIngestScanFailedV3ForBinding(binding(), detail, scanIdentity, errorCode)


  override fun ingestRestored(peers: List<PresenceRestoredPeer>): Int =
    MobileCoreBridge.nativeIngestRestoredForBinding(binding(),
      peers.map { it.peerId }.toTypedArray(),
      peers.map { it.name }.toTypedArray(),
      peers.map { it.connected }.toBooleanArray()
    )

  override fun ingestSecurityFailure(peerId: String?, failure: RadioFailure, encryptionStatus: Int?): Int =
    MobileCoreBridge.nativeIngestSecurityFailureForBinding(binding(), peerId, failure.kind.wire, encryptionStatus ?: MobileCoreBridge.ABSENT_INT, failure.detail)

  override fun ingestSecurity(peerId: String, security: SecurityFacts): Int =
    MobileCoreBridge.nativeIngestSecurityForBinding(binding(),
      peerId,
      security.bond,
      security.encryption,
      security.authentication,
      security.secureConnections,
      tristate(security.pairingPossible)
    )

  override fun ingestDropped(ingressClass: String, detail: String): Int =
    MobileCoreBridge.nativeIngestDroppedForBinding(binding(), ingressClass, detail)

  override fun ingestDroppedCount(ingressClass: String, detail: String, count: Long): Int =
    MobileCoreBridge.nativeIngestDroppedCountForBinding(binding(), ingressClass, detail, count)
}
