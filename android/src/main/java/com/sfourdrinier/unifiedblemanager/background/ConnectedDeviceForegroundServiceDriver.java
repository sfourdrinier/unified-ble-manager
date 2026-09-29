package com.sfourdrinier.unifiedblemanager.background;

public interface ConnectedDeviceForegroundServiceDriver {
  void start(String reason);
  default ForegroundServiceNotificationConfiguration notificationConfiguration() { return null; }
  default void start(String reason, ForegroundServiceNotificationConfiguration configuration) {
    if (configuration != null) throw new ForegroundServiceControlException(
        "foregroundServiceNotConfigured", "The driver cannot apply an explicit notification configuration.");
    start(reason);
  }
  void stop();
  void update(String title, String body);
}
