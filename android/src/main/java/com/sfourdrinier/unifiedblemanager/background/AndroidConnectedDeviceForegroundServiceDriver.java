package com.sfourdrinier.unifiedblemanager.background;

import android.Manifest;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.ResultReceiver;

import com.facebook.react.bridge.ReactApplicationContext;
import com.sfourdrinier.unifiedblemanager.BlePlxForegroundService;

import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

public final class AndroidConnectedDeviceForegroundServiceDriver
    implements ConnectedDeviceForegroundServiceDriver {
  private final ReactApplicationContext context;

  public AndroidConnectedDeviceForegroundServiceDriver(ReactApplicationContext context) {
    this.context = context;
  }

  @Override
  public void start(String reason) {
    final ForegroundServiceNotificationConfiguration configuration = configuration();
    requireRuntimePermissions();
    final CountDownLatch acknowledgement = new CountDownLatch(1);
    final AtomicInteger resultCode = new AtomicInteger(0);
    final AtomicReference<String> resultMessage = new AtomicReference<>();
    final ResultReceiver receiver = new ResultReceiver(new Handler(Looper.getMainLooper())) {
      @Override
      protected void onReceiveResult(int code, Bundle resultData) {
        resultCode.set(code);
        if (resultData != null) resultMessage.set(resultData.getString("message"));
        acknowledgement.countDown();
      }
    };
    final Intent intent = BlePlxForegroundService.startIntent(context, configuration)
        .putExtra(BlePlxForegroundService.EXTRA_ACK, receiver);
    try {
      final ComponentName started = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
          ? context.startForegroundService(intent)
          : context.startService(intent);
      if (started == null) {
        throw new ForegroundServiceControlException(
            "foregroundServiceNotConfigured",
            "Android could not resolve the configured connected-device foreground service. Rebuild the native app.");
      }
      try {
      if (!acknowledgement.await(5, TimeUnit.SECONDS)) {
          context.stopService(new Intent(context, BlePlxForegroundService.class));
          throw new ForegroundServiceControlException(
              "foregroundServiceStartTimedOut",
              "Android did not acknowledge foreground-service promotion within five seconds; retry the lease.");
        }
      } catch (InterruptedException error) {
        Thread.currentThread().interrupt();
        throw new ForegroundServiceControlException(
            "foregroundServiceStartInterrupted",
            "Android foreground-service promotion was interrupted; retry the lease.",
            error);
      }
      if (resultCode.get() != BlePlxForegroundService.ACK_STARTED) {
        throw new ForegroundServiceControlException(
            "foregroundServiceStartNotAllowed",
            resultMessage.get() == null ? "Android failed to promote the connected-device service." : resultMessage.get());
      }
    } catch (SecurityException error) {
      throw new ForegroundServiceControlException(
          "foregroundServicePermissionDenied",
          "Android denied the connected-device foreground service. Grant Bluetooth and notification permissions, then retry.",
          error);
    } catch (IllegalStateException error) {
      throw new ForegroundServiceControlException(
          "foregroundServiceStartNotAllowed",
          "Android did not allow the connected-device foreground service to start from the current app state. Bring the app to the foreground and retry.",
          error);
    }
  }

  @Override
  public void stop() {
    try {
      context.getSharedPreferences("unified-ble-manager", Context.MODE_PRIVATE)
          .edit()
          .putBoolean(BlePlxForegroundService.SESSION_INTENT_PREFERENCE, false)
          .apply();
      context.stopService(new Intent(context, BlePlxForegroundService.class));
    } catch (RuntimeException error) {
      throw new ForegroundServiceControlException(
          "foregroundServiceStopFailed",
          "Android could not stop the connected-device foreground service; retry releasing the lease.",
          error);
    }
  }

  private ForegroundServiceNotificationConfiguration configuration() {
    try {
      final ApplicationInfo application = context.getPackageManager().getApplicationInfo(
          context.getPackageName(), PackageManager.GET_META_DATA);
      return ForegroundServiceNotificationConfiguration.fromMetadata(metadataMap(application.metaData));
    } catch (PackageManager.NameNotFoundException error) {
      throw new ForegroundServiceControlException(
          "foregroundServiceNotConfigured",
          "Android application metadata is unavailable; rebuild the native app.",
          error);
    }
  }

  private void requireRuntimePermissions() {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S
        && context.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) != PackageManager.PERMISSION_GRANTED) {
      throw new ForegroundServiceControlException(
          "foregroundServicePermissionDenied",
          "Bluetooth connect permission is required before acquiring a connected-device background lease.");
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU
        && context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
      throw new ForegroundServiceControlException(
          "foregroundServicePermissionDenied",
          "Notification permission is required before acquiring a connected-device background lease.");
    }
  }

  private static Map<String, String> metadataMap(Bundle metadata) {
    final Map<String, String> values = new HashMap<>();
    if (metadata == null) return values;
    for (String key : metadata.keySet()) {
      final Object value = metadata.get(key);
      if (value instanceof String) values.put(key, (String) value);
    }
    return values;
  }
}
