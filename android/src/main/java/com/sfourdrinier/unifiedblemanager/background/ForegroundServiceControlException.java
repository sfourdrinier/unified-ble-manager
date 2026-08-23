package com.sfourdrinier.unifiedblemanager.background;

public final class ForegroundServiceControlException extends RuntimeException {
  public final String code;

  public ForegroundServiceControlException(String code, String message) {
    super(message);
    this.code = code;
  }

  public ForegroundServiceControlException(String code, String message, Throwable cause) {
    super(message, cause);
    this.code = code;
  }
}
