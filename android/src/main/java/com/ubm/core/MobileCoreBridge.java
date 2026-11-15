// android/src/main/java/com/ubm/core/MobileCoreBridge.java
//
// JNI facade of the process-owned mobile Rust owner (`crates/ubm-mobile`,
// natives in `bindings/jni/src/mobile.rs`, library `libubm5_jni_echo.so`).
//
// Ownership: native signatures are single-owned by the Rust cdylib. Both
// the shipped AAR and standalone JVM smoke compile this authoritative facade;
// there is no separately maintained probe-side copy.
//
// Shape (wire revision `ubm-mobile-wire/3`, docs/MOBILE_RUST_WIRE.md):
//   * each bound host owns its platform radio and wake listener; exact binding
//     routes keep callbacks and session identities with that host;
//   * one session per RN manager: `nativeOpenSession` admits and answers
//     `{sessionId, contractRevision, wireRevision, buildIdentity}`;
//   * `nativeInvoke` never blocks: the callback receives the envelope JSON
//     exactly once, on a Rust worker thread;
//   * `nativeDrain` is synchronous and cheap; call it after `onWake`;
//   * every `RadioHost` request is answered exactly once through one
//     `nativeComplete*` call carrying the same request id;
//   * unsolicited platform facts enter through `nativeIngest*`.
//
// Java never parses JS args: `argsJson` is forwarded verbatim to Rust.
// Optional integers use sentinels documented per method (never 0 for a
// real value that can be 0).

package com.ubm.core;

public final class MobileCoreBridge {
    static {
        System.loadLibrary("ubm5_jni_echo");
    }

    private MobileCoreBridge() {}

    /** Sentinel for an absent optional {@code int} (RSSI, TX power, GATT status). */
    public static final int ABSENT_INT = Integer.MIN_VALUE;

    /** Completion/ingress status codes returned by the native calls. */
    public static final int STATUS_DELIVERED = 0;
    public static final int STATUS_LATE = 1;
    public static final int STATUS_MISMATCHED = 2;
    public static final int STATUS_ACCEPTED = 0;
    public static final int STATUS_DROPPED_ADVERTISEMENT = 1;
    public static final int STATUS_DROPPED_NOTIFICATION = 2;
    public static final int STATUS_DROPPED_CONTROL = 3;
    public static final int STATUS_CLOSED = 4;
    /** No host is installed (or it was shut down). */
    public static final int STATUS_NO_HOST = -1;

    /** Typed failure: {@code code|domain|operation|detail} wire message. */
    public static final class MobileCoreException extends RuntimeException {
        private static final long serialVersionUID = 1L;
        public final String code;
        public final String domain;
        public final String operation;

        public MobileCoreException(String wire) {
            super(wire);
            String[] parts = wire.split("\\|", 4);
            this.code = parts.length > 0 ? parts[0] : "";
            this.domain = parts.length > 1 ? parts[1] : "";
            this.operation = parts.length > 2 ? parts[2] : "";
        }
    }

    /** JS wakeup: records are waiting for {@code sessionId}; call {@link #nativeDrain}. */
    public interface WakeListener {
        void onWake(long sessionId);
    }

    /** Receives one invoke envelope (JSON text) exactly once. */
    public interface InvokeCallback {
        void onResult(String envelopeJson);
    }

    /**
     * The platform radio. Every method must return immediately (post to the
     * adapter's own thread) and answer {@code requestId} exactly once with a
     * {@code nativeComplete*} call. String enums are the frozen wire strings;
     * a {@code null} String means "absent". {@code legacy} is -1 (platform
     * default), 0 or 1.
     */
    public static final class OrderedRegistration {
        public long member, sourceMember, epoch;
    }
    /** Input descriptor for the private v2 ordered-open admission protocol. */
    public static final class OrderedOpenIdentity {
        public final int version, slot, itemCapacity, byteCapacity;
        public final String owner, peer, connection, database, snapshot;
        public final long generation, original, group, attachment, revision;
        public final long[] members, services, characteristics;
        public final String[] peers, serviceUuids, characteristicUuids;
        public OrderedOpenIdentity(int version, String owner, int slot, long generation, long original, long group,
                String peer, String connection, String database, String snapshot, long attachment, long revision,
                long[] members, String[] peers, String[] serviceUuids, long[] services, String[] characteristicUuids,
                long[] characteristics, int itemCapacity, int byteCapacity) {
            this.version = version; this.owner = owner; this.slot = slot; this.generation = generation;
            this.original = original; this.group = group; this.peer = peer; this.connection = connection;
            this.database = database; this.snapshot = snapshot; this.attachment = attachment; this.revision = revision;
            this.members = members.clone(); this.peers = peers.clone(); this.serviceUuids = serviceUuids.clone();
            this.services = services.clone(); this.characteristicUuids = characteristicUuids.clone();
            this.characteristics = characteristics.clone(); this.itemCapacity = itemCapacity; this.byteCapacity = byteCapacity;
        }
    }
    /** Ordered source records include subsequent-source-failure (failure fields)
     * and failure-observations-omitted (unknown number; no count). */
    public static final class OrderedRecord {
        public String kind, provenance, delivery, scope, unit, reason, nativeDomain, nativeCode, bridgeCode, detail, rejectedKind, failureKind, nativeName;
        public OrderedRegistration member, failureMember, rejectedMember;
        public long ordinal, scopeMember, count, byteCount, rejectedByteCount, barrierId, rejectedBarrierId, markerId, rejectedMarkerId;
        public boolean hasCount, hasByteCount, uncertainTail, dispatched;
        public int gattStatus;
        public byte[] value;
    }
    public static final class OrderedResult {
        public String kind, generationKind, generationValue, ownershipKind, failureKind, detail, nativeName, status, delivery, admission, writeState, ownerIncarnation, reasonKind, nativeDomain;
        public long authorityId;
        public int gattStatus, slot;
        public boolean dispatched, hasFailure, hasNativeCode;
        public long slotGeneration, nativeCode;
        public long recoveryKey, ordinal;
        public OrderedRegistration[] members;
        public OrderedRegistration member;
        public boolean hasBatch;
        public long batch;
        public OrderedRecord[] records;
    }

