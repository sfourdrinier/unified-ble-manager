// bindings/jni/java/com/ubm/core/TestMobile.java
//
// JVM exchange through the real JNI mobile surface: a Java RadioHost
// answers every request through nativeComplete*, the host routes an
// ingested advertisement to a scanning session, the wake listener fires,
// and drain returns the record. Exits non-zero on any mismatch.

package com.ubm.core;

import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.TimeUnit;

public final class TestMobile {
    private TestMobile() {}

    static volatile String lastPreferredPhy = null;
    static volatile long lastNotificationEpoch = -1;
    static volatile boolean holdSetupWrites = false;
    static final BlockingQueue<Long> setupWrites = new ArrayBlockingQueue<>(4);
    static final java.util.concurrent.atomic.AtomicInteger backgroundReleases = new java.util.concurrent.atomic.AtomicInteger();

    static class Radio implements MobileCoreBridge.RadioHost {
        boolean scanIdentityV2 = false;
        int scans = 0;
        String firstScanIdentity;
        String secondScanIdentity;
        private final String admissionOwner = java.util.UUID.randomUUID().toString();
        public String orderedAdmissionOwner(int version) { return version == 3 ? admissionOwner : ""; }
        public void orderedOpen(long id, long group, long[] members, String[] peers, String[] services, long[] serviceOccurrences, String[] characteristics, long[] characteristicOccurrences, int itemCapacity, int byteCapacity, String connectionGeneration, String databaseGeneration, String snapshotOwner, long snapshotAttachment, long snapshotRevision) {
            throw new AssertionError("unexpected ordered open");
        }
        public void orderedRecoverOpen(long id, long group, long openRequestId, String peerId) { throw new AssertionError("unexpected ordered recovery"); }
        public void orderedAcknowledgeOpen(long id, int version, long group, long openRequestId, String peerId) { throw new AssertionError("unexpected ordered acknowledgement"); }
        public void orderedEnable(long id, String operation, String peer, String service, long serviceOccurrence, String characteristic, long characteristicOccurrence, String connection, String kind, String generation, long group, long member, long sourceMember, long memberEpoch, long activationEpoch, String requested, String preferred) { throw new AssertionError("unexpected ordered enable"); }
        public void orderedGraphPermitReservation(long id, String operation, long authorityId, long request, String peerId, String connectionGeneration, String snapshotOwner, long snapshotAttachment, long snapshotRevision) { MobileCoreBridge.nativeCompleteFailure(id, "unsupported", MobileCoreBridge.ABSENT_INT, "synthetic permit reservation unavailable", false, null); }
        public void orderedGraphAuthority(long id, String operation, long authorityValue) { MobileCoreBridge.nativeCompleteFailure(id, "unsupported", MobileCoreBridge.ABSENT_INT, "synthetic graph authority unavailable", false, null); }
        public void orderedGraphPermit(long id, String operation, String peerId, String connectionGeneration, String snapshotOwner, long snapshotAttachment, long snapshotRevision, long authorityId, long permitId) {
            MobileCoreBridge.nativeCompleteFailure(id, "unsupported", MobileCoreBridge.ABSENT_INT, "synthetic graph pin unavailable", false, null);
        }
        public void orderedMarkerWrite(long id, String operation, String generationKind, String generationValue, long group, long markerId, String connectionGeneration, String snapshotOwner, long snapshotAttachment, long snapshotRevision, String peerId, String serviceUuid, long serviceOccurrence, String characteristicUuid, long characteristicOccurrence, byte[] value, boolean withResponse) {
            throw new AssertionError("unexpected ordered marker write");
        }
        public void orderedRequest(long id, String operation, String generationKind, String generationValue, long group, long member, long sourceMember, long epoch, long batch) {
            throw new AssertionError("unexpected ordered request");
        }
        public void adapterState(long id) { MobileCoreBridge.nativeCompleteAdapter(id, "available", "granted", "on", null); }
        public void startScan(long id, String[] services, String[] addresses, String mode, String callbackType, int legacy, long reportDelayMs, String phy) {
            check(services.length == 1 && services[0].equals("0000180d-0000-1000-8000-00805f9b34fb"), "scan filter reaches the radio canonicalized");
            if (scanIdentityV2) {
                String identity = Long.toString(id);
                if (++scans == 1) firstScanIdentity = identity; else secondScanIdentity = identity;
                if (scans == 1) check(advertisementV2("early-peer", identity) == MobileCoreBridge.STATUS_ACCEPTED, "synchronous advertisement accepted before start completion");
                check(MobileCoreBridge.nativeCompleteScanStartedV2(id, identity) == MobileCoreBridge.STATUS_ACCEPTED, "v2 start completion accepted");
                return;
            }
            MobileCoreBridge.nativeCompleteUnit(id);
        }
        public void stopScan(long id) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void connect(long id, String peer, boolean auto, String[] preferredPhy, String expectedGeneration) {
            lastPreferredPhy = String.join(",", preferredPhy);
            MobileCoreBridge.nativeCompleteUnit(id);
        }
        public void disconnect(long id, String peer) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void discover(long id, String peer) {
            MobileCoreBridge.nativeCompleteDiscovered(id, new int[] {0, 1, 2},
                new String[] {"180d", "2a37", "2902"}, new long[] {0, 0, 0}, new int[] {0, 0x1c, 0});
        }
        public void read(long id, String p, String s, long so, String c, long co) { MobileCoreBridge.nativeCompleteRead(id, new byte[] {0x42}, "read-response"); }
        public void write(long id, String p, String s, long so, String c, long co, byte[] v, boolean r) {
            if (holdSetupWrites) { setupWrites.add(id); return; }
            MobileCoreBridge.nativeCompleteUnit(id);
        }
        public void readDescriptor(long id, String p, String s, long so, String c, long co, String d, long dco) { MobileCoreBridge.nativeCompleteBytes(id, new byte[] {1, 0}); }
        public void writeDescriptor(long id, String p, String s, long so, String c, long co, String d, long dco, byte[] v) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void enableNotifications(long id, String p, String s, long so, String c, long co, long epoch, String req, String pref) {
            lastNotificationEpoch = epoch;
            MobileCoreBridge.nativeCompleteNotifyEnabled(id, "notification");
        }
        public void disableNotifications(long id, String p, String s, long so, String c, long co, long epoch) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void readMtu(long id, String p) { MobileCoreBridge.nativeCompleteMtu(id, 247); }
        public void readWriteLimits(long id, String p) { MobileCoreBridge.nativeCompleteWriteLimits(id, 512, 20); }
        public void readWriteReadiness(long id, String p) {
            MobileCoreBridge.nativeCompleteFailure(id, "unsupported", -1, "Android reports no write-without-response readiness", false, null);
        }
        public void requestMtu(long id, String p, int mtu) { MobileCoreBridge.nativeCompleteMtu(id, mtu); }
        public void readRssi(long id, String p) { MobileCoreBridge.nativeCompleteRssi(id, -60); }
        public void requestConnectionPriority(long id, String p, String priority) { MobileCoreBridge.nativeCompleteAccepted(id, true); }
        public boolean subrateAvailable() { return false; }
        public void requestSubrate(long id, String p, String mode) { MobileCoreBridge.nativeCompleteSubrateStatus(id, 0); }
        public void readPhy(long id, String p) { MobileCoreBridge.nativeCompletePhy(id, "le-2m", "le-2m"); }
        public void requestPhy(long id, String p, String tx, String rx) { MobileCoreBridge.nativeCompletePhyRequest(id, true, "le-2m", "le-2m"); }
        public void securityState(long id, String p) { MobileCoreBridge.nativeCompleteSecurity(id, "not-bonded", "unknown", "unknown", "unknown", 1); }
        public void createBond(long id, String p, String transport) { MobileCoreBridge.nativeCompleteSecurity(id, "bonded", "encrypted", "unknown", "unknown", -1); }
        public void cancelBond(long id, String p) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void resolvePeer(long id, String peerId) { MobileCoreBridge.nativeCompleteResolvedPeer(id, null, null); }
        public void connectedPeers(long id, String[] services) { MobileCoreBridge.nativeCompleteConnectedPeers(id, new String[] {"AA:BB:CC:DD:EE:FF"}, new String[] {"foreign link"}); }
        public void bondedPeers(long id) { MobileCoreBridge.nativeCompleteBondedPeers(id, new String[] {"AA:BB:CC:DD:EE:FF"}, new String[] {null}); }
        public void acquireBackground(long id, String kind, String reason) { MobileCoreBridge.nativeCompleteLease(id, "lease-1"); }
        public void releaseBackground(long id, String lease) { backgroundReleases.incrementAndGet(); MobileCoreBridge.nativeCompleteUnit(id); }
        public void updateBackgroundNotification(long id, String lease, String title, String body) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void associateCompanion(long id, String name, String service, String filtersJson) { MobileCoreBridge.nativeCompleteCompanion(id, 7L, "AA:BB:CC:DD:EE:FF", null, false); }
        public void listCompanion(long id) { MobileCoreBridge.nativeCompleteCompanionList(id, new long[0], new String[0], new String[0]); }
        public void disassociateCompanion(long id, long associationId) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void observePresence(long id, String peerId) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void unobservePresence(long id, String peerId) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void close(long id) { MobileCoreBridge.nativeCompleteClosed(id, new String[0], new String[0], new long[0], new String[0], new long[0], new String[0]); }
        public void cancel(long id) {}
    }

