package com.ubm.core;

import com.sfourdrinier.unifiedblemanager.rustcore.JniBoundMobileCorePort;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.TimeUnit;

/** Standalone JVM test against the shipped JNI facade and Kotlin port. */
public final class BoundMobileCorePortHarness {
    static class Radio implements java.lang.reflect.InvocationHandler {
        final JniBoundMobileCorePort port;
        final String owner = java.util.UUID.randomUUID().toString();
        final ArrayBlockingQueue<Long> requests = new ArrayBlockingQueue<>(4);
        final MobileCoreBridge.RadioHost host = (MobileCoreBridge.RadioHost) java.lang.reflect.Proxy.newProxyInstance(
            MobileCoreBridge.RadioHost.class.getClassLoader(), new Class<?>[] { MobileCoreBridge.RadioHost.class }, this);
        final boolean retireFailedInstall;
        long binding;
        int attempts;
        Radio(JniBoundMobileCorePort port) { this(port, false); }
        Radio(JniBoundMobileCorePort port, boolean retireFailedInstall) {
            this.port = port;
            this.retireFailedInstall = retireFailedInstall;
        }
        @Override public Object invoke(Object proxy, java.lang.reflect.Method method, Object[] args) {
            switch (method.getName()) {
                case "acceptBindingRoute":
                    binding = (Long) args[1];
                    return port.acceptBindingRoute((Integer) args[0], binding);
                case "orderedAdmissionOwner":
                    int version = (Integer) args[0];
                    return version == 3 || (version == 4 && retireFailedInstall) ? owner : "";
                case "subrateAvailable": return false;
                case "adapterState": requests.add((Long) args[0]); return null;
                case "ownerFence":
                    long request = (Long) args[0];
                    if (++attempts == 1) {
                        MobileCoreBridge.nativeCompleteFailureForBinding(binding, request, "platform", MobileCoreBridge.ABSENT_INT, "retry", true, null);
                    } else {
                        MobileCoreBridge.OwnerRetirementResult result = new MobileCoreBridge.OwnerRetirementResult();
                        result.ownerIncarnation = (String) args[1]; result.retirementId = (String) args[2]; result.state = "quiescent";
                        port.completeOwnerRetirement(request, result);
                    }
                    return null;
                default: throw new AssertionError("unexpected radio call: " + method.getName());
            }
        }
    }
    static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
    static long session(JniBoundMobileCorePort port) {
        String result = port.openSession("test", port.wireRevision(), "scope");
        return Long.parseLong(result.replaceAll(".*\\\"sessionId\\\":\\\"?([0-9]+)\\\"?.*", "$1"));
    }
    public static void main(String[] args) throws Exception {
        System.loadLibrary("ubm5_jni_echo");
        JniBoundMobileCorePort a = new JniBoundMobileCorePort();
        JniBoundMobileCorePort b = new JniBoundMobileCorePort();
        check(a.orderedRecordingVersion() == MobileCoreBridge.nativeOrderedRecordingVersion(),
            "cold version probe does not require installation");
        try {
            a.continuationRecordingControl("status", "cold", "", 0, 0);
            throw new AssertionError("cold recording control must not select a foreign host");
        } catch (com.sfourdrinier.unifiedblemanager.rustcore.RustCoreRejection expected) {
            check("capability.unsupported".equals(expected.getCode()), "cold control fails closed");
        }
        check(!a.acceptBindingRoute(1, 123), "binding cannot be assigned before installation");
        Radio ra = new Radio(a), rb = new Radio(b);
        ArrayBlockingQueue<Long> wa = new ArrayBlockingQueue<>(16), wb = new ArrayBlockingQueue<>(16);
        a.installHost(ra.host, wa::add, "a", "adapter");
        b.installHost(rb.host, wb::add, "b", "adapter");
        check(!a.acceptBindingRoute(1, rb.binding), "installed port cannot be rebound");
        try { a.installHost(ra.host, wa::add, "a", "adapter"); throw new AssertionError("port reused"); }
        catch (IllegalStateException expected) { }
        long sa = session(a), sb = session(b);
        check(sa == sb, "session IDs should overlap");
        ArrayBlockingQueue<String> qa = new ArrayBlockingQueue<>(1), qb = new ArrayBlockingQueue<>(1);
        a.invoke(sa, "adapter.state", "{}", qa::add);
        b.invoke(sb, "adapter.state", "{}", qb::add);
        long ar = ra.requests.poll(5, TimeUnit.SECONDS), br = rb.requests.poll(5, TimeUnit.SECONDS);
        check(a.completeAdapter(ar, "available", "granted", "on", null) == 0, "first completion");
        check(qa.poll(5, TimeUnit.SECONDS) != null && qb.poll(200, TimeUnit.MILLISECONDS) == null, "first cannot complete second");
        check(b.completeAdapter(br, "unavailable", "granted", "off", null) == 0, "second completion");
        check(qb.poll(5, TimeUnit.SECONDS) != null, "second answer delivered");
        check(a.completeUnit(ar) == MobileCoreBridge.STATUS_LATE, "late completion fenced");
        wa.clear(); wb.clear();
        check(a.ingestAdapterState(new com.sfourdrinier.unifiedblemanager.rustcore.AdapterFacts("available", "granted", "on", null)) == 0, "first observation accepted");
        check(wa.poll(5, TimeUnit.SECONDS) == sa, "first session woke");
        check(wb.poll(200, TimeUnit.MILLISECONDS) == null, "observation cannot wake second host");
        check(a.drain(sa, 64, 8192).contains("\"power\":\"on\""), "first host drains observation");
        check(!b.drain(sb, 64, 8192).contains("\"power\":\"on\""), "second host cannot drain observation");
        ArrayBlockingQueue<String> control = new ArrayBlockingQueue<>(1);
        a.orderedRecordingReceiptControl(sa, "reserve", "[\"binding-receipt\",\"caller\"]", control::add);
        check(control.poll(5, TimeUnit.SECONDS).contains("\"state\":\"reserved\""), "ordered reservation reaches first host");
        b.continuationDescribeBacklog(control::add);
        check(control.poll(5, TimeUnit.SECONDS).contains("\"ok\":true"), "continuation reaches second host");
        check(a.shutdownHost().contains("release-failed"), "unproved native retirement remains visible");
        check(a.shutdownHost().contains("release-failed"), "cleanup remains retryable");
        check(a.completeUnit(ar) == MobileCoreBridge.STATUS_LATE, "retained old route rejects stale callback");
        check(b.hostInstalled(), "second remains installed");
        check(b.shutdownHost().contains("release-failed"), "second cleanup remains visible");

        JniBoundMobileCorePort failed = new JniBoundMobileCorePort();
        Radio rf = new Radio(failed, true);
        try { failed.installHost(rf.host, id -> {}, "", "adapter"); throw new AssertionError("invalid install succeeded"); }
        catch (MobileCoreBridge.MobileCoreException expected) { }
        check(!failed.hostInstalled(), "failed install not published");
        check(failed.shutdownHost().contains("release-failed"), "failed install cleanup retained");
        check(failed.shutdownHost().contains("\"state\":\"released\""), "failed install cleanup retried");
        java.lang.reflect.Field retainedRadio = JniBoundMobileCorePort.class.getDeclaredField("retainedRadio");
        retainedRadio.setAccessible(true);
        check(retainedRadio.get(failed) == null, "confirmed retirement releases the port's radio reference");
        check(failed.completeUnit(1) == MobileCoreBridge.STATUS_NO_HOST, "retired token cannot route to another host");
        System.out.println("bound Kotlin port JNI isolation passed");
        System.exit(0);
    }
}
