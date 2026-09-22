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
    static final java.util.concurrent.atomic.AtomicInteger backgroundReleases = new java.util.concurrent.atomic.AtomicInteger();

    static final class Radio implements MobileCoreBridge.RadioHost {
        public void adapterState(long id) { MobileCoreBridge.nativeCompleteAdapter(id, "available", "granted", "on", null); }
        public void startScan(long id, String[] services, String[] addresses, String mode, String callbackType, int legacy) {
            check(services.length == 1 && services[0].equals("0000180d-0000-1000-8000-00805f9b34fb"), "scan filter reaches the radio canonicalized");
            MobileCoreBridge.nativeCompleteUnit(id);
        }
        public void stopScan(long id) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void connect(long id, String peer, boolean auto, String[] preferredPhy) {
            lastPreferredPhy = String.join(",", preferredPhy);
            MobileCoreBridge.nativeCompleteUnit(id);
        }
        public void disconnect(long id, String peer) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void discover(long id, String peer) {
            MobileCoreBridge.nativeCompleteDiscovered(id, new int[] {0, 1, 2},
                new String[] {"180d", "2a37", "2902"}, new long[] {0, 0, 0}, new int[] {0, 0x1c, 0});
        }
        public void read(long id, String p, String s, long so, String c, long co) { MobileCoreBridge.nativeCompleteRead(id, new byte[] {0x42}, "read-response"); }
        public void write(long id, String p, String s, long so, String c, long co, byte[] v, boolean r) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void readDescriptor(long id, String p, String s, long so, String c, long co, String d, long dco) { MobileCoreBridge.nativeCompleteBytes(id, new byte[] {1, 0}); }
        public void writeDescriptor(long id, String p, String s, long so, String c, long co, String d, long dco, byte[] v) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void enableNotifications(long id, String p, String s, long so, String c, long co, long epoch, String req, String pref) {
            lastNotificationEpoch = epoch;
            MobileCoreBridge.nativeCompleteNotifyEnabled(id, "notification");
        }
        public void disableNotifications(long id, String p, String s, long so, String c, long co) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void readMtu(long id, String p) { MobileCoreBridge.nativeCompleteMtu(id, 247); }
        public void readWriteLimits(long id, String p) { MobileCoreBridge.nativeCompleteWriteLimits(id, 512, 20); }
        public void requestMtu(long id, String p, int mtu) { MobileCoreBridge.nativeCompleteMtu(id, mtu); }
        public void readRssi(long id, String p) { MobileCoreBridge.nativeCompleteRssi(id, -60); }
        public void requestConnectionPriority(long id, String p, String priority) { MobileCoreBridge.nativeCompleteAccepted(id, true); }
        public void readPhy(long id, String p) { MobileCoreBridge.nativeCompletePhy(id, "le-2m", "le-2m"); }
        public void requestPhy(long id, String p, String tx, String rx) { MobileCoreBridge.nativeCompletePhyRequest(id, true, "le-2m", "le-2m"); }
        public void securityState(long id, String p) { MobileCoreBridge.nativeCompleteSecurity(id, "not-bonded", "unknown", "unknown", "unknown", 1); }
        public void createBond(long id, String p, String transport) { MobileCoreBridge.nativeCompleteSecurity(id, "bonded", "encrypted", "unknown", "unknown", -1); }
        public void cancelBond(long id, String p) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void bondedPeers(long id) { MobileCoreBridge.nativeCompleteBondedPeers(id, new String[] {"AA:BB:CC:DD:EE:FF"}, new String[] {null}); }
        public void acquireBackground(long id, String kind, String reason) { MobileCoreBridge.nativeCompleteLease(id, "lease-1"); }
        public void releaseBackground(long id, String lease) { backgroundReleases.incrementAndGet(); MobileCoreBridge.nativeCompleteUnit(id); }
        public void updateBackgroundNotification(long id, String lease, String title, String body) { MobileCoreBridge.nativeCompleteUnit(id); }
        public void associateCompanion(long id, String name, String service) { MobileCoreBridge.nativeCompleteCompanion(id, 7L, "AA:BB:CC:DD:EE:FF", null, false); }
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
        check(MobileCoreBridge.nativeWireRevision().equals("ubm-mobile-wire/1"), "wire revision");
        check(MobileCoreBridge.nativeBuildIdentityJson().contains("\"schema\":\"ubm-native-build-identity/1\""), "build identity");
        check(MobileCoreBridge.nativeCompleteUnit(1) == MobileCoreBridge.STATUS_NO_HOST, "no host before install");
        MobileCoreBridge.nativeInstallHost(new Radio(), wakes::add, "android", "jvm-test", "jvm-adapter");
        check(MobileCoreBridge.nativeHostInstalled(), "host installed");
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
        String open = MobileCoreBridge.nativeOpenSession("rn", "ubm-mobile-wire/1", "jvm-module");
        check(open.contains("\"wireRevision\":\"ubm-mobile-wire/1\""), "admission record");
        long session = Long.parseLong(open.replaceAll(".*\"sessionId\":(\\d+).*", "$1"));
        String adapter = call(session, "adapter.state", "{}");
        check(adapter.contains("\"ok\":true") && adapter.contains("\"power\":\"on\""), "adapter.state through the RadioHost");
        String scan = call(session, "scan.start", "{\"serviceUuids\":[\"180D\"],\"duplicatePolicy\":\"all\",\"operationId\":\"s1\"}");
        check(scan.contains("\"ok\":true"), "scan.start");
        int status = MobileCoreBridge.nativeIngestAdvertisement("AA:BB:CC:DD:EE:FF", "AA:BB:CC:DD:EE:FF", "Polar H10", -58,
            MobileCoreBridge.ABSENT_INT, new String[] {"180D"}, new int[] {0x006b}, new byte[][] {{0, (byte) 0x80, (byte) 0xff}},
            new String[0], new byte[0][], 1, null, new String[0], 0x0341, new byte[] {2, 1, 6});
        check(status == MobileCoreBridge.STATUS_ACCEPTED, "advertisement accepted");
        Long woken = wakes.poll(5, TimeUnit.SECONDS);
        check(woken != null && woken == session, "wake for the scanning session");
        String drained = MobileCoreBridge.nativeDrain(session, 256, 65536);
        check(drained.contains("\"t\":\"adv\"") && drained.contains("\"payloadB64\":\"AID/\"") && drained.contains("\"appearance\":833") && drained.contains("\"rawRecordB64\":\"AgEG\""), "drain carries the advertisement");
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
        String shutdown = MobileCoreBridge.nativeShutdownHost();
        check(shutdown.contains("\"state\":\"released\""), "host shutdown releases: " + shutdown);
        check(!MobileCoreBridge.nativeHostInstalled(), "host removed");
        System.out.println("mobile JNI exchange: OK");
        System.exit(0);
    }
}
