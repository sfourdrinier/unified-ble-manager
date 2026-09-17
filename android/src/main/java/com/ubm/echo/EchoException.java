// android/src/main/java/com/ubm/echo/EchoException.java
//
// AAR-local copy of the typed JNI failure (see EchoBridge.java header for
// ownership: signatures single-owned by `bindings/jni`; this copy ships in
// the AAR because the Android build files are frozen and the native
// `libubm5_jni_echo.so` throws this exact class by name — without it on the
// AAR classpath a native failure would escalate to NoClassDefFoundError
// instead of the typed `code|domain|operation` identity).
// `UbmAarJniParityTest` fails the build when the copies drift.

package com.ubm.echo;

/**
 * Typed failure carrying the frozen C-UBM {@code code} + {@code domain} +
 * {@code operation} identity.
 *
 * <p>Constructed from the shared {@code code|domain|operation|detail} wire
 * form (the message): fields are parsed views of the message, so the typed
 * identity and the wire literal can never disagree. Wire fields never
 * contain {@code '|'} (frozen vocabularies plus Rust-side details).
 */
public final class EchoException extends RuntimeException {
    private final String code;
    private final String domain;
    private final String operation;
    private final String detail;

    public EchoException(String wire) {
        super(wire);
        String[] parts = wire.split("\\|", -1);
        this.code = parts.length > 0 ? parts[0] : "";
        this.domain = parts.length > 1 ? parts[1] : "";
        this.operation = parts.length > 2 ? parts[2] : "";
        this.detail = parts.length > 3 ? parts[3] : "";
    }

    public String code() {
        return code;
    }

    public String domain() {
        return domain;
    }

    public String operation() {
        return operation;
    }

    public String detail() {
        return detail;
    }
}