    static void check(boolean condition, String what) {
        if (!condition) {
            System.err.println("FAIL: " + what);
            System.exit(1);
        }
        System.out.println("ok: " + what);
    }

    static long nextAdmission = 0;

    static int advertisementV2(String peer, String identity) {
        return MobileCoreBridge.nativeIngestAdvertisementV2(peer, null, null, -58,
            MobileCoreBridge.ABSENT_INT, new String[] {"180D"}, new int[0], new byte[0][],
            new String[0], new byte[0][], -1, null, null, MobileCoreBridge.ABSENT_INT,
            null, null, -1L, identity);
    }

    static String operationId(String envelope) {
        java.util.regex.Matcher match = java.util.regex.Pattern.compile("\"operationId\":\"([^\"]+)\"").matcher(envelope);
        if (!match.find()) throw new AssertionError("missing scan operation: " + envelope);
        return match.group(1);
    }

    /** Every invoke naming an operation carries the session's next admission (wire rule). */
    static String call(long session, String op, String args) throws InterruptedException {
        if (args.contains("\"operationId\"") && !op.equals("op.cancel") && !op.equals("scan.stop")) {
            args = "{\"admission\":" + (++nextAdmission) + "," + args.substring(1);
        }
        BlockingQueue<String> result = new ArrayBlockingQueue<>(1);
        MobileCoreBridge.nativeInvoke(session, op, args, result::add);
        String envelope = result.poll(5, TimeUnit.SECONDS);
        check(envelope != null, op + " answers");
        return envelope;
    }

