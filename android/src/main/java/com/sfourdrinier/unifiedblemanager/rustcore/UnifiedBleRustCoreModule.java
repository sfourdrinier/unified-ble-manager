// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/UnifiedBleRustCoreModule.java

package com.sfourdrinier.unifiedblemanager.rustcore;

import android.util.Log;

import androidx.annotation.NonNull;

import com.facebook.react.bridge.Arguments;
import com.facebook.react.bridge.Promise;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.bridge.WritableMap;
import com.facebook.react.module.annotations.ReactModule;
import com.sfourdrinier.unifiedblemanager.NativeUnifiedBleRustCoreSpec;

import java.security.SecureRandom;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * The {@code UnifiedBleRustCore} TurboModule (frozen contract,
 * {@code src/NativeUnifiedBleRustCore.ts}): a thin shell over
 * {@link RustCoreSessions}. It maps promises and the wake emitter; every
 * decision lives in the JVM-tested classes and in Rust. There is no legacy
 * route behind this module.
 */
@ReactModule(name = UnifiedBleRustCoreModule.NAME)
public class UnifiedBleRustCoreModule extends NativeUnifiedBleRustCoreSpec {
  public static final String NAME = "UnifiedBleRustCore";
  private static final String TAG = "UnifiedBleRustCore";

  private final ExecutorService executor =
      Executors.newSingleThreadExecutor(runnable -> new Thread(runnable, "ubm-rust-core-module"));
  private final RustCoreProcessHost host;
  private final ReactCompanionChooser companionChooser;
  private final RustCoreSessions sessions;

  public UnifiedBleRustCoreModule(ReactApplicationContext reactContext) {
    super(reactContext);
    this.host = RustCoreProcessHost.shared(reactContext);
    this.companionChooser = new ReactCompanionChooser(reactContext);
    host.attachCompanionChooser(companionChooser);
    this.sessions = new RustCoreSessions(
        JniMobileCorePort.INSTANCE,
        host,
        executor,
        sessionId -> {
          emitWake(sessionId);
          return kotlin.Unit.INSTANCE;
        },
        reactContext::getPackageName,
        new SecureRandom(),
        message -> {
          Log.w(TAG, message);
          return kotlin.Unit.INSTANCE;
        });
  }

  @NonNull
  @Override
  public String getName() {
    return NAME;
  }

  @Override
  public void openSession(String owner, String expectedWireRevision, Promise promise) {
    sessions.openSession(owner, expectedWireRevision, reply(promise));
  }

  @Override
  public void invoke(String sessionId, String op, String argsJson, Promise promise) {
    sessions.invoke(sessionId, op, argsJson, reply(promise));
  }

  @Override
  public void drain(String sessionId, double maxItems, double maxBytes, Promise promise) {
    sessions.drain(sessionId, maxItems, maxBytes, reply(promise));
  }

  @Override
  public void closeSession(String sessionId, Promise promise) {
    sessions.closeSession(sessionId, reply(promise));
  }

  @Override
  public void nativeBuildIdentity(Promise promise) {
    sessions.nativeBuildIdentity(reply(promise));
  }

  @Override
  public void contractRevision(Promise promise) {
    sessions.contractRevision(reply(promise));
  }

  @Override
  public void wireRevision(Promise promise) {
    sessions.wireRevision(reply(promise));
  }

  @Override
  public void randomBytes(double length, Promise promise) {
    sessions.randomBytes(length, reply(promise));
  }

  @Override
  public void restorationIdentity(String requestJson, Promise promise) {
    sessions.restorationIdentity(requestJson, reply(promise));
  }

  @Override
  public void declareBackgroundContinuation(String declarationJson, Promise promise) {
    sessions.declareContinuation(declarationJson, reply(promise));
  }

  @Override
  public void continuationStatus(Promise promise) {
    sessions.continuationStatus(reply(promise));
  }

  @Override
  public void prepareContinuationClaim(double maxItems, double maxBytes, Promise promise) {
    sessions.prepareContinuationClaim(maxItems, maxBytes, reply(promise));
  }

  @Override
  public void acknowledgeContinuationClaim(String claimToken, Promise promise) {
    sessions.acknowledgeContinuationClaim(claimToken, reply(promise));
  }

  @Override
  public void invalidate() {
    sessions.invalidate();
    host.detachCompanionChooser(companionChooser);
    companionChooser.detach();
    executor.shutdown();
    super.invalidate();
  }

  private void emitWake(String sessionId) {
    WritableMap payload = Arguments.createMap();
    payload.putString("sessionId", sessionId);
    emitOnSessionWake(payload);
  }

  private static RustCoreSessions.Reply reply(Promise promise) {
    return new RustCoreSessions.Reply() {
      @Override
      public void resolve(String value) {
        promise.resolve(value);
      }

      @Override
      public void reject(RustCoreRejection rejection) {
        promise.reject(rejection.getCode(), rejection.toJson());
      }
    };
  }
}
