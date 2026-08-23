package com.sfourdrinier.unifiedblemanager.expo;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageInfo;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;

import androidx.annotation.NonNull;

import com.facebook.react.bridge.Arguments;
import com.facebook.react.bridge.Promise;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.bridge.ReadableMap;
import com.facebook.react.bridge.WritableArray;
import com.facebook.react.bridge.WritableMap;
import com.facebook.react.modules.core.PermissionAwareActivity;
import com.facebook.react.modules.core.PermissionListener;
import com.facebook.react.module.annotations.ReactModule;
import com.sfourdrinier.unifiedblemanager.NativeUnifiedBleExpoRuntimeSpec;

import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

@ReactModule(name = UnifiedBleExpoRuntimeModule.NAME)
public final class UnifiedBleExpoRuntimeModule extends NativeUnifiedBleExpoRuntimeSpec {
  public static final String NAME = "UnifiedBleExpoRuntime";
  private static final String LEGACY_LOCATION_POLICY_METADATA =
      "com.sfourdrinier.unifiedblemanager.expo.legacy-location-policy";
  private static final String CONFIGURATION_MARKER_METADATA =
      "com.sfourdrinier.unifiedblemanager.expo.configuration-marker";
  private static final String CONFIGURATION_MARKER = "unified-ble-expo-v1";
  private static final String NEVER_FOR_LOCATION_METADATA =
      "com.sfourdrinier.unifiedblemanager.expo.never-for-location";
  private static final String REQUIRED_HARDWARE_METADATA =
      "com.sfourdrinier.unifiedblemanager.expo.required-hardware";
  private static final int PERMISSION_REQUEST_CODE = 0x5543;

  private final ReactApplicationContext reactContext;
  private final PermissionListener permissionListener = this::onRequestPermissionsResult;
  private Promise pendingPermission;
  private String[] pendingPermissions;

  public UnifiedBleExpoRuntimeModule(ReactApplicationContext reactContext) {
    super(reactContext);
    this.reactContext = reactContext;
  }

  @Override
  @NonNull
  public String getName() {
    return NAME;
  }

  @Override
  public synchronized void getRuntimeConfiguration(Promise promise) {
    try {
      requireConfigurationMarker();
      final String legacyLocationPolicy = legacyLocationPolicy();
      final WritableMap result = Arguments.createMap();
      result.putString("platform", "android");
      result.putString("configurationDigest", configurationDigest(legacyLocationPolicy));
      result.putString("legacyLocationPolicy", legacyLocationPolicy);
      promise.resolve(result);
    } catch (RuntimeException error) {
      promise.reject("nativeConfigurationInvalid", error.getMessage(), error);
    }
  }

  @Override
  public synchronized void requestPermissions(ReadableMap request, Promise promise) {
    try {
      if (!request.hasKey("purpose") || !"scan-and-connect".equals(request.getString("purpose"))) {
        throw new IllegalArgumentException("The Expo permission purpose must be scan-and-connect.");
      }
      final String[] requiredPermissions = runtimePermissions();
      final List<String> missingPermissions = new ArrayList<>();
      for (String permission : requiredPermissions) {
        if (reactContext.checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED) {
          missingPermissions.add(permission);
        }
      }
      if (missingPermissions.isEmpty()) {
        promise.resolve(permissionResult(true));
        return;
      }
      final Activity activity = reactContext.getCurrentActivity();
      if (!(activity instanceof PermissionAwareActivity)) {
        throw new IllegalStateException(
            "A foreground React Native Activity that supports permission requests is required.");
      }
      if (pendingPermission != null) {
        throw new IllegalStateException("An Expo Bluetooth permission request is already in progress.");
      }
      pendingPermission = promise;
      pendingPermissions = missingPermissions.toArray(new String[0]);
      ((PermissionAwareActivity) activity).requestPermissions(
          pendingPermissions, PERMISSION_REQUEST_CODE, permissionListener);
    } catch (RuntimeException error) {
      promise.reject(permissionErrorCode(error), error.getMessage(), error);
    }
  }

  @Override
  public synchronized void openSettings(ReadableMap request, Promise promise) {
    try {
      final String target = request.hasKey("target") ? request.getString("target") : null;
      final Intent intent;
      if ("app".equals(target)) {
        intent = new Intent(
            Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
            Uri.parse("package:" + reactContext.getPackageName()));
      } else if ("bluetooth".equals(target)) {
        intent = new Intent(Settings.ACTION_BLUETOOTH_SETTINGS);
      } else if ("location-services".equals(target)) {
        intent = new Intent(Settings.ACTION_LOCATION_SOURCE_SETTINGS);
      } else {
        throw new IllegalArgumentException("The Expo settings target is invalid.");
      }
      intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
      if (intent.resolveActivity(reactContext.getPackageManager()) == null) {
        throw new UnsupportedOperationException("The requested Android settings screen is unavailable.");
      }
      reactContext.startActivity(intent);
      promise.resolve(null);
    } catch (RuntimeException error) {
      promise.reject("settingsUnavailable", error.getMessage(), error);
    }
  }

  private synchronized boolean onRequestPermissionsResult(
      int requestCode,
      String[] permissions,
      int[] grantResults) {
    if (requestCode != PERMISSION_REQUEST_CODE || pendingPermission == null || pendingPermissions == null) {
      return false;
    }
    final Promise promise = pendingPermission;
    final String[] requested = pendingPermissions;
    pendingPermission = null;
    pendingPermissions = null;
    boolean granted = true;
    for (String permission : requested) {
      if (reactContext.checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED) {
        granted = false;
        break;
      }
    }
    promise.resolve(permissionResult(granted));
    return true;
  }

