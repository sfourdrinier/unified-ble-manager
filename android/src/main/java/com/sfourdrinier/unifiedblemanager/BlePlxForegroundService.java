package com.sfourdrinier.unifiedblemanager;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.os.Handler;
import android.os.Looper;
import android.os.ResultReceiver;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.os.Bundle;

import androidx.annotation.Nullable;

import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceNotificationConfiguration;

import java.util.HashMap;
import java.util.Map;

/** Explicit connected-device foreground lease. It performs no scan or reconnect work. */
public final class BlePlxForegroundService extends Service {
  public static final String ACTION_START = "com.sfourdrinier.unifiedblemanager.background.START";
  public static final String EXTRA_ACK = "backgroundStartAcknowledgement";
  public static final int ACK_STARTED = 1;
  public static final int ACK_FAILED = 2;
  public static final String SESSION_INTENT_PREFERENCE =
      "unified-ble-manager.background.session-intent-exists";
  private static final String EXTRA_CHANNEL_ID = "channelId";
  private static final String EXTRA_CHANNEL_NAME = "channelName";
  private static final String EXTRA_TITLE = "title";
  private static final String EXTRA_BODY = "body";
  private static final String EXTRA_ICON_NAME = "iconName";
  private static final String EXTRA_RESTART_STICKY = "restartSticky";

  public static Intent startIntent(
      Context context,
      ForegroundServiceNotificationConfiguration configuration) {
    return new Intent(context, BlePlxForegroundService.class)
        .setAction(ACTION_START)
        .putExtra(EXTRA_CHANNEL_ID, configuration.getChannelId())
        .putExtra(EXTRA_CHANNEL_NAME, configuration.getChannelName())
        .putExtra(EXTRA_TITLE, configuration.getTitle())
        .putExtra(EXTRA_BODY, configuration.getBody())
        .putExtra(EXTRA_ICON_NAME, configuration.getIconName())
        .putExtra(EXTRA_RESTART_STICKY, configuration.restartWhileSessionIntentExists());
  }

  @Override
  public int onStartCommand(Intent intent, int flags, int startId) {
    try {
      final ForegroundServiceNotificationConfiguration configuration;
      if (intent == null) {
        configuration = configurationFromMetadata();
        if (!getSharedPreferences("unified-ble-manager", MODE_PRIVATE)
            .getBoolean(SESSION_INTENT_PREFERENCE, false)) {
          stopSelf();
          return START_NOT_STICKY;
        }
      } else if (ACTION_START.equals(intent.getAction())) {
        configuration = ForegroundServiceNotificationConfiguration.fromValues(
            required(intent, EXTRA_CHANNEL_ID),
            required(intent, EXTRA_CHANNEL_NAME),
            required(intent, EXTRA_TITLE),
            intent.getStringExtra(EXTRA_BODY),
            intent.getStringExtra(EXTRA_ICON_NAME),
            intent.getBooleanExtra(EXTRA_RESTART_STICKY, false));
      } else {
        stopSelf();
        return START_NOT_STICKY;
      }

      final String channelId = configuration.getChannelId();
      final String channelName = configuration.getChannelName();
      final String title = configuration.getTitle();
      final String body = configuration.getBody();
      if (!getSharedPreferences("unified-ble-manager", MODE_PRIVATE)
          .edit()
          .putBoolean(SESSION_INTENT_PREFERENCE, configuration.restartWhileSessionIntentExists())
          .commit()) {
        throw new IllegalStateException("Android could not persist the foreground-service session intent.");
      }
      ensureChannel(channelId, channelName);
      final Notification.Builder builder =
          Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
              ? new Notification.Builder(this, channelId)
              : new Notification.Builder(this);
      builder
          .setSmallIcon(iconResource(this, configuration.getIconName()))
          .setContentTitle(title)
          .setOngoing(true)
          .setCategory(Notification.CATEGORY_SERVICE);
      if (body != null) builder.setContentText(body);
      final Notification notification = builder.build();
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
        startForeground(
            ForegroundServiceNotificationConfiguration.NOTIFICATION_ID,
            notification,
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE);
      } else {
        startForeground(ForegroundServiceNotificationConfiguration.NOTIFICATION_ID, notification);
      }
      acknowledge(intent, ACK_STARTED, null);
      return configuration.restartWhileSessionIntentExists() ? START_STICKY : START_NOT_STICKY;
    } catch (RuntimeException error) {
      acknowledge(intent, ACK_FAILED, error.getMessage());
      getSharedPreferences("unified-ble-manager", MODE_PRIVATE)
          .edit()
          .putBoolean(SESSION_INTENT_PREFERENCE, false)
          .apply();
      stopSelf();
      return START_NOT_STICKY;
    }
  }

  @Override
  public void onDestroy() {
    stopForeground(STOP_FOREGROUND_REMOVE);
    super.onDestroy();
  }

  @Nullable
  @Override
  public IBinder onBind(Intent intent) {
    return null;
  }

  private void ensureChannel(String channelId, String channelName) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return;
    final NotificationManager manager = getSystemService(NotificationManager.class);
    if (manager == null) throw new IllegalStateException("Notification service is unavailable");
    manager.createNotificationChannel(
        new NotificationChannel(channelId, channelName, NotificationManager.IMPORTANCE_LOW));
  }

  private static String required(Intent intent, String name) {
    final String value = intent.getStringExtra(name);
    if (value == null || value.isEmpty()) {
      throw new IllegalStateException("Required foreground-service notification field is missing: " + name);
    }
    return value;
  }

  private static int iconResource(Context context, String iconName) {
    if (iconName == null) return android.R.drawable.stat_sys_data_bluetooth;
    int resource = context.getResources().getIdentifier(iconName, "drawable", context.getPackageName());
    if (resource == 0) resource = context.getResources().getIdentifier(iconName, "mipmap", context.getPackageName());
    if (resource == 0) {
      throw new com.sfourdrinier.unifiedblemanager.background.ForegroundServiceControlException(
          "foregroundServiceNotConfigured",
          "The configured foreground-service notification icon does not exist: " + iconName);
    }
    return resource;
  }

  private ForegroundServiceNotificationConfiguration configurationFromMetadata() {
    try {
      final ApplicationInfo application = getPackageManager().getApplicationInfo(
          getPackageName(), PackageManager.GET_META_DATA);
      final Bundle bundle = application.metaData;
      final Map<String, String> metadata = new HashMap<>();
      if (bundle != null) {
        for (String key : bundle.keySet()) {
          Object value = bundle.get(key);
          if (value instanceof String) metadata.put(key, (String) value);
        }
      }
      return ForegroundServiceNotificationConfiguration.fromMetadata(metadata);
    } catch (PackageManager.NameNotFoundException error) {
      throw new IllegalStateException("Managed foreground-service metadata is unavailable", error);
    }
  }

  private void acknowledge(Intent intent, int code, String message) {
    if (intent == null) return;
    final ResultReceiver receiver = intent.getParcelableExtra(EXTRA_ACK);
    if (receiver == null) return;
    final Bundle result = new Bundle();
    if (message != null) result.putString("message", message);
    receiver.send(code, result);
  }

}
