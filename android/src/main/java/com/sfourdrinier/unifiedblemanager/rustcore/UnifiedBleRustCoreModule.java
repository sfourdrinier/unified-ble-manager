// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/UnifiedBleRustCoreModule.java

package com.sfourdrinier.unifiedblemanager.rustcore;

import android.util.Log;

import androidx.annotation.NonNull;

import com.facebook.react.bridge.Arguments;
import com.facebook.react.bridge.Promise;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.bridge.ReactMethod;
import com.facebook.react.bridge.WritableMap;
import com.facebook.react.turbomodule.core.CallInvokerHolderImpl;
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

  static {
    // This is a distinct JSI component from the radio protocol runtime. The
    // RustCore module owns its lifecycle so a scan-recovery page cannot outlive
    // its exact process-host binding.
    System.loadLibrary("unified_ble_native_protocol");
  }

  private final ExecutorService executor =
      Executors.newSingleThreadExecutor(runnable -> new Thread(runnable, "ubm-rust-core-module"));
  private final RustCoreProcessHost host;
  private final ReactCompanionChooser companionChooser;
  private final RustCoreSessions sessions;
  private final Runnable recoveryObserver = this::emitRecoveryHint;
  private final kotlin.jvm.functions.Function1<Long, kotlin.Unit> scanRecoverySelectionObserver =
      generation -> {
        installSelectedScanFailureRecoveryBinaryRuntime();
        return kotlin.Unit.INSTANCE;
      };
  private final int scanRecoveryJsiOwnerId = System.identityHashCode(this);

  public UnifiedBleRustCoreModule(ReactApplicationContext reactContext) {
    super(reactContext);
    this.host = RustCoreProcessHost.shared(reactContext);
    this.companionChooser = new ReactCompanionChooser(reactContext);
    host.attachCompanionChooser(companionChooser);
    host.addRecoveryObserver(recoveryObserver);
    host.addScanFailureRecoverySelectionObserver(scanRecoverySelectionObserver);
    this.sessions = new RustCoreSessions(
        host.getCore(),
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
    installSelectedScanFailureRecoveryBinaryRuntime();
  }

  /** Called only by the dedicated JSI HostObject on the JS thread. */
  private com.ubm.core.MobileCoreBridge.ScanFailureRecoveryPage copySelectedScanFailureRecoveryPage(
      String headId, String offset, long selectionGeneration, long binding) {
    if (headId == null || headId.isEmpty() || offset == null || offset.isEmpty()) {
      throw new IllegalArgumentException("scan recovery binary page requires opaque head and offset handles");
    }
    return host.readSelectedScanFailureRecoveryPage(
        headId, offset, selectionGeneration, binding);
  }

  /** Called only by the RustCore JSI HostObject; owner ids stay opaque. */
  private com.ubm.core.MobileCoreBridge.ScanFailureRecoveryRawToken copySelectedScanFailureRecoveryRawToken(
      String ownerId, int tokenIndex, String offset, String maxBytes, long selectionGeneration, long binding) {
    if (ownerId == null || ownerId.isEmpty() || tokenIndex < 0 || offset == null || maxBytes == null)
      throw new IllegalArgumentException("invalid opaque raw recovery token request");
    return host.readSelectedScanFailureRecoveryRawToken(
        ownerId, tokenIndex, offset, maxBytes, selectionGeneration, binding);
  }

  private void installSelectedScanFailureRecoveryBinaryRuntime() {
    final CallInvokerHolderImpl jsCallInvokerHolder =
        (CallInvokerHolderImpl) getReactApplicationContext().getJSCallInvokerHolder();
    if (jsCallInvokerHolder == null) {
      throw new IllegalStateException("React Native JS CallInvoker is unavailable");
    }
    try {
      final SelectedScanFailureRecoveryBinding selection =
          host.selectedScanFailureRecoveryBinding();
      installScanFailureRecoveryBinaryRuntime(
          jsCallInvokerHolder,
          scanRecoveryJsiOwnerId,
          selection.getGeneration(),
          selection.getBinding());
    } catch (RustCoreRejection unavailable) {
      if (!"capability.unsupported".equals(unavailable.getCode())) throw unavailable;
    } catch (IllegalStateException unavailable) {
      // No native bootstrap selection is a supported ordinary state.
    }
  }

  /** Installs a RustCore-owned global HostObject; never reuses the radio transport. */
  private native void installScanFailureRecoveryBinaryRuntime(
      CallInvokerHolderImpl jsCallInvokerHolder,
      int ownerId,
      long selectionGeneration,
      long binding);

  /** Removes the HostObject before this module releases its process-host route. */
  private native void uninstallScanFailureRecoveryBinaryRuntime(int ownerId);

  @NonNull
  @Override
  public String getName() {
    return NAME;
  }

  @Override
  public void configureTerminalEvidenceMaximumBytes(String maximumBytes, Promise promise) {
    executor.execute(() -> {
      try {
        if (maximumBytes == null || !maximumBytes.matches("[1-9][0-9]*"))
          throw new IllegalArgumentException("terminal evidence maximum must be a positive decimal integer");
        MobileCoreBridge.nativeConfigureTerminalEvidenceMaximumBytes(Long.parseLong(maximumBytes));
        promise.resolve(null);
      } catch (Throwable error) {
        promise.reject("UnifiedBleRustCore", error.getMessage(), error);
      }
    });
  }

  @Override
  public void openSession(String owner, String expectedWireRevision, Promise promise) {
    sessions.openSession(owner, expectedWireRevision, reply(promise));
  }

  @Override
  public void chooseAccessory(String requestId, String optionsJson, double timeoutMs, Promise promise) {
    promise.reject("UnifiedBleRustCore", "{\"code\":\"capability.unsupported\",\"domain\":\"chooser\",\"operation\":\"accessory.choose\",\"detail\":\"Use Android companion association for OS accessory setup\"}");
  }

  @Override
  public void cancelAccessoryChoice(String requestId, Promise promise) { promise.resolve(null); }

  @Override
  public void accessoryChooserAvailable(Promise promise) { promise.resolve(companionChooser.available()); }

  @Override
  public void authorizedAccessories(Promise promise) {
    promise.reject("UnifiedBleRustCore", "{\"code\":\"capability.unsupported\",\"domain\":\"capability\",\"operation\":\"accessory.authorized\",\"detail\":\"AccessorySetupKit saved accessories are unavailable on Android\"}");
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
  public void negotiateScanEndVersion(String sessionId, String offeredJson, Promise promise) {
    sessions.negotiateScanEndVersion(sessionId, offeredJson, reply(promise));
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

  @Override public void orderedRecordingVersion(Promise promise) {
    sessions.orderedRecordingVersion(new RustCoreSessions.Reply() {
      @Override public void resolve(String value) { promise.resolve(Integer.valueOf(value)); }
      @Override public void reject(RustCoreRejection error) { promise.reject(error.getCode(), error.toJson()); }
    });
  }
  @Override public void orderedRecordingReserve(String sessionId, String id, Promise promise) {
    sessions.orderedRecordingReceiptControl(sessionId, id, "reserve", reply(promise));
  }
  @Override public void orderedRecordingStartStatus(String sessionId, String id, Promise promise) {
    sessions.orderedRecordingReceiptControl(sessionId, id, "status", reply(promise));
  }
  @Override public void orderedRecordingRetire(String sessionId, String id, Promise promise) {
    sessions.orderedRecordingReceiptControl(sessionId, id, "retire", reply(promise));
  }
  @Override public void orderedRecordingStart(String sessionId, String handle, String id, String maxBytes, String maxRecords,
                                               String declarationJson, double budgetMs, Promise promise) {
    sessions.orderedRecordingStart(sessionId, handle, id, maxBytes, maxRecords, declarationJson, budgetMs, reply(promise));
  }
  @Override public void orderedRecordingContext(String sessionId, String id, Promise promise) {
    sessions.orderedRecordingControl(sessionId, id, false, reply(promise));
  }
  @Override public void orderedRecordingStop(String sessionId, String id, Promise promise) {
    sessions.orderedRecordingControl(sessionId, id, true, reply(promise));
  }
  @Override public void orderedRecordingOfflineContext(String id, Promise promise) {
    sessions.orderedRecordingOffline("context", id, reply(promise));
  }
  @Override public void orderedRecordingOfflineStatus(String id, Promise promise) {
    sessions.orderedRecordingOffline("status", id, reply(promise));
  }
  @Override public void orderedRecordingOfflineStop(String id, Promise promise) {
    sessions.orderedRecordingOffline("stop", id, reply(promise));
  }

  @Override public void continuationRecordingStatus(String id, Promise promise) {
    sessions.recordingControl("status", id, "", 0, 0, reply(promise));
  }
  @Override public void continuationRecordingPrepare(String id, double maxItems, double maxBytes, Promise promise) {
    sessions.recordingControl("prepare", id, "", maxItems, maxBytes, reply(promise));
  }
  @Override public void continuationRecordingAcknowledge(String id, String token, Promise promise) {
    sessions.recordingControl("acknowledge", id, token, 0, 0, reply(promise));
  }
  @Override public void continuationRecordingStop(String id, Promise promise) {
    sessions.recordingControl("stop", id, "", 0, 0, reply(promise));
  }
  @Override public void continuationRecordingClear(String id, Promise promise) {
    sessions.recordingControl("clear", id, "", 0, 0, reply(promise));
  }
  @Override public void notificationFailureSpoolConfigure(String recordingId, String maxBytes, String maxRecords, Promise promise) {
    promise.resolve("unsupported");
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
  public void prepareRetainedContinuationClaimV1(double maxItems, double maxBytes, Promise promise) {
    sessions.prepareRetainedContinuationClaimV1(maxItems, maxBytes, reply(promise));
  }

  @Override
  public void acknowledgeRetainedContinuationClaimV1(String token, Promise promise) {
    sessions.acknowledgeRetainedContinuationClaimV1(token, reply(promise));
  }

  @Override
  public void prepareRetainedContinuationClaimV2(double maxItems, double maxBytes, Promise promise) {
    sessions.prepareRetainedContinuationClaimV2(maxItems, maxBytes, reply(promise));
  }

  @Override
  public void acknowledgeRetainedContinuationClaimV2(String token, Promise promise) {
    sessions.acknowledgeRetainedContinuationClaimV2(token, reply(promise));
  }

  @Override
  public void readPeerFailureRecovery(String recoveryRecord, String offset, Promise promise) {
    sessions.readPeerFailureRecovery(recoveryRecord, offset, reply(promise));
  }

  @Override
  public void acknowledgePeerFailureRecovery(String recoveryRecord, Promise promise) {
    sessions.acknowledgePeerFailureRecovery(recoveryRecord, reply(promise));
  }

  @Override
  public void retireRetainedContinuationClaimReceiptV1(String token, Promise promise) {
    sessions.retireRetainedContinuationClaimReceiptV1(token, reply(promise));
  }

  @Override public void scanFailureRecoveryOpenSelected(Promise promise) {
    try { promise.resolve(host.openSelectedScanFailureRecovery()); }
    catch (RustCoreRejection error) { promise.reject(error.getCode(), error.toJson()); }
  }
  @Override public void scanFailureRecoveryOwners(String leaseId, Promise promise) {
    try { promise.resolve(host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).scanFailureRecoveryOwners(leaseId)); }
    catch (RuntimeException error) { promise.reject("scanFailureRecoveryOwners", error.getMessage(), error); }
  }
  @Override public void scanFailureRecoveryMember(String ownerHandle, double index, Promise promise) {
    try {
      if (index < 0 || index != Math.rint(index)) throw new IllegalArgumentException("member index must be a non-negative integer");
      promise.resolve(host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).scanFailureRecoveryMember(ownerHandle, (int) index));
    }
    catch (RuntimeException error) { promise.reject("scanFailureRecoveryMember", error.getMessage(), error); }
  }
  @Override public void scanFailureRecoveryHead(String ownerHandle, double index, Promise promise) {
    try {
      if (index < 0 || index != Math.rint(index) || index > Integer.MAX_VALUE)
        throw new IllegalArgumentException("member index must be a non-negative safe integer");
      promise.resolve(host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).scanFailureRecoveryHead(ownerHandle, (int) index));
    }
    catch (RuntimeException error) { promise.reject("scanFailureRecoveryHead", error.getMessage(), error); }
  }
  @Override public void scanFailureRecoveryControl(String headHandle, String operation, Promise promise) {
    try { promise.resolve(host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).scanFailureRecoveryHeadControl(headHandle, operation)); }
    catch (RuntimeException error) { promise.reject("scanFailureRecoveryControl", error.getMessage(), error); }
  }
  @Override public void scanFailureRecoveryPendingCleanup(String leaseId, Promise promise) {
    try { promise.resolve(host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).scanFailureRecoveryPendingCleanup(leaseId)); }
    catch (RuntimeException error) { promise.reject("scanFailureRecoveryPendingCleanup", error.getMessage(), error); }
  }
  @Override public void scanFailureRecoveryRetireCleanup(String cleanupReceiptId, Promise promise) {
    try { host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).scanFailureRecoveryRetireCleanup(cleanupReceiptId); promise.resolve(null); }
    catch (RuntimeException error) { promise.reject("scanFailureRecoveryRetireCleanup", error.getMessage(), error); }
  }
  @Override public void scanFailureRecoveryRawInfo(String ownerHandle, Promise promise) {
    try { promise.resolve(host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).rawInfo(ownerHandle)); }
    catch (RuntimeException error) { promise.reject("scanFailureRecoveryRawInfo", error.getMessage(), error); }
  }
  @Override public void scanFailureRecoveryRawMember(String ownerHandle, double index, Promise promise) {
    try {
      if (index < 0 || index != Math.rint(index) || index > Integer.MAX_VALUE)
        throw new IllegalArgumentException("raw member index must be a non-negative safe integer");
      promise.resolve(host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).rawMember(ownerHandle, (int) index));
    } catch (RuntimeException error) { promise.reject("scanFailureRecoveryRawMember", error.getMessage(), error); }
  }
  @Override public void scanFailureRecoveryRawControl(String ownerHandle, String operation, Promise promise) {
    try { promise.resolve(host.scanFailureRecoveryPort(host.selectedScanFailureRecoveryRecordingId()).rawControl(ownerHandle, operation)); }
    catch (RuntimeException error) { promise.reject("scanFailureRecoveryRawControl", error.getMessage(), error); }
  }

  @Override
  public void invalidate() {
    uninstallScanFailureRecoveryBinaryRuntime(scanRecoveryJsiOwnerId);
    host.removeScanFailureRecoverySelectionObserver(scanRecoverySelectionObserver);
    host.removeRecoveryObserver(recoveryObserver);
    sessions.invalidate();
    host.detachCompanionChooser(companionChooser);
    companionChooser.detach();
    executor.shutdown();
    super.invalidate();
  }

  private void emitRecoveryHint() {
    WritableMap payload = Arguments.createMap();
    payload.putString("kind", "cleanup");
    emitOnRecoveryHint(payload);
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