    /** Private v4 owner-control receipt; field names and types are decoded by JNI. */
    public static final class OwnerRetirementResult {
        public String ownerIncarnation, retirementId, state, reason;
        public String[] outstandingClasses = new String[0];
        public OrderedResult[] failures = new OrderedResult[0];
    }

    public interface RadioHost {
        /** Accept an exact binding before any request is dispatched. Legacy hosts refuse it. */
        default boolean acceptBindingRoute(int version, long binding) { return false; }
        /** Versioned identity of the actual ordered-open admission owner; empty means unsupported. */
        String orderedAdmissionOwner(int version);
        default void ownerFence(long requestId, String ownerIncarnation, String retirementId) { throw new UnsupportedOperationException("owner control v4"); }
        default void ownerStatus(long requestId, String ownerIncarnation, String retirementId) { throw new UnsupportedOperationException("owner control v4"); }
        default void orderedOpenV2(long requestId, OrderedOpenIdentity identity) { throw new UnsupportedOperationException("ordered admission v2"); }
        default void orderedRecoverOpenV2(long requestId, OrderedOpenIdentity identity) { throw new UnsupportedOperationException("ordered admission v2"); }
        default void orderedAcknowledgeOpenV2(long requestId, OrderedOpenIdentity identity) { throw new UnsupportedOperationException("ordered admission v2"); }
        void orderedOpen(long requestId, long group, long[] members, String[] peers, String[] services,
                         long[] serviceOccurrences, String[] characteristics, long[] characteristicOccurrences,
                         int itemCapacity, int byteCapacity, String connectionGeneration, String databaseGeneration,
                         String snapshotOwner, long snapshotAttachment, long snapshotRevision);
        void orderedRecoverOpen(long requestId, long groupId, long openRequestId, String peerId);
        void orderedAcknowledgeOpen(long requestId, int version, long groupId, long openRequestId, String peerId);
        void orderedRequest(long requestId, String operation, String generationKind, String generationValue,
                            long group, long member, long sourceMember, long epoch, long batch);

        void orderedGraphPermitReservation(long requestId, String operation, long authorityId, long request, String peerId, String connectionGeneration, String snapshotOwner, long snapshotAttachment, long snapshotRevision);
        void orderedGraphAuthority(long requestId, String operation, long authorityValue);
        void orderedGraphPermit(long requestId, String operation, String peerId, String connectionGeneration, String snapshotOwner, long snapshotAttachment, long snapshotRevision, long authorityId, long permitId);
        void orderedMarkerWrite(long requestId, String operation, String generationKind, String generationValue, long group, long markerId, String connectionGeneration, String snapshotOwner, long snapshotAttachment, long snapshotRevision, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, byte[] value, boolean withResponse);

        /** Answer: {@link #nativeCompleteAdapter}. */
        void adapterState(long requestId);

        /** Answer: {@link #nativeCompleteUnit}. {@code scanMode}: low-power|balanced|low-latency|opportunistic;
         * {@code callbackType}: all-matches|first-match; {@code reportDelayMs}: 0 for immediate delivery;
         * {@code phy}: null|all-supported|1m|coded, with explicit PHY requiring nonlegacy scanning. */
        void startScan(long requestId, String[] serviceUuids, String[] deviceAddresses, String scanMode, String callbackType, int legacy, long reportDelayMs, String phy);

        /** Answer: {@link #nativeCompleteUnit}. */
        void stopScan(long requestId);

        /**
         * Answer: {@link #nativeCompleteUnit} once connected (a restored, still-connected peer answers at once).
         * {@code preferredPhy}: le-1m|le-2m|le-coded, the PHYs to establish the link on (empty = no
         * preference; never with {@code autoConnect}). A radio that cannot establish the link on them
         * (API &lt; 26, or the link is already up) answers {@link #nativeCompleteFailure} kind
         * {@code unsupported}, dispatched=false; it never connects without them.
         */
        void connect(long requestId, String peerId, boolean autoConnect, String[] preferredPhy, String expectedGeneration);

        /** Answer: {@link #nativeCompleteUnit} once the OS confirms. */
        void disconnect(long requestId, String peerId);

        /** Answer: {@link #nativeCompleteDiscovered}. */
        void discover(long requestId, String peerId);

        /** Answer: {@link #nativeCompleteRead}. */
        void read(long requestId, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence);

        /** Answer: {@link #nativeCompleteUnit}. */
        void write(long requestId, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, byte[] value, boolean withResponse);

        /** Answer: {@link #nativeCompleteBytes}. */
        void readDescriptor(long requestId, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, String descriptorUuid, long descriptorOccurrence);

        /** Answer: {@link #nativeCompleteUnit}. */
        void writeDescriptor(long requestId, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, String descriptorUuid, long descriptorOccurrence, byte[] value);

