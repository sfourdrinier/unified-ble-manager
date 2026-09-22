// android/src/main/java/com/ubm/echo/EchoBridge.java
//
// AAR-local JNI entry declarations for the UBM 5.0 core (`libubm5_jni_echo.so`).
//
// Ownership: native signatures are single-owned by the Rust cdylib
// (`bindings/jni/src/lib.rs`, `Java_com_ubm_*` exports) with the probe-side
// declarations at `bindings/jni/java/com/ubm/echo/EchoBridge.java`. This copy
// exists because the shipped AAR compiles only `android/src/main/java` and
// the Android build files are frozen: the JNI runtime links natives by
// fully-qualified class name, so the shipped provider needs these exact
// declarations on its own compile classpath. Any signature change must
// update both copies; `UbmAarJniParityTest` fails the build when they drift.

package com.ubm.echo;

/**
 * Sessions are native {@code CoreSession} values behind {@code long} handles.
 * Every method throws {@link EchoException} (typed {@code code}/{@code domain}/
 * {@code operation} plus the wire message) instead of returning error codes:
 * unimplemented or invalid paths reject loudly, never silently.
 *
 * <p>u64 counters cross as decimal strings ({@code BigInteger.toString()} in,
 * {@code new BigInteger(text)} out): lossless to 2^64-1.
 */
public final class EchoBridge {
    static {
        System.loadLibrary("ubm5_jni_echo");
    }

    private EchoBridge() {}

    public static native long nativeOpen(String revision);

    public static native byte[] nativeEchoBytes(long handle, byte[] input);

    public static native byte[] nativeEchoBytesChunked(long handle, byte[] input, int chunks);

    public static native String nativeEchoCounter(long handle, String decimal);

    public static native void nativeCancel(long handle);

    public static native void nativeClose(long handle);

    public static native String nativeRevision();

    public static native String nativeCentralStatus(long handle);

    public static native String nativeExpireSweep(long handle, String nowMs);

    public static native String nativeDestroy(long handle);

    public static native void nativeBleTransition(long handle, String transition);

    public static native String nativeStagedStep(long handle, String line);

    public static native String nativeStagedDrainLog(long handle);

    public static native String nativeStagedCounters(long handle);

    public static native String nativeBleScanStart(long handle, String owner, String timeoutMs, String nowMs);

    public static native String nativeBleScanTake(long handle);

    public static native String nativeBleScanStop(long handle, String opId, String nowMs);
}
