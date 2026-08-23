package com.sfourdrinier.unifiedblemanager.background;

public interface ConnectedDeviceForegroundServiceDriver {
  void start(String reason);
  void stop();
}
