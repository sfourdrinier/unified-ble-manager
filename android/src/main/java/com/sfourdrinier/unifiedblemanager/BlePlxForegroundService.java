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

import androidx.annotation.Nullable;

import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceNotificationConfiguration;

/** Explicit connected-device foreground lease. It performs no scan or reconnect work. */
public final class BlePlxForegroundService extends Service {
  public static final String ACTION_START = "com.sfourdrinier.unifiedblemanager.background.START";
  private static final String EXTRA_CHANNEL_ID = "channelId";
  private static final String EXTRA_CHANNEL_NAME = "channelName";
  private static final String EXTRA_TITLE = "title";
  private static final String EXTRA_BODY = "body";
  private static final String EXTRA_ICON_RESOURCE = "iconResource";

  public static Intent startIntent(
      Context context,
      ForegroundServiceNotificationConfiguration configuration) {
    return new Intent(context, BlePlxForegroundService.class)
        .setAction(ACTION_START)
        .putExtra(EXTRA_CHANNEL_ID, configuration.getChannelId())
        .putExtra(EXTRA_CHANNEL_NAME, configuration.getChannelName())
        .putExtra(EXTRA_TITLE, configuration.getTitle())
        .putExtra(EXTRA_BODY, configuration.getBody())
        .putExtra(EXTRA_ICON_RESOURCE, iconResource(context, configuration.getIconName()));
  }

  @Override
  public int onStartCommand(Intent intent, int flags, int startId) {
    if (intent == null || !ACTION_START.equals(intent.getAction())) {
      stopSelf();
      return START_NOT_STICKY;
    }

    final String channelId = required(intent, EXTRA_CHANNEL_ID);
    final String channelName = required(intent, EXTRA_CHANNEL_NAME);
    final String title = required(intent, EXTRA_TITLE);
    final String body = intent.getStringExtra(EXTRA_BODY);
    ensureChannel(channelId, channelName);
    final Notification.Builder builder =
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.O
            ? new Notification.Builder(this, channelId)
            : new Notification.Builder(this);
    builder
        .setSmallIcon(intent.getIntExtra(EXTRA_ICON_RESOURCE, android.R.drawable.stat_sys_data_bluetooth))
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
    return START_NOT_STICKY;
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
}
