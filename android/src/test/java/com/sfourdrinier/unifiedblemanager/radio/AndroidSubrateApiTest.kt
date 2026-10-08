package com.sfourdrinier.unifiedblemanager.radio

import org.junit.Assert.*
import org.junit.Test

class AndroidSubrateApiTest {
  class CurrentGatt {
    var requested = -1
    var status = 0
    var failure: Throwable? = null
    fun requestSubrateMode(mode: Int): Int {
      failure?.let { throw it }
      requested = mode
      return status
    }
  }
  class PreviewGatt { fun requestSubrateMode(mode: Int): Boolean = mode == 0 }

  @Test fun probeRequiresTheMinorVersionAndThePublicIntSignature() {
    assertFalse(AndroidSubrateApi(3600000, CurrentGatt::class.java).available)
    assertFalse(AndroidSubrateApi(3600001, PreviewGatt::class.java).available)
    assertFalse(AndroidSubrateApi(3600001, Any::class.java).available)
    assertTrue(AndroidSubrateApi(3600001, CurrentGatt::class.java).available)
  }

  @Test fun allPresetsReturnTheNativeStatusAndUnwrapPermissionFailure() {
    val api = AndroidSubrateApi(3600001, CurrentGatt::class.java)
    val gatt = CurrentGatt()
    gatt.status = 6
    listOf("default" to 2, "low-latency" to 0, "low-power" to 1, "high-throughput" to 3).forEach { (name, value) ->
      assertEquals(6, api.request(gatt, name))
      assertEquals(value, gatt.requested)
    }
    val denied = SecurityException("companion association required")
    gatt.failure = denied
    try { api.request(gatt, "low-power"); fail("expected denial") } catch (error: SecurityException) { assertSame(denied, error) }
    try { api.request(gatt, "system-update"); fail("expected invalid argument") } catch (_: IllegalArgumentException) { }
  }
}
