// android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java

package com.sfourdrinier.unifiedblemanager.protocol;

import android.os.Build;
import android.app.Activity;
import android.bluetooth.BluetoothDevice;
import android.companion.AssociationRequest;
import android.companion.AssociationInfo;
import android.companion.CompanionDeviceManager;
import android.companion.BluetoothDeviceFilter;
import android.content.IntentSender;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.ParcelUuid;
import android.util.Log;

import androidx.annotation.NonNull;

import com.facebook.react.bridge.Arguments;
import com.facebook.react.bridge.Promise;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.bridge.ReadableMap;
import com.facebook.react.bridge.RuntimeExecutor;
import com.facebook.react.bridge.WritableMap;
import com.facebook.react.bridge.ActivityEventListener;
import com.facebook.react.module.annotations.ReactModule;
import com.sfourdrinier.unifiedblemanager.NativeUnifiedBleProtocolControlSpec;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.regex.Pattern;

import com.sfourdrinier.unifiedblemanager.background.AndroidConnectedDeviceForegroundServiceDriver;
import com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry;
import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceControlException;
@ReactModule(name = UnifiedBleProtocolControlModule.NAME)
public final class UnifiedBleProtocolControlModule extends NativeUnifiedBleProtocolControlSpec
    implements ActivityEventListener {
  public static final String NAME = "UnifiedBleProtocolControl";
  private static final String TAG = "UnifiedBleProtocol";
  private static final int NATIVE_PROTOCOL_VERSION =
      com.sfourdrinier.unifiedblemanager.protocol.generated.NativeProtocolV2SchemaKt.NATIVE_PROTOCOL_VERSION;
  private static final int ABI_VERSION =
      com.sfourdrinier.unifiedblemanager.protocol.generated.NativeProtocolV2SchemaKt.NATIVE_PROTOCOL_ABI_VERSION;
  private static final int CONTROL_SURFACE_VERSION =
      com.sfourdrinier.unifiedblemanager.protocol.generated.NativeProtocolV2SchemaKt.NATIVE_PROTOCOL_CONTROL_SURFACE_VERSION;
  private static final int CONTRACT_VERSION = 1;
  private static final int MAXIMUM_CONTROL_RECORD_BYTES = 262144;
  private static final int MAXIMUM_BINARY_PAYLOAD_BYTES = 524288;
  private static final double MAXIMUM_SAFE_INTEGER = 9007199254740991.0;
  private static final String RESTORATION_DOMAIN = "ubm-restoration-v1";
  private static final Pattern RESTORATION_TOKEN = Pattern.compile("[A-Za-z0-9][A-Za-z0-9._-]{0,127}");
  private static final char[] URL_ALPHABET =
      "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_".toCharArray();

  static {
    System.loadLibrary("unified_ble_native_protocol");
  }

  private long nativeHandle = nativeCreate();
  private final ReactApplicationContext reactContext;
  private AttachmentIdentity attachment;
  private String ownerId;
  private final ConnectedDeviceForegroundServiceLeaseRegistry backgroundLeases;
  private long nextBackgroundLease = 1L;
  private static final int ASSOCIATION_REQUEST_CODE = 0x5542;
  private Promise pendingAssociation;
  private int pendingAssociationId = 0;

  public UnifiedBleProtocolControlModule(ReactApplicationContext reactContext) {
    super(reactContext);
    this.reactContext = reactContext;
    this.backgroundLeases = new ConnectedDeviceForegroundServiceLeaseRegistry(
        new AndroidConnectedDeviceForegroundServiceDriver(reactContext),
        () -> "background-" + Long.toString(nextBackgroundLease++));
    reactContext.addActivityEventListener(this);
  }

  @Override
  @NonNull
  public String getName() {
    return NAME;
  }

  @Override
  public synchronized void bootstrapRestorationIdentity(ReadableMap request, Promise promise) {
    try {
      final String restorationId = requiredRestorationToken(request, "restorationId", 128);
      final String generation = requiredRestorationToken(request, "generation", 64);
      final String applicationId = requiredStringValue(reactContext.getPackageName(), "applicationId");
      final String restore = deriveRestorationValue(applicationId, restorationId, generation, "restore");
      final WritableMap result = Arguments.createMap();
      result.putString("applicationId", applicationId);
      result.putString("restorationId", restorationId);
      result.putString("generation", generation);
      result.putString("restoreIdentifier", applicationId + ".ubm." + restore.substring(0, 22));
      result.putString("namespaceValue", "ubm-ns:" + deriveRestorationValue(applicationId, restorationId, generation, "namespace"));
      result.putString("clientId", "ubm-client:" + deriveRestorationValue(applicationId, restorationId, generation, "client"));
      result.putString("hostSessionScope", "ubm-host:" + deriveRestorationValue(applicationId, restorationId, generation, "host"));
      promise.resolve(result);
    } catch (RuntimeException error) {
      Log.e(TAG, "bootstrapRestorationIdentity failed", error);
      promise.reject("nativeRestorationBootstrap", error.getMessage(), error);
    }
  }

  @Override
  public synchronized void acquireBackground(ReadableMap request, Promise promise) {
    try {
      final String kind = requiredString(request, "kind");
      if (!"connected-device".equals(kind)) {
        throw new ForegroundServiceControlException(
            "invalidBackgroundRequest",
            "The background lease kind must be connected-device.");
      }
      final String reason = requiredString(request, "reason");
      final String leaseId = backgroundLeases.acquire(reason);
      final WritableMap result = Arguments.createMap();
      result.putString("leaseId", leaseId);
      promise.resolve(result);
    } catch (RuntimeException error) {
      Log.e(TAG, "acquireBackground failed", error);
      promise.reject(backgroundErrorCode(error, "nativeBackgroundAcquire"), error.getMessage(), error);
    }
  }

  @Override
  public synchronized void releaseBackground(ReadableMap request, Promise promise) {
    try {
      final String leaseId = requiredString(request, "leaseId");
      backgroundLeases.release(leaseId);
      promise.resolve(null);
    } catch (RuntimeException error) {
      Log.e(TAG, "releaseBackground failed", error);
      promise.reject(backgroundErrorCode(error, "nativeBackgroundRelease"), error.getMessage(), error);
    }
  }

  @Override
  public synchronized void associateCompanionDevice(ReadableMap request, Promise promise) {
    try {
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O ||
          !reactContext.getPackageManager().hasSystemFeature(PackageManager.FEATURE_COMPANION_DEVICE_SETUP)) {
        throw new ForegroundServiceControlException(
            "unsupportedAssociation",
            "Companion Device Manager association requires Android API 26 and companion-device setup support.");
      }
      if (pendingAssociation != null) {
        throw new ForegroundServiceControlException(
            "associationBusy", "A Companion Device Manager association is already in progress.");
      }
      final Activity activity = reactContext.getCurrentActivity();
      if (activity == null) {
        throw new ForegroundServiceControlException(
            "associationActivityUnavailable",
            "A foreground Activity is required to launch Companion Device Manager system UI.");
      }
      final String name = optionalBoundedString(request, "name", 128);
      final String serviceUuid = optionalBoundedString(request, "serviceUuid", 36);
      final BluetoothDeviceFilter.Builder filter = new BluetoothDeviceFilter.Builder();
      if (name != null) filter.setNamePattern(Pattern.compile(Pattern.quote(name)));
      if (serviceUuid != null) {
        filter.addServiceUuid(ParcelUuid.fromString(normalizeUuid(serviceUuid)), null);
      }
      final AssociationRequest associationRequest = new AssociationRequest.Builder()
          .addDeviceFilter(filter.build())
          .setSingleDevice(true)
          .build();
      pendingAssociation = promise;
      pendingAssociationId = 0;
      final CompanionDeviceManager manager =
          (CompanionDeviceManager) reactContext.getSystemService(android.content.Context.COMPANION_DEVICE_SERVICE);
      if (manager == null) throw new IllegalStateException("Companion Device Manager is unavailable.");
      manager.associate(associationRequest, new CompanionDeviceManager.Callback() {
        @Override
        public void onDeviceFound(IntentSender intentSender) {
          launchAssociationUi(activity, intentSender);
        }

        @Override
        public void onAssociationPending(IntentSender intentSender) {
          launchAssociationUi(activity, intentSender);
        }

        @Override
        public void onAssociationCreated(AssociationInfo associationInfo) {
          resolveAssociation(associationInfo.getId(), null, null);
        }

        @Override
        public void onFailure(CharSequence error) {
          rejectAssociation(
              "associationFailed",
              error == null ? "Companion Device Manager association failed." : error.toString());
        }
      }, null);
    } catch (RuntimeException error) {
      rejectAssociationPromise(promise, errorCode(error, "associationFailed"), error.getMessage(), error);
    }
  }

  @Override
  public synchronized void claimRestoration(Promise promise) {
    promise.reject(
        "unsupportedRestoration",
        "Android does not provide a native BLE restoration journal.");
  }

  private void launchAssociationUi(Activity activity, IntentSender intentSender) {
    try {
      activity.startIntentSenderForResult(
          intentSender, ASSOCIATION_REQUEST_CODE, null, 0, 0, 0);
    } catch (IntentSender.SendIntentException error) {
      rejectAssociation("associationUiLaunchFailed", "Companion Device Manager system UI could not be launched.");
    }
  }

  @Override
  public synchronized void onActivityResult(Activity activity, int requestCode, int resultCode, Intent data) {
    if (requestCode != ASSOCIATION_REQUEST_CODE || pendingAssociation == null) return;
    if (resultCode != Activity.RESULT_OK || data == null) {
      rejectAssociation("associationCancelled", "Companion Device Manager association was cancelled.");
      return;
    }
    final BluetoothDevice device;
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      device = data.getParcelableExtra(CompanionDeviceManager.EXTRA_DEVICE, BluetoothDevice.class);
    } else {
      device = data.getParcelableExtra(CompanionDeviceManager.EXTRA_DEVICE);
    }
    final String peerId = device == null ? null : device.getAddress();
    final String displayName = device == null ? null : device.getName();
    resolveAssociation(pendingAssociationId, peerId, displayName);
  }

  @Override
  public void onNewIntent(Intent intent) {}

  private void resolveAssociation(int associationId, String peerId, String displayName) {
    if (pendingAssociation == null) return;
    final Promise promise = pendingAssociation;
    pendingAssociation = null;
    final WritableMap result = Arguments.createMap();
    result.putString("source", "associated");
    result.putInt("associationId", associationId);
    if (peerId == null) result.putNull("peerId"); else result.putString("peerId", peerId);
    if (displayName == null) result.putNull("displayName"); else result.putString("displayName", displayName);
    promise.resolve(result);
  }

  private void rejectAssociation(String code, String message) {
    final Promise promise = pendingAssociation;
    pendingAssociation = null;
    if (promise != null) promise.reject(code, message);
  }

  private static void rejectAssociationPromise(Promise promise, String code, String message, Throwable error) {
    promise.reject(code, message == null ? "Companion Device Manager association failed." : message, error);
  }

  @Override
  public synchronized void handshake(ReadableMap request, Promise promise) {
    try {
      requireVersionRange(request.getMap("nativeProtocol"), "nativeProtocol", NATIVE_PROTOCOL_VERSION);
      requireVersionRange(request.getMap("abi"), "abi", ABI_VERSION);
      requireVersionRange(request.getMap("controlSurface"), "controlSurface", CONTROL_SURFACE_VERSION);
      requireVersionRange(request.getMap("backendContract"), "backendContract", CONTRACT_VERSION);
      requireVersionRange(request.getMap("capabilitySchema"), "capabilitySchema", CONTRACT_VERSION);
      requireVersionRange(request.getMap("eventSchema"), "eventSchema", CONTRACT_VERSION);
      requireVersionRange(request.getMap("traceFormat"), "traceFormat", CONTRACT_VERSION);
      final AttachmentIdentity requestedAttachment = attachmentFrom(request);
      final String requestedOwner = requiredString(request, "ownerId");
      if (attachment != null &&
          (!attachment.equals(requestedAttachment) || !requestedOwner.equals(ownerId))) {
        throw new IllegalStateException("An active native protocol attachment already owns this module");
      }
      nativeHandshake(
          nativeHandle,
          requestedAttachment.attachmentId,
          requestedAttachment.backendInstanceId,
          requestedAttachment.backendGeneration,
          requestedAttachment.adapterId,
          requestedAttachment.adapterGeneration,
          requestedOwner,
          versionRanges(request));
      attachment = requestedAttachment;
      ownerId = requestedOwner;
      final WritableMap result = Arguments.createMap();
      result.putInt("nativeProtocol", NATIVE_PROTOCOL_VERSION);
      result.putInt("abi", ABI_VERSION);
      result.putInt("controlSurface", CONTROL_SURFACE_VERSION);
      result.putInt("backendContract", CONTRACT_VERSION);
      result.putInt("capabilitySchema", CONTRACT_VERSION);
      result.putInt("eventSchema", CONTRACT_VERSION);
      result.putInt("traceFormat", CONTRACT_VERSION);
      result.putInt("maximumControlRecordBytes", MAXIMUM_CONTROL_RECORD_BYTES);
      result.putInt("maximumBinaryPayloadBytes", MAXIMUM_BINARY_PAYLOAD_BYTES);
      result.putBoolean("phyAvailable", Build.VERSION.SDK_INT >= Build.VERSION_CODES.O);
      result.putBoolean("securityAvailable", true);
      promise.resolve(result);
    } catch (RuntimeException error) {
      Log.e(TAG, "handshake failed", error);
      promise.reject("nativeProtocolHandshake", error.getMessage(), error);
    }
  }

  @Override
  public synchronized void installExecutionRuntime(Promise promise) {
    try {
      requireOpen();
      final RuntimeExecutor runtimeExecutor = reactContext.getCatalystInstance().getRuntimeExecutor();
      if (runtimeExecutor == null) {
        throw new IllegalStateException("React Native RuntimeExecutor is unavailable");
      }
      UnifiedBleProtocolJsiBinding.install(runtimeExecutor, nativeHandle, reactContext);
      promise.resolve(null);
    } catch (RuntimeException error) {
      Log.e(TAG, "installExecutionRuntime failed", error);
      promise.reject("nativeProtocolJsiInstall", error.getMessage(), error);
    }
  }

  @Override
  public synchronized void cancelOperation(ReadableMap correlation, Promise promise) {
    boolean nativeCancellationRecorded = false;
    try {
      requireCurrent(attachmentFrom(requiredMap(correlation, "attachment")));
      final AttachmentIdentity operationAttachment =
          attachmentFrom(requiredMap(correlation, "attachment"));
      final String state = nativeCancel(
          nativeHandle,
          operationAttachment.attachmentId,
          operationAttachment.backendInstanceId,
          operationAttachment.backendGeneration,
          operationAttachment.adapterId,
          operationAttachment.adapterGeneration,
          requiredPositiveInteger(correlation, "dispatchEpoch"),
          requiredString(correlation, "nonce"));
      if ("cancellationRequested".equals(state)) {
        nativeCancellationRecorded = true;
        UnifiedBleProtocolJsiBinding.cancelOperation(
            nativeHandle,
            requiredPositiveInteger(correlation, "dispatchEpoch"),
            requiredString(correlation, "nonce"));
      }
      final WritableMap result = Arguments.createMap();
      result.putString("state", state);
      promise.resolve(result);
    } catch (RuntimeException error) {
      Log.e(TAG, "cancelOperation failed", error);
      if (nativeCancellationRecorded) {
        boolean cleanupComplete = true;
        try {
          UnifiedBleProtocolJsiBinding.close(nativeHandle);
        } catch (RuntimeException cleanupError) {
          cleanupComplete = false;
          Log.e(TAG, "cancelOperation native dispatcher cleanup failed", cleanupError);
        }
        try {
          nativeClose(
              nativeHandle,
              attachment.attachmentId,
              attachment.backendInstanceId,
              attachment.backendGeneration,
              attachment.adapterId,
              attachment.adapterGeneration);
        } catch (RuntimeException cleanupError) {
          cleanupComplete = false;
          Log.e(TAG, "cancelOperation native runtime cleanup failed", cleanupError);
        }
        if (cleanupComplete) {
          closeOwnedState();
        }
      }
      promise.reject("invalidCorrelation", error.getMessage(), error);
    }
  }

  @Override
  public synchronized void adoptRestoration(ReadableMap request, Promise promise) {
    Log.w(TAG, "adoptRestoration is unavailable because Android has no native BLE restoration journal");
    promise.reject(
        "unsupportedRestoration",
        "Android does not provide a native BLE restoration journal");
  }

  @Override
  public synchronized void closeAttachment(ReadableMap requestedAttachment, Promise promise) {
    try {
      requireCurrent(attachmentFrom(requestedAttachment));
      UnifiedBleProtocolJsiBinding.close(nativeHandle);
      nativeClose(
          nativeHandle,
          attachment.attachmentId,
          attachment.backendInstanceId,
          attachment.backendGeneration,
          attachment.adapterId,
          attachment.adapterGeneration);
      closeOwnedState();
      promise.resolve(null);
    } catch (RuntimeException error) {
      Log.e(TAG, "closeAttachment failed", error);
      promise.reject("nativeProtocolClose", error.getMessage(), error);
    }
  }

  @Override
  public synchronized void invalidate() {
    try {
      backgroundLeases.close();
    } catch (RuntimeException error) {
      Log.e(TAG, "connected-device foreground service cleanup failed during invalidation", error);
    }
    if (nativeHandle != 0L) {
      UnifiedBleProtocolJsiBinding.close(nativeHandle);
      nativeDestroy(nativeHandle);
      nativeHandle = 0L;
    }
    attachment = null;
    ownerId = null;
    super.invalidate();
  }

  private void closeOwnedState() {
    attachment = null;
    ownerId = null;
  }

  private static String backgroundErrorCode(RuntimeException error, String fallback) {
    return error instanceof ForegroundServiceControlException
        ? ((ForegroundServiceControlException) error).code
        : fallback;
  }

  private void requireOpen() {
    if (attachment == null || ownerId == null) {
      throw new IllegalStateException("Native protocol attachment is not open");
    }
  }

  private void requireCurrent(AttachmentIdentity requestedAttachment) {
    requireOpen();
    if (!attachment.equals(requestedAttachment)) {
      throw new IllegalArgumentException("Native protocol attachment is stale");
    }
  }

  private static AttachmentIdentity attachmentFrom(ReadableMap map) {
    return new AttachmentIdentity(
        requiredString(map, "attachmentId"),
        requiredString(map, "backendInstanceId"),
        requiredString(map, "backendGeneration"),
        requiredString(map, "adapterId"),
        requiredString(map, "adapterGeneration"));
  }

  private static ReadableMap requiredMap(ReadableMap map, String key) {
    final ReadableMap value = map.getMap(key);
    if (value == null) {
      throw new IllegalArgumentException("Required native protocol map is missing: " + key);
    }
    return value;
  }

  private static String requiredString(ReadableMap map, String key) {
    final String value = map.getString(key);
    return requiredStringValue(value, key);
  }

  private static String requiredStringValue(String value, String key) {
    if (value == null || value.isEmpty()) {
      throw new IllegalArgumentException("Required native protocol string is missing: " + key);
    }
    return value;
  }

  private static String optionalBoundedString(ReadableMap map, String key, int maximumBytes) {
    if (!map.hasKey(key) || map.isNull(key)) return null;
    final String value = map.getString(key);
    if (value == null || value.trim().isEmpty() || value.getBytes(StandardCharsets.UTF_8).length > maximumBytes) {
      throw new IllegalArgumentException("Invalid association filter: " + key);
    }
    return value;
  }

  private static String normalizeUuid(String value) {
    if (value.matches("[0-9A-Fa-f]{4}")) return "0000" + value + "-0000-1000-8000-00805F9B34FB";
    if (value.matches("[0-9A-Fa-f]{8}")) return value + "-0000-1000-8000-00805F9B34FB";
    if (value.matches("[0-9A-Fa-f]{8}(-[0-9A-Fa-f]{4}){3}-[0-9A-Fa-f]{12}")) return value;
    throw new IllegalArgumentException("Association serviceUuid must be a valid Bluetooth UUID");
  }

  private static String errorCode(RuntimeException error, String fallback) {
    return error instanceof ForegroundServiceControlException
        ? ((ForegroundServiceControlException) error).code
        : fallback;
  }

  private static String requiredRestorationToken(ReadableMap map, String key, int maximumBytes) {
    final String value = requiredString(map, key);
    if (!RESTORATION_TOKEN.matcher(value).matches() || value.getBytes(StandardCharsets.UTF_8).length > maximumBytes) {
      throw new IllegalArgumentException("Invalid restoration token: " + key);
    }
    return value;
  }

  private static String deriveRestorationValue(
      String applicationId, String restorationId, String generation, String label) {
    final byte[] root = sha256(concatenate(
        utf8(RESTORATION_DOMAIN),
        lengthPrefixed(applicationId),
        lengthPrefixed(restorationId),
        lengthPrefixed(generation)));
    return base64Url(sha256(concatenate(root, new byte[] {0}, utf8(label))));
  }

  private static byte[] utf8(String value) {
    return value.getBytes(StandardCharsets.UTF_8);
  }

  private static byte[] lengthPrefixed(String value) {
    final byte[] bytes = utf8(value);
    final ByteArrayOutputStream output = new ByteArrayOutputStream(4 + bytes.length);
    output.write((bytes.length >>> 24) & 0xff);
    output.write((bytes.length >>> 16) & 0xff);
    output.write((bytes.length >>> 8) & 0xff);
    output.write(bytes.length & 0xff);
    output.write(bytes, 0, bytes.length);
    return output.toByteArray();
  }

  private static byte[] concatenate(byte[]... values) {
    final ByteArrayOutputStream output = new ByteArrayOutputStream();
    for (byte[] value : values) output.write(value, 0, value.length);
    return output.toByteArray();
  }

  private static byte[] sha256(byte[] value) {
    try {
      return MessageDigest.getInstance("SHA-256").digest(value);
    } catch (NoSuchAlgorithmException error) {
      throw new IllegalStateException("SHA-256 is unavailable", error);
    }
  }

  private static String base64Url(byte[] value) {
    final StringBuilder output = new StringBuilder((value.length * 4 + 2) / 3);
    for (int index = 0; index < value.length; index += 3) {
      final int first = value[index] & 0xff;
      output.append(URL_ALPHABET[first >>> 2]);
      if (index + 1 >= value.length) {
        output.append(URL_ALPHABET[(first & 0x03) << 4]);
        break;
      }
      final int second = value[index + 1] & 0xff;
      output.append(URL_ALPHABET[((first & 0x03) << 4) | (second >>> 4)]);
      if (index + 2 >= value.length) {
        output.append(URL_ALPHABET[(second & 0x0f) << 2]);
        break;
      }
      final int third = value[index + 2] & 0xff;
      output.append(URL_ALPHABET[((second & 0x0f) << 2) | (third >>> 6)]);
      output.append(URL_ALPHABET[third & 0x3f]);
    }
    return output.toString();
  }

  private static long requiredPositiveInteger(ReadableMap map, String key) {
    final double value = map.getDouble(key);
    if (!Double.isFinite(value) || value < 1.0 || value > MAXIMUM_SAFE_INTEGER || value != Math.rint(value)) {
      throw new IllegalArgumentException("Native protocol integer is invalid: " + key);
    }
    return (long) value;
  }

  private static void requireVersionRange(ReadableMap range, String axis, int selectedVersion) {
    if (range == null) {
      throw new IllegalArgumentException("Native protocol version range is missing: " + axis);
    }
    requireVersionRangeValues(
        requiredPositiveInteger(range, "minimum"),
        requiredPositiveInteger(range, "maximum"),
        axis,
        selectedVersion);
  }

  private static void requireVersionRangeValues(long minimum, long maximum, String axis, int selectedVersion) {
    if (minimum > maximum || minimum > selectedVersion || maximum < selectedVersion) {
      throw new IllegalArgumentException("Native protocol version range is incompatible: " + axis);
    }
  }

  private static long[] versionRanges(ReadableMap request) {
    final String[] axes = {
        "nativeProtocol",
        "abi",
        "controlSurface",
        "backendContract",
        "capabilitySchema",
        "eventSchema",
        "traceFormat"
    };
    final long[] ranges = new long[axes.length * 2];
    for (int index = 0; index < axes.length; index += 1) {
      final ReadableMap range = requiredMap(request, axes[index]);
      ranges[index * 2] = requiredPositiveInteger(range, "minimum");
      ranges[index * 2 + 1] = requiredPositiveInteger(range, "maximum");
    }
    return ranges;
  }

  private static final class AttachmentIdentity {
    private final String attachmentId;
    private final String backendInstanceId;
    private final String backendGeneration;
    private final String adapterId;
    private final String adapterGeneration;

    private AttachmentIdentity(
        String attachmentId,
        String backendInstanceId,
        String backendGeneration,
        String adapterId,
        String adapterGeneration) {
      this.attachmentId = attachmentId;
      this.backendInstanceId = backendInstanceId;
      this.backendGeneration = backendGeneration;
      this.adapterId = adapterId;
      this.adapterGeneration = adapterGeneration;
    }

    @Override
    public boolean equals(Object candidate) {
      if (!(candidate instanceof AttachmentIdentity)) {
        return false;
      }
      final AttachmentIdentity other = (AttachmentIdentity) candidate;
      return attachmentId.equals(other.attachmentId) &&
          backendInstanceId.equals(other.backendInstanceId) &&
          backendGeneration.equals(other.backendGeneration) &&
          adapterId.equals(other.adapterId) &&
          adapterGeneration.equals(other.adapterGeneration);
    }

    @Override
    public int hashCode() {
      int result = attachmentId.hashCode();
      result = 31 * result + backendInstanceId.hashCode();
      result = 31 * result + backendGeneration.hashCode();
      result = 31 * result + adapterId.hashCode();
      return 31 * result + adapterGeneration.hashCode();
    }
  }

  private static native long nativeCreate();
  private static native void nativeDestroy(long handle);
  private static native void nativeHandshake(
      long handle,
      String attachmentId,
      String backendInstanceId,
      String backendGeneration,
      String adapterId,
      String adapterGeneration,
      String ownerId,
      long[] versionRanges);
  private static native String nativeCancel(
      long handle,
      String attachmentId,
      String backendInstanceId,
      String backendGeneration,
      String adapterId,
      String adapterGeneration,
      long dispatchEpoch,
      String nonce);
  private static native void nativeClose(
      long handle,
      String attachmentId,
      String backendInstanceId,
      String backendGeneration,
      String adapterId,
      String adapterGeneration);
}
