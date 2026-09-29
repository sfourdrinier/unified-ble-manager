package com.sfourdrinier.unifiedblemanager.background;

public final class ForegroundServiceControlException extends RuntimeException {
  public final String code;
  /** A service start was accepted but no successful lease receipt was produced. */
  public final boolean cleanupRequired;

  public ForegroundServiceControlException(String code, String message) {
    this(code, message, null, false);
  }

  public ForegroundServiceControlException(String code, String message, Throwable cause) {
    this(code, message, cause, false);
  }

  public ForegroundServiceControlException(String code, String message, Throwable cause, boolean cleanupRequired) {
    super(message, cause);
    this.code = code;
    this.cleanupRequired = cleanupRequired;
  }
}
