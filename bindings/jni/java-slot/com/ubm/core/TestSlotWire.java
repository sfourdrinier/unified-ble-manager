package com.ubm.core;

public final class TestSlotWire {
    static { System.loadLibrary("ubm5_jni_echo"); }
    private static native int nativeVerify(TestSlotWire target, Object result, int expected);
    public static final class OwnerResult {
        public String ownerIncarnation = "12345678-1234-4abc-8def-123456789abc";
        public String retirementId = "22345678-1234-4abc-8def-123456789abc";
        public String state = "quiescent";
        public String reason;
        public String[] outstandingClasses = new String[0];
        public MobileCoreBridge.OrderedResult[] failures = new MobileCoreBridge.OrderedResult[0];
    }
    private int retireCalls;
    public void ownerFence(long attempt, String owner, String retirement) {
        check(attempt == 10 && owner.equals("12345678-1234-4abc-8def-123456789abc"));
        check(retirement.equals("22345678-1234-4abc-8def-123456789abc"));
        retireCalls++;
    }
    public void ownerStatus(long attempt, String owner, String retirement) {
        check(attempt == 11 && owner.equals("12345678-1234-4abc-8def-123456789abc"));
        check(retirement.equals("22345678-1234-4abc-8def-123456789abc"));
        retireCalls++;
    }
    private static void check(boolean value) { if (!value) throw new AssertionError(); }
    private void verify(long attempt, MobileCoreBridge.OrderedOpenIdentity v) {
        check((attempt == 7 || attempt == 8) && v.version == 2 && v.slot == 3 && v.generation == -1L);
        check(v.original == 7 && v.group == -1L && v.owner.equals("12345678-1234-4abc-8def-123456789abc"));
        check(v.peer.equals("peer") && v.connection.equals("conn") && v.database.equals("db") && v.snapshot.equals("graph"));
        check(v.attachment == -1L && v.revision == 9 && v.itemCapacity == 4 && v.byteCapacity == 32);
        check(v.members.length == 1 && v.members[0] == -1L && v.peers[0].equals("peer"));
        check(v.serviceUuids[0].equals("service") && v.services[0] == -1L);
        check(v.characteristicUuids[0].equals("characteristic") && v.characteristics[0] == 9);
    }
    public void orderedOpenV2(long attempt, MobileCoreBridge.OrderedOpenIdentity identity) { verify(attempt, identity); }
    public void orderedAcknowledgeOpenV2(long attempt, MobileCoreBridge.OrderedOpenIdentity identity) { verify(attempt, identity); }
    public static void main(String[] args) {
        TestSlotWire target = new TestSlotWire();
        OwnerResult ownerResult = new OwnerResult();
        check(nativeVerify(target, ownerResult, 10) == 1 && target.retireCalls == 0);
        check(nativeVerify(target, ownerResult, 11) == 1 && target.retireCalls == 0);
        check(nativeVerify(target, ownerResult, 12) == 1 && target.retireCalls == 2);
        MobileCoreBridge.OrderedResult ownerFailure = new MobileCoreBridge.OrderedResult();
        ownerFailure.failureKind = "busy"; ownerFailure.detail = "retained failure";
        ownerFailure.nativeDomain = "native-domain"; ownerFailure.hasNativeCode = true;
        ownerFailure.nativeCode = -42; ownerFailure.gattStatus = 7; ownerFailure.dispatched = false;
        ownerResult.state = "fenced-pending";
        ownerResult.outstandingClasses = new String[]{"callbacks"};
        ownerResult.failures = new MobileCoreBridge.OrderedResult[]{ownerFailure};
        check(nativeVerify(target, ownerResult, 12) == 1);
        ownerResult.state = "rejected"; ownerResult.reason = "conflicting-retirement";
        check(nativeVerify(target, ownerResult, 12) == 1);
        ownerResult.state = "quiescent"; ownerResult.reason = null;
        ownerResult.outstandingClasses = new String[0];
        ownerResult.failures = new MobileCoreBridge.OrderedResult[0];
        ownerResult.ownerIncarnation = "12345678-1234-4abc-8def-123456789abd";
        check(nativeVerify(target, ownerResult, 12) == 0);
        MobileCoreBridge.OrderedResult result = new MobileCoreBridge.OrderedResult();
        result.kind = "open-result"; result.status = "pending"; result.ownerIncarnation = "12345678-1234-4abc-8def-123456789abc";
        result.slot = 3; result.slotGeneration = -1L;
        check(nativeVerify(target, result, 0) == 1);
        check(nativeVerify(target, result, 1) == 0);
        result.status = "failed"; result.ownershipKind = "created"; result.failureKind = "platform";
        result.detail = "original failure"; result.nativeDomain = "native-domain"; result.nativeCode = -42;
        result.hasNativeCode = true; result.gattStatus = Integer.MIN_VALUE; result.dispatched = true;
        result.generationKind = "counter"; result.generationValue = "8"; result.members = new MobileCoreBridge.OrderedRegistration[0];
        check(nativeVerify(target, result, 2) == 1);
        result.kind = "open-retired";
        check(nativeVerify(target, result, 1) == 1);
        result.slotGeneration = 2;
        check(nativeVerify(target, result, 1) == 0);
        result.slot = 64;
        check(nativeVerify(target, result, 1) == 0);
        System.out.println("v2 JNI slot wire passed");
    }
}
