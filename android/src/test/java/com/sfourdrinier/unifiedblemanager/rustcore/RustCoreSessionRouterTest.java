// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreSessionRouterTest.java

package com.sfourdrinier.unifiedblemanager.rustcore;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import com.ubm.echo.EchoException;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import org.junit.Before;
import org.junit.Test;

public final class RustCoreSessionRouterTest {

  private static final String REV = "C-UBM.0.1.2-DRAFT";

  private static class FakeBridge implements RustCoreSessionRouter.Bridge {
    final List<String> calls = new ArrayList<>();
    String revision = REV;
    RuntimeException openFailure;
    long nextHandle = 101L;

    @Override
    public long open(String revision) {
      calls.add("open:" + revision);
      if (openFailure != null) {
        throw openFailure;
      }
      return nextHandle++;
    }

    @Override
    public String revision() {
      calls.add("revision");
      return revision;
    }

    @Override
    public void close(long handle) {
      calls.add("close:" + handle);
    }

    @Override
    public byte[] echoBytes(long handle, byte[] input) {
      calls.add("echoBytes:" + handle);
      return input;
    }

    @Override
    public String echoCounter(long handle, String decimal) {
      calls.add("echoCounter:" + handle + ":" + decimal);
      return decimal;
    }

    @Override
    public String centralStatus(long handle) {
      calls.add("centralStatus:" + handle);
      return "{\"ok\":true,\"central\":\"up\"}";
    }

    @Override
    public String expireSweep(long handle, String nowMs) {
      calls.add("expireSweep:" + handle + ":" + nowMs);
      return "3";
    }

    @Override
    public String destroy(long handle) {
      calls.add("destroy:" + handle);
      return "destroyed";
    }

    @Override
    public void bleTransition(long handle, String transition) {
      calls.add("bleTransition:" + handle + ":" + transition);
    }

    @Override
    public String stagedStep(long handle, String line) {
      calls.add("stagedStep:" + handle);
      return "{\"ok\":true}";
    }

    @Override
    public String stagedDrainLog(long handle) {
      calls.add("stagedDrainLog:" + handle);
      return "";
    }

    @Override
    public String stagedCounters(long handle) {
      calls.add("stagedCounters:" + handle);
      return "{}";
    }

    @Override
    public String bleScanStart(long handle, String owner, String timeoutMs, String nowMs) {
      calls.add("bleScanStart:" + handle + ":" + owner + ":" + timeoutMs + ":" + nowMs);
      return "{\"ok\":true,\"op_id\":\"op-1\"}";
    }

    @Override
    public String bleScanTake(long handle) {
      calls.add("bleScanTake:" + handle);
      return "{\"obs\":1}";
    }

    @Override
    public String bleScanStop(long handle, String opId, String nowMs) {
      calls.add("bleScanStop:" + handle + ":" + opId + ":" + nowMs);
      return "{\"ok\":true}";
    }
  }

  private FakeBridge bridge;
  private RustCoreSessionRouter router;

  @Before
  public void setUp() {
    bridge = new FakeBridge();
    router = new RustCoreSessionRouter(bridge);
  }

  @Test
  public void openReturnsSessionAndVerifiesRevision() {
    String id = router.openSession("owner-a");
    assertNotNull(id);
    assertFalse(id.isEmpty());
    assertTrue(bridge.calls.contains("open:" + REV));
    assertTrue(bridge.calls.contains("revision"));
  }

  @Test
  public void emptyOwnerFailsClosed() {
    try {
      router.openSession("");
      fail("empty owner must reject");
    } catch (RustCoreSessionRouter.RouterException expected) {
      assertEquals("argument.invalid", expected.code);
    }
    assertTrue(bridge.calls.isEmpty());
  }

  @Test
  public void revisionSkewClosesHandleAndRejects() {
    bridge.revision = "C-UBM.9.9.9-DRAFT";
    try {
      router.openSession("owner-a");
      fail("skewed revision must reject");
    } catch (RustCoreSessionRouter.RouterException expected) {
      assertEquals("protocol.incompatible", expected.code);
    }
    assertTrue(bridge.calls.contains("close:101"));
  }

  @Test
  public void nativeOpenFailurePropagatesWireIdentity() {
    bridge.openFailure = new EchoException("protocol.incompatible|core|echo-session.open|contract-revision.mismatch");
    try {
      router.openSession("owner-a");
      fail("native failure must propagate");
    } catch (EchoException expected) {
      assertEquals("protocol.incompatible", expected.code());
      assertEquals("echo-session.open", expected.operation());
    }
  }

  @Test
  public void invokeRoutesTheDocumentedOpTable() {
    String id = router.openSession("owner-a");
    assertOk(router.invoke(id, "central.status", "{}"), "{\"ok\":true,\"central\":\"up\"}");
    assertOk(router.invoke(id, "echo.counter", "{\"decimal\":\"41\"}"), "41");
    assertOk(router.invoke(id, "kernel.expire-sweep", "{\"nowMs\":\"1000\"}"), "3");
    assertOk(router.invoke(id, "kernel.destroy", "{}"), "destroyed");
    assertOk(router.invoke(id, "staged.step", "{\"line\":\"{}\"}"), "{\"ok\":true}");
    assertOk(router.invoke(id, "scan.start", "{\"owner\":\"o\",\"timeoutMs\":\"8000\",\"nowMs\":\"1000\"}"), "{\"ok\":true,\"op_id\":\"op-1\"}");
    assertOk(router.invoke(id, "scan.take", "{}"), "{\"obs\":1}");
    assertOk(router.invoke(id, "scan.stop", "{\"opId\":\"op-1\",\"nowMs\":\"2000\"}"), "{\"ok\":true}");
    assertTrue(bridge.calls.contains("bleScanStart:101:o:8000:1000"));
    assertTrue(bridge.calls.contains("bleScanStop:101:op-1:2000"));
  }

