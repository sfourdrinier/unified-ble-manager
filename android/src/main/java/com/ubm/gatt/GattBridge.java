// android/src/main/java/com/ubm/gatt/GattBridge.java
//
// AAR-local JNI entry declarations for the HOST-ANDROID GATT bridge slice
// (`libubm5_jni_echo.so`).
//
// Ownership: native signatures are single-owned by the Rust cdylib
// (`bindings/jni/src/lib.rs`, `Java_com_ubm_gatt_*` exports) with the
// probe-side declarations at `bindings/jni/java/com/ubm/gatt/GattBridge.java`.
// This copy exists because the shipped AAR compiles only
// `android/src/main/java` and the Android build files are frozen: the JNI
// runtime links natives by fully-qualified class name, so the shipped
// provider needs these exact declarations on its own compile classpath. Any
// signature change must update both copies; `UbmAarJniParityTest` fails the
// build when they drift.

package com.ubm.gatt;

/**
 * HOST-ANDROID GATT bridge (UBM 5.0).
 *
 * <p>Maps Android GATT callbacks to real Central transitions without ever
 * blocking a binder thread: callbacks call {@link #nativeEnqueueGattEvent}
 * (validate + store only) and return; a worker thread applies the queue with
 * {@link #nativeDrainGattEvents} and reads one JSON observation object per
 * queued line (newline-joined). Step-level core rejections arrive as data
 * ({@code {"ok":false,"code":...}} with frozen contract identities); only
 * the session lifetime throws {@code EchoException}.
 *
 * <p>Wire form is positional and pipe-delimited ({@code kind|arg|...}); no
 * argument may contain {@code '|'} (the drain enforces exact per-kind arity,
 * so extras reject {@code argument.invalid} as data). Byte values cross as
 * lowercase hex. Sessions are the same {@code long} handles owned by
 * {@code com.ubm.echo.EchoBridge}: the owner must enqueue {@code release}
 * and drain it before {@code nativeClose} (release-then-close). Supported
 * kinds:
 *
 * <ul>
 *   <li>scan: {@code scan.start|owner|timeoutMs|nowMs|uuidCsv|dupPolicy|mergePolicy},
 *       {@code scan.platform-started|op}, {@code scan.stop|op|nowMs},
 *       {@code scan.platform-event|op|event|nowMs}
 *   <li>peers/links: {@code peer.resolve|domain|value},
 *       {@code connect|peer|lease|timeoutMs|nowMs},
 *       {@code link.established|peer}, {@code link.released|peer},
 *       {@code disconnect|peer|lease|nowMs}, {@code peer.loss|peer|nowMs}
 *   <li>database: {@code discovery.begin|peer}, {@code discovery.complete|peer},
 *       {@code discovery.fail|peer}, {@code services-changed|peer},
 *       {@code path.register|peer|svc|svcOcc|char|charOcc|desc|descOcc|props|lease}
 *       ({@code -} for absent UUID/occurrence)
 *   <li>IO: {@code read.start|path|timeoutMs|nowMs},
 *       {@code write.start|path|mode|valueLen|maximum|modeSupported|timeoutMs|nowMs},
 *       {@code op.dispatch|op}, {@code op.settle|op|kind|valid|ordinal|nowMs},
 *       {@code op.cancel|op|nowMs}
 *   <li>notify: {@code subscribe|path|policy|itemCap|byteCap|consumer|timeoutMs|nowMs},
 *       {@code subscribe.enable-settled|path|success|nowMs},
 *       {@code unsubscribe|path|consumer|nowMs},
 *       {@code subscribe.disable-settled|path|nowMs},
 *       {@code notify.deliver|path|hex}
 *   <li>lifecycle: {@code expire-sweep|nowMs}, {@code adapter.reset|nowMs},
 *       {@code release} (real destroy transition; idempotent)
 * </ul>
 *
 * <p>Unknown kinds fail closed with
 * {@code capability.unsupported|capability}; oversize lines fail
 * {@code bytes.too-large}; a full queue (1024 lines) fails
 * {@code stream.quota}. Nothing unimplemented passes silently.
 */
public final class GattBridge {
    static {
        System.loadLibrary("ubm5_jni_echo");
    }

    private GattBridge() {}

    public static native int nativeEnqueueGattEvent(long handle, String wire);

    public static native String nativeDrainGattEvents(long handle);

    public static native int nativeGattQueueDepth(long handle);
}
