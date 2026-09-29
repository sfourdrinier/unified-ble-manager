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

import com.sfourdrinier.unifiedblemanager.BlePlxForegroundService;

import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

public final class AndroidConnectedDeviceForegroundServiceDriver
    implements ConnectedDeviceForegroundServiceDriver {
  private final Context context;

  public AndroidConnectedDeviceForegroundServiceDriver(Context context) {
    this.context = context;
  }

  @Override
  public void start(String reason) {
    start(reason, configuration());
  }

  @Override
  public ForegroundServiceNotificationConfiguration notificationConfiguration() { return configuration(); }

  @Override
  public void start(String reason, ForegroundServiceNotificationConfiguration configuration) {
    if (Looper.myLooper() == Looper.getMainLooper()) {
      throw new ForegroundServiceControlException(
          "foregroundServiceMainThreadUnavailable",
          "Android foreground-service acquisition cannot wait for promotion on the main thread.");
    }
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
    boolean accepted = false;
    try {
      final ComponentName started = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
          ? context.startForegroundService(intent)
          : context.startService(intent);
      if (started == null) {
        throw new ForegroundServiceControlException(
            "foregroundServiceNotConfigured",
            "Android could not resolve the configured connected-device foreground service. Rebuild the native app.");
      }
      accepted = true;
      try {
      if (!acknowledgement.await(5, TimeUnit.SECONDS)) {
          if (!context.getSharedPreferences("unified-ble-manager", Context.MODE_PRIVATE)
              .edit()
              .putBoolean(BlePlxForegroundService.SESSION_INTENT_PREFERENCE, false)
              .commit()) {
            throw new ForegroundServiceControlException(
                "foregroundServiceStopFailed",
                "Android could not persist the timed-out connected-device foreground service release; retry the lease.");
          }
          context.stopService(new Intent(context, BlePlxForegroundService.class));
          accepted = false;
          throw new ForegroundServiceControlException(
              "foregroundServiceStartTimedOut",
              "Android did not acknowledge foreground-service promotion within five seconds; retry the lease.");
        }
      } catch (InterruptedException error) {
        Thread.currentThread().interrupt();
        throw new ForegroundServiceControlException(
            "foregroundServiceStartInterrupted",
            "Android foreground-service promotion was interrupted; retry the lease.",
            error, true);
      }
      if (resultCode.get() != BlePlxForegroundService.ACK_STARTED) {
        throw new ForegroundServiceControlException(
            "foregroundServiceStartNotAllowed",
            resultMessage.get() == null ? "Android failed to promote the connected-device service." : resultMessage.get());
      }
    } catch (SecurityException error) {
      throw new ForegroundServiceControlException(
          "foregroundServicePermissionDenied",
          "Android denied the connected-device foreground service. Check Bluetooth permission and the application's foreground-service entitlements, then retry.",
          error, accepted);
    } catch (IllegalStateException error) {
      throw new ForegroundServiceControlException(
          "foregroundServiceStartNotAllowed",
          "Android did not allow the connected-device foreground service to start from the current app state. Bring the app to the foreground and retry.",
          error, accepted);
    } catch (RuntimeException error) {
      if (error instanceof ForegroundServiceControlException) {
        ForegroundServiceControlException failure = (ForegroundServiceControlException) error;
        if (!accepted) throw failure;
        throw new ForegroundServiceControlException(failure.code, failure.getMessage(), failure.getCause(), true);
      }
      throw new ForegroundServiceControlException("foregroundServiceStartFailed",
          error.getMessage() == null ? error.getClass().getName() : error.getMessage(), error, accepted);
    }
  }

  @Override
  public void stop() {
    try {
      if (!context.getSharedPreferences("unified-ble-manager", Context.MODE_PRIVATE)
          .edit()
          .putBoolean(BlePlxForegroundService.SESSION_INTENT_PREFERENCE, false)
          .commit()) {
        throw new ForegroundServiceControlException(
            "foregroundServiceStopFailed",
            "Android could not persist the connected-device foreground service release; retry releasing the lease.");
      }
      context.stopService(new Intent(context, BlePlxForegroundService.class));
    } catch (RuntimeException error) {
      throw new ForegroundServiceControlException(
          "foregroundServiceStopFailed",
          "Android could not stop the connected-device foreground service; retry releasing the lease.",
          error);
    }
  }

  @Override
  public void update(String title, String body) {
    try {
      BlePlxForegroundService.updateNotification(title, body);
    } catch (RuntimeException error) {
      if (error instanceof ForegroundServiceControlException) throw error;
      throw new ForegroundServiceControlException(
          "foregroundServiceNotificationUpdateFailed",
          "Android could not update the connected-device foreground-service notification; retry while the lease is active.",
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
    for (String permission : requiredRuntimePermissions(Build.VERSION.SDK_INT)) {
      if (context.checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED) {
        throw new ForegroundServiceControlException(
            "foregroundServicePermissionDenied", "Bluetooth connect permission is required before acquiring a connected-device background lease.");
      }
    }
  }

  // Android requires the service notification, but POST_NOTIFICATIONS is not
  // a prerequisite for FGS startup; denial changes where the OS displays it.
  static String[] requiredRuntimePermissions(int sdk) {
    return sdk >= 31 ? new String[] { Manifest.permission.BLUETOOTH_CONNECT } : new String[0];
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