        /**
         * Write the CCCD. {@code requested} (notification|indication|null) is a
         * hard mode: write it or fail {@code unsupported} before any effect.
         * Without it, {@code preferred} (or notification) is used when the
         * property allows, else the other mode. Stamp every value of this
         * instance with {@code epoch}. Answer: {@link #nativeCompleteNotifyEnabled}
         * with the mode actually written.
         */
        void enableNotifications(long requestId, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, long epoch, String requested, String preferred);
        void orderedEnable(long requestId, String operation, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, String connectionGeneration, String generationKind, String generationValue, long group, long member, long sourceMember, long memberEpoch, long activationEpoch, String requested, String preferred);

        /** Answer: {@link #nativeCompleteUnit}. */
        void disableNotifications(long requestId, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, long epoch);

        /** Answer: {@link #nativeCompleteMtu} (0 = not measured). */
        void readMtu(long requestId, String peerId);

        /**
         * The largest single write the OS accepts on the link, per mode.
         * Answer: {@link #nativeCompleteWriteLimits}.
         */
        void readWriteLimits(long requestId, String peerId);

        /**
         * Apple write-without-response readiness. Android has no equivalent
         * signal. Answer: {@link #nativeCompleteFailure} kind {@code unsupported},
         * dispatched=false. Do not invent a ready=true from a GATT commit.
         */
        void readWriteReadiness(long requestId, String peerId);

        /** Answer: {@link #nativeCompleteMtu} with the negotiated MTU. */
        void requestMtu(long requestId, String peerId, int mtu);

        /** Answer: {@link #nativeCompleteRssi}. */
        void readRssi(long requestId, String peerId);

        /** {@code priority}: low-power|balanced|high-throughput. Answer: {@link #nativeCompleteAccepted}. */
        void requestConnectionPriority(long requestId, String peerId, String priority);

        /** Public SDK 36.1 int-returning API availability, independently of app entitlements. */
        boolean subrateAvailable();
        /** Answer with the actual BluetoothStatusCodes value through nativeCompleteSubrateStatus. */
        void requestSubrate(long requestId, String peerId, String mode);

        /** Answer: {@link #nativeCompletePhy}. */
        void readPhy(long requestId, String peerId);

        /** {@code tx}/{@code rx}: le-1m|le-2m|le-coded|null (at least one set). Answer: {@link #nativeCompletePhyRequest}. */
        void requestPhy(long requestId, String peerId, String tx, String rx);

        /** Answer: {@link #nativeCompleteSecurity}. */
        void securityState(long requestId, String peerId);

        /** {@code transport}: auto|le. Answer: {@link #nativeCompleteSecurity} with the resulting state. */
        void createBond(long requestId, String peerId, String transport);

        /** Answer: {@link #nativeCompleteUnit}. */
        void cancelBond(long requestId, String peerId);

        /** Answer: {@link #nativeCompleteBondedPeers}. */
        void bondedPeers(long requestId);
        /** Read system GATT connections; services must be empty on Android. */
        void connectedPeers(long requestId, String[] services);
        void resolvePeer(long requestId, String peerId);

        /** {@code kind}: connected-device. Answer: {@link #nativeCompleteLease}. */
        void acquireBackground(long requestId, String kind, String reason);

        /** Answer: {@link #nativeCompleteUnit}. */
        void releaseBackground(long requestId, String leaseId);

        /** Answer: {@link #nativeCompleteUnit}. */
        void updateBackgroundNotification(long requestId, String leaseId, String title, String body);

        /** Answer: {@link #nativeCompleteCompanion}. */
        void associateCompanion(long requestId, String name, String serviceUuid, String filtersJson);

        /** This app's associations. Answer: {@link #nativeCompleteCompanionList}. */
        void listCompanion(long requestId);

        /** Removes one association by id. Answer: {@link #nativeCompleteUnit}. */
        void disassociateCompanion(long requestId, long associationId);

        /**
         * Arms Companion Device Manager device presence for one associated
         * peer (API 31+). Answer: {@link #nativeCompleteUnit}. Appearances
         * arrive through {@link #nativeIngestRestored}.
         */
        void observePresence(long requestId, String peerId);

        /** Disarms device presence for one peer (idle when none is armed). Answer: {@link #nativeCompleteUnit}. */
        void unobservePresence(long requestId, String peerId);

        /** Disable every live notification. Answer: {@link #nativeCompleteClosed}. */
        void close(long requestId);

        /** Best-effort cancel of an in-flight request (its later answer is counted late). */
        void cancel(long requestId);
    }

    // -- identity -------------------------------------------------------

    /** The binary's {@code ubm-native-build-identity/1} record (JSON). */
    public static native String nativeBuildIdentityJson();

    /** The core's contract revision, answered by Rust. */
    public static native String nativeContractRevision();

    /** The mobile wire revision ({@code ubm-mobile-wire/3}). */
    public static native String nativeWireRevision();

    // -- host (one per process) ------------------------------------------

    /** {@code platform}: android|apple. Throws if a host is already installed. */
    public static native void nativeInstallHost(RadioHost radio, WakeListener wake, String platform, String owner, String adapterLabel);

    public static native boolean nativeHostInstalled();

    /** Shut the host down; returns the cleanup record JSON. */
    public static native String nativeShutdownHost();

    // -- sessions ---------------------------------------------------------

    /**
     * Returns {@code {"sessionId":n,"contractRevision":..,"wireRevision":..,"buildIdentity":{..}}}.
     * {@code backgroundScope} (non-empty) names the React Native module
     * instance: foreground-service leases its sessions acquire outlive the
     * session and end with {@link #nativeReleaseBackgroundScope}.
     */
    public static native String nativeOpenSession(String owner, String expectedWireRevision, String backgroundScope);

