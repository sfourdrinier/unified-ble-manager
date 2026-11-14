package com.ubm.core;

/** Standalone JVM test run against the existing synthetic JNI round-trip library. */
public final class TestSlotWire {
    static { System.loadLibrary("ubm5_jni_echo"); }
    private static native int nativeVerify(TestSlotWire target, Object result, int expected);
    private int calls;
    public void ownerFence(long id, String owner, String retirement) { check(id == 10 && owner.equals(OWNER) && retirement.equals(RETIREMENT)); calls++; }
    public void ownerStatus(long id, String owner, String retirement) { check(id == 11 && owner.equals(OWNER) && retirement.equals(RETIREMENT)); calls++; }
    private static final String OWNER = "12345678-1234-4abc-8def-123456789abc";
    private static final String RETIREMENT = "22345678-1234-4abc-8def-123456789abc";
    private static void check(boolean value) { if (!value) throw new AssertionError(); }
    public static void main(String[] args) throws Exception {
        Class<?> dto = Class.forName("com.ubm.core.MobileCoreBridge$OwnerRetirementResult");
        MobileCoreBridge.RadioHost.class.getMethod("ownerFence", long.class, String.class, String.class);
        MobileCoreBridge.RadioHost.class.getMethod("ownerStatus", long.class, String.class, String.class);
        Object result = dto.getConstructor().newInstance();
        dto.getField("ownerIncarnation").set(result, OWNER);
        dto.getField("retirementId").set(result, RETIREMENT);
        dto.getField("state").set(result, "quiescent");
        TestSlotWire target = new TestSlotWire();
        check(nativeVerify(target, result, 10) == 1 && target.calls == 0);
        check(nativeVerify(target, result, 11) == 1 && target.calls == 0);
        check(nativeVerify(target, result, 12) == 1 && target.calls == 2);
        check(nativeVerify(target, result, 12) == 1 && target.calls == 4);
        dto.getField("state").set(result, "rejected");
        dto.getField("reason").set(result, "conflicting-retirement");
        check(nativeVerify(target, result, 12) == 1);
        MobileCoreBridge.OrderedResult failure = new MobileCoreBridge.OrderedResult();
        failure.failureKind = "busy"; failure.detail = "retained failure"; failure.gattStatus = 7;
        failure.nativeDomain = "native-domain"; failure.hasNativeCode = true; failure.nativeCode = -42;
        dto.getField("state").set(result, "fenced-pending");
        dto.getField("outstandingClasses").set(result, new String[]{"callbacks"});
        dto.getField("failures").set(result, new MobileCoreBridge.OrderedResult[]{failure});
        check(nativeVerify(target, result, 12) == 1);
        dto.getField("ownerIncarnation").set(result, "12345678-1234-4abc-8def-123456789abd");
        check(nativeVerify(target, result, 12) == 0);
        dto.getField("ownerIncarnation").set(result, OWNER);
        int status = (Integer) MobileCoreBridge.class.getMethod("nativeCompleteOwnerRetirement", long.class, dto).invoke(null, 10L, result);
        check(status == MobileCoreBridge.STATUS_NO_HOST);
        System.out.println("Android owner JNI wire passed");
    }
}
