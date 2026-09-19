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
    if (intent == null) return;
    try {
      final boolean sessionIntentExists = context.getSharedPreferences("unified-ble-manager", Context.MODE_PRIVATE)
          .getBoolean(BlePlxForegroundService.SESSION_INTENT_PREFERENCE, false);
      final ForegroundServiceNotificationConfiguration configuration = configuration(context);
      if (!shouldRecover(intent.getAction(), sessionIntentExists, configuration.restartWhileSessionIntentExists())) {
        return;
      }
      final Intent start = BlePlxForegroundService.startIntent(context, configuration);
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) context.startForegroundService(start);
      else context.startService(start);
    } catch (RuntimeException error) {
      Log.e(TAG, "Connected-device foreground-service recovery failed", error);
    }
  }

  /**
   * Pure restart decision: a recovery action (boot completed or our own
   * package replaced) AND a persisted session intent AND a configured
   * while-session-intent restart. Everything else fails closed (no start).
   * Reads stay fail-closed at the call site (a missing prefs file or key
   * yields {@code false}); this function only combines the three facts.
   */
  static boolean shouldRecover(String action, boolean sessionIntentExists, boolean restartConfigured) {
    final boolean recoveryAction = Intent.ACTION_BOOT_COMPLETED.equals(action)
        || Intent.ACTION_MY_PACKAGE_REPLACED.equals(action);
    return recoveryAction && sessionIntentExists && restartConfigured;
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