    /** Module invalidation: release every background lease of the scope; returns the cleanup record JSON. */
    public static native String nativeReleaseBackgroundScope(String backgroundScope);

    public static native void nativeInvoke(long sessionId, String op, String argsJson, InvokeCallback callback);

    public static native String nativeDrain(long sessionId, int maxItems, int maxBytes);
    public static native String nativeNegotiateScanEndVersion(long sessionId, String offeredJson);
    public static native String nativeNegotiateScanEndVersionForBinding(long binding, long sessionId, String offeredJson);
    public static native String nativeNegotiateScanEndVersionQualified(long sessionId, String offeredJson);
    public static native String nativeNegotiateScanEndVersionQualifiedForBinding(long binding, long sessionId, String offeredJson);
    /** Trusted process bootstrap only; never exposed through the TurboModule. */
    public static native String nativeContinuationConfigureRecordingDirectory(String path);
    public static native String nativeContinuationConfigureRecordingDirectoryForBinding(long binding, String path);
    /** Trusted native bootstrap only; fixes the process-wide durable terminal quota before host install. */
    public static native void nativeConfigureTerminalEvidenceMaximumBytes(long maximumBytes);
    public static native String nativeContinuationRecordingControl(String operation, String id, String token, int maxItems, int maxBytes);
    /** Host-independent journal access; offline stop is refused while any native host is retained. */
    public static native String nativeContinuationRecordingControlOffline(String operation, String id, String token, int maxItems, int maxBytes);
    public static native int nativeOrderedRecordingVersionOffline();
    public static native int nativeOrderedRecordingVersion();
    public static native void nativeOrderedRecordingReceiptControl(long sessionId, String operation, String id, InvokeCallback callback);
    public static native void nativeContinuationOrderedOffline(String operation, String id, InvokeCallback callback);
    public static native void nativeContinuationStartOrdered(long sessionId, String handle, String recordingId, String maxBytes, String maxRecords, String declarationJson, long budgetMs, InvokeCallback callback);
    public static native void nativeContinuationStopOrdered(long sessionId, String id, InvokeCallback callback);
    public static native void nativeContinuationOrderedContext(long sessionId, String id, InvokeCallback callback);
    // Binding routes never select another host if this binding is retired.
    public static native void nativeContinuationDescribeBacklogForBinding(long binding, InvokeCallback callback);
    public static native String nativeContinuationDeclarationReplacementFailureForBinding(long binding, String declaration);
    public static native void nativeContinuationExecuteForBinding(long binding, String peerId, String declaration, InvokeCallback callback);
    public static native String nativeContinuationReserveDeclarationForBinding(long binding, String declaration);
    public static native String nativeContinuationCommitDeclarationForBinding(long binding, String token);
    public static native String nativeContinuationCancelDeclarationForBinding(long binding, String token);
    public static native String nativeContinuationSeedDeclarationForBinding(long binding, String declaration);
    public static native void nativeContinuationPrepareClaimForBinding(long binding, int maxItems, int maxBytes, InvokeCallback callback);
    public static native void nativeContinuationAcknowledgeClaimForBinding(long binding, String token, InvokeCallback callback);
    public static native void nativeContinuationPrepareClaimVersionedForBinding(long binding, int version, int maxItems, int maxBytes, InvokeCallback callback);
    public static native void nativeContinuationAcknowledgeClaimV1ForBinding(long binding, String token, InvokeCallback callback);
    public static native void nativeContinuationAcknowledgeClaimV2ForBinding(long binding, String token, InvokeCallback callback);
    public static native void nativeContinuationReadPeerFailureRecoveryForBinding(long binding, String recoveryRecord, String offset, InvokeCallback callback);
    public static native void nativeContinuationAcknowledgePeerFailureRecoveryForBinding(long binding, String recoveryRecord, InvokeCallback callback);
    public static native void nativeContinuationRetireClaimReceiptForBinding(long binding, String token, InvokeCallback callback);
    public static native void nativeContinuationLegacyEvidencePageForBinding(long binding, String token, long afterOrdinal, int maxItems, int maxBytes, InvokeCallback callback);
    public static native void nativeContinuationLegacyEvidenceRetireForBinding(long binding, String token, long ordinal, InvokeCallback callback);
    public static native void nativeContinuationPrepareClaimVersioned(int version, int maxItems, int maxBytes, InvokeCallback callback);
    public static native void nativeContinuationAcknowledgeClaimV1(String token, InvokeCallback callback);
    public static native void nativeContinuationAcknowledgeClaimV2(String token, InvokeCallback callback);
    public static native void nativeContinuationReadPeerFailureRecovery(String recoveryRecord, String offset, InvokeCallback callback);
    public static native void nativeContinuationAcknowledgePeerFailureRecovery(String recoveryRecord, InvokeCallback callback);
    public static native void nativeContinuationRetireClaimReceipt(String token, InvokeCallback callback);
    public static native void nativeContinuationLegacyEvidencePage(String token, long afterOrdinal, int maxItems, int maxBytes, InvokeCallback callback);
    public static native void nativeContinuationLegacyEvidenceRetire(String token, long ordinal, InvokeCallback callback);
    public static native String nativeContinuationRecordingControlForBinding(long binding, String operation, String id, String token, int maxItems, int maxBytes);
    public static native void nativeOrderedRecordingReceiptControlForBinding(long binding, long sessionId, String operation, String id, InvokeCallback callback);
    public static native void nativeContinuationStartOrderedForBinding(long binding, long sessionId, String handle, String id, String maxBytes, String maxRecords, String declaration, long budgetMs, InvokeCallback callback);
    public static native void nativeContinuationStopOrderedForBinding(long binding, long sessionId, String id, InvokeCallback callback);
    public static native void nativeContinuationOrderedContextForBinding(long binding, long sessionId, String id, InvokeCallback callback);
    public static native void nativeContinuationOrderedOfflineForBinding(long binding, String operation, String id, InvokeCallback callback);
    public static native int nativeOrderedRecordingVersionForBinding(long binding);

