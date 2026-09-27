package com.sfourdrinier.unifiedblemanager.background

import org.junit.Assert.*
import org.junit.Test

class ForegroundServicePermissionPolicyTest {
  @Test fun `notification permission is not an Android foreground service startup prerequisite`() {
    assertArrayEquals(emptyArray<String>(), AndroidConnectedDeviceForegroundServiceDriver.requiredRuntimePermissions(30))
    assertArrayEquals(arrayOf("android.permission.BLUETOOTH_CONNECT"), AndroidConnectedDeviceForegroundServiceDriver.requiredRuntimePermissions(33))
  }
}
