package com.sfourdrinier.unifiedblemanager.background

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ConnectedDeviceForegroundServiceLeaseRegistryTest {
  @Test fun `display text update does not replace pinned acquisition configuration`() {
    val configuration = ForegroundServiceNotificationConfiguration.fromValues("ble", "BLE", "Recording", null, null, false)
    val updates = mutableListOf<String>()
    var starts = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun notificationConfiguration() = configuration
      override fun start(reason: String) { starts++ }
      override fun start(reason: String, value: ForegroundServiceNotificationConfiguration) { start(reason) }
      override fun stop() {}
      override fun update(title: String, body: String?) { updates.add(title) }
    }
    var id = 0
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-${++id}" }
    val first = registry.acquire("first")
    registry.update(first, "Live heart rate", "72 bpm")
    val second = registry.acquire("second", configuration)
    assertEquals(1, starts)
    assertEquals(2, registry.activeLeaseCount())
    assertEquals(listOf("Live heart rate"), updates)
    assertTrue(runCatching { registry.acquire("different", configuration.withText("Other", null)) }.exceptionOrNull() is ForegroundServiceControlException)
    registry.release(first)
    registry.release(second)
  }

  @Test fun `blank update title is rejected before driver effects and preserves ownership`() {
    val configuration = ForegroundServiceNotificationConfiguration.fromValues("ble", "BLE", "Recording", null, null, false)
    var updates = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun notificationConfiguration() = configuration
      override fun start(reason: String) {}
      override fun start(reason: String, value: ForegroundServiceNotificationConfiguration) { start(reason) }
      override fun stop() {}
      override fun update(title: String, body: String?) { updates++ }
    }
    var id = 0
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-${++id}" }
    val first = registry.acquire("first")
    val failure = runCatching { registry.update(first, "   ", null) }.exceptionOrNull()
    assertTrue(failure is ForegroundServiceControlException)
    assertEquals("foregroundServiceNotConfigured", (failure as ForegroundServiceControlException).code)
    assertEquals(0, updates)
    assertTrue(registry.hasLease(first))
    registry.acquire("same", configuration)
    registry.close()
  }
  @Test fun `accepted start failure retains refused compensation until retry succeeds`() {
    var starts = 0
    var stops = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) {
        starts++
        throw ForegroundServiceControlException("foregroundServiceStartInterrupted", "accepted start interrupted", InterruptedException("interrupted"), true)
      }
      override fun stop() { if (++stops == 1) throw IllegalStateException("stop refused") }
      override fun update(title: String, body: String?) {}
    }
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" }
    val failure = runCatching { registry.acquire("presence") }.exceptionOrNull()
    assertTrue(failure is ForegroundServiceControlException)
    assertEquals(1, failure!!.suppressed.size)
    assertTrue(registry.hasPendingStartCleanup())
    assertEquals(0, registry.activeLeaseCount())
    registry.close()
    assertFalse(registry.hasPendingStartCleanup())
    assertEquals(2, stops)
    assertEquals(1, starts)
  }
  @Test
  fun `explicit notification is pinned while any shared lease survives`() {
    val configuration = ForegroundServiceNotificationConfiguration.fromValues("ble", "BLE", "Recording", null, null, false)
    val different = ForegroundServiceNotificationConfiguration.fromValues("ble", "BLE", "Other", null, null, false)
    val starts = mutableListOf<ForegroundServiceNotificationConfiguration>()
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun notificationConfiguration() = configuration
      override fun start(reason: String) = error("must dispatch exact configuration")
      override fun start(reason: String, value: ForegroundServiceNotificationConfiguration) { starts.add(value) }
      override fun stop() {}
      override fun update(title: String, body: String?) {}
    }
    var id = 0
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-${++id}" }
    val first = registry.acquire("presence", configuration)
    val second = registry.acquire("manager")
    assertEquals(1, starts.size)
    assertTrue(runCatching { registry.acquire("other", different) }.exceptionOrNull() is ForegroundServiceControlException)
    assertEquals(2, registry.activeLeaseCount())
    registry.release(first)
    assertEquals(1, registry.activeLeaseCount())
    registry.release(second)
    registry.acquire("new", different)
    assertEquals(listOf(configuration, different), starts)
  }
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

  @Test
  fun `notification update requires an active lease and does not start the service`() {
    val driver = RecordingServiceDriver()
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-1" }

    val failure = runCatching { registry.update("lease-1", "Glucose 108", "Private") }.exceptionOrNull()

    assertTrue(failure is ForegroundServiceControlException)
    assertEquals("invalidBackgroundLease", (failure as ForegroundServiceControlException).code)
    assertEquals(0, driver.starts.size)

    val lease = registry.acquire("active-workout")
    registry.update(lease, "Glucose 108", "Private")
    assertEquals(listOf("Glucose 108|Private"), driver.updates)
  }

  private class RecordingServiceDriver(
    private var failFirstStart: Boolean = false,
    private var failFirstStop: Boolean = false
  ) : ConnectedDeviceForegroundServiceDriver {
    val starts = mutableListOf<String>()
    val updates = mutableListOf<String>()
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

    override fun update(title: String, body: String?) {
      updates += "$title|${body ?: ""}"
    }
  }
}
