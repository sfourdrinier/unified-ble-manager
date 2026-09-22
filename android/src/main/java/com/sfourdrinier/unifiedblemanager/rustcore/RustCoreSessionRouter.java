// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreSessionRouter.java

package com.sfourdrinier.unifiedblemanager.rustcore;

import com.ubm.echo.EchoException;
import java.util.UUID;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Pure-Java session store + op router behind {@code UnifiedBleRustCoreModule}.
 *
 * <p>Zero React Native dependencies (fully JVM-testable): the native module
 * is a thin shell mapping these results onto {@code Promise}s. Sessions wrap
 * native {@code CoreSession} handles; {@link #invoke} routes the documented
 * op table to the {@link Bridge} and ALWAYS returns the frozen C-UBM wire
 * form ({@code ok/value/code/domain/operation}) — domain failures travel as
 * data, never as thrown host exceptions. Unknown ops fail loud with
 * {@code capability.unsupported}; there is no legacy fallback.
 */
public final class RustCoreSessionRouter {

  /** The linked {@code ubm-core} contract revision this router executes. */
  public static final String CONTRACT_REVISION = "C-UBM.0.1.2-DRAFT";

  /** Router-owned failure (args/session/op validation). */
  public static final class RouterException extends RuntimeException {
    public final String code;
    public final String domain;
    public final String operation;

    public RouterException(String code, String domain, String operation, String detail) {
      super(code + "|" + domain + "|" + operation + "|" + detail);
      this.code = code;
      this.domain = domain;
      this.operation = operation;
    }
  }

  /** One routed op outcome in the frozen wire form. */
  public static final class InvokeResult {
    public final boolean ok;
    public final String value;
    public final String code;
    public final String domain;
    public final String operation;

    private InvokeResult(boolean ok, String value, String code, String domain, String operation) {
      this.ok = ok;
      this.value = value;
      this.code = code;
      this.domain = domain;
      this.operation = operation;
    }

    public static InvokeResult ok(String value, String operation) {
      return new InvokeResult(true, value, "", "", operation);
    }

    public static InvokeResult failure(String code, String domain, String operation) {
      return new InvokeResult(false, "", code, domain, operation);
    }
  }

  /** JNI bridge surface (EchoBridge-shaped; fakeable on the JVM). */
  public interface Bridge {
    long open(String revision);

    String revision();

    void close(long handle);

    byte[] echoBytes(long handle, byte[] input);

    String echoCounter(long handle, String decimal);

    String centralStatus(long handle);

    String expireSweep(long handle, String nowMs);

    String destroy(long handle);

    void bleTransition(long handle, String transition);

    String stagedStep(long handle, String line);

    String stagedDrainLog(long handle);

    String stagedCounters(long handle);

    String bleScanStart(long handle, String owner, String timeoutMs, String nowMs);

    String bleScanTake(long handle);

    String bleScanStop(long handle, String opId, String nowMs);
  }

  /**
   * Thin platform read behind {@code adapter.state}: returns the frozen
   * adapter record JSON ({@code availability}, {@code authorization},
   * {@code power}, {@code backendGeneration}, {@code updatedAt},
   * {@code safeReason}). A separate seam (not the JNI bridge) because the
   * value is live platform state, and because this router must stay
   * dependency-free and JVM-testable: the Android production reader lives
   * in {@code RustCoreAdapterStateReader}, never here.
   */
  public interface AdapterStateReader {
    String readAdapterStateJson();
  }

  /**
   * Zero resource counters: the router tracks no live core resources
   * beyond open sessions, so every counter reads 0 honestly. Key set must
   * match the provider's {@code parseResourceCounters} exactly.
   */
  static final String ZERO_COUNTERS_JSON =
      "{\"activeScanControllers\":0,\"scanConsumers\":0,\"chooserSessions\":0,"
          + "\"connectionLeases\":0,\"physicalLinks\":0,\"databaseSnapshots\":0,"
          + "\"physicalCccdEnablements\":0,\"subscriptionConsumers\":0,\"queuedOperations\":0,"
          + "\"dispatchedOperations\":0,\"retainedByteBuffers\":0,\"restorationRecords\":0,"
          + "\"orphanedIpcOwners\":0}";

  private final Bridge bridge;
  private final AdapterStateReader adapterStates;
  private final ConcurrentHashMap<String, Long> sessions = new ConcurrentHashMap<>();
  private final java.util.Set<String> closed = ConcurrentHashMap.newKeySet();

  public RustCoreSessionRouter(Bridge bridge) {
    this(bridge, RustCoreSessionRouter::defaultAdapterStateJson);
  }

  public RustCoreSessionRouter(Bridge bridge, AdapterStateReader adapterStates) {
    this.bridge = bridge;
    this.adapterStates = adapterStates;
  }

  /**
   * Default reader (no platform view): honest unknowns. Production passes
   * the platform reader; this exists so the router stays constructible
   * without Android dependencies.
   */
  private static String defaultAdapterStateJson() {
    return "{\"availability\":\"unknown\",\"authorization\":\"unknown\",\"power\":\"unknown\","
        + "\"backendGeneration\":\"router-default\",\"updatedAt\":"
        + System.currentTimeMillis()
        + ",\"safeReason\":\"no platform adapter reader installed\"}";
  }

  /**
   * Opens one native session after verifying the linked revision. A foreign
   * revision closes the just-opened handle and rejects loud
   * ({@code protocol.incompatible}) — no effect without valid init.
   */
  public String openSession(String owner) {
    if (owner == null || owner.isEmpty()) {
      throw new RouterException("argument.invalid", "core", "rust-core.open", "empty-owner");
    }
    long handle = bridge.open(CONTRACT_REVISION);
    if (!CONTRACT_REVISION.equals(bridge.revision())) {
      try {
        bridge.close(handle);
      } catch (RuntimeException ignored) {
        // Best-effort cleanup: the skew rejection below is the outcome.
      }
      throw new RouterException(
          "protocol.incompatible", "core", "rust-core.open", "contract-revision.mismatch");
    }
    String sessionId = UUID.randomUUID().toString();
    sessions.put(sessionId, handle);
    return sessionId;
  }

  /** Releases one session. Idempotent: closing twice (or racing close) is a no-op. */
  public void closeSession(String sessionId) {
    if (sessionId == null || sessionId.isEmpty()) {
      throw new RouterException("argument.invalid", "core", "rust-core.close", "empty-session");
    }
    if (!closed.add(sessionId)) {
      return;
    }
    Long handle = sessions.remove(sessionId);
    if (handle != null) {
      bridge.close(handle);
    }
  }

  /** Routes one op through an open session. Never throws for domain failures. */
  public InvokeResult invoke(String sessionId, String op, String argsJson) {
    try {
      Long handle = sessions.get(sessionId);
      if (handle == null || closed.contains(sessionId)) {
        throw new RouterException("argument.invalid", "core", opName(op), "unknown-session");
      }
      String operation = opName(op);
      switch (operation) {
        case "central.status":
          return InvokeResult.ok(bridge.centralStatus(handle), operation);
        case "echo.bytes": {
          byte[] input = base64Decode(stringArg(argsJson, "input", operation));
          return InvokeResult.ok(base64Encode(bridge.echoBytes(handle, input)), operation);
        }
        case "echo.counter":
          return InvokeResult.ok(
              bridge.echoCounter(handle, stringArg(argsJson, "decimal", operation)), operation);
        case "kernel.expire-sweep":
          return InvokeResult.ok(
              bridge.expireSweep(handle, stringArg(argsJson, "nowMs", operation)), operation);
        case "kernel.destroy":
          return InvokeResult.ok(bridge.destroy(handle), operation);
        case "ble.transition":
          bridge.bleTransition(handle, stringArg(argsJson, "transition", operation));
          return InvokeResult.ok("", operation);
        case "staged.step":
          return InvokeResult.ok(
              bridge.stagedStep(handle, stringArg(argsJson, "line", operation)), operation);
        case "staged.drain":
          return InvokeResult.ok(bridge.stagedDrainLog(handle), operation);
        case "staged.counters":
          return InvokeResult.ok(bridge.stagedCounters(handle), operation);
        case "scan.start":
          return InvokeResult.ok(
              bridge.bleScanStart(
                  handle,
                  stringArg(argsJson, "owner", operation),
                  stringArg(argsJson, "timeoutMs", operation),
                  stringArg(argsJson, "nowMs", operation)),
              operation);
        case "scan.take":
          return InvokeResult.ok(bridge.bleScanTake(handle), operation);
        case "scan.stop":
          return InvokeResult.ok(
              bridge.bleScanStop(
                  handle, stringArg(argsJson, "opId", operation), stringArg(argsJson, "nowMs", operation)),
              operation);
        case "adapter.state":
          return InvokeResult.ok(adapterStates.readAdapterStateJson(), operation);
        case "counters.describe":
          return InvokeResult.ok(ZERO_COUNTERS_JSON, operation);
        case "events.take":
          return InvokeResult.ok("null", operation);
        case "op.cancel":
          return InvokeResult.ok("{\"state\":\"not-cancellable\"}", operation);
        case "session.dispose":
          bridge.destroy(handle);
          return InvokeResult.ok("{\"state\":\"released\"}", operation);
        default:
          return InvokeResult.failure("capability.unsupported", "capability", operation);
      }
    } catch (EchoException error) {
      return InvokeResult.failure(error.code(), error.domain(), error.operation());
    } catch (RouterException error) {
      return InvokeResult.failure(error.code, error.domain, error.operation);
    }
  }

  private static String opName(String op) {
    return op == null ? "" : op;
  }

  /**
   * Strict string-argument extractor for the flat {@code {"key":"value"}}
   * args objects this router accepts. Hand-rolled (no {@code org.json}):
   * {@code java.util.Base64} needs API 26+ and {@code android.util} is absent
   * on JVM unit tests — this router stays dependency-free and fully
   * JVM-testable instead.
   */
  static String stringArg(String argsJson, String key, String operation) {
    if (argsJson == null) {
      throw new RouterException("argument.invalid", "core", operation, "missing-args");
    }
    String needle = "\"" + key + "\"";
    int keyAt = argsJson.indexOf(needle);
    if (keyAt < 0) {
      throw new RouterException("argument.invalid", "core", operation, "missing-arg:" + key);
    }
    int colonAt = argsJson.indexOf(':', keyAt + needle.length());
    if (colonAt < 0) {
      throw new RouterException("argument.invalid", "core", operation, "malformed-args");
    }
    int quoteAt = argsJson.indexOf('"', colonAt + 1);
    if (quoteAt < 0) {
      throw new RouterException("argument.invalid", "core", operation, "malformed-args");
    }
    StringBuilder value = new StringBuilder();
    for (int index = quoteAt + 1; index < argsJson.length(); index++) {
      char current = argsJson.charAt(index);
      if (current == '\\' && index + 1 < argsJson.length()) {
        char next = argsJson.charAt(index + 1);
        if (next == '"' || next == '\\') {
          value.append(next);
          index++;
          continue;
        }
      }
      if (current == '"') {
        return value.toString();
      }
      value.append(current);
    }
    throw new RouterException("argument.invalid", "core", operation, "malformed-args");
  }

  private static final char[] BASE64_ALPHABET =
      "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".toCharArray();
  private static final int[] BASE64_INDEX = new int[128];

  static {
    for (int index = 0; index < BASE64_INDEX.length; index++) {
      BASE64_INDEX[index] = -1;
    }
    for (int index = 0; index < BASE64_ALPHABET.length; index++) {
      BASE64_INDEX[BASE64_ALPHABET[index]] = index;
    }
  }

  /** Standard Base64 (see {@link #stringArg} for why this is hand-rolled). */
  public static String base64Encode(byte[] input) {
    StringBuilder out = new StringBuilder(((input.length + 2) / 3) * 4);
    for (int index = 0; index < input.length; index += 3) {
      int first = input[index] & 0xFF;
      int second = index + 1 < input.length ? input[index + 1] & 0xFF : 0;
      int third = index + 2 < input.length ? input[index + 2] & 0xFF : 0;
      int triplet = (first << 16) | (second << 8) | third;
      out.append(BASE64_ALPHABET[(triplet >>> 18) & 0x3F]);
      out.append(BASE64_ALPHABET[(triplet >>> 12) & 0x3F]);
      out.append(index + 1 < input.length ? BASE64_ALPHABET[(triplet >>> 6) & 0x3F] : '=');
      out.append(index + 2 < input.length ? BASE64_ALPHABET[triplet & 0x3F] : '=');
    }
    return out.toString();
  }

  /** Standard Base64 decode. Malformed input fails loud. */
  public static byte[] base64Decode(String input) {
    if (input == null) {
      throw new RouterException("argument.invalid", "core", "rust-core.decode", "null-input");
    }
    if (input.length() % 4 != 0) {
      throw new RouterException("argument.invalid", "core", "rust-core.decode", "malformed-base64");
    }
    byte[] out = new byte[(input.length() / 4) * 3];
    int size = 0;
    for (int index = 0; index < input.length(); index += 4) {
      int[] sextets = new int[4];
      int padding = 0;
      for (int slot = 0; slot < 4; slot++) {
        char current = input.charAt(index + slot);
        if (current == '=') {
          padding++;
          sextets[slot] = 0;
        } else if (current < 128 && BASE64_INDEX[current] >= 0) {
          if (padding > 0) {
            throw new RouterException("argument.invalid", "core", "rust-core.decode", "malformed-base64");
          }
          sextets[slot] = BASE64_INDEX[current];
        } else {
          throw new RouterException("argument.invalid", "core", "rust-core.decode", "malformed-base64");
        }
      }
      int triplet = (sextets[0] << 18) | (sextets[1] << 12) | (sextets[2] << 6) | sextets[3];
      out[size++] = (byte) ((triplet >>> 16) & 0xFF);
      if (padding < 2) {
        out[size++] = (byte) ((triplet >>> 8) & 0xFF);
      }
      if (padding == 0) {
        out[size++] = (byte) (triplet & 0xFF);
      }
    }
    byte[] trimmed = new byte[size];
    System.arraycopy(out, 0, trimmed, 0, size);
    return trimmed;
  }
}
