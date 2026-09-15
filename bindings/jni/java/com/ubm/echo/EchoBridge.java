package com.ubm.echo;

/**
 * Feasibility JNI bridge for the UBM 5.0 FFI slice (FFI-NATIVE card).
 *
 * <p>Sessions are native {@code EchoCore} values behind {@code long} handles.
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

    /** Test-only panic probe: must throw, never abort the VM. */
    public static native void nativePanicProbe(long handle);

    public static native String nativeRevision();
}
