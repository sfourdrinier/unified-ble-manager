package com.sfourdrinier.unifiedblemanager.background;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.os.Build;
import android.util.Log;

import com.sfourdrinier.unifiedblemanager.BlePlxForegroundService;

/** Restores only the configured foreground service; it never scans or reconnects. */
public final class BlePlxForegroundServiceRecoveryReceiver extends BroadcastReceiver {
  private static final String TAG = "UnifiedBleRecovery";

  @Override
  public void onReceive(Context context, Intent intent) {
    if (intent == null || (!Intent.ACTION_BOOT_COMPLETED.equals(intent.getAction())
        && !Intent.ACTION_MY_PACKAGE_REPLACED.equals(intent.getAction()))) return;
    if (!context.getSharedPreferences("unified-ble-manager", Context.MODE_PRIVATE)
        .getBoolean(BlePlxForegroundService.SESSION_INTENT_PREFERENCE, false)) return;
    try {
      final ForegroundServiceNotificationConfiguration configuration = configuration(context);
      if (!configuration.restartWhileSessionIntentExists()) return;
      final Intent start = BlePlxForegroundService.startIntent(context, configuration);
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) context.startForegroundService(start);
      else context.startService(start);
    } catch (RuntimeException error) {
      Log.e(TAG, "Connected-device foreground-service recovery failed", error);
    }
  }

  private static ForegroundServiceNotificationConfiguration configuration(Context context) {
    try {
      final android.content.pm.ApplicationInfo application =
          context.getPackageManager().getApplicationInfo(
              context.getPackageName(), android.content.pm.PackageManager.GET_META_DATA);
      final java.util.Map<String, String> metadata = new java.util.HashMap<>();
      if (application.metaData != null) {
        for (String key : application.metaData.keySet()) {
          final Object value = application.metaData.get(key);
          if (value instanceof String) metadata.put(key, (String) value);
        }
      }
      return ForegroundServiceNotificationConfiguration.fromMetadata(metadata);
    } catch (android.content.pm.PackageManager.NameNotFoundException error) {
      throw new ForegroundServiceControlException(
          "foregroundServiceNotConfigured", "Managed foreground-service metadata is unavailable", error);
    }
  }
}
