package com.ubm.core;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicReference;

/** Exercises the typed scan failure JNI entry against the actual binding host. */
public final class TestScanFailureEvidence {
    private static native void nativeTestSeedScanFailureRecoveryForBinding(long binding, String recordingId, String privateDirectory);
    private static native void nativeTestSetRecoveryHandleLimit(long binding, int limit);
    private static native int nativeTestRecoveryLeaseCount(long binding);
    private static native int nativeTestRecoveryHandleCount(long binding);
    private static native void nativeTestClearRecoveryCleanupAliases(long binding);
    public static void main(String[] args) throws Exception {
        try {
            run();
            System.exit(0);
        } catch (Throwable failure) {
            failure.printStackTrace();
            System.exit(1);
        }
    }

    private static void run() throws Exception {
        // The fused page entry must reject a forged binding before it can
        // resolve a handle or return bytes. This runs against the production
        // JNI cdylib, rather than a Kotlin/JS mock.
        try {
            MobileCoreBridge.nativeScanFailureRecoveryReadPageForBinding(-1, "forged-head", "0");
            throw new AssertionError("forged fused-page binding accepted");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        try {
            MobileCoreBridge.nativeScanFailureRecoveryMemberForBinding(-1, "forged-owner", 0);
            throw new AssertionError("forged member binding accepted");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        TestBindingRoute.Radio radio = new TestBindingRoute.Radio() {
            String retirement;
            @Override public String orderedAdmissionOwner(int version) {
                return super.orderedAdmissionOwner(version == 4 ? 3 : version);
            }
            @Override public void ownerFence(long request, String owner, String retirementId) {
                retirement = retirementId;
                MobileCoreBridge.OwnerRetirementResult result = new MobileCoreBridge.OwnerRetirementResult();
                result.ownerIncarnation = owner;
                result.retirementId = retirementId;
                result.state = "fenced-pending";
                result.outstandingClasses = new String[] {"cleanup"};
                result.failures = new MobileCoreBridge.OrderedResult[0];
                TestBindingRoute.check(MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(
                    binding, request, result) == MobileCoreBridge.STATUS_ACCEPTED,
                    "tagged owner retirement must reach its owning host");
            }
            @Override public void ownerStatus(long request, String owner, String retirementId) {
                TestBindingRoute.check(retirementId.equals(retirement), "retirement identity changed on retry");
                MobileCoreBridge.OwnerRetirementResult result = new MobileCoreBridge.OwnerRetirementResult();
                result.ownerIncarnation = owner;
                result.retirementId = retirementId;
                result.state = "quiescent";
                TestBindingRoute.check(MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(
                    binding + 1, request, result) != MobileCoreBridge.STATUS_ACCEPTED,
                    "foreign binding must not settle retirement");
                result.retirementId = "00000000-0000-4000-8000-000000000001";
                TestBindingRoute.check(MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(
                    binding, request, result) == MobileCoreBridge.STATUS_MISMATCHED,
                    "wrong retirement identity must not settle retirement");
                result.retirementId = retirementId;
                TestBindingRoute.check(MobileCoreBridge.nativeCompleteOwnerRetirementForBinding(
                    binding, request, result) == MobileCoreBridge.STATUS_ACCEPTED,
                    "status must reach its owning host");
            }
            @Override public void close(long request) {
                TestBindingRoute.check(MobileCoreBridge.nativeCompleteClosedForBinding(
                    binding, request, new String[0], new String[0], new long[0],
                    new String[0], new long[0], new String[0]) == MobileCoreBridge.STATUS_ACCEPTED,
                    "tagged close completion must reach its owning host");
            }
        };
        long binding = MobileCoreBridge.nativeInstallHostForBinding(
            1, radio, id -> {}, "android", "scan-evidence-test", "adapter");
        java.nio.file.Path recoveryDirectory = java.nio.file.Files.createTempDirectory("ubm-jni-scan-recovery-");
        String configured = MobileCoreBridge.nativeContinuationConfigureRecordingDirectoryForBinding(
            binding, recoveryDirectory.toString());
        TestBindingRoute.check(configured.contains("\"ok\":true"), "recovery storage configured: " + configured);
        nativeTestSeedScanFailureRecoveryForBinding(
            binding, "fixture-recovery", recoveryDirectory.toString());
        String capacityFirst = MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(binding, "fixture-recovery");
        nativeTestSetRecoveryHandleLimit(binding, 1);
        nativeTestSeedScanFailureRecoveryForBinding(
            binding, "fixture-capacity-rejected", recoveryDirectory.toString());
        int leasesBeforeRejectedAttach = nativeTestRecoveryLeaseCount(binding);
        try {
            MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(binding, "fixture-capacity-rejected");
            throw new AssertionError("capacity-rejected attach reached the core registry");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        TestBindingRoute.check(nativeTestRecoveryLeaseCount(binding) == leasesBeforeRejectedAttach,
            "rejected attach leaves no hidden registry lease");
        nativeTestSetRecoveryHandleLimit(binding, 1024);
        String attached = MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(binding, "fixture-recovery");
        TestBindingRoute.check(attached.equals(capacityFirst), "capacity probe preserves stable alias");
        String lease = attached.replaceAll(".*\"leaseId\":\"([^\"]+)\".*", "$1");
        TestBindingRoute.check(!lease.equals(attached), "opaque recovery lease issued");
        String owners = MobileCoreBridge.nativeScanFailureRecoveryOwnersForBinding(binding, lease);
        String owner = owners.replaceAll(".*\"ownerId\":\"([^\"]+)\".*", "$1");
        TestBindingRoute.check(!owner.equals(owners), "durable owner visible");
        // The ordinary fixture is deliberately prepared, so its opaque owner
        // must reject raw-source inspection. Use the separate captured-but-
        // unprepared fixture below for canonical raw values.
        try {
            MobileCoreBridge.nativeScanFailureRecoveryRawInfoForBinding(binding, owner);
            throw new AssertionError("prepared owner exposed raw source");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        nativeTestSeedScanFailureRecoveryForBinding(
            binding, "fixture-raw", recoveryDirectory.toString());
        String rawLease = MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(binding, "fixture-raw")
            .replaceAll(".*\"leaseId\":\"([^\"]+)\".*", "$1");
        String rawOwners = MobileCoreBridge.nativeScanFailureRecoveryOwnersForBinding(binding, rawLease);
        TestBindingRoute.check(rawOwners.contains("\"prepared\":false"),
            "raw fixture remains captured and unprepared: " + rawOwners);
        String rawOwner = rawOwners.replaceAll(".*\"ownerId\":\"([^\"]+)\".*", "$1");
        String rawInfo = MobileCoreBridge.nativeScanFailureRecoveryRawInfoForBinding(binding, rawOwner);
        TestBindingRoute.check(rawInfo.contains("\"memberCount\":1") && rawInfo.contains("\"tokenCount\":"),
            "raw owner info is returned through the binding-local owner: " + rawInfo);
        String rawMember = MobileCoreBridge.nativeScanFailureRecoveryRawMemberForBinding(binding, rawOwner, 0);
        TestBindingRoute.check(rawMember.contains("11111111111111111111111111111111") &&
            rawMember.contains("22222222222222222222222222222222"),
            "raw member metadata: " + rawMember);
        MobileCoreBridge.ScanFailureRecoveryRawToken rawToken =
            MobileCoreBridge.nativeScanFailureRecoveryRawTokenForBinding(binding, rawOwner, 0, "0", "65536");
        TestBindingRoute.check(rawToken.kind.equals("array") && rawToken.count.equals("3") && rawToken.bytes == null,
            "raw array token is atomically described without payload: " + rawToken.kind);
        MobileCoreBridge.ScanFailureRecoveryRawToken rawBytes =
            MobileCoreBridge.nativeScanFailureRecoveryRawTokenForBinding(binding, rawOwner, 1, "1", "64");
        TestBindingRoute.check(rawBytes.kind.equals("bytes") && rawBytes.total.equals("128") &&
            rawBytes.bytes.length == 64 && rawBytes.bytes[0] == 0x5a,
            "raw byte token preserves a bounded atomic window");
        try {
            MobileCoreBridge.nativeScanFailureRecoveryRawTokenForBinding(binding, rawOwner, 0, "0", "65537");
            throw new AssertionError("raw token accepted an unbounded byte request");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        for (int i = 0; i < 1100; i++) {
            String repeatedLease = MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(binding, "fixture-recovery");
            String repeatedOwners = MobileCoreBridge.nativeScanFailureRecoveryOwnersForBinding(binding, lease);
            TestBindingRoute.check(repeatedLease.equals(attached) && repeatedOwners.equals(owners),
                "recovery polling keeps stable bounded aliases");
        }
        String member = MobileCoreBridge.nativeScanFailureRecoveryMemberForBinding(binding, owner, 0);
        TestBindingRoute.check(member.contains("11111111111111111111111111111111") &&
            member.contains("22222222222222222222222222222222"), "member metadata: " + member);
        String headResult = MobileCoreBridge.nativeScanFailureRecoveryHeadForBinding(binding, owner, 0);
        String head = headResult.replaceAll(".*\"headId\":\"([^\"]+)\".*", "$1");
        TestBindingRoute.check(!head.equals(headResult), "durable head visible");
        TestBindingRoute.check(MobileCoreBridge.nativeScanFailureRecoveryHeadForBinding(binding, owner, 0)
            .equals(headResult), "head polling keeps stable alias");
        MobileCoreBridge.ScanFailureRecoveryPage page =
            MobileCoreBridge.nativeScanFailureRecoveryReadPageForBinding(binding, head, "0");
        TestBindingRoute.check(page.bytes.length > 0 && page.offset.equals("0") &&
            Long.parseLong(page.nextOffset) >= page.bytes.length, "atomic recovery page");
        String cursor = "0";
        boolean done = false;
        while (!done) {
            MobileCoreBridge.ScanFailureRecoveryPage drained =
                MobileCoreBridge.nativeScanFailureRecoveryReadPageForBinding(binding, head, cursor);
            TestBindingRoute.check(drained.offset.equals(cursor) && drained.bytes.length > 0,
                "drain page remains internally coherent");
            cursor = drained.nextOffset;
            done = drained.done;
        }
        MobileCoreBridge.nativeScanFailureRecoveryHeadControlForBinding(binding, head, "acknowledge");
        // Finalization replaces the live head with one cleanup alias. Saturate
        // that exact replacement budget to prove reservation happens before
        // the irreversible durable finalization.
        nativeTestSetRecoveryHandleLimit(binding, nativeTestRecoveryHandleCount(binding));
        CountDownLatch raceStart = new CountDownLatch(1);
        AtomicReference<MobileCoreBridge.ScanFailureRecoveryPage> racedPage = new AtomicReference<>();
        AtomicReference<Throwable> racedFailure = new AtomicReference<>();
        AtomicReference<String> racedFinalization = new AtomicReference<>();
        Thread reader = new Thread(() -> {
            try {
                raceStart.await();
                racedPage.set(MobileCoreBridge.nativeScanFailureRecoveryReadPageForBinding(binding, head, "0"));
            } catch (Throwable failure) { racedFailure.set(failure); }
        }, "ubm-recovery-page-reader");
        Thread controller = new Thread(() -> {
            try {
                raceStart.await();
                MobileCoreBridge.nativeScanFailureRecoveryHeadControlForBinding(binding, head, "retire");
                MobileCoreBridge.nativeScanFailureRecoveryHeadControlForBinding(binding, head, "release");
                racedFinalization.set(MobileCoreBridge.nativeScanFailureRecoveryHeadControlForBinding(
                    binding, head, "finalize"));
            } catch (Throwable failure) { racedFailure.compareAndSet(null, failure); }
        }, "ubm-recovery-page-controller");
        reader.start(); controller.start(); raceStart.countDown(); reader.join(); controller.join();
        TestBindingRoute.check(racedFinalization.get() != null,
            "concurrent finalization must return its cleanup receipt: " + racedFailure.get());
        if (racedPage.get() != null) {
            MobileCoreBridge.ScanFailureRecoveryPage raced = racedPage.get();
            TestBindingRoute.check(raced.offset.equals("0") && raced.bytes.length > 0 &&
                Long.parseLong(raced.nextOffset) >= raced.bytes.length,
                "raced page is whole, never metadata/payload split");
        } else {
            TestBindingRoute.check(racedFailure.get() instanceof MobileCoreBridge.MobileCoreException,
                "raced recovery refusal remains structured: " + racedFailure.get());
        }
        String finalized = racedFinalization.get();
        nativeTestSetRecoveryHandleLimit(binding, 1024);
        String cleanup = finalized.replaceAll(".*\\\"cleanupReceiptId\\\":\\\"([^\\\"]+)\\\".*", "$1");
        TestBindingRoute.check(!cleanup.equals(finalized), "finalized cleanup receipt issued");
        String pendingCleanup = MobileCoreBridge.nativeScanFailureRecoveryPendingCleanupForBinding(
            binding, lease);
        TestBindingRoute.check(pendingCleanup.contains(cleanup), "pending cleanup retains issued receipt");
        for (int i = 0; i < 1100; i++) {
            TestBindingRoute.check(MobileCoreBridge.nativeScanFailureRecoveryPendingCleanupForBinding(
                binding, lease).equals(pendingCleanup), "cleanup polling keeps a stable bounded alias");
        }
        MobileCoreBridge.nativeScanFailureRecoveryRetireCleanupForBinding(binding, cleanup);
        try {
            MobileCoreBridge.nativeScanFailureRecoveryRetireCleanupForBinding(binding, cleanup);
            throw new AssertionError("retired cleanup receipt remained live");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        String reattached = MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(binding, "fixture-recovery");
        String reattachedLease = reattached.replaceAll(".*\\\"leaseId\\\":\\\"([^\\\"]+)\\\".*", "$1");
        TestBindingRoute.check(!reattachedLease.equals(lease),
            "terminal cleanup reclaims the old lease alias instead of permanently exhausting handles");
        try {
            MobileCoreBridge.nativeScanFailureRecoveryMemberForBinding(binding, owner, 0);
            throw new AssertionError("terminal cleanup left the prior owner alias live");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        String reattachedOwners = MobileCoreBridge.nativeScanFailureRecoveryOwnersForBinding(
            binding, reattachedLease);
        TestBindingRoute.check(reattachedOwners.equals("{\"owners\":[]}"),
            "terminal cleanup exposes no stale durable owner through a reacquired lease: " + reattachedOwners);
        for (int i = 0; i < 1100; i++) {
            TestBindingRoute.check(MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(
                binding, "fixture-recovery").equals(reattached),
                "post-cleanup polling remains bounded after alias reclamation");
        }
        nativeTestSeedScanFailureRecoveryForBinding(
            binding, "fixture-cleanup-batch", recoveryDirectory.toString());
        String batchLease = MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(
            binding, "fixture-cleanup-batch").replaceAll(".*\\\"leaseId\\\":\\\"([^\\\"]+)\\\".*", "$1");
        String[] batchOwners = MobileCoreBridge.nativeScanFailureRecoveryOwnersForBinding(binding, batchLease)
            .split("\\\"ownerId\\\":\\\"");
        TestBindingRoute.check(batchOwners.length == 3, "two durable batch owners");
        TestBindingRoute.check(batchOwners[1].contains("\"producerFinished\":true")
            && batchOwners[2].contains("\"producerFinished\":true"),
            "each multi-owner fixture reservation is durably producer-finished");
        for (int i = 1; i < batchOwners.length; i++) {
            String ownerId = batchOwners[i].substring(0, batchOwners[i].indexOf('"'));
            MobileCoreBridge.nativeScanFailureRecoveryRawControlForBinding(binding, ownerId, "retire");
            MobileCoreBridge.nativeScanFailureRecoveryRawControlForBinding(binding, ownerId, "release");
            MobileCoreBridge.nativeScanFailureRecoveryRawControlForBinding(binding, ownerId, "finalize");
        }
        nativeTestClearRecoveryCleanupAliases(binding);
        int beforeBatch = nativeTestRecoveryHandleCount(binding);
        nativeTestSetRecoveryHandleLimit(binding, beforeBatch + 1);
        try {
            MobileCoreBridge.nativeScanFailureRecoveryPendingCleanupForBinding(binding, batchLease);
            throw new AssertionError("cleanup inventory partially minted aliases");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        TestBindingRoute.check(nativeTestRecoveryHandleCount(binding) == beforeBatch,
            "cleanup batch rejection is all-or-nothing");
        nativeTestSetRecoveryHandleLimit(binding, 1024);
        nativeTestSeedScanFailureRecoveryForBinding(
            binding, "fixture-selection-b", recoveryDirectory.toString());
        MobileCoreBridge.nativeScanFailureRecoveryRevokeForBinding(binding);
        try {
            MobileCoreBridge.nativeScanFailureRecoveryRawInfoForBinding(binding, rawOwner);
            throw new AssertionError("selection switch retained A raw authority");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        try {
            MobileCoreBridge.nativeScanFailureRecoveryHeadForBinding(binding, owner, 0);
            throw new AssertionError("selection switch retained A owner authority");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        String selectedBLease = MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(
            binding, "fixture-selection-b").replaceAll(".*\\\"leaseId\\\":\\\"([^\\\"]+)\\\".*", "$1");
        String selectedBOwners = MobileCoreBridge.nativeScanFailureRecoveryOwnersForBinding(binding, selectedBLease);
        String selectedBOwner = selectedBOwners.replaceAll(".*\\\"ownerId\\\":\\\"([^\\\"]+)\\\".*", "$1");
        TestBindingRoute.check(!selectedBOwner.equals(rawOwner), "B owner epoch differs from stale A");
        try { MobileCoreBridge.nativeScanFailureRecoveryRawInfoForBinding(binding, rawOwner); throw new AssertionError("B reused A raw authority"); } catch (MobileCoreBridge.MobileCoreException expected) { }
        try { MobileCoreBridge.nativeScanFailureRecoveryHeadForBinding(binding, owner, 0); throw new AssertionError("B reused A owner authority"); } catch (MobileCoreBridge.MobileCoreException expected) { }
        try { MobileCoreBridge.nativeScanFailureRecoveryRawTokenForBinding(binding, rawOwner, 1, "0", "64"); throw new AssertionError("B reused A raw token authority"); } catch (MobileCoreBridge.MobileCoreException expected) { }
        try { MobileCoreBridge.nativeScanFailureRecoveryRawControlForBinding(binding, rawOwner, "retire"); throw new AssertionError("B reused A raw control authority"); } catch (MobileCoreBridge.MobileCoreException expected) { }
        long session = TestBindingRoute.session(binding);
        TestBindingRoute.invoke(binding, session, "scan.start",
            "{\"admission\":1,\"serviceUuids\":[],\"duplicatePolicy\":\"all\",\"operationId\":\"scan\"}");
        String foreign = Long.toString(Long.parseLong(radio.scanIdentity) + 1);
        String refusal = MobileCoreBridge.nativeScanFailureIngestEvidenceForBinding(
            binding, "original detail", foreign, 6);
        TestBindingRoute.check(refusal.contains("\"status\":\"refused\"")
            && refusal.contains("\"closed\":false")
            && refusal.contains("original detail")
            && refusal.contains("androidScanErrorCode")
            && refusal.contains("[\"i64\",\"6\"]"), "typed refusal: " + refusal);
        String accepted = MobileCoreBridge.nativeScanFailureIngestEvidenceForBinding(
            binding, "accepted detail", radio.scanIdentity, 6);
        TestBindingRoute.check(accepted.contains("\"status\":\"accepted\""), "retry: " + accepted);
        try {
            MobileCoreBridge.nativeScanFailureIngestEvidenceForBinding(-1, "foreign", radio.scanIdentity, 6);
            throw new AssertionError("unknown binding accepted");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        TestBindingRoute.invoke(binding, session, "session.dispose", "{}");
        String shutdown = MobileCoreBridge.nativeShutdownHostForBinding(binding);
        TestBindingRoute.check(shutdown.contains("\"state\":\"release-failed\""), "pending owner must remain retained: " + shutdown);
        nativeTestSeedScanFailureRecoveryForBinding(
            binding, "fixture-teardown", recoveryDirectory.toString());
        String teardownLease = MobileCoreBridge.nativeScanFailureRecoveryAttachForBinding(binding, "fixture-teardown")
            .replaceAll(".*\\\"leaseId\\\":\\\"([^\\\"]+)\\\".*", "$1");
        String teardownOwner = MobileCoreBridge.nativeScanFailureRecoveryOwnersForBinding(binding, teardownLease)
            .replaceAll(".*\\\"ownerId\\\":\\\"([^\\\"]+)\\\".*", "$1");
        String teardownHead = MobileCoreBridge.nativeScanFailureRecoveryHeadForBinding(binding, teardownOwner, 0)
            .replaceAll(".*\\\"headId\\\":\\\"([^\\\"]+)\\\".*", "$1");
        CountDownLatch teardownStart = new CountDownLatch(1);
        AtomicReference<MobileCoreBridge.ScanFailureRecoveryPage> teardownPage = new AtomicReference<>();
        AtomicReference<Throwable> teardownFailure = new AtomicReference<>();
        AtomicReference<String> teardownShutdown = new AtomicReference<>();
        Thread teardownReader = new Thread(() -> {
            try {
                teardownStart.await();
                teardownPage.set(MobileCoreBridge.nativeScanFailureRecoveryReadPageForBinding(
                    binding, teardownHead, "0"));
            } catch (Throwable failure) { teardownFailure.set(failure); }
        }, "ubm-recovery-teardown-reader");
        Thread teardownCloser = new Thread(() -> {
            try {
                teardownStart.await();
                teardownShutdown.set(MobileCoreBridge.nativeShutdownHostForBinding(binding));
            } catch (Throwable failure) { teardownFailure.compareAndSet(null, failure); }
        }, "ubm-recovery-teardown-closer");
        teardownReader.start(); teardownCloser.start(); teardownStart.countDown();
        teardownReader.join(); teardownCloser.join();
        TestBindingRoute.check(teardownShutdown.get() != null
            && teardownShutdown.get().contains("\"state\":\"released\""),
            "concurrent teardown releases the binding: " + teardownShutdown.get());
        if (teardownPage.get() != null) {
            TestBindingRoute.check(teardownPage.get().offset.equals("0") && teardownPage.get().bytes.length > 0,
                "teardown race returns a whole page before revocation");
        } else {
            TestBindingRoute.check(teardownFailure.get() instanceof MobileCoreBridge.MobileCoreException,
                "teardown race rejects stale page through the typed boundary: " + teardownFailure.get());
        }
        shutdown = teardownShutdown.get();
        // Binding retirement revokes every opaque recovery handle. A late
        // fused-page call cannot observe a half-page or revive the host.
        try {
            MobileCoreBridge.nativeScanFailureRecoveryReadPageForBinding(binding, "stale-head", "0");
            throw new AssertionError("retired fused-page binding accepted");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        try {
            MobileCoreBridge.nativeScanFailureRecoveryMemberForBinding(binding, "stale-owner", 0);
            throw new AssertionError("retired member binding accepted");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        try {
            MobileCoreBridge.nativeScanFailureRecoveryRawInfoForBinding(binding, rawOwner);
            throw new AssertionError("retired binding exposed raw recovery info");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
        try {
            MobileCoreBridge.nativeScanFailureRecoveryRawTokenForBinding(
                binding, rawOwner, 1, "0", "64");
            throw new AssertionError("retired binding exposed a raw token window");
        } catch (MobileCoreBridge.MobileCoreException expected) { }
    }
}
