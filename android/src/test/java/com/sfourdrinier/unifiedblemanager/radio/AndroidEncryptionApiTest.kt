package com.sfourdrinier.unifiedblemanager.radio

import org.junit.Assert.*
import org.junit.Test

class AndroidEncryptionApiTest {
  class Status(val key: Int, private val algorithmValue: Int) {
    fun getKeySize(): Int = key
    fun getAlgorithm(): Int = algorithmValue
  }
  class Device {
    var answer: Status? = null
    var failure: Throwable? = null
    var transport = -1
    fun getEncryptionStatus(transport: Int): Status? {
      this.transport = transport
      failure?.let { throw it }
      return answer
    }
  }

  @Test fun snapshotsAreMinorVersionGatedAndExplicitlyLeScoped() {
    val old = AndroidEncryptionApi(3600000, Device::class.java)
    assertFalse(old.snapshotAvailable)
    assertEquals("unsupported", old.read(Device()))
    val api = AndroidEncryptionApi(3600001, Device::class.java)
    val device = Device()
    assertTrue(api.snapshotAvailable)
    assertEquals("unknown", api.read(device))
    assertEquals(2, device.transport)
    device.answer = Status(16, 2)
    assertEquals("encrypted", api.read(device))
    device.answer = Status(16, 0)
    assertEquals("not-encrypted", api.read(device))
    device.answer = Status(16, 3)
    assertEquals("encrypted", api.read(device))
    device.answer = Status(16, -1)
    assertEquals("unknown", api.read(device))
    val denied = SecurityException("BLUETOOTH_CONNECT")
    device.failure = denied
    try { api.read(device); fail("expected denial") } catch (error: SecurityException) { assertSame(denied, error) }
  }

  @Test fun eventFailuresAndMalformedObservationsNeverBecomeEncrypted() {
    assertEquals("encrypted", AndroidEncryptionApi.event(0, true))
    assertEquals("not-encrypted", AndroidEncryptionApi.event(0, false))
    assertThrows(AndroidEncryptionFailure::class.java) { AndroidEncryptionApi.event(5, true) }
    assertThrows(IllegalArgumentException::class.java) { AndroidEncryptionApi.event(0, null) }
  }
}
