package com.ubm.core;

import java.nio.file.Files;
import java.nio.file.Path;

/**
 * JVM regression for terminal-evidence recovery through the actual JNI
 * foreign boundary. Fixtures are staged in the host-selected temporary
 * SQLite ledger; every recovery operation below goes through the shipped
 * MobileCoreBridge JNI surface and opaque binding-local aliases.
 */
public final class TestTerminalEvidenceRecovery {
    private TestTerminalEvidenceRecovery() {}

    static native void nativeTestSeedTerminalEvidenceForBinding(
        long binding, String producer, String managerId, String operationId,
        String incarnation, byte[] bytes, boolean committed);

    static final class Radio extends TestMobile.Radio {
        long binding;

        @Override public boolean acceptBindingRoute(int version, long token) {
            if (version != 1) return false;
            binding = token;
            return true;
        }
    }

    static void check(boolean value, String message) {
        if (!value) throw new AssertionError(message);
        System.out.println("ok: " + message);
    }

    static String field(String json, String name) {
        java.util.regex.Matcher match = java.util.regex.Pattern
            .compile("\\\"" + java.util.regex.Pattern.quote(name) + "\\\":\\\"([^\\\"]+)\\\"")
            .matcher(json);
        if (!match.find()) throw new AssertionError("missing " + name + ": " + json);
        return match.group(1);
    }

    static void expectInvalid(Runnable call, String message) {
        try {
            call.run();
            throw new AssertionError(message + " was accepted");
        } catch (MobileCoreBridge.MobileCoreException expected) {
            check(expected.code.equals("argument.invalid"), message + " is rejected");
        }
    }

    static String handleForOperation(String json, String operationId) {
        java.util.regex.Matcher match = java.util.regex.Pattern.compile(
            "\\\"handle\\\":\\\"([^\\\"]+)\\\"[^}]*\\\"operationId\\\":\\\""
                + java.util.regex.Pattern.quote(operationId) + "\\\"").matcher(json);
        if (!match.find()) throw new AssertionError("missing handle for " + operationId + ": " + json);
        return match.group(1);
    }

    public static void main(String[] args) throws Exception {
        Path directory = Files.createTempDirectory("ubm-jni-terminal-evidence-");
        check(MobileCoreBridge.nativeContinuationConfigureRecordingDirectory(directory.toString()).contains("\"ok\":true"),
            "temporary SQLite recording directory configured before host install");
        MobileCoreBridge.nativeConfigureTerminalEvidenceMaximumBytes(4096);

        Radio radio = new Radio();
        long binding = MobileCoreBridge.nativeInstallHostForBinding(
            1, radio, ignored -> {}, "android", "terminal-evidence-jni", "jvm");
        check(binding == radio.binding && binding > 0, "tagged JNI host installed");

        nativeTestSeedTerminalEvidenceForBinding(binding, "scan", "manager", "staged", "1", new byte[] {1, 2, 3, 4}, false);
        nativeTestSeedTerminalEvidenceForBinding(binding, "subscription", "manager", "committed", "1", new byte[] {'C', 'D', 'E', 'F'}, true);

        String firstLease = field(MobileCoreBridge.nativeTerminalEvidenceRecoveryOpenSelectedForBinding(binding), "leaseId");
        String limited = MobileCoreBridge.nativeTerminalEvidenceRecoveryListForBinding(binding, firstLease, 1, 4096);
        check(limited.contains("\"more\":true") && limited.contains("\"items\":[{"),
            "max-items page is bounded and reports more through JNI");

        String lease = field(MobileCoreBridge.nativeTerminalEvidenceRecoveryOpenSelectedForBinding(binding), "leaseId");
        expectInvalid(() -> MobileCoreBridge.nativeTerminalEvidenceRecoveryListForBinding(binding, firstLease, 8, 4096),
            "stale recovery lease after a new selection");
        expectInvalid(() -> MobileCoreBridge.nativeTerminalEvidenceRecoveryListForBinding(binding, "forged-lease", 8, 4096),
            "forged recovery lease");

        String page = MobileCoreBridge.nativeTerminalEvidenceRecoveryListForBinding(binding, lease, 8, 4096);
        check(page.contains("\"state\":\"staged\"") && page.contains("\"state\":\"committed\""),
            "staged and committed terminal rows cross the JNI response schema");
        check(page.contains("\"reference\":\"te1-") && page.contains("\"byteLength\":\"4\"")
                && page.contains("\"digest\":") && page.contains("\"formatVersion\":1"),
            "committed terminal metadata crosses the JNI response schema: " + page);
        check(page.contains("\"more\":false"), "complete page reports no more");
        String status = MobileCoreBridge.nativeTerminalEvidenceRecoveryListWithStatusForBinding(binding, lease, 8, 4096);
        check(status.contains("\"busyProducers\":[]") && status.contains("\"state\":\"staged\"") && status.contains("\"state\":\"committed\""),
            "status listing preserves selected rows and reports no unrelated owner through JNI");

        String handle = handleForOperation(page, "committed");
        expectInvalid(() -> MobileCoreBridge.nativeTerminalEvidenceRecoveryReadForBinding(binding, "forged-entry", "0", 2),
            "forged recovery entry");
        String firstRead = MobileCoreBridge.nativeTerminalEvidenceRecoveryReadForBinding(binding, handle, "0", 2);
        String secondRead = MobileCoreBridge.nativeTerminalEvidenceRecoveryReadForBinding(binding, handle, "2", 2);
        check(firstRead.contains("\"bytesBase64\":\"Q0Q=\"") && firstRead.contains("\"more\":true"),
            "bounded recovery read reports its continuation");
        check(secondRead.contains("\"bytesBase64\":\"RUY=\"") && secondRead.contains("\"more\":false"),
            "bounded recovery read returns exact final bytes");

        // Treat the first response as lost. The second JNI call must recover
        // the same idempotent acknowledgement instead of releasing another row.
        String firstAck = MobileCoreBridge.nativeTerminalEvidenceRecoveryAcknowledgeForBinding(binding, handle);
        String retriedAck = MobileCoreBridge.nativeTerminalEvidenceRecoveryAcknowledgeForBinding(binding, handle);
        check(firstAck.equals("{}") && retriedAck.equals(firstAck),
            "lost acknowledgement reply retries idempotently through JNI");
        String afterAck = MobileCoreBridge.nativeTerminalEvidenceRecoveryListForBinding(binding, lease, 8, 4096);
        check(!afterAck.contains("\"operationId\":\"committed\""),
            "acknowledgement releases only the selected terminal row");
        MobileCoreBridge.nativeTerminalEvidenceRecoveryOpenSelectedForBinding(binding);
        expectInvalid(() -> MobileCoreBridge.nativeTerminalEvidenceRecoveryAcknowledgeForBinding(binding, handle),
            "duplicate acknowledgement expires with its recovery lease");
        System.out.println("terminal evidence JNI recovery exchange: OK");
    }
}