    // -- trusted scan-failure recovery ---------------------------------
    // These are intentionally not TurboModule methods.  The Android process
    // owner calls them only after app-private recording storage is configured
    // and a native policy has accepted the opaque recording id. Returned ids
    // are binding-local handles, never recovery credentials or file paths.
    public static native String nativeScanFailureRecoveryAttachForBinding(long binding, String recordingId);
    public static native String nativeScanFailureRecoveryOwnersForBinding(long binding, String leaseId);
    /** Opaque-owner member metadata (`{member:{recipient,delivery}}`); neither
     * field is a recovery capability and both use lower-case hexadecimal. */
    public static native String nativeScanFailureRecoveryMemberForBinding(long binding, String ownerId, int memberIndex);
    public static native String nativeScanFailureRecoveryHeadForBinding(long binding, String ownerId, int memberIndex);
    /** One atomic bounded recovery page.  Metadata and bytes share one native
     * read, so ACK/release cannot create a cross-call page mismatch. */
    public static final class ScanFailureRecoveryPage {
        public final String offset;
        public final String nextOffset;
        public final byte[] bytes;
        public final boolean done;
        public ScanFailureRecoveryPage(String offset, String nextOffset, byte[] bytes, boolean done) {
            this.offset = offset;
            this.nextOffset = nextOffset;
            this.bytes = bytes.clone();
            this.done = done;
        }
    }
    public static native ScanFailureRecoveryPage nativeScanFailureRecoveryReadPageForBinding(long binding, String headId, String offset);
    /** Metadata for a bounded scan-recovery page. Use the companion byte[] method for payload bytes. */
    public static native String nativeScanFailureRecoveryReadForBinding(long binding, String headId, String offset);
    /**
     * Fresh bounded binary payload for exactly the page named by {@code headId}
     * and {@code offset}. This is intentionally not JSON: the React Native
     * boundary converts it to Uint8Array without numeric-array expansion.
     */
    public static native byte[] nativeScanFailureRecoveryReadBytesForBinding(long binding, String headId, String offset);
    /** acknowledge | retire | release | finalize. Finalize returns an opaque cleanup receipt id. */
    public static native String nativeScanFailureRecoveryHeadControlForBinding(long binding, String headId, String operation);
    /** Raw source metadata for an opaque owner. This never exposes a recovery capability. */
    public static native String nativeScanFailureRecoveryRawInfoForBinding(long binding, String ownerId);
    /** One raw member selected through an opaque owner. */
    public static native String nativeScanFailureRecoveryRawMemberForBinding(long binding, String ownerId, int memberIndex);
    /** Native-host selection fence: revokes every opaque recovery alias for this binding. */
    public static native void nativeScanFailureRecoveryRevokeForBinding(long binding);
    /**
     * One bounded raw evidence token. Scalar fields are canonical decimal
     * text where Java cannot retain an exact u64; binary token windows remain
     * byte[] and are never JSON numeric arrays.
     */
    public static final class ScanFailureRecoveryRawToken {
        public final String kind;
        public final String value;
        public final String total;
        public final String count;
        public final byte[] bytes;
        public ScanFailureRecoveryRawToken(String kind, String value, String total, String count, byte[] bytes) {
            this.kind = kind;
            this.value = value;
            this.total = total;
            this.count = count;
            this.bytes = bytes == null ? null : bytes.clone();
        }
    }
    public static native ScanFailureRecoveryRawToken nativeScanFailureRecoveryRawTokenForBinding(
        long binding, String ownerId, int tokenIndex, String offset, String maxBytes);
    public static native String nativeScanFailureRecoveryRawControlForBinding(long binding, String ownerId, String operation);
    public static native String nativeScanFailureRecoveryPendingCleanupForBinding(long binding, String leaseId);
    public static native void nativeScanFailureRecoveryRetireCleanupForBinding(long binding, String cleanupReceiptId);
    /** Selected process-native terminal evidence only; all returned ids are binding-local aliases. */
    public static native String nativeTerminalEvidenceRecoveryOpenSelectedForBinding(long binding);
    public static native String nativeTerminalEvidenceRecoveryListForBinding(long binding, String leaseId, int maxItems, long maxBytes);
    public static native String nativeTerminalEvidenceRecoveryListWithStatusForBinding(long binding, String leaseId, int maxItems, long maxBytes);
    public static native String nativeTerminalEvidenceRecoveryReadForBinding(long binding, String handle, String offset, int maxBytes);
    public static native String nativeTerminalEvidenceRecoveryAcknowledgeForBinding(long binding, String handle);
    /** Executes a declared standing order in the process-owned Rust executor. */
    public static native void nativeContinuationExecute(String peerId, String declarationJson, InvokeCallback callback);

    /** Seals and prepares a replay-safe continuation handoff without acknowledging its bytes. */
    public static native void nativeContinuationPrepareClaim(int maxItems, int maxBytes, InvokeCallback callback);

