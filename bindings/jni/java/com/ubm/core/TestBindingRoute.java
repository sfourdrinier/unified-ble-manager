package com.ubm.core;

import java.lang.ref.WeakReference;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.TimeUnit;

/** Exercises binding selection through the production JNI symbols on a JVM. */
public final class TestBindingRoute {
    static native int nativeTestOwnerDispatch(long binding);
    static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
    static class Radio extends TestMobile.Radio {
        long binding;
        volatile int releases;
        final BlockingQueue<Long> requests = new ArrayBlockingQueue<>(4);
        public boolean acceptBindingRoute(int version, long token) {
            if (version != 1) return false;
            binding = token;
            return true;
        }
        String scanIdentity;
        @Override public void startScan(long id, String[] services, String[] addresses, String mode, String callback, int legacy, long delay, String phy) {
            scanIdentity = Long.toString(id);
            check(advertisement(binding, "early-" + binding, scanIdentity) == 0, "tagged early callback accepted");
            check(MobileCoreBridge.nativeCompleteScanStartedV2ForBinding(binding, id, scanIdentity) == 0, "tagged start completed");
        }
        @Override public void stopScan(long id) { MobileCoreBridge.nativeCompleteUnitForBinding(binding, id); }
        @Override public void adapterState(long request) { requests.add(request); }
        @Override public void connect(long id, String peer, boolean auto, String[] phy, String generation) { MobileCoreBridge.nativeCompleteUnitForBinding(binding, id); }
        @Override public void disconnect(long id, String peer) { MobileCoreBridge.nativeCompleteUnitForBinding(binding, id); }
        @Override public void discover(long id, String peer) {
            MobileCoreBridge.nativeCompleteDiscoveredForBinding(binding, id, new int[] {0, 1, 2}, new String[] {"180d", "2a37", "2902"}, new long[] {0, 0, 0}, new int[] {0, 0x1c, 0});
        }
        @Override public void enableNotifications(long id, String p, String s, long so, String c, long co, long epoch, String req, String pref) { MobileCoreBridge.nativeCompleteNotifyEnabledForBinding(binding, id, "notification"); }
        @Override public void disableNotifications(long id, String p, String s, long so, String c, long co, long epoch) { MobileCoreBridge.nativeCompleteUnitForBinding(binding, id); }
        @Override public void acquireBackground(long id, String kind, String reason) {
            MobileCoreBridge.nativeCompleteLeaseForBinding(binding, id, "lease-" + binding);
        }
        @Override public void releaseBackground(long id, String lease) {
            if (++releases == 1) MobileCoreBridge.nativeCompleteFailureForBinding(binding, id, "platform", MobileCoreBridge.ABSENT_INT, "synthetic failure", true, null);
            else MobileCoreBridge.nativeCompleteUnitForBinding(binding, id);
        }
    }
    static class RetiringRadio extends Radio {
        int fences, statuses;
        String originalOwner, retirement;
        boolean reentrantRejected;
        @Override public String orderedAdmissionOwner(int version) {
            return super.orderedAdmissionOwner(version == 4 ? 3 : version);
        }
        @Override public void ownerFence(long request, String owner, String retirementId) {
            fences++;
            if (fences == 1) {
                originalOwner = owner;
                retirement = retirementId;
                try {
                    MobileCoreBridge.nativeInstallHostForBinding(1, this, id -> {}, "android", "reentrant", "adapter");
                    throw new AssertionError("reentrant reuse succeeded");
                } catch (MobileCoreBridge.MobileCoreException expected) { reentrantRejected = true; }
                // Native may have fenced, but the first response is lost.
                return;
            }
            check(owner.equals(originalOwner) && retirementId.equals(retirement), "retry must preserve exact owner and retirement identity");
            if (fences == 2) {
                MobileCoreBridge.OwnerRetirementResult stale = new MobileCoreBridge.OwnerRetirementResult();
                stale.ownerIncarnation = owner; stale.retirementId = retirementId; stale.state = "quiescent";
                check(MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(binding, request - 1, stale) == MobileCoreBridge.STATUS_LATE, "old attempt cannot complete retry");
                check(MobileCoreBridge.nativeCompleteFailureForBinding(binding, request, "platform", MobileCoreBridge.ABSENT_INT, "synthetic owner fence failure", true, null) == 0, "unpublished cleanup failure routed");
                return;
            }
            MobileCoreBridge.OwnerRetirementResult result = new MobileCoreBridge.OwnerRetirementResult();
            result.ownerIncarnation = fences == 3 ? "00000000-0000-4000-8000-000000000001" : owner;
            result.retirementId = retirementId;
            result.state = fences == 3 ? "quiescent" : "fenced-pending";
            if (fences == 4) result.reason = "wrong-owner";
            MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(binding, request, result);
        }
        @Override public void ownerStatus(long request, String owner, String retirementId) {
            check(owner.equals(originalOwner) && retirementId.equals(retirement), "status must preserve owner and retirement identity");
            statuses++;
            MobileCoreBridge.OwnerRetirementResult result = new MobileCoreBridge.OwnerRetirementResult();
            result.ownerIncarnation = owner;
            result.retirementId = retirementId;
            result.state = "quiescent";
            if (statuses == 1) result.outstandingClasses = new String[] {"callbacks"};
            MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(binding, request, result);
        }
    }
    static long failedInstallRetirement() throws Exception {
        RetiringRadio radio = new RetiringRadio();
        WeakReference<RetiringRadio> weak = new WeakReference<>(radio);
        try {
            MobileCoreBridge.nativeInstallHostForBinding(1, radio, id -> {}, "android", "", "adapter");
            throw new AssertionError("invalid host opened");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        long token = radio.binding;
        check(token > 0, "failed installation assigned token");
        check(MobileCoreBridge.nativeShutdownHostForBinding(token).contains("release-failed"), "lost fence reply retains debt");
        check(radio.fences == 1 && radio.reentrantRejected, "fence dispatched without holding installation lock");
        radio = null;
        for (int i = 0; i < 5; i++) { System.gc(); Thread.sleep(10); }
        radio = weak.get();
        check(radio != null, "failed cleanup retains original adapter");
        String failed = MobileCoreBridge.nativeShutdownHostForBinding(token);
        check(failed.contains("release-failed") && failed.contains("synthetic owner fence failure"), "failed fence remains retained and reports native detail");
        check(MobileCoreBridge.nativeShutdownHostForBinding(token).contains("release-failed"), "wrong owner cannot retire binding");
        check(MobileCoreBridge.nativeShutdownHostForBinding(token).contains("release-failed"), "contradictory pending rejected");
        check(radio.fences == 4 && radio.statuses == 0, "wrong-owner pending cannot establish a fence");
        check(MobileCoreBridge.nativeShutdownHostForBinding(token).contains("release-failed"), "fenced pending remains retained");
        check(MobileCoreBridge.nativeShutdownHostForBinding(token).contains("release-failed"), "contradictory quiescent receipt retains binding");
        check(MobileCoreBridge.nativeShutdownHostForBinding(token).contains("\"state\":\"released\""), "exact v4 receipt retires unpublished binding");
        check(radio.statuses == 2 && radio.fences == 5, "retry repeats fence until confirmed then queries status");
        check(MobileCoreBridge.nativeCompleteUnitForBinding(token, 1) == MobileCoreBridge.STATUS_NO_HOST, "retired route rejects stale callback");
        radio = null;
        for (int i = 0; i < 100 && weak.get() != null; i++) { System.gc(); Thread.sleep(10); }
        check(weak.get() == null, "retired adapter is no longer strongly retained");
        check(MobileCoreBridge.nativeShutdownHostForBinding(token).contains("\"state\":\"released\""), "lost terminal reply is replayed");
        return token;
    }
    static int advertisement(long binding, String peer, String identity) {
        return MobileCoreBridge.nativeIngestAdvertisementV2ForBinding(binding, peer, null, null, -58,
            MobileCoreBridge.ABSENT_INT, new String[0], new int[0], new byte[0][],
            new String[0], new byte[0][], -1, null, null, MobileCoreBridge.ABSENT_INT,
            null, null, -1L, identity);
    }
    static String invoke(long binding, long session, String op, String args) throws Exception {
        BlockingQueue<String> result = new ArrayBlockingQueue<>(1);
        MobileCoreBridge.nativeInvokeForBinding(binding, session, op, args, result::add);
        String reply = result.poll(5, TimeUnit.SECONDS);
        check(reply != null && reply.contains("\"ok\":true"), "tagged " + op + " succeeds: " + reply);
        return reply;
    }
    static long session(long binding) {
        String open = MobileCoreBridge.nativeOpenSessionForBinding(binding, "rn", "ubm-mobile-wire/3", "jvm-binding");
        return Long.parseLong(open.replaceAll(".*\\\"sessionId\\\":(\\d+).*", "$1"));
    }
    static void unknownCallbacks() throws Exception {
        int count = 0;
        for (java.lang.reflect.Method method : MobileCoreBridge.class.getDeclaredMethods()) {
            if (!(method.getName().startsWith("nativeComplete") || method.getName().startsWith("nativeIngest")) || !method.getName().endsWith("ForBinding") || method.getReturnType() != int.class) continue;
            Object[] values = new Object[method.getParameterCount()];
            Class<?>[] types = method.getParameterTypes();
            for (int i = 0; i < types.length; i++) {
                if (types[i] == long.class) values[i] = -1L;
                else if (types[i] == int.class) values[i] = 0;
                else if (types[i] == boolean.class) values[i] = false;
            }
            check(((Integer) method.invoke(null, values)) == MobileCoreBridge.STATUS_NO_HOST,
                method.getName() + " must reject unknown binding before decoding");
            count++;
        }
        check(count >= 35, "all tagged callback symbols are exercised: " + count);
    }
    static void unknownControls(long binding) throws Exception {
        int count = 0;
        for (java.lang.reflect.Method method : MobileCoreBridge.class.getDeclaredMethods()) {
            String name = method.getName();
            if (!name.endsWith("ForBinding") || !(name.startsWith("nativeContinuation") || name.startsWith("nativeOrderedRecording"))) continue;
            Object[] values = new Object[method.getParameterCount()];
            Class<?>[] types = method.getParameterTypes();
            for (int i = 0; i < types.length; i++) {
                if (types[i] == long.class) values[i] = i == 0 ? binding : 0L;
                else if (types[i] == int.class) values[i] = 0;
            }
            try {
                method.invoke(null, values);
                throw new AssertionError(name + " accepted retired binding");
            } catch (java.lang.reflect.InvocationTargetException expected) {
                check(expected.getCause() instanceof MobileCoreBridge.MobileCoreException, name + " must reject binding before decoding: " + expected.getCause());
            }
            count++;
        }
        check(count >= 18, "all tagged continuation controls are exercised: " + count);
    }
    public static void main(String[] args) throws Exception {
        int routeVersion;
        try {
            routeVersion = (Integer) MobileCoreBridge.class.getMethod("nativeBindingRouteVersion").invoke(null);
        } catch (NoSuchMethodException missing) {
            throw new AssertionError("binding route must be discoverable before installation", missing);
        }
        check(routeVersion == 1, "binding route negotiation advertises version 1");
        check(MobileCoreBridge.nativeWireRevision().equals("ubm-mobile-wire/3"), "v3 ABI changed");
        check(MobileCoreBridge.nativeCompleteUnitForBinding(999, 1) == MobileCoreBridge.STATUS_NO_HOST, "unknown binding");
        unknownCallbacks();
        Radio unsupported = new Radio();
        String[] installingStopAnswer = {null};
        Radio installingStop = new Radio() {
            @Override public String orderedAdmissionOwner(int version) {
                if (version == 4) {
                    installingStopAnswer[0] = MobileCoreBridge.nativeContinuationRecordingControl("stop", "capture", "", 0, 0);
                    throw new IllegalStateException("stop test completed before route retention");
                }
                return super.orderedAdmissionOwner(version);
            }
        };
        try { MobileCoreBridge.nativeInstallHostForBinding(1, installingStop, id -> {}, "android", "", "adapter"); }
        catch (RuntimeException expected) { }
        check(installingStopAnswer[0] != null && installingStopAnswer[0].contains("storage.owner-conflict"),
            "legacy stop is fenced during installation: " + installingStopAnswer[0]);
        try {
            MobileCoreBridge.nativeInstallHostForBinding(2, unsupported, id -> {}, "android", "unsupported", "adapter");
            throw new AssertionError("unsupported route version installed");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(unsupported.binding == 0, "unsupported version rejected before adapter callback");
        Radio mismatched = new Radio() {
            @Override public String orderedAdmissionOwner(int version) {
                return version == 4 ? "00000000-0000-4000-8000-000000000001" : super.orderedAdmissionOwner(version);
            }
        };
        try {
            MobileCoreBridge.nativeInstallHostForBinding(1, mismatched, id -> {}, "android", "mismatch", "adapter");
            throw new AssertionError("mismatched owner installed");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(mismatched.binding == 0 && MobileCoreBridge.nativeFindRetainedBinding(mismatched) == 0,
            "mismatched identities are refused before route disclosure");
        boolean[] admissionNegotiated = {false};
        boolean[] negotiatedBeforeRoute = {false};
        Radio refused = new Radio() {
            @Override public String orderedAdmissionOwner(int version) {
                String owner = super.orderedAdmissionOwner(version);
                if (version == 3 && !owner.isEmpty()) admissionNegotiated[0] = true;
                return owner;
            }
            @Override public boolean acceptBindingRoute(int version, long token) {
                negotiatedBeforeRoute[0] = admissionNegotiated[0];
                return false;
            }
        };
        try {
            MobileCoreBridge.nativeInstallHostForBinding(1, refused, id -> {}, "android", "refused", "adapter");
            throw new AssertionError("refused binding installed");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(negotiatedBeforeRoute[0], "exact admission identity negotiated before route acceptance even without v4 retirement");
        long refusedToken = MobileCoreBridge.nativeFindRetainedBinding(refused);
        check(refusedToken > 0 && refused.binding == 0, "unreturned install token discoverable by exact adapter");
        check(MobileCoreBridge.nativeCompleteUnitForBinding(refusedToken, 1) == MobileCoreBridge.STATUS_NO_HOST, "failed installation leaves no route");
        System.out.println("checking failed installation retirement");
        Radio throwing = new Radio() {
            @Override public String orderedAdmissionOwner(int version) { return super.orderedAdmissionOwner(version == 4 ? 3 : version); }
            @Override public boolean acceptBindingRoute(int version, long token) { throw new IllegalStateException("refused before saving token"); }
            @Override public void ownerFence(long request, String owner, String retirement) {
                MobileCoreBridge.OwnerRetirementResult receipt = new MobileCoreBridge.OwnerRetirementResult();
                receipt.ownerIncarnation = owner; receipt.retirementId = retirement; receipt.state = "quiescent";
                MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(binding, request, receipt);
            }
        };
        try { MobileCoreBridge.nativeInstallHostForBinding(1, throwing, id -> {}, "android", "throwing", "adapter"); throw new AssertionError("throwing install succeeded"); }
        catch (RuntimeException expected) { }
        throwing.binding = MobileCoreBridge.nativeFindRetainedBinding(throwing);
        check(throwing.binding > 0, "throwing adapter remains exactly discoverable");
        check(MobileCoreBridge.nativeShutdownHostForBinding(throwing.binding).contains("\"state\":\"released\""), "throwing adapter can retire by discovered token");
        long retiredToken = failedInstallRetirement();
        System.out.println("checking active binding routes");
        RetiringRadio installedV4 = new RetiringRadio();
        long v4 = MobileCoreBridge.nativeInstallHostForBinding(1, installedV4, id -> {}, "android", "v4", "adapter");
        check(nativeTestOwnerDispatch(v4) == 1 && installedV4.fences == 1, "installed v4 owner accepts retirement dispatch while retaining slot admission");
        Radio first = new Radio();
        BlockingQueue<Long> firstWakes = new ArrayBlockingQueue<>(16);
        BlockingQueue<Long> secondWakes = new ArrayBlockingQueue<>(16);
        long a = MobileCoreBridge.nativeInstallHostForBinding(1, first, firstWakes::add, "android", "first", "adapter");
        check(a > refusedToken && a == first.binding, "failed token is never reused");
        try {
            MobileCoreBridge.nativeInstallHostForBinding(1, first, id -> {}, "android", "duplicate", "adapter");
            throw new AssertionError("same adapter accepted twice");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(first.binding == a, "duplicate install cannot overwrite old binding");
        check(MobileCoreBridge.nativeShutdownHost().contains("release-failed"), "legacy shutdown cannot claim a tagged host was released");
        BlockingQueue<String> firstResult = new ArrayBlockingQueue<>(1);
        System.out.println("opening first session");
        long firstSession = session(a);
        MobileCoreBridge.nativeInvokeForBinding(a, firstSession, "adapter.state", "{}", firstResult::add);
        Long firstRequest = first.requests.poll(5, TimeUnit.SECONDS);
        check(firstRequest != null, "first request dispatched");
        BlockingQueue<String> lease = new ArrayBlockingQueue<>(1);
        MobileCoreBridge.nativeInvokeForBinding(a, firstSession, "background.acquire", "{\"kind\":\"connected-device\",\"reason\":\"route test\"}", lease::add);
        String acquired = lease.poll(5, TimeUnit.SECONDS);
        check(acquired != null && acquired.contains("lease-" + a), "old host acquired a cleanup obligation");
        Radio second = new Radio();
        long b = MobileCoreBridge.nativeInstallHostForBinding(1, second, secondWakes::add, "android", "second", "adapter");
        check(b > 0 && b != a && b == second.binding, "replacement gets distinct binding");
        check(!MobileCoreBridge.nativeHostInstalled(), "legacy host probe cannot select a tagged host");
        check(MobileCoreBridge.nativeShutdownHostForBinding(retiredToken).contains("\"state\":\"released\""), "retirement replay cannot touch later host");
        check(MobileCoreBridge.nativeCompleteUnitForBinding(retiredToken, 1) == MobileCoreBridge.STATUS_NO_HOST, "retired token cannot route to later host");
        unknownControls(retiredToken);
        unknownCallbacks();
        Radio alias = new Radio() {
            @Override public String orderedAdmissionOwner(int version) {
                return version == 4 || version == 3 ? first.orderedAdmissionOwner(3) : "";
            }
        };
        try {
            MobileCoreBridge.nativeInstallHostForBinding(1, alias, id -> {}, "android", "alias", "adapter");
            throw new AssertionError("same native owner admitted twice");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(alias.binding == 0, "owner alias rejected before token disclosure");
        Radio v3Alias = new Radio() {
            @Override public String orderedAdmissionOwner(int version) {
                return version == 3 ? first.orderedAdmissionOwner(3) : "";
            }
        };
        try {
            MobileCoreBridge.nativeInstallHostForBinding(1, v3Alias, id -> {}, "android", "alias-v3", "adapter");
            throw new AssertionError("v3-only owner alias admitted twice");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(v3Alias.binding == 0, "v3-only owner alias rejected before token disclosure");
        Radio failedOpen = new Radio();
        try {
            MobileCoreBridge.nativeInstallHostForBinding(1, failedOpen, id -> {}, "android", "", "adapter");
            throw new AssertionError("invalid host opened");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(failedOpen.binding > b, "failed host open consumed a fresh token");
        check(MobileCoreBridge.nativeCompleteUnitForBinding(failedOpen.binding, 1) == MobileCoreBridge.STATUS_NO_HOST, "failed open did not publish a route");
        long failedToken = failedOpen.binding;
        check(MobileCoreBridge.nativeShutdownHostForBinding(failedToken).contains("unpublished binding remains retained"), "failed installation reports retained cleanup ownership");
        try {
            MobileCoreBridge.nativeInstallHostForBinding(1, failedOpen, id -> {}, "android", "reuse", "adapter");
            throw new AssertionError("unretired failed-install adapter reused");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(failedOpen.binding == failedToken, "failed-install owner identity remains retained");
        BlockingQueue<String> secondResult = new ArrayBlockingQueue<>(1);
        System.out.println("opening overlapping session");
        long secondSession = session(b);
        check(firstSession == secondSession, "per-host session IDs overlap");
        for (String name : new String[] {"nativeContinuationConfigureRecordingDirectoryForBinding", "nativeContinuationDescribeBacklogForBinding", "nativeContinuationExecuteForBinding", "nativeContinuationPrepareClaimVersionedForBinding", "nativeContinuationAcknowledgeClaimV1ForBinding", "nativeContinuationRetireClaimReceiptForBinding", "nativeOrderedRecordingReceiptControlForBinding", "nativeContinuationStartOrderedForBinding", "nativeContinuationStopOrderedForBinding", "nativeContinuationOrderedContextForBinding"}) {
            boolean found = false;
            for (java.lang.reflect.Method method : MobileCoreBridge.class.getDeclaredMethods()) {
                if (method.getName().equals(name)) found = true;
            }
            check(found, "shipped facade exposes " + name);
        }
        BlockingQueue<String> controls = new ArrayBlockingQueue<>(1);
        MobileCoreBridge.nativeOrderedRecordingReceiptControlForBinding(a, firstSession, "reserve", "[\"binding-receipt\",\"caller\"]", controls::add);
        String reservation = controls.poll(5, TimeUnit.SECONDS);
        check(reservation != null && reservation.contains("\"state\":\"reserved\""), "first host reserves receipt: " + reservation);
        String handle = reservation.replaceAll(".*\\\"handle\\\":\\\"([^\\\"]+)\\\".*", "$1");
        check(!handle.equals(reservation), "receipt handle present");
        MobileCoreBridge.nativeOrderedRecordingReceiptControlForBinding(b, secondSession, "status", handle, controls::add);
        String foreign = controls.poll(5, TimeUnit.SECONDS);
        check(foreign != null && foreign.contains("\"ok\":false"), "foreign host cannot read receipt: " + foreign);
        MobileCoreBridge.nativeOrderedRecordingReceiptControlForBinding(a, firstSession, "retire", handle, controls::add);
        String retirement = controls.poll(5, TimeUnit.SECONDS);
        check(retirement != null && retirement.contains("\"state\":\"retired\""), "owner retires receipt: " + retirement);
        MobileCoreBridge.nativeOrderedRecordingReceiptControlForBinding(a, firstSession, "retire", handle, controls::add);
        check(retirement.equals(controls.poll(5, TimeUnit.SECONDS)), "receipt retirement replay preserves identity");
        MobileCoreBridge.nativeContinuationDescribeBacklogForBinding(a, controls::add);
        check(controls.poll(5, TimeUnit.SECONDS) != null, "continuation backlog reaches exact host");
        MobileCoreBridge.nativeContinuationPrepareClaimVersionedForBinding(b, 99, 10, 1024, controls::add);
        String unsupportedClaim = controls.poll(5, TimeUnit.SECONDS);
        check(unsupportedClaim != null && unsupportedClaim.contains("capability.unsupported"), "unsupported claim version is rejected: " + unsupportedClaim);
        MobileCoreBridge.nativeContinuationPrepareClaimVersionedForBinding(b, 1, 10, 1024, controls::add);
        String claim = controls.poll(5, TimeUnit.SECONDS);
        check(claim != null && claim.contains("\"ok\":true"), "versioned claim reaches second host: " + claim);
        MobileCoreBridge.nativeContinuationRetireClaimReceiptForBinding(b, "foreign-token", controls::add);
        String noReceipt = controls.poll(5, TimeUnit.SECONDS);
        check(noReceipt != null && noReceipt.contains("\"ok\":false"), "foreign claim receipt cannot be retired: " + noReceipt);
        String declaration = "{\"onAppearance\":\"native\",\"peerId\":\"AA:BB:CC:DD:EE:FF\",\"resubscribe\":[{\"serviceUuid\":\"0000180d-0000-1000-8000-00805f9b34fb\",\"serviceOccurrence\":1,\"characteristicUuid\":\"00002a37-0000-1000-8000-00805f9b34fb\",\"characteristicOccurrence\":1}]}";
        for (long binding : new long[] {a, b}) {
            check(MobileCoreBridge.nativeContinuationSeedDeclarationForBinding(binding, declaration).contains("seeded"), "seed exact host declaration");
            MobileCoreBridge.nativeContinuationExecuteForBinding(binding, "AA:BB:CC:DD:EE:FF", declaration, controls::add);
            String result = controls.poll(5, TimeUnit.SECONDS);
            check(result != null && result.contains("continuation.completed"), "continuation executes through exact host: " + result);
        }
        MobileCoreBridge.nativeContinuationPrepareClaimVersionedForBinding(a, 1, 10, 1024, controls::add);
        String firstClaim = controls.poll(5, TimeUnit.SECONDS);
        check(firstClaim != null && firstClaim.contains("claimToken"), "first host produces retained token: " + firstClaim);
        String firstToken = firstClaim.replaceAll(".*\"claimToken\":\"([^\"]+)\".*", "$1");
        MobileCoreBridge.nativeContinuationAcknowledgeClaimV1ForBinding(b, firstToken, controls::add);
        String wrongAck = controls.poll(5, TimeUnit.SECONDS);
        check(wrongAck != null && wrongAck.contains("\"ok\":false"), "foreign host rejects claim token: " + wrongAck);
        MobileCoreBridge.nativeContinuationAcknowledgeClaimV1ForBinding(a, firstToken, controls::add);
        String rightAck = controls.poll(5, TimeUnit.SECONDS);
        check(rightAck != null && rightAck.contains("\"ok\":true"), "owning host acknowledges claim: " + rightAck);
        MobileCoreBridge.nativeContinuationAcknowledgeClaimV1ForBinding(a, firstToken, controls::add);
        check(rightAck.equals(controls.poll(5, TimeUnit.SECONDS)), "lost claim reply is replayed by owner");
        MobileCoreBridge.nativeContinuationRetireClaimReceiptForBinding(b, firstToken, controls::add);
        check(controls.poll(5, TimeUnit.SECONDS).contains("\"ok\":false"), "foreign host cannot retire claim receipt");
        MobileCoreBridge.nativeContinuationRetireClaimReceiptForBinding(a, firstToken, controls::add);
        check(controls.poll(5, TimeUnit.SECONDS).contains("\"ok\":true"), "owning host retires claim receipt");
        MobileCoreBridge.nativeContinuationPrepareClaimVersionedForBinding(b, 1, 10, 1024, controls::add);
        String secondClaim = controls.poll(5, TimeUnit.SECONDS);
        check(secondClaim != null && secondClaim.contains("claimToken"), "second host retains its own claim: " + secondClaim);
        String secondToken = secondClaim.replaceAll(".*\"claimToken\":\"([^\"]+)\".*", "$1");
        MobileCoreBridge.nativeContinuationAcknowledgeClaimV1ForBinding(b, secondToken, controls::add);
        check(controls.poll(5, TimeUnit.SECONDS).contains("\"ok\":true"), "second host releases its claim");
        MobileCoreBridge.nativeContinuationRetireClaimReceiptForBinding(b, secondToken, controls::add);
        check(controls.poll(5, TimeUnit.SECONDS).contains("\"ok\":true"), "second host retires its own receipt");
        try { MobileCoreBridge.nativeContinuationDescribeBacklogForBinding(retiredToken, controls::add); throw new AssertionError("retired continuation accepted"); }
        catch (MobileCoreBridge.MobileCoreException expected) { }
        try { MobileCoreBridge.nativeOrderedRecordingReceiptControlForBinding(retiredToken, firstSession, "status", handle, controls::add); throw new AssertionError("retired receipt accepted"); }
        catch (MobileCoreBridge.MobileCoreException expected) { }
        try {
            MobileCoreBridge.nativeContinuationDescribeBacklog(ignored -> {});
            throw new AssertionError("untagged continuation addressed a tagged host");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        try {
            MobileCoreBridge.nativeContinuationRecordingControl("prepare", "capture", "", 1, 1);
            throw new AssertionError("untagged recording control addressed a tagged host");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(MobileCoreBridge.nativeContinuationRecordingControl("stop", "capture", "", 0, 0)
            .contains("storage.owner-conflict"), "legacy stop cannot bypass retained tagged hosts");
        try {
            MobileCoreBridge.nativeNegotiateScanEndVersionQualifiedForBinding(a, firstSession, "[2]");
            throw new AssertionError("unqualified v2 selected");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        check(MobileCoreBridge.nativeNegotiateScanEndVersionQualifiedForBinding(a, firstSession, "[1,2]").contains("\"version\":1"), "first host selects qualified v1");
        check(MobileCoreBridge.nativeNegotiateScanEndVersionForBinding(b, secondSession, "[1,2]").contains("\"version\":2"), "second host negotiates v2");
        try {
            MobileCoreBridge.nativeNegotiateScanEndVersionQualifiedForBinding(b, secondSession, "[1,2]");
            throw new AssertionError("v2 session silently downgraded");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        try {
            MobileCoreBridge.nativeNegotiateScanEndVersion(firstSession, "[1]");
            throw new AssertionError("legacy selection addressed tagged session");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        String firstScan = invoke(a, firstSession, "scan.start", "{\"admission\":1,\"serviceUuids\":[],\"duplicatePolicy\":\"all\",\"operationId\":\"first\"}");
        String secondScan = invoke(b, secondSession, "scan.start", "{\"admission\":1,\"serviceUuids\":[],\"duplicatePolicy\":\"all\",\"operationId\":\"second\"}");
        check(!first.scanIdentity.equals(second.scanIdentity), "physical request identities differ across hosts");
        check(advertisement(b, "foreign", first.scanIdentity) == 1, "foreign identity cannot enter replacement binding");
        check(MobileCoreBridge.nativeIngestScanFailedV2ForBinding(b, "foreign failure", first.scanIdentity) == 3, "foreign failure cannot terminate replacement binding");
        check(advertisement(b, "own", second.scanIdentity) == 0, "own scan accepted");
        String scanDrain = "";
        long until = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!scanDrain.contains("own") && System.nanoTime() < until) {
            Thread.sleep(10);
            scanDrain += MobileCoreBridge.nativeDrainForBinding(b, secondSession, 128, 65536);
        }
        check(scanDrain.contains("early-" + b) && scanDrain.contains("own") && !scanDrain.contains("foreign\"") && !scanDrain.contains("scan-end"), "tagged ingress preserves early events and rejects foreign identity: " + scanDrain);
        invoke(a, firstSession, "scan.stop", "{\"operationId\":\"" + TestMobile.operationId(firstScan) + "\"}");
        check(MobileCoreBridge.nativeIngestScanFailedV3ForBinding(b, "second platform failure", second.scanIdentity, 6) == 0, "second host failure accepted");
        String terminal = "";
        long terminalDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!terminal.contains("scan-end") && System.nanoTime() < terminalDeadline) {
            Thread.sleep(10);
            terminal += MobileCoreBridge.nativeDrainForBinding(b, secondSession, 128, 65536);
        }
        check(terminal.contains("\"version\":2") && terminal.contains("second platform failure") && terminal.contains("\"androidScanErrorCode\":6") && terminal.contains("\"startOperationId\":\"second\"") && terminal.contains("\"generation\":\"" + second.scanIdentity + "\""), "v2 cause and source identity cross JNI: " + terminal);
        MobileCoreBridge.nativeDrainForBinding(a, firstSession, 128, 65536);

        try { MobileCoreBridge.nativeInvoke(firstSession, "adapter.state", "{}", ignored -> {}); throw new AssertionError("untagged invoke accepted tagged session"); }
        catch (MobileCoreBridge.MobileCoreException expected) { }
        try { MobileCoreBridge.nativeReleaseBackgroundScope("jvm-binding"); throw new AssertionError("untagged scope accepted tagged host"); }
        catch (MobileCoreBridge.MobileCoreException expected) { }

        MobileCoreBridge.nativeInvokeForBinding(a, firstSession, "adapter.state", "{}", ignored -> {});
        Long overlappingRequest = first.requests.poll(5, TimeUnit.SECONDS);
        check(overlappingRequest != null, "old session invokes old host after replacement");
        check(MobileCoreBridge.nativeCompleteAdapterForBinding(a, overlappingRequest, "available", "granted", "on", null) == 0, "old overlapping request completed");
        firstWakes.clear();
        check(MobileCoreBridge.nativeIngestAdapterStateForBinding(a, "available", "granted", "on", null) == 0, "old host receives unique event");
        check(firstWakes.poll(5, TimeUnit.SECONDS) != null, "old session has a drainable event");
        String oldDrain = MobileCoreBridge.nativeDrainForBinding(a, firstSession, 64, 8192);
        String newDrain = MobileCoreBridge.nativeDrainForBinding(b, secondSession, 64, 8192);
        check(oldDrain.contains("\"power\":\"on\"") && !newDrain.contains("\"power\":\"on\""), "drain selects the old host event: " + oldDrain + " / " + newDrain);
        BlockingQueue<String> secondLease = new ArrayBlockingQueue<>(1);
        MobileCoreBridge.nativeInvokeForBinding(b, secondSession, "background.acquire", "{\"kind\":\"connected-device\",\"reason\":\"scope test\"}", secondLease::add);
        check(secondLease.poll(5, TimeUnit.SECONDS) != null, "replacement acquired lease");
        check(MobileCoreBridge.nativeReleaseBackgroundScopeForBinding(b, "jvm-binding").contains("release-failed"), "replacement scope reports its release failure");
        check(second.releases == 1 && first.releases == 0, "scope release uses exact owner despite equal scope names");
        check(MobileCoreBridge.nativeReleaseBackgroundScopeForBinding(b, "jvm-binding").contains("released"), "replacement scope retries its cleanup");

        MobileCoreBridge.nativeInvokeForBinding(b, secondSession, "adapter.state", "{}", secondResult::add);
        Long secondRequest = second.requests.poll(5, TimeUnit.SECONDS);
        check(secondRequest != null, "second request dispatched");
        check(MobileCoreBridge.nativeCompleteAdapterForBinding(b, firstRequest, "available", "granted", "on", null) == MobileCoreBridge.STATUS_LATE, "old request cannot complete current host");
        check(firstResult.poll(100, TimeUnit.MILLISECONDS) == null && secondResult.poll(100, TimeUnit.MILLISECONDS) == null, "wrong route did not complete either request");
        check(MobileCoreBridge.nativeCompleteAdapterForBinding(a, firstRequest, "available", "granted", "on", null) == 0, "old completion reaches retained host");
        check(firstResult.poll(5, TimeUnit.SECONDS) != null, "old completion delivered");
        check(MobileCoreBridge.nativeCompleteAdapterForBinding(b, secondRequest, "available", "granted", "on", null) == 0, "new completion reaches current host");
        check(secondResult.poll(5, TimeUnit.SECONDS) != null, "new completion delivered");
        check(MobileCoreBridge.nativeIngestOrderedWakeForBinding(a, 1, 99) != MobileCoreBridge.STATUS_NO_HOST, "old wake remains routed");
        firstWakes.clear();
        secondWakes.clear();
        check(MobileCoreBridge.nativeIngestAdapterStateForBinding(a, "available", "granted", "on", null) == 0, "old ingress accepted");
        Long wake = firstWakes.poll(5, TimeUnit.SECONDS);
        check(wake != null && wake == firstSession, "old ingress wakes old session");
        check(secondWakes.poll(150, TimeUnit.MILLISECONDS) == null, "old ingress does not wake replacement");
        check(MobileCoreBridge.nativeShutdownHostForBinding(999).contains("release-failed"), "unknown shutdown rejected");
        System.out.println("checking cleanup");
        check(MobileCoreBridge.nativeShutdownHostForBinding(a).contains("release-failed"), "failed cleanup retains old binding");
        check(first.releases == 1, "first cleanup attempt reached old adapter");
        check(MobileCoreBridge.nativeShutdownHostForBinding(a).contains("release-failed"), "unproved native retirement remains retained after retry");
        check(first.releases == 2, "cleanup retry reached old adapter and completed");
        check(MobileCoreBridge.nativeCompleteUnitForBinding(a, firstRequest) == MobileCoreBridge.STATUS_LATE, "old callbacks still routed");
        check(MobileCoreBridge.nativeCompleteUnitForBinding(b, secondRequest) == MobileCoreBridge.STATUS_LATE, "stale shutdown leaves replacement intact");
        int admitted = 0;
        for (int i = 0; i < 40; i++) {
            Radio debt = new Radio() {
                @Override public boolean acceptBindingRoute(int version, long token) { binding = token; return false; }
            };
            String failure = "";
            try { MobileCoreBridge.nativeInstallHostForBinding(1, debt, id -> {}, "android", "debt", "adapter"); }
            catch (MobileCoreBridge.MobileCoreException expected) { failure = expected.getMessage(); }
            if (debt.binding == 0) {
                check(failure.contains("retained binding limit"), "saturation reports the reason");
                check(admitted >= 20 && admitted < 32, "retained owner admission has a finite bound");
                check(MobileCoreBridge.nativeFindRetainedBinding(debt) == 0, "saturation refuses before retaining adapter");
                break;
            }
            check(MobileCoreBridge.nativeFindRetainedBinding(debt) == debt.binding, "admitted debt remains discoverable");
            admitted++;
        }
        check(admitted < 32, "saturation reached");
        System.out.println("binding JNI exchange: OK");
        System.exit(0);
    }
}
