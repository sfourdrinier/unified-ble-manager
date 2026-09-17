// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/UnifiedBleRustCoreModule.java

package com.sfourdrinier.unifiedblemanager.rustcore;

import androidx.annotation.NonNull;

import com.facebook.react.bridge.Arguments;
import com.facebook.react.bridge.Promise;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.bridge.ReactMethod;
import com.facebook.react.bridge.WritableMap;
import com.sfourdrinier.unifiedblemanager.NativeUnifiedBleRustCoreSpec;
import com.ubm.echo.EchoBridge;
import com.ubm.echo.EchoException;

/**
 * R01/D3(a) production session facade: the {@code UnifiedBleRustCore}
 * TurboModule over the JNI cdylib. A thin shell — every decision lives in
 * {@link RustCoreSessionRouter} (JVM-tested); this class only maps results
 * onto {@code Promise}s. Unknown ops reject loud; there is no legacy path.
 */
public class UnifiedBleRustCoreModule extends NativeUnifiedBleRustCoreSpec {
  public static final String NAME = "UnifiedBleRustCore";

  private final RustCoreSessionRouter router;

  public UnifiedBleRustCoreModule(ReactApplicationContext reactContext) {
    super(reactContext);
    this.router =
        new RustCoreSessionRouter(new EchoBridgeAdapter(), new RustCoreAdapterStateReader(reactContext));
  }

  @NonNull
  @Override
  public String getName() {
    return NAME;
  }

  @ReactMethod
  @Override
  public void openSession(String owner, Promise promise) {
    try {
      WritableMap handle = Arguments.createMap();
      handle.putString("sessionId", router.openSession(owner));
      promise.resolve(handle);
    } catch (RustCoreSessionRouter.RouterException error) {
      promise.reject(error.code, error.getMessage(), error);
    } catch (EchoException error) {
      promise.reject(error.code(), error.getMessage(), error);
    }
  }

  @ReactMethod
  @Override
  public void invoke(String sessionId, String op, String argsJson, Promise promise) {
    RustCoreSessionRouter.InvokeResult result = router.invoke(sessionId, op, argsJson);
    WritableMap body = Arguments.createMap();
    body.putBoolean("ok", result.ok);
    body.putString("value", result.value);
    body.putString("code", result.code);
    body.putString("domain", result.domain);
    body.putString("operation", result.operation);
    promise.resolve(body);
  }

  @ReactMethod
  @Override
  public void close(String sessionId, Promise promise) {
    try {
      router.closeSession(sessionId);
      promise.resolve(null);
    } catch (RustCoreSessionRouter.RouterException error) {
      promise.reject(error.code, error.getMessage(), error);
    } catch (EchoException error) {
      promise.reject(error.code(), error.getMessage(), error);
    }
  }

  @ReactMethod
  @Override
  public void contractRevision(Promise promise) {
    promise.resolve(RustCoreSessionRouter.CONTRACT_REVISION);
  }

  /** Production {@link RustCoreSessionRouter.Bridge} over the JNI cdylib. */
  static final class EchoBridgeAdapter implements RustCoreSessionRouter.Bridge {
    @Override
    public long open(String revision) {
      return EchoBridge.nativeOpen(revision);
    }

    @Override
    public String revision() {
      return EchoBridge.nativeRevision();
    }

    @Override
    public void close(long handle) {
      EchoBridge.nativeClose(handle);
    }

    @Override
    public byte[] echoBytes(long handle, byte[] input) {
      return EchoBridge.nativeEchoBytes(handle, input);
    }

    @Override
    public String echoCounter(long handle, String decimal) {
      return EchoBridge.nativeEchoCounter(handle, decimal);
    }

    @Override
    public String centralStatus(long handle) {
      return EchoBridge.nativeCentralStatus(handle);
    }

    @Override
    public String expireSweep(long handle, String nowMs) {
      return EchoBridge.nativeExpireSweep(handle, nowMs);
    }

    @Override
    public String destroy(long handle) {
      return EchoBridge.nativeDestroy(handle);
    }

    @Override
    public void bleTransition(long handle, String transition) {
      EchoBridge.nativeBleTransition(handle, transition);
    }

    @Override
    public String stagedStep(long handle, String line) {
      return EchoBridge.nativeStagedStep(handle, line);
    }

    @Override
    public String stagedDrainLog(long handle) {
      return EchoBridge.nativeStagedDrainLog(handle);
    }

    @Override
    public String stagedCounters(long handle) {
      return EchoBridge.nativeStagedCounters(handle);
    }

    @Override
    public String bleScanStart(long handle, String owner, String timeoutMs, String nowMs) {
      return EchoBridge.nativeBleScanStart(handle, owner, timeoutMs, nowMs);
    }

    @Override
    public String bleScanTake(long handle) {
      return EchoBridge.nativeBleScanTake(handle);
    }

    @Override
    public String bleScanStop(long handle, String opId, String nowMs) {
      return EchoBridge.nativeBleScanStop(handle, opId, nowMs);
    }
  }
}
