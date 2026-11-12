package com.ubm.core;

import java.lang.reflect.Proxy;
import java.util.concurrent.atomic.AtomicInteger;

/** Synthetic radio exercising the actual Java/JNI/ForeignRadio request boundary. */
public final class TestOrdered {
    private static native int nativeExercise(MobileCoreBridge.RadioHost radio);
    private static void check(boolean value, String message) {
        if (!value) throw new AssertionError(message);
    }
    private static String receipt(long session, String operation, String id) {
        java.util.concurrent.CountDownLatch done = new java.util.concurrent.CountDownLatch(1);
        String[] result = { null };
        MobileCoreBridge.nativeOrderedRecordingReceiptControl(session, operation, id, value -> { result[0] = value; done.countDown(); });
        try { check(done.await(3, java.util.concurrent.TimeUnit.SECONDS), "receipt callback"); }
        catch (InterruptedException error) { throw new AssertionError(error); }
        return result[0];
    }
    private static MobileCoreBridge.OrderedRegistration member() {
        MobileCoreBridge.OrderedRegistration m = new MobileCoreBridge.OrderedRegistration();
        m.member = -1; m.sourceMember = -1; m.epoch = -1;
        return m;
    }
    public static void main(String[] args) {
        AtomicInteger drains = new AtomicInteger();
        AtomicInteger markerStates = new AtomicInteger();
        long[] heldOpen = {0};
        long[] heldBarrier = {0};
        int[] barriers = {0};
        int[] settlements = {0};
        int[] enables = {0};
        boolean[] closed = {false};
        MobileCoreBridge.RadioHost radio = (MobileCoreBridge.RadioHost) Proxy.newProxyInstance(
            TestOrdered.class.getClassLoader(), new Class<?>[] { MobileCoreBridge.RadioHost.class },
            (proxy, method, a) -> {
                String methodName = method.getName();
                if (methodName.equals("subrateAvailable")) return false;
                if (methodName.equals("orderedAdmissionOwner")) return "12345678-1234-4abc-8def-123456789abc";
                long id = (Long) a[0];
                if (methodName.equals("discover")) {
                    MobileCoreBridge.nativeCompleteDiscoveredScoped(id, new int[0], new String[0], new long[0], new int[0], "graph", -1, -1);
                } else if (methodName.equals("orderedEnable")) {
                    check(a[7].equals("connection") && a[8].equals("counter") && a[9].equals("18446744073709551615"), "activation identity");
                    for (int i = 10; i <= 14; i++) check((Long)a[i] == -1, "activation unsigned bits");
                    String op = (String)a[1];
                    if (op.equals("enable")) { if (enables[0]++ == 0) MobileCoreBridge.nativeCompleteNotifyEnabled(id, "unknown"); else MobileCoreBridge.nativeCompleteOrderedEnableReleased(id, "path-stale", 133, "released", false, "native-code"); }
                    else if (op.equals("disable") || op.equals("forget-receipt")) MobileCoreBridge.nativeCompleteUnit(id);
                    else { MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult(); result.kind = "enable-status"; int n = settlements[0]++; result.status = n == 0 ? "pending" : n == 1 ? "enabled" : n == 2 ? "released" : "uncertain"; result.delivery = "indication"; result.failureKind = "path-stale"; result.gattStatus = 133; result.nativeName = "native-code"; result.detail = "synthetic"; MobileCoreBridge.nativeCompleteOrdered(id, result); }
                } else if (methodName.equals("orderedRecoverOpen")) {
                    check((Long)a[1] == -1 && (Long)a[2] == -2 && a[3].equals("peer"), "recovery exact identity bits");
                    MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult(); result.kind = "opened"; result.generationKind = "counter"; result.generationValue = "18446744073709551615"; result.members = new MobileCoreBridge.OrderedRegistration[] {member()}; MobileCoreBridge.nativeCompleteOrdered(id, result);
                } else if (methodName.equals("orderedAcknowledgeOpen")) {
                    check((Integer)a[1] == 1 && a[4].equals("peer"), "acknowledgement version and peer identity");
                    MobileCoreBridge.nativeCompleteUnit(id);
                } else if (methodName.equals("orderedOpen")) {
                    check(a[10].equals("connection") && a[11].equals("database") && a[12].equals("graph") && (Long)a[13] == -1 && (Long)a[14] == -1, "snapshot identity bits");
                    if ((Long)a[1] >= 8 && (Long)a[1] <= 10) { MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult(); result.kind = "open-failed"; result.failureKind = "path-stale"; result.gattStatus = MobileCoreBridge.ABSENT_INT; result.detail = "synthetic failure"; result.ownershipKind = (Long)a[1] == 8 ? "indeterminate" : (Long)a[1] == 9 ? "not-created" : "created"; result.recoveryKey = -1; result.generationKind = "counter"; result.generationValue = "18446744073709551615"; result.members = new MobileCoreBridge.OrderedRegistration[] {member()}; MobileCoreBridge.nativeCompleteOrdered(id, result); return null; }
                    if ((Long)a[1] == 7) { heldOpen[0] = id; return null; }
                    check((Long)a[1] == -1 && ((long[])a[2])[0] == -1 && ((long[])a[5])[0] == -1 && ((long[])a[7])[0] == -1, "open preserves unsigned bits");
                    check(MobileCoreBridge.nativeIngestOrderedWake(-1, -1) == MobileCoreBridge.STATUS_ACCEPTED, "tagged early wake accepted");
                    MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult();
                    result.kind = "opened"; result.generationKind = "counter"; result.generationValue = "18446744073709551615";
                    result.members = new MobileCoreBridge.OrderedRegistration[] { member() };
                    check(MobileCoreBridge.nativeCompleteOrdered(id, result) == MobileCoreBridge.STATUS_ACCEPTED, "open completion");
                } else if (methodName.equals("orderedRequest")) {
                    String op = (String)a[1];
                    check(a[2].equals("counter") && a[3].equals("18446744073709551615"), "tagged generation");
                    if ((Long)a[4] == 7) {
                        if (op.equals("barrier")) {
                            long barrier = (Long)a[8];
                            check(barrier == (barriers[0]++ == 0 ? -2 : -1), "barrier order and full bits");
                            if (barrier == -1) heldBarrier[0] = id;
                            else MobileCoreBridge.nativeCompleteUnit(id);
                        } else if (op.equals("drain")) {
                            MobileCoreBridge.OrderedRecord record = new MobileCoreBridge.OrderedRecord();
                            MobileCoreBridge.OrderedRecord[] records;
                            if (closed[0]) { record.kind = "terminal"; record.reason = "overflow"; record.uncertainTail = true; record.rejectedKind = "barrier"; record.rejectedBarrierId = -2; records = new MobileCoreBridge.OrderedRecord[] {record}; }
                            else {
                                check(barriers[0] == 2, "both barriers staged");
                                record.kind = "value"; record.member = member(); record.ordinal = 1; record.value = new byte[] {0, (byte)128, (byte)255}; record.provenance = "notification"; record.delivery = "unknown";
                                MobileCoreBridge.OrderedRecord barrier = new MobileCoreBridge.OrderedRecord(); barrier.kind = "barrier"; barrier.barrierId = -2;
                                MobileCoreBridge.OrderedRecord after = new MobileCoreBridge.OrderedRecord(); after.kind = "value"; after.member = member(); after.ordinal = 2; after.value = new byte[] {7}; after.provenance = "notification"; after.delivery = "unknown";
                                MobileCoreBridge.OrderedRecord cancelled = new MobileCoreBridge.OrderedRecord(); cancelled.kind = "barrier"; cancelled.barrierId = -1; cancelled.uncertainTail = true;
                                records = new MobileCoreBridge.OrderedRecord[] {record, barrier, after, cancelled};
                            }
                            MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult();
                            result.kind = "records"; result.hasBatch = true; result.batch = closed[0] ? 10 : 9; result.records = records;
                            check(MobileCoreBridge.nativeCompleteOrdered(id, result) == MobileCoreBridge.STATUS_DELIVERED, "late-open drain");
                        } else if (op.equals("acknowledge-batch")) {
                            check((Long)a[8] == (closed[0] ? 10 : 9), "late-open exact ack");
                            MobileCoreBridge.nativeCompleteUnit(id);
                        } else if (op.equals("close")) { closed[0] = true; MobileCoreBridge.nativeCompleteUnit(id); }
                        else if (op.equals("remove") || op.equals("forget-removal-receipt")) MobileCoreBridge.nativeCompleteUnit(id);
                        else throw new AssertionError(op);
                        return null;
                    }
                    check((Long)a[4] == -1, "group bits");
                    if (op.equals("retain-claim")) {
                        check((Long)a[5] == -1 && (Long)a[7] == -1, "claim member and epoch bits");
                        if ((Long)a[6] == -2) MobileCoreBridge.nativeCompleteFailure(id, "path-stale", -1, "stale source member", false, null);
                        else {
                            check((Long)a[6] == -1, "claim source member bits");
                            MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult();
                            result.kind = "claim"; result.generationKind = "counter"; result.generationValue = "18446744073709551615"; result.member = member();
                            MobileCoreBridge.nativeCompleteOrdered(id, result);
                        }
                    } else if (op.equals("cancel-marker-write")) {
                        check((Long)a[8] == -1, "cancel marker unsigned identity");
                        MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult(); result.kind = "marker-write-status"; result.admission = "not-admitted"; result.writeState = "not-started";
                        MobileCoreBridge.nativeCompleteOrdered(id, result);
                    } else if (op.equals("marker-write-settlement")) {
                        MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult(); result.kind = "marker-write-status";
                        if (markerStates.getAndIncrement() == 0) { result.admission = "pending"; result.writeState = "not-started"; }
                        else if (markerStates.get() == 2) { result.admission = "admitted"; result.ordinal = -1; result.writeState = "initiating"; }
                        else { result.admission = "admitted"; result.ordinal = -1; result.writeState = "started"; result.hasFailure = true; result.failureKind = "platform"; result.gattStatus = MobileCoreBridge.ABSENT_INT; result.nativeName = "dispatch-error"; result.detail = "uncertain"; result.dispatched = true; }
                        MobileCoreBridge.nativeCompleteOrdered(id, result);
                    } else if (op.equals("drain")) {
                        drains.incrementAndGet();
                        MobileCoreBridge.OrderedRecord terminal = new MobileCoreBridge.OrderedRecord();
                        terminal.kind = "terminal"; terminal.reason = "source-failed"; terminal.uncertainTail = true;
                        terminal.failureKind = "permission-denied"; terminal.gattStatus = MobileCoreBridge.ABSENT_INT; terminal.detail = "Bluetooth permission denied while maintaining notification source"; terminal.dispatched = true; terminal.failureMember = member();
                        terminal.rejectedKind = "loss"; terminal.scope = "member"; terminal.scopeMember = -1;
                        terminal.unit = "source-records"; terminal.hasCount = true; terminal.count = -1;
                        MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult();
                        result.kind = "records"; result.hasBatch = true; result.batch = -1;
                        MobileCoreBridge.OrderedRecord marker = new MobileCoreBridge.OrderedRecord(); marker.kind = "marker"; marker.markerId = -1; marker.ordinal = Long.MIN_VALUE;
                        MobileCoreBridge.OrderedRecord rejectedMarker = new MobileCoreBridge.OrderedRecord(); rejectedMarker.kind = "terminal"; rejectedMarker.reason = "overflow"; rejectedMarker.rejectedKind = "marker"; rejectedMarker.rejectedMarkerId = -1;
                        MobileCoreBridge.OrderedRecord status = new MobileCoreBridge.OrderedRecord(); status.kind = "terminal"; status.reason = "source-failed"; status.uncertainTail = true; status.failureKind = "gatt-status"; status.gattStatus = 42; status.nativeDomain = "android"; status.nativeCode = "42"; status.detail = "Android notification source GATT failure"; status.dispatched = true; status.failureMember = member();
                        MobileCoreBridge.OrderedRecord later = new MobileCoreBridge.OrderedRecord();
                        later.kind = "subsequent-source-failure"; later.failureKind = "gatt-status"; later.gattStatus = 257; later.dispatched = true; later.detail = "Android notification source failure";
                        MobileCoreBridge.OrderedRecord omitted = new MobileCoreBridge.OrderedRecord(); omitted.kind = "failure-observations-omitted";
                        result.records = new MobileCoreBridge.OrderedRecord[] { marker, terminal, status, later, omitted, rejectedMarker };
                        check(MobileCoreBridge.nativeCompleteOrdered(id, result) == MobileCoreBridge.STATUS_ACCEPTED, "drain completion");
                    } else if (op.equals("acknowledge-batch")) {
                        check((Long)a[8] == -1 && drains.get() == 2, "exact ack follows replay");
                        MobileCoreBridge.nativeCompleteUnit(id);
                    } else if (op.equals("register") || op.equals("retire")) {
                        check((Long)a[5] == -1 && (Long)a[7] == -1, "register/retire member and epoch bits");
                        if ((Long)a[6] == -2) MobileCoreBridge.nativeCompleteFailure(id, "path-stale", -1, "stale registration", false, null);
                        else {
                            check((Long)a[6] == -1, "register/retire source bits");
                            if (op.equals("retire")) MobileCoreBridge.nativeCompleteUnit(id);
                            else { MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult(); result.kind = "registered"; result.member = member(); result.member.epoch = -2; MobileCoreBridge.nativeCompleteOrdered(id, result); }
                        }
                    } else throw new AssertionError(op);
                } else if (methodName.equals("cancel")) {
                    if (id == heldBarrier[0]) {
                        check(MobileCoreBridge.nativeCompleteUnit(id) == MobileCoreBridge.STATUS_LATE, "late barrier completion");
                        return null;
                    }
                    check(id == heldOpen[0], "cancel owns pending open");
                    MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult();
                    result.kind = "opened"; result.generationKind = "counter"; result.generationValue = "18446744073709551615"; result.members = new MobileCoreBridge.OrderedRegistration[] {member()};
                    check(MobileCoreBridge.nativeCompleteOrdered(id, result) == MobileCoreBridge.STATUS_LATE, "late open retained");
                } else if (methodName.equals("close")) MobileCoreBridge.nativeCompleteClosed(id, new String[0], new String[0], new long[0], new String[0], new long[0], new String[0]);
                else if (!methodName.equals("cancel")) throw new AssertionError(methodName);
                return null;
            });
        MobileCoreBridge.nativeInstallHost(radio, id -> {}, "android", "jni-test", "adapter");
        check(nativeExercise(radio) == 1, "ordered request/completion round trip; drains=" + drains.get());
        if (args.length == 1 && args[0].equals("--ordered-only")) {
            MobileCoreBridge.nativeShutdownHost();
            System.out.println("ordered JNI exchange: OK");
            System.exit(0);
        }
        String opened = MobileCoreBridge.nativeOpenSession("recording-owner", "ubm-mobile-wire/3", "jni-test");
        long session = Long.parseLong(opened.replaceAll(".*\"sessionId\":(\\d+).*", "$1"));
        String otherOpened = MobileCoreBridge.nativeOpenSession("recording-other", "ubm-mobile-wire/3", "jni-test");
        long other = Long.parseLong(otherOpened.replaceAll(".*\"sessionId\":(\\d+).*", "$1"));
        String reservation = receipt(session, "reserve", "[\"jni-receipt\",\"jni-test-caller\"]");
        check(reservation.contains("\"state\":\"reserved\""), "receipt reservation");
        String handle = reservation.replaceAll(".*\"handle\":\"([^\"]+)\".*", "$1");
        check(receipt(other, "status", handle).contains("\"ok\":false"), "foreign session denied");
        java.util.concurrent.CountDownLatch startDone = new java.util.concurrent.CountDownLatch(1);
        String[] startResult = { null };
        MobileCoreBridge.nativeContinuationStartOrdered(session, handle, "jni-receipt", "100", "10", "{}", 5000, value -> { startResult[0] = value; startDone.countDown(); });
        try { check(startDone.await(3, java.util.concurrent.TimeUnit.SECONDS), "invalid start callback"); }
        catch (InterruptedException error) { throw new AssertionError(error); }
        check(startResult[0] != null && startResult[0].contains("\"ok\":false"), "invalid declaration refused before start");
        check(receipt(session, "retire", handle).contains("\"state\":\"retired\""), "receipt retirement");
        check(receipt(session, "retire", handle).contains("\"state\":\"retired\""), "receipt retirement replay");
        MobileCoreBridge.nativeShutdownHost();
        System.out.println("ordered JNI exchange: OK");
        System.exit(0);
    }
}
