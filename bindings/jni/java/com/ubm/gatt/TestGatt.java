package com.ubm.gatt;

import com.ubm.echo.EchoBridge;
import com.ubm.echo.EchoException;
import java.util.ArrayList;
import java.util.List;

/**
 * JVM exchange harness for the HOST-ANDROID GATT bridge slice.
 *
 * <p>Exercises the real built cdylib through real JNI: enqueue validation
 * (binder-thread contract), drain of a full scan/connect/discover/IO cycle
 * against the REAL Central, queued cancel, notify bounds, service-change
 * staleness, adapter reset, release idempotence, fail-closed identities, and
 * close invalidation. Fails loudly with a non-zero exit; no skips.
 */
public final class TestGatt {
    static final String REV = "C-UBM.0.1.2-DRAFT";
    static int passed = 0;

    public static void main(String[] args) {
        long session = EchoBridge.nativeOpen(REV);

        // Binder-thread contract: enqueue validates + stores, never drives.
        check("depth starts at zero", GattBridge.nativeGattQueueDepth(session) == 0);
        check("enqueue returns depth one",
                GattBridge.nativeEnqueueGattEvent(session, "expire-sweep|0") == 1);
        check("depth observes one", GattBridge.nativeGattQueueDepth(session) == 1);
        String sweepProbe = GattBridge.nativeDrainGattEvents(session);
        check("harmless probe drains", sweepProbe.contains("\"ok\":true"), sweepProbe);
        check("quiet line surfaces empty effect sections",
                sweepProbe.contains("\"effects\":[]") && sweepProbe.contains("\"observations\":[]"),
                sweepProbe);
        expectWire("enqueue empty rejects", () -> GattBridge.nativeEnqueueGattEvent(session, ""),
                "argument.invalid|core|gatt-enqueue|event-empty");
        expectWire("enqueue unknown kind drains fail-closed", () -> {
            String drained = drainRaw(session, "teleport|nowhere|0");
            if (!drained.contains("\"ok\":false")
                    || !drained.contains("capability.unsupported")) {
                throw new AssertionError("drain did not fail closed: " + drained);
            }
            return null;
        }, null);
        check("queue drained empty", GattBridge.nativeGattQueueDepth(session) == 0);

        // Full scan/connect/discover/IO cycle against the REAL Central.
        List<String> setup = new ArrayList<>();
        setup.add("scan.start|owner-a|5000|1000||all|none");
        String scanOut = drainSingle(session, setup.get(0));
        String scanOp = opOf(scanOut);
        check("admission surfaces its timer effect",
                scanOut.contains("\"effects\":[{\"kind\":\"timer.schedule\""), scanOut);
        check("admission surfaces its typed observation",
                scanOut.contains("\"observations\":[{\"kind\":\"central.scan-start\""), scanOut);
        drainSingle(session, "scan.platform-started|" + scanOp);
        String peerOut = drainSingle(session, "peer.resolve|public-address|AA:BB:CC:DD:EE:FF");
        check("peer key scoped", peerOut.contains("\"peer\":\"public-address:"));
        String peer = peerOut.split("\"peer\":\"")[1].split("\"")[0];
        String connectOut = drainSingle(session,
                "connect|" + peer + "|lease-a|5000|1000");
        String connectOp = opOf(connectOut);
        drainSingle(session, "link.established|" + peer);
        drainSingle(session, "discovery.begin|" + peer);
        drainSingle(session, "discovery.complete|" + peer);
        String pathOut = drainSingle(session,
                "path.register|" + peer + "|180d|0|2a37|0|-|-|11|lease-a");
        check("path registered at zero", pathOut.contains("\"path\":0"));
        String readOut = drainSingle(session, "read.start|0|5000|1000");
        String readOp = opOf(readOut);
        String dispatched = drainSingle(session, "op.dispatch|" + readOp);
        int radioAt = dispatched.indexOf("\"kind\":\"radio.dispatch\"");
        int publishAt = dispatched.indexOf("\"kind\":\"state.publish\"");
        check("dispatch surfaces radio effect then state publish",
                radioAt >= 0 && publishAt > radioAt, dispatched);
        check("effects bind the driving op",
                dispatched.contains("\"op\":\"" + readOp + "\""), dispatched);
        String settled = drainSingle(session,
                "op.settle|" + readOp + "|success|true|7|1000");
        check("IO settles succeeded", settled.contains("succeeded"));
        drainSingle(session, "op.settle|" + connectOp + "|success|true|8|1000");

        // Queued cancel: admitted but never dispatched, then cancelled.
        String read2 = drainSingle(session, "read.start|0|5000|1000");
        String read2Op = opOf(read2);
        String cancelled = drainSingle(session, "op.cancel|" + read2Op + "|1000");
        check("queued cancel aborts", cancelled.contains("aborted"));

        // Notify: subscribe, enable, deliver, bounds.
        drainSingle(session, "subscribe|0|error|8|1024|consumer-a|5000|1000");
        drainSingle(session, "subscribe.enable-settled|0|true|1000");
        String delivered = drainSingle(session, "notify.deliver|0|0102");
        check("notify delivered", delivered.contains("\"outcome\":\"delivered\""));
        StringBuilder huge = new StringBuilder("notify.deliver|0|");
        for (int i = 0; i < 4096; i++) {
            huge.append("ab");
        }
        String bounded = drainSingle(session, huge.toString());
        check("notify burst accounted", bounded.contains("deliveries"));
        expectWire("notify bad hex rejects as data", () -> {
            String line = drainRaw(session, "notify.deliver|0|zz");
            if (!line.contains("\"ok\":false") || !line.contains("bytes.invalid")) {
                throw new AssertionError("bad hex not rejected: " + line);
            }
            return null;
        }, null);

        // Service change stales handles; adapter reset settles and re-scopes.
        drainSingle(session, "services-changed|" + peer);
        String stale = drainRaw(session, "read.start|0|5000|1000");
        check("stale handle fails closed",
                stale.contains("\"ok\":false") && stale.contains("gatt.stale-handle"), stale);
        String reset = drainSingle(session, "adapter.reset|2000");
        check("adapter reset settles", reset.contains("\"settled\""));

        // Release drives the real destroy transition; idempotent. It shuts
        // the kernel down, so it runs on a dedicated session last.
        long releaser = EchoBridge.nativeOpen(REV);
        String released = drainSingle(releaser, "release");
        check("release releases", released.contains("\"state\":\"released\""));
        String releasedAgain = drainSingle(releaser, "release");
        check("release idempotent", releasedAgain.contains("\"state\":\"released\""));
        check("usable after release drive",
                EchoBridge.nativeCentralStatus(releaser).contains(REV));
        EchoBridge.nativeClose(releaser);

        // Close invalidation: enqueue and drain both fail lifecycle.destroyed.
        EchoBridge.nativeClose(session);
        final long closedFinal = session;
        expectWire("post-close enqueue", () -> GattBridge.nativeEnqueueGattEvent(closedFinal, "release"),
                "lifecycle.destroyed|core|gatt-enqueue|unknown-or-closed-handle");
        expectWire("post-close drain", () -> GattBridge.nativeDrainGattEvents(closedFinal),
                "lifecycle.destroyed|core|gatt-drain|unknown-or-closed-handle");
        expectWire("post-close depth", () -> GattBridge.nativeGattQueueDepth(closedFinal),
                "lifecycle.destroyed|core|gatt-queue-depth|unknown-or-closed-handle");

        System.out.println("jni-gatt-roundtrip: OK (" + passed + " checks)");
    }

    static String drainSingle(long session, String wire) {
        String out = drainRaw(session, wire);
        check("drain observes " + wire.split("\\|")[0], out.contains("\"ok\":true"), out);
        return out;
    }

    static String drainRaw(long session, String wire) {
        GattBridge.nativeEnqueueGattEvent(session, wire);
        return GattBridge.nativeDrainGattEvents(session);
    }

    static String opOf(String line) {
        String key = "\"op\":\"";
        int start = line.indexOf(key);
        if (start < 0) {
            throw new AssertionError("line carries no op: " + line);
        }
        start += key.length();
        int end = line.indexOf('"', start);
        return line.substring(start, end);
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
            if (wire == null) {
                // Data-assertion path already checked inside; only reach here
                // when no exception was expected to escape.
                throw new AssertionError("FAIL " + name + ": unexpected throw " + e.getMessage());
            }
            check(name, e.getMessage().equals(wire),
                    "got <" + e.getMessage() + "> want <" + wire + ">");
            return;
        }
        if (wire != null) {
            throw new AssertionError("FAIL " + name + ": no exception thrown");
        }
        passed++;
        System.out.println("  ok: " + name);
    }
}
