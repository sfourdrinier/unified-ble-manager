package com.sfourdrinier.unifiedblemanager.background

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectedDeviceForegroundServiceLeaseRegistryTest {
  @Test
  fun `first acquire starts one service and final release stops it`() {
    val driver = RecordingServiceDriver()
    val ids = ArrayDeque(listOf("lease-1", "lease-2"))
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { ids.removeFirst() }

    val first = registry.acquire("active-workout")
    val second = registry.acquire("device-sync")

    assertEquals("lease-1", first)
    assertEquals("lease-2", second)
    assertEquals(listOf("active-workout"), driver.starts)
    assertEquals(2, registry.activeLeaseCount())

    registry.release(first)
    assertEquals(0, driver.stopCount)
    registry.release(second)
    assertEquals(1, driver.stopCount)
    assertEquals(0, registry.activeLeaseCount())
  }

  @Test
  fun `failed first start records no lease and can be retried`() {
    val driver = RecordingServiceDriver(failFirstStart = true)
    val ids = ArrayDeque(listOf("lease-1", "lease-2"))
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { ids.removeFirst() }

    val failure = runCatching { registry.acquire("active-workout") }.exceptionOrNull()

    assertTrue(failure is ForegroundServiceControlException)
    assertEquals("foregroundServiceStartNotAllowed", (failure as ForegroundServiceControlException).code)
    assertEquals(0, registry.activeLeaseCount())
    assertEquals("lease-2", registry.acquire("active-workout"))
    assertEquals(1, registry.activeLeaseCount())
  }

  @Test
  fun `unknown or repeated release cannot decrement another lease`() {
    val driver = RecordingServiceDriver()
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-1" }
    val lease = registry.acquire("active-workout")

    registry.release(lease)
    val failure = runCatching { registry.release(lease) }.exceptionOrNull()

    assertTrue(failure is ForegroundServiceControlException)
    assertEquals("invalidBackgroundLease", (failure as ForegroundServiceControlException).code)
    assertEquals(1, driver.stopCount)
    assertFalse(registry.hasLease(lease))
  }

  @Test
  fun `failed close retains lease ownership and retries the stop`() {
    val driver = RecordingServiceDriver(failFirstStop = true)
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-1" }
    val lease = registry.acquire("active-workout")

    val failure = runCatching { registry.close() }.exceptionOrNull()

    assertTrue(failure is ForegroundServiceControlException)
    assertEquals("foregroundServiceStopFailed", (failure as ForegroundServiceControlException).code)
    assertEquals(1, registry.activeLeaseCount())
    assertTrue(registry.hasLease(lease))

    registry.close()

    assertEquals(2, driver.stopCount)
    assertEquals(0, registry.activeLeaseCount())
    assertFalse(registry.hasLease(lease))
  }

  private class RecordingServiceDriver(
    private var failFirstStart: Boolean = false,
    private var failFirstStop: Boolean = false
  ) : ConnectedDeviceForegroundServiceDriver {
    val starts = mutableListOf<String>()
    var stopCount = 0

    override fun start(reason: String) {
      if (failFirstStart) {
        failFirstStart = false
        throw ForegroundServiceControlException(
          "foregroundServiceStartNotAllowed",
          "Android did not allow the connected-device foreground service to start from the current app state."
        )
      }
      starts += reason
    }

    override fun stop() {
      stopCount += 1
      if (failFirstStop) {
        failFirstStop = false
        throw ForegroundServiceControlException(
          "foregroundServiceStopFailed",
          "Android could not stop the connected-device foreground service; retry releasing the lease."
        )
      }
    }
  }
}
