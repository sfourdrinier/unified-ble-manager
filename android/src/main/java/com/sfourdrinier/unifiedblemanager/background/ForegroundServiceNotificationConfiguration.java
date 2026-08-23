package com.sfourdrinier.unifiedblemanager.background;

import java.util.Map;

public final class ForegroundServiceNotificationConfiguration {
  public static final int NOTIFICATION_ID = 0x55424d;
  public static final String OWNERSHIP_METADATA =
      "com.sfourdrinier.unifiedblemanager.foreground-service-ownership";
  public static final String CHANNEL_ID_METADATA =
      "com.sfourdrinier.unifiedblemanager.foreground-service.channel-id";
  public static final String CHANNEL_NAME_METADATA =
      "com.sfourdrinier.unifiedblemanager.foreground-service.channel-name";
  public static final String TITLE_METADATA =
      "com.sfourdrinier.unifiedblemanager.foreground-service.title";
  public static final String BODY_METADATA =
      "com.sfourdrinier.unifiedblemanager.foreground-service.body";
  public static final String ICON_METADATA =
      "com.sfourdrinier.unifiedblemanager.foreground-service.icon";
  public static final String RESTART_METADATA =
      "com.sfourdrinier.unifiedblemanager.foreground-service.restart";

  private final String channelId;
  private final String channelName;
  private final String title;
  private final String body;
  private final String iconName;
  private final boolean restartWhileSessionIntentExists;

  private ForegroundServiceNotificationConfiguration(
      String channelId,
      String channelName,
      String title,
      String body,
      String iconName,
      boolean restartWhileSessionIntentExists) {
    this.channelId = channelId;
    this.channelName = channelName;
    this.title = title;
    this.body = body;
    this.iconName = iconName;
    this.restartWhileSessionIntentExists = restartWhileSessionIntentExists;
  }

  public static ForegroundServiceNotificationConfiguration fromMetadata(Map<String, String> metadata) {
    if (!"service=1".equals(metadata.get(OWNERSHIP_METADATA))) {
      throw notConfigured("foreground-service ownership metadata is absent");
    }
    return new ForegroundServiceNotificationConfiguration(
        required(metadata, CHANNEL_ID_METADATA),
        required(metadata, CHANNEL_NAME_METADATA),
        required(metadata, TITLE_METADATA),
        optional(metadata.get(BODY_METADATA)),
        optional(metadata.get(ICON_METADATA)),
        "while-session-intent-exists".equals(metadata.get(RESTART_METADATA)));
  }

  public static ForegroundServiceNotificationConfiguration fromValues(
      String channelId,
      String channelName,
      String title,
      String body,
      String iconName,
      boolean restartWhileSessionIntentExists) {
    return new ForegroundServiceNotificationConfiguration(
        requiredValue(channelId, CHANNEL_ID_METADATA),
        requiredValue(channelName, CHANNEL_NAME_METADATA),
        requiredValue(title, TITLE_METADATA),
        optional(body),
        optional(iconName),
        restartWhileSessionIntentExists);
  }

  public String getChannelId() {
    return channelId;
  }

  public String getChannelName() {
    return channelName;
  }

  public String getTitle() {
    return title;
  }

  public String getBody() {
    return body;
  }

  public String getIconName() {
    return iconName;
  }

  public boolean restartWhileSessionIntentExists() {
    return restartWhileSessionIntentExists;
  }

  private static String required(Map<String, String> metadata, String key) {
    return requiredValue(metadata.get(key), key);
  }

  private static String requiredValue(String value, String key) {
    final String normalized = optional(value);
    if (normalized == null) throw notConfigured("required notification metadata is absent: " + key);
    return normalized;
  }

  private static String optional(String value) {
    if (value == null) return null;
    final String normalized = value.trim();
    return normalized.isEmpty() ? null : normalized;
  }

  private static ForegroundServiceControlException notConfigured(String detail) {
    return new ForegroundServiceControlException(
        "foregroundServiceNotConfigured",
        "The connected-device foreground service is not configured: " + detail
            + ". Rebuild with background.android.mode set to connected-device-foreground-service.");
  }
}
