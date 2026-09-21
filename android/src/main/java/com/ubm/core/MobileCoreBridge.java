// android/src/main/java/com/ubm/core/MobileCoreBridge.java
//
// JNI facade of the process-owned mobile Rust owner (`crates/ubm-mobile`,
// natives in `bindings/jni/src/mobile.rs`, library `libubm5_jni_echo.so`).
//
// Ownership: native signatures are single-owned by the Rust cdylib. The
// probe-side copy at `bindings/jni/java/com/ubm/core/MobileCoreBridge.java`
// must stay byte-identical (the JNI crate's `java_copies_are_identical`
// test fails when they drift); the shipped AAR compiles this copy.
//
// Shape (wire revision `ubm-mobile-wire/1`, docs/MOBILE_RUST_WIRE.md):
//   * one host per process: `nativeInstallHost` binds the platform radio
//     (`RadioHost`, implemented over OwnedAndroidGattRadio) and the wake
//     listener; the host outlives every RN manager (restoration, FGS);
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
    public interface RadioHost {
        /** Answer: {@link #nativeCompleteAdapter}. */
        void adapterState(long requestId);

        /** Answer: {@link #nativeCompleteUnit}. {@code scanMode}: low-power|balanced|low-latency|opportunistic; {@code callbackType}: all-matches|first-match. */
        void startScan(long requestId, String[] serviceUuids, String[] deviceAddresses, String scanMode, String callbackType, int legacy);

        /** Answer: {@link #nativeCompleteUnit}. */
        void stopScan(long requestId);

        /**
         * Answer: {@link #nativeCompleteUnit} once connected (a restored, still-connected peer answers at once).
         * {@code preferredPhy}: le-1m|le-2m|le-coded, the PHYs to establish the link on (empty = no
         * preference; never with {@code autoConnect}). A radio that cannot establish the link on them
         * (API &lt; 26, or the link is already up) answers {@link #nativeCompleteFailure} kind
         * {@code unsupported}, dispatched=false; it never connects without them.
         */
        void connect(long requestId, String peerId, boolean autoConnect, String[] preferredPhy);

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

        /** Answer: {@link #nativeCompleteUnit}. */
        void disableNotifications(long requestId, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence);

        /** Answer: {@link #nativeCompleteMtu} (0 = not measured). */
        void readMtu(long requestId, String peerId);

        /**
         * The largest single write the OS accepts on the link, per mode.
         * Answer: {@link #nativeCompleteWriteLimits}.
         */
        void readWriteLimits(long requestId, String peerId);

        /** Answer: {@link #nativeCompleteMtu} with the negotiated MTU. */
        void requestMtu(long requestId, String peerId, int mtu);

        /** Answer: {@link #nativeCompleteRssi}. */
        void readRssi(long requestId, String peerId);

        /** {@code priority}: low-power|balanced|high-throughput. Answer: {@link #nativeCompleteAccepted}. */
        void requestConnectionPriority(long requestId, String peerId, String priority);

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

        /** {@code kind}: connected-device. Answer: {@link #nativeCompleteLease}. */
        void acquireBackground(long requestId, String kind, String reason);

        /** Answer: {@link #nativeCompleteUnit}. */
        void releaseBackground(long requestId, String leaseId);

        /** Answer: {@link #nativeCompleteUnit}. */
        void updateBackgroundNotification(long requestId, String leaseId, String title, String body);

        /** Answer: {@link #nativeCompleteCompanion}. */
        void associateCompanion(long requestId, String name, String serviceUuid);

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

    /** The mobile wire revision ({@code ubm-mobile-wire/1}). */
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

    // -- completions (answer each RadioHost request exactly once) ---------

    public static native int nativeCompleteUnit(long requestId);

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
     * 1 (characteristic of the last service) or 2 (descriptor of the last
     * characteristic); {@code properties[i]} is the characteristic's
     * property bits (read 0x02, write-without-response 0x04, write 0x08,
     * notify 0x10, indicate 0x20 — Android {@code BluetoothGattCharacteristic}
     * values), 0 for other levels.
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

    public static native int nativeCompletePhy(long requestId, String tx, String rx);

    /** tx/rx are null exactly when not accepted. */
    public static native int nativeCompletePhyRequest(long requestId, boolean accepted, String tx, String rx);

    /** bond: bonded|bonding|not-bonded|unknown|unsupported; encryption: encrypted|not-encrypted|unknown|unsupported; authentication: authenticated|unauthenticated|unknown|unsupported; secureConnections: yes|no|unknown|unsupported; pairingPossible: -1 unknown, 0, 1. */
    public static native int nativeCompleteSecurity(long requestId, String bond, String encryption, String authentication, String secureConnections, int pairingPossible);

    /** names[i] may be null. */
    public static native int nativeCompleteBondedPeers(long requestId, String[] peerIds, String[] names);

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
    public static native int nativeIngestAdvertisement(String peerId, String address, String localName, int rssi, int txPower, String[] serviceUuids, int[] companyIds, byte[][] manufacturerPayloads, String[] serviceDataUuids, byte[][] serviceDataPayloads, int connectable, String[] solicitedServiceUuids, String[] overflowServiceUuids, int appearance, byte[] rawRecord);

    /** status: platform GATT status or ABSENT_INT. */
    public static native int nativeIngestConnection(String peerId, boolean connected, int status);

    public static native int nativeIngestServicesChanged(String peerId);

    public static native int nativeIngestNotification(String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, long epoch, byte[] value);

    public static native int nativeIngestAdapterState(String availability, String authorization, String power, String safeReason);

    public static native int nativeIngestScanFailed(String detail);

    public static native int nativeIngestSecurity(String peerId, String bond, String encryption, String authentication, String secureConnections, int pairingPossible);

    /** names[i] may be null. */
    public static native int nativeIngestRestored(String[] peerIds, String[] names, boolean[] connected);

    /** class: advertisement|notification|control. */
    public static native int nativeIngestDropped(String ingressClass, String detail);
}