    /** Acknowledges decoded bytes; failed native cleanup remains owned for retry. */
    public static native void nativeContinuationAcknowledgeClaim(String claimToken, InvokeCallback callback);

    /** Read-only queued counters, or JSON null when no continuation is owned. */
    public static native void nativeContinuationDescribeBacklog(InvokeCallback callback);

    /** Returns the active-declaration conflict reason, or null when replacement is admissible. */
  public static native String nativeContinuationDeclarationReplacementFailure(String declarationJson);
  public static native String nativeContinuationReserveDeclaration(String declarationJson);
  public static native String nativeContinuationCommitDeclaration(String reservationToken);
  public static native String nativeContinuationCancelDeclaration(String reservationToken);
  public static native String nativeContinuationSeedDeclaration(String declarationJson);

    // -- completions (answer each RadioHost request exactly once) ---------

    public static native int nativeCompleteOwnerRetirement(long requestId, OwnerRetirementResult result);
    public static native int nativeCompleteOrdered(long requestId, OrderedResult result);
    public static native int nativeCompleteOrderedEnableReleased(long requestId, String kind, int gattStatus, String detail, boolean dispatched, String nativeName);
    public static native int nativeCompleteDiscoveredScoped(long requestId, int[] levels, String[] uuids, long[] occurrences, int[] properties, String snapshotOwner, long snapshotAttachment, long snapshotRevision);

    public static native int nativeCompleteUnit(long requestId);
    /**
     * Optional scan identity extension. Missing probe symbol means legacy v1;
     * version 2 promises all three methods below. Android assigns one opaque
     * identity before startScan, returns it with the start completion, and
     * includes it in every callback, including callbacks during startScan.
     * The Rust receiver must retain early callbacks until the matching start
     * is established and compare identities again when processing ingress.
     * These declarations do not imply that the linked Rust binary supports v2.
     */
    public static native int nativeScanIdentityWireVersion();
    public static native int nativeCompleteScanStartedV2(long requestId, String scanIdentity);
    public static native int nativeIngestScanFailedV2(String detail, String scanIdentity);
    public static native int nativeIngestScanFailedV3(String detail, String scanIdentity, int errorCode);
    public static native int nativeIngestAdvertisementV2(String peerId, String address, String localName, int rssi, int txPower, String[] serviceUuids, int[] companyIds, byte[][] manufacturerPayloads, String[] serviceDataUuids, byte[][] serviceDataPayloads, int connectable, String[] solicitedServiceUuids, String[] overflowServiceUuids, int appearance, byte[] rawRecord, String cachedName, long captureTimestampMs, String scanIdentity);
    public static native int nativeIngestAdvertisementV2ForBinding(long binding, String peerId, String address, String localName, int rssi, int txPower, String[] serviceUuids, int[] companyIds, byte[][] manufacturerPayloads, String[] serviceDataUuids, byte[][] serviceDataPayloads, int connectable, String[] solicitedServiceUuids, String[] overflowServiceUuids, int appearance, byte[] rawRecord, String cachedName, long captureTimestampMs, String scanIdentity);
    public static native int nativeCompleteScanStartedV2ForBinding(long binding, long requestId, String scanIdentity);
    public static native int nativeIngestScanFailedV2ForBinding(long binding, String detail, String scanIdentity);
    public static native int nativeIngestScanFailedV3ForBinding(long binding, String detail, String scanIdentity, int errorCode);
    /** Binding-scoped form of nativeScanFailureIngestEvidence. */
    public static native String nativeScanFailureIngestEvidenceForBinding(long binding, String detail, String scanIdentity, int errorCode);


    /** A descriptor read's value. */
    public static native int nativeCompleteBytes(long requestId, byte[] value);

    /**
     * A characteristic read's value and what the platform says it is:
     * {@code read-response} | {@code read-or-notification}.
     */
    public static native int nativeCompleteRead(long requestId, byte[] value, String provenance);

    /** availability: available|unavailable|unsupported|unknown; authorization: granted|denied|restricted|not-determined|unavailable|unknown; power: on|off|resetting|unsupported|unknown. */
    public static native int nativeCompleteAdapter(long requestId, String availability, String authorization, String power, String safeReason);

    /**
     * Discovery tree in pre-order: {@code levels[i]} is 0 (service),
     * 1 (characteristic of the last service), 2 (descriptor of the last
     * characteristic) or 3 (included-service reference of the last service).
     * Service properties encode unknown/primary/secondary as 0/1/2 in
     * bits 0..1 and observed inclusion-list presence in bit 2. Inclusion
     * rows carry the target's UUID and occurrence, with zero properties.
     * Descriptor properties are zero; characteristic properties carry
     * property bits (read 0x02, write-without-response 0x04, write 0x08,
     * notify 0x10, indicate 0x20 — Android {@code BluetoothGattCharacteristic}
     * values).
     */
    public static native int nativeCompleteDiscovered(long requestId, int[] levels, String[] uuids, long[] occurrences, int[] properties);

    /** delivery: notification|indication|unknown (the mode actually written). */
    public static native int nativeCompleteNotifyEnabled(long requestId, String delivery);

    /** mtu: 23..517, or 0 when not measured (readMtu only). */
    public static native int nativeCompleteMtu(long requestId, int mtu);

    /** Both limits are positive byte counts: with-response, without-response. */
    public static native int nativeCompleteWriteLimits(long requestId, int withResponse, int withoutResponse);

    public static native int nativeCompleteRssi(long requestId, int rssi);

    public static native int nativeCompleteAccepted(long requestId, boolean accepted);
    public static native int nativeCompleteSubrateStatus(long requestId, int status);

