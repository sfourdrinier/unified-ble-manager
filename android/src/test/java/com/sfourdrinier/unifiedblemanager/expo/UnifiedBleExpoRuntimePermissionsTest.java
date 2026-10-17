// android/src/test/java/com/sfourdrinier/unifiedblemanager/expo/UnifiedBleExpoRuntimePermissionsTest.java
package com.sfourdrinier.unifiedblemanager.expo;

import static org.junit.Assert.assertArrayEquals;
import org.junit.Test;

public class UnifiedBleExpoRuntimePermissionsTest {
  @Test public void requiredLocationCanBeRequestedOnAndroid12() {
    assertArrayEquals(new String[] {
      "android.permission.BLUETOOTH_SCAN", "android.permission.BLUETOOTH_CONNECT",
      "android.permission.ACCESS_COARSE_LOCATION", "android.permission.ACCESS_FINE_LOCATION"
    }, UnifiedBleExpoRuntimeModule.scanAndConnectRuntimePermissions(31, "required"));
  }
  @Test public void autoLocationDoesNotPromptOnAndroid12() {
    assertArrayEquals(new String[] { "android.permission.BLUETOOTH_SCAN", "android.permission.BLUETOOTH_CONNECT" },
      UnifiedBleExpoRuntimeModule.scanAndConnectRuntimePermissions(31, "auto"));
  }
  @Test public void android11RequestsLegacyLocation() {
    assertArrayEquals(new String[] { "android.permission.ACCESS_COARSE_LOCATION", "android.permission.ACCESS_FINE_LOCATION" },
      UnifiedBleExpoRuntimeModule.scanAndConnectRuntimePermissions(30, "auto"));
  }
}