  @Test
  public void echoBytesRoundTripsBase64() {
    String id = router.openSession("owner-a");
    // "hi" -> aGk=
    RustCoreSessionRouter.InvokeResult result = router.invoke(id, "echo.bytes", "{\"input\":\"aGk=\"}");
    assertTrue(result.ok);
    assertEquals("aGk=", result.value);
  }

  @Test
  public void unknownOpFailsLoud() {
    String id = router.openSession("owner-a");
    RustCoreSessionRouter.InvokeResult result = router.invoke(id, "gatt.connect", "{}");
    assertFalse(result.ok);
    assertEquals("capability.unsupported", result.code);
  }

  @Test
  public void unknownSessionFailsLoud() {
    RustCoreSessionRouter.InvokeResult result = router.invoke("no-such-session", "central.status", "{}");
    assertFalse(result.ok);
    assertEquals("argument.invalid", result.code);
  }

  @Test
  public void missingArgFailsLoud() {
    String id = router.openSession("owner-a");
    RustCoreSessionRouter.InvokeResult result = router.invoke(id, "scan.stop", "{\"nowMs\":\"2000\"}");
    assertFalse(result.ok);
    assertEquals("argument.invalid", result.code);
  }

  @Test
  public void bridgeFailureKeepsWireIdentity() {
    FakeBridge failing = new FakeBridge() {
      @Override
      public String centralStatus(long handle) {
        throw new EchoException("lifecycle.destroyed|core|central-status|session-closed");
      }
    };
    RustCoreSessionRouter failingRouter = new RustCoreSessionRouter(failing);
    String failingId = failingRouter.openSession("owner-a");
    RustCoreSessionRouter.InvokeResult result = failingRouter.invoke(failingId, "central.status", "{}");
    assertFalse(result.ok);
    assertEquals("lifecycle.destroyed", result.code);
    assertEquals("central-status", result.operation);
  }

  @Test
  public void closeIsIdempotentAndRetiresTheSession() {
    String id = router.openSession("owner-a");
    router.closeSession(id);
    router.closeSession(id);
    assertEquals(1, bridge.calls.stream().filter(call -> call.equals("close:101")).count());
    RustCoreSessionRouter.InvokeResult result = router.invoke(id, "central.status", "{}");
    assertFalse(result.ok);
    assertEquals("argument.invalid", result.code);
  }

  @Test
  public void creationSurfaceServesAdapterCountersEventsCancel() {
    RustCoreSessionRouter platformRouter =
        new RustCoreSessionRouter(bridge, () -> "{\"availability\":\"available\"}");
    String id = platformRouter.openSession("owner-a");
    assertOk(
        platformRouter.invoke(id, "adapter.state", "{}"), "{\"availability\":\"available\"}");
    RustCoreSessionRouter.InvokeResult counters =
        platformRouter.invoke(id, "counters.describe", "{}");
    assertTrue(counters.ok);
    for (String key :
        new String[] {
          "activeScanControllers",
          "scanConsumers",
          "chooserSessions",
          "connectionLeases",
          "physicalLinks",
          "databaseSnapshots",
          "physicalCccdEnablements",
          "subscriptionConsumers",
          "queuedOperations",
          "dispatchedOperations",
          "retainedByteBuffers",
          "restorationRecords",
          "orphanedIpcOwners"
        }) {
      assertTrue("missing counter " + key, counters.value.contains("\"" + key + "\":0"));
    }
    assertOk(platformRouter.invoke(id, "events.take", "{}"), "null");
    assertOk(platformRouter.invoke(id, "op.cancel", "{\"operationId\":\"op-1\"}"), "{\"state\":\"not-cancellable\"}");
  }

  @Test
  public void defaultAdapterReaderReportsHonestUnknowns() {
    String id = router.openSession("owner-a");
    RustCoreSessionRouter.InvokeResult result = router.invoke(id, "adapter.state", "{}");
    assertTrue(result.ok);
    assertTrue(result.value.contains("\"availability\":\"unknown\""));
    assertTrue(result.value.contains("\"authorization\":\"unknown\""));
    assertTrue(result.value.contains("\"power\":\"unknown\""));
    assertTrue(result.value.contains("\"backendGeneration\":\"router-default\""));
    assertTrue(result.value.contains("\"updatedAt\":"));
    assertTrue(result.value.contains("\"safeReason\":\"no platform adapter reader installed\""));
  }

  @Test
  public void disposeDrivesNativeDestroyAndSessionStillCloses() {
    String id = router.openSession("owner-a");
    assertOk(router.invoke(id, "session.dispose", "{}"), "{\"state\":\"released\"}");
    assertTrue(bridge.calls.contains("destroy:101"));
    router.closeSession(id);
    assertTrue(bridge.calls.contains("close:101"));
  }

  @Test
  public void base64CodecMatchesKnownVectors() {
    assertEquals("aGk=", RustCoreSessionRouter.base64Encode("hi".getBytes(StandardCharsets.UTF_8)));
    assertEquals("", RustCoreSessionRouter.base64Encode(new byte[0]));
    assertEquals("hi", new String(RustCoreSessionRouter.base64Decode("aGk="), StandardCharsets.UTF_8));
    assertEquals("+//+", RustCoreSessionRouter.base64Encode(new byte[] {(byte) 0xfb, (byte) 0xff, (byte) 0xfe}));
  }

  private static void assertOk(RustCoreSessionRouter.InvokeResult result, String value) {
    assertTrue("expected ok, got " + result.code + "|" + result.operation, result.ok);
    assertEquals(value, result.value);
  }
}