    public static native int nativeCompletePhy(long requestId, String tx, String rx);

    /** tx/rx are null exactly when not accepted. */
    public static native int nativeCompletePhyRequest(long requestId, boolean accepted, String tx, String rx);

    /** bond: bonded|bonding|not-bonded|unknown|unsupported; encryption: encrypted|not-encrypted|unknown|unsupported; authentication: authenticated|unauthenticated|unknown|unsupported; secureConnections: yes|no|unknown|unsupported; pairingPossible: -1 unknown, 0, 1. */
    public static native int nativeCompleteSecurity(long requestId, String bond, String encryption, String authentication, String secureConnections, int pairingPossible);

    /** names[i] may be null. */
    public static native int nativeCompleteBondedPeers(long requestId, String[] peerIds, String[] names);
    public static native int nativeCompleteResolvedPeer(long requestId, String peerId, String name);
    public static native int nativeCompleteConnectedPeers(long requestId, String[] peerIds, String[] names);

    public static native int nativeCompleteLease(long requestId, String leaseId);

    /** alreadyAssociated: the platform already held this association; nothing new was created. */
    public static native int nativeCompleteCompanion(long requestId, long associationId, String peerId, String displayName, boolean alreadyAssociated);

    /** One entry per association: parallel arrays, null entries where the platform reports none. */
    public static native int nativeCompleteCompanionList(long requestId, long[] associationIds, String[] peerIds, String[] displayNames);

    /** One entry per characteristic scope whose release failed (empty arrays = all released). */
    public static native int nativeCompleteClosed(long requestId, String[] peerIds, String[] serviceUuids, long[] serviceOccurrences, String[] characteristicUuids, long[] characteristicOccurrences, String[] details);

    /**
     * kind: not-connected|peer-unknown|path-stale|busy|permission-denied|
     * permission-restricted|permission-not-determined|adapter-off|
     * adapter-unavailable|adapter-resetting|gatt-status|cancelled|unsupported|platform;
     * gattStatus: ABSENT_INT when none; dispatched: false when the platform
     * refused before sending anything to the peer (write-without-response
     * queue full, oversize value), so a failed write reports commit
     * not-dispatched instead of uncertain.
     */
    /** {@code nativeCode}: the platform's own named failure code (Android foreground service, companion chooser), or null. */
    public static native int nativeCompleteFailure(long requestId, String kind, int gattStatus, String detail, boolean dispatched, String nativeCode);

    // -- ingress (unsolicited platform facts) ---------------------------

    /**
     * rssi/txPower: ABSENT_INT when absent; manufacturer company ids pair with
     * payloads by index; serviceData uuids pair with payloads by index;
     * connectable: -1 unknown, 0, 1; solicited/overflow UUID arrays: null when
     * the platform does not report them. appearance: GAP Appearance 0..65535, or
     * ABSENT_INT when not carried; rawRecord: the raw advertising bytes, or null
     * when not reported.
     */
    public static native int nativeIngestAdvertisement(String peerId, String address, String localName, int rssi, int txPower, String[] serviceUuids, int[] companyIds, byte[][] manufacturerPayloads, String[] serviceDataUuids, byte[][] serviceDataPayloads, int connectable, String[] solicitedServiceUuids, String[] overflowServiceUuids, int appearance, byte[] rawRecord, String cachedName, long captureTimestampMs);

    /** status: platform GATT status or ABSENT_INT. */
    public static native int nativeIngestConnection(String peerId, boolean connected, int status);

    public static native int nativeIngestConnectionForGeneration(String peerId, String expectedGeneration, boolean connected, int status);

    public static native int nativeIngestServicesChanged(String peerId);

    public static native int nativeIngestNotification(String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, long epoch, byte[] value);

    /** Android ordered source wake; both identities retain the exact unsigned bit pattern. */
    public static native int nativeIngestOrderedWake(long sourceGeneration, long groupId);

    public static native int nativeIngestOrderedWakeFailure(long sourceGeneration, long groupId);
    public static native int nativeIngestNotificationSourceFailed(String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, long epoch, String kind, int gattStatus, String detail, boolean dispatched, String nativeCode);

    public static native int nativeIngestAdapterState(String availability, String authorization, String power, String safeReason);

    public static native int nativeIngestScanFailed(String detail);

    /**
     * Returns accepted or the exact refused error as a typed token stream.
     * The caller retains the input and may retry it until it is accepted.
     */
    public static native String nativeScanFailureIngestEvidence(String detail, String scanIdentity, int errorCode);

    public static native int nativeIngestSecurity(String peerId, String bond, String encryption, String authentication, String secureConnections, int pairingPossible);
    public static native int nativeIngestSecurityFailure(String peerId, String kind, int encryptionStatus, String detail);

    /** names[i] may be null. */
    public static native int nativeIngestRestored(String[] peerIds, String[] names, boolean[] connected);

    /** class: advertisement|notification|control. */
    public static native int nativeIngestDropped(String ingressClass, String detail);
    public static native int nativeIngestDroppedCount(String ingressClass, String detail, long count);

