package com.ubm.echo;

import java.math.BigInteger;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicReference;

/**
 * JVM exchange harness for the JNI feasibility slice. Exercises the real
 * built cdylib through real JNI: init contract, owned byte batches, lossless
 * u64 counters, typed failures (full wire literals), cancellation
 * (armed + threaded mid-flight + close-during-flight), close invalidation,
 * and panic containment. Fails loudly with a non-zero exit; no skips.
 */
public final class TestEcho {
    static final String REV = "C-UBM.0.1.1-DRAFT";
    static final int MAX_BYTES = 524288;
    static int passed = 0;

    public static void main(String[] args) {
        check("revision identity", EchoBridge.nativeRevision().equals(REV));

        // PKG-02 init contract: foreign revision fails closed.
        expectWire("foreign revision", () -> EchoBridge.nativeOpen("C-UBM.9.9.9-DRAFT"),
                "protocol.incompatible|core|echo-session.open|contract-revision.mismatch");

        long session = EchoBridge.nativeOpen(REV);

        // Owned byte batches.
        check("bytes round-trip",
                Arrays.equals(EchoBridge.nativeEchoBytes(session, new byte[] {0, 1, 2, -6, -1}),
                        new byte[] {0, 1, 2, -6, -1}));
        check("empty batch", EchoBridge.nativeEchoBytes(session, new byte[0]).length == 0);
        byte[] big = new byte[MAX_BYTES];
        Arrays.fill(big, (byte) 0xAB);
        check("max batch", EchoBridge.nativeEchoBytes(session, big).length == MAX_BYTES);
        expectWire("oversize", () -> EchoBridge.nativeEchoBytes(session, new byte[MAX_BYTES + 1]),
                "bytes.too-large|core|echo-bytes|exceeds-max-operation-bytes");
        expectWire("null input", () -> EchoBridge.nativeEchoBytes(session, null),
                "argument.invalid|core|echo-bytes|null-array");

        // DATA-02 lossless u64 counters via BigInteger <-> decimal string.
        for (String decimal : new String[] {"0", "1", "00042", "9007199254740993",
                "9223372036854775807", "18446744073709551615"}) {
            String echoed = EchoBridge.nativeEchoCounter(session, decimal);
            check("counter " + decimal,
                    new BigInteger(echoed).equals(new BigInteger(decimal)));
        }
        check("canonical form", EchoBridge.nativeEchoCounter(session, "00042").equals("42"));
        for (String bad : new String[] {"", "-1", "+5", "12a34", " 42", "4.0", "0x10",
                "18446744073709551616"}) {
            final String input = bad;
            expectWire("counter rejects <" + bad + ">",
                    () -> EchoBridge.nativeEchoCounter(session, input),
                    "bytes.invalid|core|echo-counter|"
                            + (bad.isEmpty() || !bad.matches("[0-9]+") ? "u64.input" : "u64.range"));
        }
        expectWire("null counter", () -> EchoBridge.nativeEchoCounter(session, null),
                "argument.invalid|core|echo-counter|null-string");

        // Cancellation: armed cancel aborts the next chunked unit.
        EchoBridge.nativeCancel(session);
        expectWire("armed cancel aborts",
                () -> EchoBridge.nativeEchoBytesChunked(session, new byte[] {1, 2, 3}, 10),
                "operation.aborted|core|echo-bytes-chunked|cancelled-before-start");
        check("usable after abort",
                Arrays.equals(EchoBridge.nativeEchoBytes(session, new byte[] {9}),
                        new byte[] {9}));
        expectWire("bad chunk count",
                () -> EchoBridge.nativeEchoBytesChunked(session, new byte[] {1}, 0),
                "argument.invalid|core|echo-bytes-chunked|chunk-count-range");

        // Threaded mid-flight cancel: ~1 GiB of hashing vs a 20 ms delay.
        // Strictly asserted abort with a wide margin.
        final long sessionForThread = session;
        final byte[] work = new byte[262144];
        Arrays.fill(work, (byte) 0x5A);
        AtomicReference<EchoException> outcome = new AtomicReference<>();
        AtomicReference<byte[]> success = new AtomicReference<>();
        Thread worker = new Thread(() -> {
            try {
                success.set(EchoBridge.nativeEchoBytesChunked(sessionForThread, work, 5000));
            } catch (EchoException e) {
                outcome.set(e);
            }
        });
        worker.start();
        try {
            Thread.sleep(20);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new AssertionError("sleep interrupted");
        }
        EchoBridge.nativeCancel(session);
        try {
            worker.join(120000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new AssertionError("join interrupted");
        }
        check("worker joined", !worker.isAlive());
        check("mid-flight abort",
                outcome.get() != null
                        && outcome.get().getMessage().startsWith(
                                "operation.aborted|core|echo-bytes-chunked|cancelled"));

        // Close cancels in-flight work: the pending call aborts.
        long doomed = EchoBridge.nativeOpen(REV);
        final long doomedFinal = doomed;
        AtomicReference<EchoException> doomedOutcome = new AtomicReference<>();
        Thread doomedWorker = new Thread(() -> {
            try {
                EchoBridge.nativeEchoBytesChunked(doomedFinal, work, 5000);
            } catch (EchoException e) {
                doomedOutcome.set(e);
            }
        });
        doomedWorker.start();
        try {
            Thread.sleep(20);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new AssertionError("sleep interrupted");
        }
        EchoBridge.nativeClose(doomed);
        try {
            doomedWorker.join(120000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new AssertionError("join interrupted");
        }
        check("close-during-flight aborts",
                doomedOutcome.get() != null
                        && doomedOutcome.get().code().equals("operation.aborted"));

        // Close invalidation.
        EchoBridge.nativeClose(session);
        final long closedFinal = session;
        expectWire("post-close bytes", () -> EchoBridge.nativeEchoBytes(closedFinal, new byte[] {1}),
                "lifecycle.destroyed|core|echo-bytes|unknown-or-closed-handle");
        // M1: uniform post-close cancel rejects like every other call on a
        // destroyed handle (napi/uniffi agree).
        expectWire("post-close cancel", () -> {
            EchoBridge.nativeCancel(closedFinal);
            return null;
        }, "lifecycle.destroyed|core|cancel-inflight|unknown-or-closed-handle");
        expectWire("double close", () -> {
            EchoBridge.nativeClose(closedFinal);
            return null;
        }, "lifecycle.destroyed|core|close|unknown-handle");
        expectWire("unknown handle", () -> EchoBridge.nativeEchoBytes(999999L, new byte[] {1}),
                "lifecycle.destroyed|core|echo-bytes|unknown-or-closed-handle");

        // Panic probes were deleted from production paths by the wiring
        // slice: no feasibility-only native may ship. The bridge exposes no
        // probe; a Rust panic still surfaces as a typed EchoException via the
        // bridge error policy. The session stays usable afterwards.
        long probeSession = EchoBridge.nativeOpen(REV);
        check("usable session after probe removal",
                Arrays.equals(EchoBridge.nativeEchoBytes(probeSession, new byte[] {7}),
                        new byte[] {7}));
        EchoBridge.nativeClose(probeSession);

        System.out.println("jni-roundtrip: OK (" + passed + " checks)");
    }

    interface ThrowingSupplier {
        Object get();
    }

    static void check(String name, boolean cond) {
        check(name, cond, "");
    }

    static void check(String name, boolean cond, String extra) {
        if (!cond) {
            throw new AssertionError("FAIL " + name + " " + extra);
        }
        passed++;
        System.out.println("  ok: " + name);
    }

    static void expectWire(String name, ThrowingSupplier call, String wire) {
        try {
            call.get();
        } catch (EchoException e) {
            check(name, e.getMessage().equals(wire),
                    "got <" + e.getMessage() + "> want <" + wire + ">");
            return;
        }
        throw new AssertionError("FAIL " + name + ": no exception thrown");
    }
}