    public static void main(String[] args) throws Exception {
        BlockingQueue<Long> wakes = new ArrayBlockingQueue<>(16);
        check(MobileCoreBridge.nativeScanIdentityWireVersion() == 2, "scan identity v2 is implemented by the loaded library");
        check(MobileCoreBridge.nativeWireRevision().equals("ubm-mobile-wire/3"), "wire revision");
        check(MobileCoreBridge.nativeBuildIdentityJson().contains("\"schema\":\"ubm-native-build-identity/1\""), "build identity");
        check(MobileCoreBridge.nativeCompleteUnit(1) == MobileCoreBridge.STATUS_NO_HOST, "no host before install");
        java.nio.file.Path recordingDirectory = java.nio.file.Files.createTempDirectory("ubm-jni-offline-");
        check(MobileCoreBridge.nativeContinuationConfigureRecordingDirectory(recordingDirectory.toString()).contains("\"ok\":true"), "offline storage config succeeds without radio");
        String offline = MobileCoreBridge.nativeContinuationRecordingControl("unknown", "capture", "", 0, 0);
        check(offline.contains("argument.invalid"), "offline controls retain closed operation validation");
        check(!MobileCoreBridge.nativeHostInstalled(), "offline storage must not install a radio");
        Radio firstOwner = new Radio();
        Radio replacementOwner = new Radio();
        check(firstOwner.orderedAdmissionOwner(3).equals(firstOwner.orderedAdmissionOwner(3)), "owner identity is stable");
        check(!firstOwner.orderedAdmissionOwner(3).equals(replacementOwner.orderedAdmissionOwner(3)), "replacement has a fresh owner identity");
        check(firstOwner.orderedAdmissionOwner(2).isEmpty(), "unsupported owner version is refused");
        Radio legacyOwner = new Radio() {
            @Override public String orderedAdmissionOwner(int version) {
                check(!MobileCoreBridge.nativeHostInstalled(), "installation callback can inspect host state");
                check(MobileCoreBridge.nativeShutdownHost().contains("\"state\":\"release-failed\""),
                    "shutdown during installation does not claim release");
                try {
                    MobileCoreBridge.nativeInstallHost(this, wakes::add, "android", "nested", "nested");
                    throw new AssertionError("nested installation was accepted");
                } catch (MobileCoreBridge.MobileCoreException expected) {
                    check(expected.code.equals("lifecycle.invalid-state"), "installation reservation rejects a competing host");
                }
                return version == 2 ? "12345678-1234-4abc-8def-123456789abc" : "";
            }
        };
        try {
            MobileCoreBridge.nativeInstallHost(legacyOwner, wakes::add, "android", "jvm-test", "jvm-adapter");
            check(false, "legacy adapter refused");
        } catch (MobileCoreBridge.MobileCoreException expected) {
            check(expected.code.equals("protocol.incompatible"), "legacy adapter fails closed");
            check(!MobileCoreBridge.nativeHostInstalled(), "legacy adapter leaves no host installed");
        }
        Radio invalidOwner = new Radio() {
            @Override public String orderedAdmissionOwner(int version) { return "not-a-uuid"; }
        };
        try {
            MobileCoreBridge.nativeInstallHost(invalidOwner, wakes::add, "android", "jvm-test", "jvm-adapter");
            check(false, "malformed owner refused");
        } catch (MobileCoreBridge.MobileCoreException expected) {
            check(expected.code.equals("protocol.malformed"), "malformed owner reports protocol error");
            check(!MobileCoreBridge.nativeHostInstalled(), "malformed owner leaves no host installed");
        }
        Radio nullOwner = new Radio() {
            @Override public String orderedAdmissionOwner(int version) { return null; }
        };
        try {
            MobileCoreBridge.nativeInstallHost(nullOwner, wakes::add, "android", "jvm-test", "jvm-adapter");
            check(false, "null owner refused");
        } catch (MobileCoreBridge.MobileCoreException expected) {
            check(expected.code.equals("protocol.malformed"), "null owner reports malformed protocol");
            check(!MobileCoreBridge.nativeHostInstalled(), "null owner leaves no host installed");
        }
        Radio unsupportedOwner = new Radio() {
            @Override public String orderedAdmissionOwner(int version) { return ""; }
        };
        try {
            MobileCoreBridge.nativeInstallHost(unsupportedOwner, wakes::add, "android", "jvm-test", "jvm-adapter");
            check(false, "unsupported owner refused");
        } catch (MobileCoreBridge.MobileCoreException expected) {
            check(expected.code.equals("protocol.incompatible"), "unsupported owner reports incompatible protocol");
            check(!MobileCoreBridge.nativeHostInstalled(), "unsupported owner leaves no host installed");
        }
        MobileCoreBridge.nativeInstallHost(firstOwner, wakes::add, "android", "jvm-test", "jvm-adapter");
        check(MobileCoreBridge.nativeHostInstalled(), "host installed");
        check(!MobileCoreBridge.nativeContinuationRecordingControl("stop", "missing", "", 0, 0)
            .contains("storage.owner-conflict"), "legacy host retains its own stop route");
        try {
            MobileCoreBridge.nativeInstallHost(new Radio(), wakes::add, "android", "jvm-test", "jvm-adapter");
            check(false, "second install refused");
        } catch (MobileCoreBridge.MobileCoreException expected) {
            check(expected.code.equals("lifecycle.invalid-state"), "second install refused");
        }
        try {
            MobileCoreBridge.nativeOpenSession("rn", "ubm-mobile-wire/0", "jvm-module");
            check(false, "foreign wire revision refused");
        } catch (MobileCoreBridge.MobileCoreException expected) {
            check(expected.code.equals("protocol.incompatible"), "foreign wire revision refused");
        }
        String open = MobileCoreBridge.nativeOpenSession("rn", "ubm-mobile-wire/3", "jvm-module");
        check(open.contains("\"wireRevision\":\"ubm-mobile-wire/3\""), "admission record");
        long session = Long.parseLong(open.replaceAll(".*\"sessionId\":(\\d+).*", "$1"));
        try {
            MobileCoreBridge.nativeNegotiateScanEndVersionQualified(session, "[2]");
            throw new AssertionError("unqualified scan-end v2 accepted");
        } catch (MobileCoreBridge.MobileCoreException expected) {
            check(expected.code.equals("protocol.incompatible"), "v2-only offer fails closed");
        }
        check(MobileCoreBridge.nativeNegotiateScanEndVersionQualified(session, "[1,2]").contains("\"version\":1"), "production scan-end negotiation selects qualified v1");
        String adapter = call(session, "adapter.state", "{}");
        check(adapter.contains("\"ok\":true") && adapter.contains("\"power\":\"on\""), "adapter.state through the RadioHost");
        firstOwner.scanIdentityV2 = true;
        String scanA = call(session, "scan.start", "{\"serviceUuids\":[\"180D\"],\"duplicatePolicy\":\"all\",\"operationId\":\"v2-a\"}");
        check(scanA.contains("\"ok\":true"), "v2 scan A starts");
        String earlyRecords = "";
        long earlyDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!earlyRecords.contains("early-peer") && System.nanoTime() < earlyDeadline) {
            wakes.poll(100, TimeUnit.MILLISECONDS);
            earlyRecords += MobileCoreBridge.nativeDrain(session, 256, 65536);
        }
        check(earlyRecords.contains("early-peer"), "synchronous advertisement survives start completion");
        check(call(session, "scan.stop", "{\"operationId\":\"" + operationId(scanA) + "\"}").contains("\"ok\":true"), "v2 scan A stops");
        String scanB = call(session, "scan.start", "{\"serviceUuids\":[\"180D\"],\"duplicatePolicy\":\"all\",\"operationId\":\"v2-b\"}");
        check(scanB.contains("\"ok\":true"), "v2 scan B starts");
        MobileCoreBridge.nativeDrain(session, 256, 65536);
        advertisementV2("stale-peer", firstOwner.firstScanIdentity);
        MobileCoreBridge.nativeIngestScanFailedV2("old failure", firstOwner.firstScanIdentity);
        advertisementV2("current-peer", firstOwner.secondScanIdentity);
        String v2Records = "";
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!v2Records.contains("current-peer") && System.nanoTime() < deadline) {
            wakes.poll(100, TimeUnit.MILLISECONDS);
            v2Records += MobileCoreBridge.nativeDrain(session, 256, 65536);
        }
        check(v2Records.contains("current-peer") && !v2Records.contains("stale-peer") && !v2Records.contains("scan-end"), "stale A callbacks cannot reach or terminate B: " + v2Records);
        check(call(session, "session.reconcile", "{}").contains(operationId(scanB)), "B remains owned after stale A failure");
        check(call(session, "scan.stop", "{\"operationId\":\"" + operationId(scanB) + "\"}").contains("\"ok\":true"), "v2 scan B stops");
        firstOwner.scanIdentityV2 = false;
        MobileCoreBridge.nativeDrain(session, 256, 65536);
        wakes.clear();
        String scan = call(session, "scan.start", "{\"serviceUuids\":[\"180D\"],\"duplicatePolicy\":\"all\",\"operationId\":\"s1\"}");
        check(scan.contains("\"ok\":true"), "scan.start");
        int status = MobileCoreBridge.nativeIngestAdvertisement("AA:BB:CC:DD:EE:FF", "AA:BB:CC:DD:EE:FF", "Polar H10", -58,
            MobileCoreBridge.ABSENT_INT, new String[] {"180D"}, new int[] {0x006b}, new byte[][] {{0, (byte) 0x80, (byte) 0xff}},
            new String[0], new byte[0][], 1, null, new String[0], 0x0341, new byte[] {2, 1, 6}, "cached display", 1234L);
        check(status == MobileCoreBridge.STATUS_ACCEPTED, "advertisement accepted");
        Long woken = wakes.poll(5, TimeUnit.SECONDS);
        check(woken != null && woken == session, "wake for the scanning session");
        String drained = MobileCoreBridge.nativeDrain(session, 256, 65536);
        check(drained.contains("\"t\":\"adv\"") && drained.contains("\"payloadB64\":\"AID/\"") && drained.contains("\"appearance\":833") && drained.contains("\"rawRecordB64\":\"AgEG\"") && drained.contains("\"sourceTimestampMs\":1234"), "drain carries the advertisement");
        String connect = call(session, "connection.connect", "{\"peerId\":\"AA:BB:CC:DD:EE:FF\",\"lease\":\"l1\",\"operationId\":\"c1\",\"preferredPhy\":[\"le-2m\",\"le-1m\"]}");
        check(connect.contains("\"connectionGeneration\""), "connect through the RadioHost");
        check("le-2m,le-1m".equals(lastPreferredPhy), "preferred PHYs reach the RadioHost: " + lastPreferredPhy);
        String discovered = call(session, "gatt.discover", "{\"peerId\":\"AA:BB:CC:DD:EE:FF\",\"lease\":\"l1\",\"operationId\":\"d1\"}");
        check(discovered.contains("\"ok\":true"), "discover through the RadioHost");
        String selector = "{\"serviceUuid\":\"180D\",\"serviceOccurrence\":0,\"characteristicUuid\":\"2A37\",\"characteristicOccurrence\":0}";
        String longWrite = call(session, "gatt.write", "{\"peerId\":\"AA:BB:CC:DD:EE:FF\",\"selector\":" + selector + ",\"valueB64\":\"" + "AAAA".repeat(100) + "\",\"mode\":\"with-response\",\"operationId\":\"w1\"}");
        check(longWrite.contains("\"commitState\":\"confirmed\""), "with-response write beyond one ATT payload: " + longWrite);
        String command = call(session, "gatt.write", "{\"peerId\":\"AA:BB:CC:DD:EE:FF\",\"selector\":" + selector + ",\"valueB64\":\"" + "A".repeat(28) + "\",\"mode\":\"without-response\",\"operationId\":\"w2\"}");
        check(command.contains("\"bytes.too-large\"") && command.contains("\"not-dispatched\""), "command bounded by one ATT payload: " + command);
        String rssi = call(session, "connection.rssi", "{\"peerId\":\"AA:BB:CC:DD:EE:FF\",\"lease\":\"l1\",\"operationId\":\"r1\"}");
        check(rssi.contains("\"rssi\":-60"), "connected RSSI");
        String bonded = call(session, "peers.bonded", "{\"operationId\":\"b1\"}");
        check(bonded.contains("\"source\":\"system-bonded\""), "bonded peers");
        String lease = call(session, "background.acquire", "{\"kind\":\"connected-device\",\"reason\":\"workout\"}");
        check(lease.contains("\"leaseId\":\"lease-1\""), "foreground-service lease acquired");
        String subscribed = call(session, "gatt.subscribe", "{\"peerId\":\"AA:BB:CC:DD:EE:FF\",\"selector\":" + selector + ",\"consumer\":\"continuation-0\",\"deliveryMode\":\"require-notification\",\"operationId\":\"sub1\"}");
        check(subscribed.contains("\"consumer\":\"continuation-0\""), "continuation subscription enabled");
        check(MobileCoreBridge.nativeIngestNotificationSourceFailed("AA:BB:CC:DD:EE:FF", "180D", 0, "2A37", 0, lastNotificationEpoch + 1, "gatt-status", 133, "notification delivery uncertain", true, "NOTIFICATION_DELIVERY_UNCERTAIN") == MobileCoreBridge.STATUS_ACCEPTED, "typed stale source failure crosses JNI");
        check(MobileCoreBridge.nativeIngestNotificationSourceFailed("AA:BB:CC:DD:EE:FF", "180D", 0, "2A37", 0, Long.MIN_VALUE, "gatt-status", 133, "notification delivery uncertain", true, "NOTIFICATION_DELIVERY_UNCERTAIN") == MobileCoreBridge.STATUS_ACCEPTED, "epoch 2^63 crosses JNI without clamping");
        check(MobileCoreBridge.nativeIngestNotification("AA:BB:CC:DD:EE:FF", "180D", 0, "2A37", 0, -1L, new byte[] {9}) == MobileCoreBridge.STATUS_ACCEPTED, "epoch u64 max crosses JNI without clamping");
        check(MobileCoreBridge.nativeIngestNotification("AA:BB:CC:DD:EE:FF", "180D", 0, "2A37", 0, lastNotificationEpoch, new byte[] {1, 2}) == MobileCoreBridge.STATUS_ACCEPTED, "pre-cutoff notification accepted");
        check(wakes.poll(5, TimeUnit.SECONDS) != null, "pre-cutoff notification reached the session outbox");
        String sealed = call(session, "session.quiesce", "{}");
        check(sealed.contains("\"state\":\"sealed\"") && sealed.contains("\"afterCutoffItems\":0"), "continuation outbox sealed");
        String continuationDrain = MobileCoreBridge.nativeDrain(session, 256, 65536);
        check(continuationDrain.contains("\"consumer\":\"continuation-0\"") && continuationDrain.contains("\"valueB64\":\"AQI=\""), "pre-cutoff notification drains exactly once");
        check(MobileCoreBridge.nativeIngestNotification("AA:BB:CC:DD:EE:FF", "180D", 0, "2A37", 0, lastNotificationEpoch, new byte[] {3}) == MobileCoreBridge.STATUS_ACCEPTED, "post-cutoff notification reaches native intake");
        String cutoff = sealed;
        long cutoffDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!cutoff.contains("\"afterCutoffItems\":1") && System.nanoTime() < cutoffDeadline) {
            Thread.sleep(5);
            cutoff = call(session, "session.quiesce", "{}");
        }
        check(cutoff.contains("\"afterCutoffItems\":1"), "post-cutoff notification is loss-accounted");
        String continuationDisposed = call(session, "session.continuation-dispose", "{}");
        check(continuationDisposed.contains("\"state\":\"released\"") && continuationDisposed.contains("\"afterCutoffItems\":1"), "continuation cleanup returns cutoff accounting");
        check(backgroundReleases.get() == 0, "manager destroy keeps the module's foreground service");
        String scope = MobileCoreBridge.nativeReleaseBackgroundScope("jvm-module");
        check(scope.contains("\"state\":\"released\"") && backgroundReleases.get() == 1, "module invalidation releases the lease: " + scope);
        // A process-owned standing order must collect with no application session.
        // Exercise the real JNI/core boundary, not a Kotlin copy of its policy.
        BlockingQueue<String> continuationResults = new ArrayBlockingQueue<>(1);
        String continuationSelector = "{\"serviceUuid\":\"0000180d-0000-1000-8000-00805f9b34fb\",\"serviceOccurrence\":1,\"characteristicUuid\":\"00002a37-0000-1000-8000-00805f9b34fb\",\"characteristicOccurrence\":1}";
        String declaration = "{\"onAppearance\":\"native\",\"peerId\":\"AA:BB:CC:DD:EE:FF\",\"resubscribe\":[" + continuationSelector + "]}";
        check(MobileCoreBridge.nativeContinuationSeedDeclaration("{}").contains("seeded"), "seed persisted declaration");
        String reservation = MobileCoreBridge.nativeContinuationReserveDeclaration(declaration);
        String reservationToken = reservation.split("\"reservationToken\":\"")[1].split("\"")[0];
        MobileCoreBridge.nativeContinuationExecute("AA:BB:CC:DD:EE:FF", declaration, continuationResults::add);
        String reserved = continuationResults.poll(5, TimeUnit.SECONDS);
        check(reserved != null && reserved.contains("lifecycle.invalid-state"), "pending persistence fences native admission");
        check(MobileCoreBridge.nativeContinuationCancelDeclaration(reservationToken).contains("cancelled"), "failed persistence cancels reservation");
        reservation = MobileCoreBridge.nativeContinuationReserveDeclaration(declaration);
        reservationToken = reservation.split("\"reservationToken\":\"")[1].split("\"")[0];
        check(MobileCoreBridge.nativeContinuationCommitDeclaration(reservationToken).contains("committed"), "persisted declaration commits");
        check(MobileCoreBridge.nativeContinuationSeedDeclaration("{}").contains("lifecycle.invalid-state"), "stale captured declaration cannot roll authority back");
        MobileCoreBridge.nativeContinuationExecute("AA:BB:CC:DD:EE:FF", declaration, continuationResults::add);
        String continued = continuationResults.poll(5, TimeUnit.SECONDS);
        check(continued != null && continued.contains("continuation.completed"), "native standing order reconnects and subscribes: " + continued);
        wakes.clear();
        check(MobileCoreBridge.nativeIngestNotification("AA:BB:CC:DD:EE:FF", "180D", 0, "2A37", 0, lastNotificationEpoch, new byte[] {4, 5}) == MobileCoreBridge.STATUS_ACCEPTED, "standing order receives without a JS session");
        String backlog = "";
        long backlogDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!backlog.contains("\"retainedByteBuffers\":1") && System.nanoTime() < backlogDeadline) {
            MobileCoreBridge.nativeContinuationDescribeBacklog(continuationResults::add);
            backlog = continuationResults.poll(5, TimeUnit.SECONDS);
            check(backlog != null, "standing order backlog inspection answers");
            if (!backlog.contains("\"retainedByteBuffers\":1")) Thread.sleep(5);
        }
        check(backlog.contains("\"retainedByteBuffers\":1"), "standing order notification reaches its bounded outbox: " + backlog);
        check(wakes.isEmpty(), "native continuation never publishes an unknown session wake to JavaScript");
        MobileCoreBridge.nativeContinuationPrepareClaim(256, 65536, continuationResults::add);
        String prepared = continuationResults.poll(5, TimeUnit.SECONDS);
        boolean retainedHandoff = prepared != null && prepared.contains("protocol.incompatible");
        if (retainedHandoff) {
            MobileCoreBridge.nativeContinuationPrepareClaimVersioned(1, 256, 65536, continuationResults::add);
            prepared = continuationResults.poll(5, TimeUnit.SECONDS);
        }
        check(prepared != null && prepared.contains("claimToken") && prepared.contains("BAU="), "standing order prepares its queued bytes: " + prepared);
        String claimToken = prepared.replaceAll(".*\"claimToken\":\"([^\"]+)\".*", "$1");
        if (retainedHandoff) MobileCoreBridge.nativeContinuationAcknowledgeClaimV1(claimToken, continuationResults::add);
        else MobileCoreBridge.nativeContinuationAcknowledgeClaim(claimToken, continuationResults::add);
        String acknowledged = continuationResults.poll(5, TimeUnit.SECONDS);
        check(acknowledged != null && acknowledged.contains("\"disposed\":true"), "acknowledged standing order releases: " + acknowledged);
        if (retainedHandoff) {
            MobileCoreBridge.nativeContinuationRetireClaimReceipt(claimToken, continuationResults::add);
            String retired = continuationResults.poll(5, TimeUnit.SECONDS);
            check(retired != null && retired.contains("\"ok\":true"), "first v1 receipt retires");
        }
        MobileCoreBridge.nativeContinuationExecute("AA:BB:CC:DD:EE:FF", declaration, continuationResults::add);
        String resumed = continuationResults.poll(5, TimeUnit.SECONDS);
        check(resumed != null && resumed.contains("continuation.completed"), "new continuation starts for v1 handoff: " + resumed);
        MobileCoreBridge.nativeContinuationPrepareClaimVersioned(1, 256, 65536, continuationResults::add);
        String v1 = continuationResults.poll(5, TimeUnit.SECONDS);
        check(v1 != null && v1.contains("claimToken"), "v1 prepares a retained claim: " + v1);
        String v1Token = v1.replaceAll(".*\"claimToken\":\"([^\"]+)\".*", "$1");
        MobileCoreBridge.nativeContinuationAcknowledgeClaimV1(v1Token, continuationResults::add);
        String v1Ack = continuationResults.poll(5, TimeUnit.SECONDS);
        check(v1Ack != null && v1Ack.contains("\"ok\":true"), "v1 acknowledgement succeeds: " + v1Ack);
        MobileCoreBridge.nativeContinuationAcknowledgeClaimV1(v1Token, continuationResults::add);
        check(v1Ack.equals(continuationResults.poll(5, TimeUnit.SECONDS)), "v1 acknowledgement retries recover the receipt");
        MobileCoreBridge.nativeContinuationRetireClaimReceipt(v1Token, continuationResults::add);
        String v1Retired = continuationResults.poll(5, TimeUnit.SECONDS);
        check(v1Retired != null && v1Retired.contains("\"ok\":true"), "v1 receipt retires explicitly: " + v1Retired);
        MobileCoreBridge.nativeContinuationAcknowledgeClaimV1(v1Token, continuationResults::add);
        String retiredAck = continuationResults.poll(5, TimeUnit.SECONDS);
        check(retiredAck != null && retiredAck.contains("\"ok\":false"), "retired receipt cannot be replayed: " + retiredAck);
        durableSetupContinuation(continuationSelector);
        String shutdown = MobileCoreBridge.nativeShutdownHost();
        check(shutdown.contains("\"state\":\"release-failed\""), "unproved native owner stays pending: " + shutdown);
        check(MobileCoreBridge.nativeHostInstalled(), "failed release retains callback route");
        check(MobileCoreBridge.nativeCompleteUnit(987654321) == MobileCoreBridge.STATUS_LATE, "late completion reaches retained host");
        check(MobileCoreBridge.nativeIngestOrderedWake(1, 987654321) != MobileCoreBridge.STATUS_NO_HOST, "late wake reaches retained host");
        String retry = MobileCoreBridge.nativeShutdownHost();
        check(retry.contains("\"state\":\"release-failed\""), "retry retains unresolved native owner: " + retry);
        check(MobileCoreBridge.nativeHostInstalled(), "retry retains callback route");
        System.out.println("mobile JNI exchange: OK");
        System.exit(0);
    }

    static void durableSetupContinuation(String selector) throws Exception {
        final String peer = "AA:BB:CC:DD:EE:FF";
        String order = "{\"onAppearance\":\"native\",\"peerId\":\"" + peer + "\",\"resubscribe\":[" + selector + "],"
            + "\"recording\":{\"id\":\"jni-setup\",\"maxBytes\":1048576,\"maxRecords\":1000},"
            + "\"setup\":[{\"selector\":" + selector + ",\"value\":[2,0],\"timeoutMs\":10000,\"response\":{\"subscriptionIndex\":0,\"prefix\":[240,2,0],\"minLength\":4,\"maxLength\":4,\"status\":{\"offset\":3,\"accepted\":[0]}}}]}";
        String reservation = MobileCoreBridge.nativeContinuationReserveDeclaration(order);
        String token = reservation.split("\"reservationToken\":\"")[1].split("\"")[0];
        check(MobileCoreBridge.nativeContinuationCommitDeclaration(token).contains("committed"), "setup recording declaration committed");
        BlockingQueue<String> results = new ArrayBlockingQueue<>(1);
        holdSetupWrites = true;
        MobileCoreBridge.nativeContinuationExecute(peer, order, results::add);
        for (int generation = 0; generation < 2; generation++) {
            Long write = setupWrites.poll(5, TimeUnit.SECONDS);
            check(write != null, "setup write reached actual JNI radio generation " + generation);
            MobileCoreBridge.nativeIngestNotification(peer, "180D", 0, "2A37", 0, lastNotificationEpoch, new byte[] {(byte)240,2,0,0});
            // The durable ACK is positive evidence that the response observer can
            // see it; it must not bypass the still-held ATT completion.
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
            String page = "";
            while (!page.contains("8AIAAA==") && System.nanoTime() < deadline) {
                page = MobileCoreBridge.nativeContinuationRecordingControl("prepare", "jni-setup", "", 256, 65536);
                if (!page.contains("8AIAAA==")) {
                    // A prepare pins a prefix; release only the validated empty
                    // registration/control prefix so the next read can include ACK.
                    String prefixToken = page.replaceAll(".*\"token\":\"([^\"]+)\".*", "$1");
                    if (!prefixToken.equals(page)) check(MobileCoreBridge.nativeContinuationRecordingControl("acknowledge", "jni-setup", prefixToken, 0, 0).contains("\"acknowledged\":true"), "registration prefix acknowledged");
                    Thread.sleep(5);
                }
            }
            check(page.contains("8AIAAA=="), "early application ACK persisted before ATT completion");
            if (generation == 0) check(results.isEmpty(), "application ACK cannot finish while ATT held");
            String prefixToken = page.replaceAll(".*\"token\":\"([^\"]+)\".*", "$1");
            check(MobileCoreBridge.nativeContinuationRecordingControl("prepare", "jni-setup", "", 256, 65536).equals(page), "durable ACK prefix replay is stable");
            check(MobileCoreBridge.nativeContinuationRecordingControl("acknowledge", "jni-setup", prefixToken, 0, 0).contains("\"acknowledged\":true"), "validated ACK prefix acknowledged");
            MobileCoreBridge.nativeCompleteUnit(write);
            if (generation == 0) {
                String completed = results.poll(5, TimeUnit.SECONDS);
                check(completed != null && completed.contains("continuation.completed"), "setup completes after ATT");
                MobileCoreBridge.nativeIngestConnection(peer, false, 8);
            }
        }
        holdSetupWrites = false;
        String outcome = "";
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!outcome.contains("continuation.completed") && System.nanoTime() < deadline) {
            MobileCoreBridge.nativeContinuationDescribeBacklog(results::add);
            outcome = results.poll(5, TimeUnit.SECONDS);
            check(outcome != null, "recovery status answers");
            if (!outcome.contains("continuation.completed")) Thread.sleep(5);
        }
        check(outcome.contains("continuation.completed"), "link-loss replays setup autonomously through JNI");
        MobileCoreBridge.nativeIngestNotification(peer, "180D", 0, "2A37", 0, lastNotificationEpoch, new byte[] {0,73});
        String positive = "";
        deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!positive.contains("AEk=") && System.nanoTime() < deadline) {
            positive = MobileCoreBridge.nativeContinuationRecordingControl("prepare", "jni-setup", "", 256, 65536);
            if (!positive.contains("AEk=")) {
                String prefixToken = positive.replaceAll(".*\"token\":\"([^\"]+)\".*", "$1");
                if (!prefixToken.equals(positive)) check(MobileCoreBridge.nativeContinuationRecordingControl("acknowledge", "jni-setup", prefixToken, 0, 0).contains("\"acknowledged\":true"), "pre-value control prefix acknowledged");
                Thread.sleep(5);
            }
        }
        check(positive.contains("AEk="), "positive data persisted before radio claim");
        MobileCoreBridge.nativeContinuationPrepareClaim(256, 65536, results::add);
        String claim = results.poll(5, TimeUnit.SECONDS);
        boolean retainedClaim = claim != null && claim.contains("protocol.incompatible");
        if (retainedClaim) {
            MobileCoreBridge.nativeContinuationPrepareClaimVersioned(1, 256, 65536, results::add);
            claim = results.poll(5, TimeUnit.SECONDS);
        }
        check(claim != null && claim.contains("\"id\":\"jni-setup\""), "radio claim references retained journal");
        check(claim.contains("\"consumerCount\":2"), "link recovery retains both subscription generations");
        String claimToken = claim.replaceAll(".*\"claimToken\":\"([^\"]+)\".*", "$1");
        if (retainedClaim) MobileCoreBridge.nativeContinuationAcknowledgeClaimV1(claimToken, results::add);
        else MobileCoreBridge.nativeContinuationAcknowledgeClaim(claimToken, results::add);
        check(results.poll(5, TimeUnit.SECONDS).contains("\"disposed\":true"), "setup radio released before offline cursor");
        if (retainedClaim) {
            MobileCoreBridge.nativeContinuationRetireClaimReceipt(claimToken, results::add);
            check(results.poll(5, TimeUnit.SECONDS).contains("\"ok\":true"), "setup claim receipt retires");
        }
        String retained = MobileCoreBridge.nativeContinuationRecordingControl("prepare", "jni-setup", "", 256, 65536);
        check(retained.contains("AEk="), "offline journal preserves post-recovery positive bytes after radio release");
        check(MobileCoreBridge.nativeContinuationRecordingControl("prepare", "jni-setup", "", 256, 65536).equals(retained), "offline prefix replays exactly");
        String retainedToken = retained.replaceAll(".*\"token\":\"([^\"]+)\".*", "$1");
        String receipt = MobileCoreBridge.nativeContinuationRecordingControl("acknowledge", "jni-setup", retainedToken, 0, 0);
        check(receipt.contains("\"acknowledged\":true"), "offline prefix ACK succeeds");
        check(MobileCoreBridge.nativeContinuationRecordingControl("acknowledge", "jni-setup", retainedToken, 0, 0).equals(receipt), "offline ACK is replayable");
    }
}