    // Private binding-scoped host protocol; callers must negotiate route version 1.
    /** Highest binding route version implemented by this library. Older libraries lack this symbol. */
    public static native int nativeBindingRouteVersion();
    public static native long nativeInstallHostForBinding(int version, RadioHost radio, WakeListener wake, String platform, String owner, String label);
    public static native String nativeShutdownHostForBinding(long binding);
    public static native long nativeFindRetainedBinding(RadioHost radio);
    public static native String nativeOpenSessionForBinding(long binding, String owner, String expectedWireRevision, String backgroundScope);
    public static native void nativeInvokeForBinding(long binding, long session, String op, String args, InvokeCallback callback);
    public static native String nativeDrainForBinding(long binding, long session, int maxItems, int maxBytes);
    public static native String nativeReleaseBackgroundScopeForBinding(long binding, String scope);
    public static native int nativeCompleteOwnerRetirementForBinding(long binding, long requestId, OwnerRetirementResult result);
    public static native int nativeCompleteOrderedForBinding(long binding, long requestId, OrderedResult result);
    public static native int nativeCompleteOrderedEnableReleasedForBinding(long binding, long requestId, String kind, int gattStatus, String detail, boolean dispatched, String nativeName);
    public static native int nativeCompleteDiscoveredScopedForBinding(long binding, long requestId, int[] levels, String[] uuids, long[] occurrences, int[] properties, String snapshotOwner, long snapshotAttachment, long snapshotRevision);
    public static native int nativeCompleteUnitForBinding(long binding, long requestId);
    public static native int nativeCompleteBytesForBinding(long binding, long requestId, byte[] value);
    public static native int nativeCompleteReadForBinding(long binding, long requestId, byte[] value, String provenance);
    public static native int nativeCompleteAdapterForBinding(long binding, long requestId, String availability, String authorization, String power, String safeReason);
    public static native int nativeCompleteDiscoveredForBinding(long binding, long requestId, int[] levels, String[] uuids, long[] occurrences, int[] properties);
    public static native int nativeCompleteNotifyEnabledForBinding(long binding, long requestId, String delivery);
    public static native int nativeCompleteMtuForBinding(long binding, long requestId, int mtu);
    public static native int nativeCompleteWriteLimitsForBinding(long binding, long requestId, int withResponse, int withoutResponse);
    public static native int nativeCompleteRssiForBinding(long binding, long requestId, int rssi);
    public static native int nativeCompleteAcceptedForBinding(long binding, long requestId, boolean accepted);
    public static native int nativeCompleteSubrateStatusForBinding(long binding, long requestId, int status);
    public static native int nativeCompletePhyForBinding(long binding, long requestId, String tx, String rx);
    public static native int nativeCompletePhyRequestForBinding(long binding, long requestId, boolean accepted, String tx, String rx);
    public static native int nativeCompleteSecurityForBinding(long binding, long requestId, String bond, String encryption, String authentication, String secureConnections, int pairingPossible);
    public static native int nativeCompleteBondedPeersForBinding(long binding, long requestId, String[] peerIds, String[] names);
    public static native int nativeCompleteResolvedPeerForBinding(long binding, long requestId, String peerId, String name);
    public static native int nativeCompleteConnectedPeersForBinding(long binding, long requestId, String[] peerIds, String[] names);
    public static native int nativeCompleteLeaseForBinding(long binding, long requestId, String leaseId);
    public static native int nativeCompleteCompanionForBinding(long binding, long requestId, long associationId, String peerId, String displayName, boolean alreadyAssociated);
    public static native int nativeCompleteCompanionListForBinding(long binding, long requestId, long[] associationIds, String[] peerIds, String[] displayNames);
    public static native int nativeCompleteClosedForBinding(long binding, long requestId, String[] peerIds, String[] serviceUuids, long[] serviceOccurrences, String[] characteristicUuids, long[] characteristicOccurrences, String[] details);
    public static native int nativeCompleteFailureForBinding(long binding, long requestId, String kind, int gattStatus, String detail, boolean dispatched, String nativeCode);
    public static native int nativeIngestAdvertisementForBinding(long binding, String peerId, String address, String localName, int rssi, int txPower, String[] serviceUuids, int[] companyIds, byte[][] manufacturerPayloads, String[] serviceDataUuids, byte[][] serviceDataPayloads, int connectable, String[] solicitedServiceUuids, String[] overflowServiceUuids, int appearance, byte[] rawRecord, String cachedName, long captureTimestampMs);
    public static native int nativeIngestConnectionForBinding(long binding, String peerId, boolean connected, int status);
    public static native int nativeIngestConnectionForGenerationForBinding(long binding, String peerId, String expectedGeneration, boolean connected, int status);
    public static native int nativeIngestServicesChangedForBinding(long binding, String peerId);
    public static native int nativeIngestNotificationForBinding(long binding, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, long epoch, byte[] value);
    public static native int nativeIngestOrderedWakeForBinding(long binding, long sourceGeneration, long groupId);
    public static native int nativeIngestOrderedWakeFailureForBinding(long binding, long sourceGeneration, long groupId);
    public static native int nativeIngestNotificationSourceFailedForBinding(long binding, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, long epoch, String kind, int gattStatus, String detail, boolean dispatched, String nativeCode);
    public static native int nativeIngestAdapterStateForBinding(long binding, String availability, String authorization, String power, String safeReason);
    public static native int nativeIngestScanFailedForBinding(long binding, String detail);
    public static native int nativeIngestSecurityForBinding(long binding, String peerId, String bond, String encryption, String authentication, String secureConnections, int pairingPossible);
    public static native int nativeIngestSecurityFailureForBinding(long binding, String peerId, String kind, int encryptionStatus, String detail);
    public static native int nativeIngestRestoredForBinding(long binding, String[] peerIds, String[] names, boolean[] connected);
    public static native int nativeIngestDroppedForBinding(long binding, String ingressClass, String detail);
    public static native int nativeIngestDroppedCountForBinding(long binding, String ingressClass, String detail, long count);
}