  private String[] runtimePermissions() {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      requireDeclaredPermissions(Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT);
      return new String[] { Manifest.permission.BLUETOOTH_SCAN, Manifest.permission.BLUETOOTH_CONNECT };
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
      if ("none".equals(legacyLocationPolicy())) {
        throw new IllegalStateException(
            "Android API 23-30 BLE scanning needs legacy location permission; configure permissions.android.legacyLocation and rebuild.");
      }
      requireDeclaredPermissions(
          Manifest.permission.ACCESS_COARSE_LOCATION,
          Manifest.permission.ACCESS_FINE_LOCATION);
      return new String[] { Manifest.permission.ACCESS_COARSE_LOCATION, Manifest.permission.ACCESS_FINE_LOCATION };
    }
    requireDeclaredPermissions(Manifest.permission.BLUETOOTH);
    return new String[0];
  }

  private void requireDeclaredPermissions(String... permissions) {
    final PackageInfo packageInfo;
    try {
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        packageInfo = reactContext.getPackageManager().getPackageInfo(
            reactContext.getPackageName(),
            PackageManager.PackageInfoFlags.of(PackageManager.GET_PERMISSIONS));
      } else {
        packageInfo = reactContext.getPackageManager().getPackageInfo(
            reactContext.getPackageName(), PackageManager.GET_PERMISSIONS);
      }
    } catch (PackageManager.NameNotFoundException error) {
      throw new IllegalStateException("The Android application package cannot be inspected.", error);
    }
    final List<String> declared = packageInfo.requestedPermissions == null
        ? List.of()
        : Arrays.asList(packageInfo.requestedPermissions);
    for (String permission : permissions) {
      if (!declared.contains(permission)) {
        throw new IllegalStateException(
            "The Android permission " + permission + " is not declared; rebuild with the Unified BLE Expo plugin.");
      }
    }
  }

  private WritableMap permissionResult(boolean granted) {
    final WritableMap result = Arguments.createMap();
    final WritableArray requested = Arguments.createArray();
    requested.pushString("bluetooth");
    result.putArray("requested", requested);
    final WritableArray grantedPermissions = Arguments.createArray();
    final WritableArray deniedPermissions = Arguments.createArray();
    if (granted) grantedPermissions.pushString("bluetooth");
    else deniedPermissions.pushString("bluetooth");
    result.putArray("granted", grantedPermissions);
    result.putArray("denied", deniedPermissions);
    result.putString("recommendedSettingsTarget", granted ? null : "app");
    return result;
  }

  private String legacyLocationPolicy() {
    final Bundle metadata = applicationMetadata();
    final Object value = metadata.get(LEGACY_LOCATION_POLICY_METADATA);
    if (value == null) return "none";
    if (!(value instanceof String) ||
        !("auto".equals(value) || "required".equals(value) || "none".equals(value))) {
      throw new IllegalStateException("The native Android legacy location policy is invalid; rebuild the app.");
    }
    return (String) value;
  }

  private void requireConfigurationMarker() {
    final Object marker = applicationMetadata().get(CONFIGURATION_MARKER_METADATA);
    if (!CONFIGURATION_MARKER.equals(marker)) {
      throw new IllegalStateException(
          "The Unified BLE Expo plugin configuration marker is absent; run expo prebuild and rebuild the native app.");
    }
  }

  private Bundle applicationMetadata() {
    try {
      final ApplicationInfo applicationInfo = reactContext.getPackageManager().getApplicationInfo(
          reactContext.getPackageName(), PackageManager.GET_META_DATA);
      return applicationInfo.metaData == null ? Bundle.EMPTY : applicationInfo.metaData;
    } catch (PackageManager.NameNotFoundException error) {
      throw new IllegalStateException("The Android application package cannot be inspected.", error);
    }
  }

  private String configurationDigest(String legacyLocationPolicy) {
    final Bundle metadata = applicationMetadata();
    final String canonical = "unified-ble-expo-runtime-v1\n"
        + "platform=android\n"
        + "legacyLocationPolicy=" + legacyLocationPolicy + "\n"
        + "neverForLocation=" + metadata.getString(NEVER_FOR_LOCATION_METADATA, "false") + "\n"
        + "requiredHardware=" + metadata.getString(REQUIRED_HARDWARE_METADATA, "false") + "\n";
    try {
      final byte[] digest = MessageDigest.getInstance("SHA-256")
          .digest(canonical.getBytes(StandardCharsets.UTF_8));
      final StringBuilder result = new StringBuilder(digest.length * 2);
      for (byte value : digest) result.append(String.format("%02x", value & 0xff));
      return result.toString();
    } catch (NoSuchAlgorithmException error) {
      throw new IllegalStateException("Android SHA-256 support is unavailable.", error);
    }
  }

  private String permissionErrorCode(RuntimeException error) {
    if (error.getMessage() != null && error.getMessage().contains("permission") &&
        error.getMessage().contains("not declared")) {
      return "permissionNotDeclared";
    }
    if (error instanceof UnsupportedOperationException) return "permissionUnsupported";
    if (error instanceof IllegalStateException) return "permissionActivityUnavailable";
    return "permissionRequestFailed";
  }
}
