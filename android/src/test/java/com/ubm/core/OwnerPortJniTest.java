package com.ubm.core;

import com.sfourdrinier.unifiedblemanager.rustcore.JniMobileCorePort;

/** Run with the synthetic JNI library and compiled Android port on the JVM classpath. */
public final class OwnerPortJniTest {
    public static void main(String[] args) {
        System.loadLibrary("ubm5_jni_echo");
        MobileCoreBridge.OwnerRetirementResult result = new MobileCoreBridge.OwnerRetirementResult();
        result.ownerIncarnation = "12345678-1234-4abc-8def-123456789abc";
        result.retirementId = "22345678-1234-4abc-8def-123456789abc";
        result.state = "fenced-pending";
        result.outstandingClasses = new String[]{"callbacks"};
        if (JniMobileCorePort.INSTANCE.completeOwnerRetirement(10, result) != MobileCoreBridge.STATUS_NO_HOST)
            throw new AssertionError("expected no host in the synthetic JNI process");
        System.out.println("Android owner port JNI forwarding passed");
    }
}
