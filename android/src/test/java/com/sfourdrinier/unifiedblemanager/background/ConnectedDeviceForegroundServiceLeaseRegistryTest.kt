package com.sfourdrinier.unifiedblemanager.background

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import com.sfourdrinier.unifiedblemanager.rustcore.ForegroundServiceBackgroundPort

class ConnectedDeviceForegroundServiceLeaseRegistryTest {
  @Test fun `a finishing start cannot clear a newer same-owner start identity`() = finishingStartPreservesNewerIdentity("second")

  @Test fun `a finishing start cannot clear a newer start with a reused lease ID`() = finishingStartPreservesNewerIdentity("first")

  private fun finishingStartPreservesNewerIdentity(newerId: String) {
    val entered = java.util.concurrent.CountDownLatch(1)
    val finish = java.util.concurrent.CountDownLatch(1)
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) {
        entered.countDown()
        check(finish.await(5, java.util.concurrent.TimeUnit.SECONDS))
        throw IllegalStateException("start failed")
      }
      override fun stop() {}
      override fun update(title: String, body: String?) {}
    }
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "first" }
    val owner = Any()
    val worker = Thread { runCatching { registry.acquireOwned(owner, "first") } }
    worker.start()
    try {
      assertTrue(entered.await(5, java.util.concurrent.TimeUnit.SECONDS))
      // Model the critical interleaving: the next start has published its ID
      // before the first acquisition's finally block can clear its own ID.
      val ownersField = registry.javaClass.getDeclaredField("owners").apply { isAccessible = true }
      val owners = ownersField.get(registry) as Map<*, *>
      val state = owners[owner]!!
      val id = state.javaClass.getDeclaredField("startInFlightId").apply { isAccessible = true }
      id.set(state, newerId)
      // The IDs may be reused after a failed start; distinguish acquisitions
      // even when the externally visible lease string is identical.
      state.javaClass.declaredFields.firstOrNull { it.name == "startInFlightToken" }
        ?.apply { isAccessible = true }?.set(state, Any())
      finish.countDown()
      worker.join(5000)
      assertFalse(worker.isAlive)
      assertEquals(newerId, registry.snapshotOwner(owner).pendingStartId)
    } finally {
      finish.countDown()
      worker.join(5000)
    }
  }
  @Test fun `same owner retries its pending start before acquiring again`() {
    var starts = 0
    var stops = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) {
        if (++starts == 1) throw ForegroundServiceControlException("interrupted", "accepted", InterruptedException(), true)
      }
      override fun stop() { if (++stops == 1) throw IllegalStateException("refused") }
      override fun update(title: String, body: String?) {}
    }
    var id = 0
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-${++id}" }
    val owner = Any()
    assertTrue(runCatching { registry.acquireOwned(owner, "first") }.isFailure)
    assertEquals("lease-2", registry.acquireOwned(owner, "retry"))
    assertFalse(registry.snapshotOwner(owner).pendingStartCleanup)
    assertTrue(registry.snapshotOwner(owner).failures.isEmpty())
    assertEquals(2, stops)
  }
  @Test fun `snapshot retains a release in progress across a nonblocking fence`() {
    val entered = java.util.concurrent.CountDownLatch(1)
    val finish = java.util.concurrent.CountDownLatch(1)
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) {}
      override fun stop() { entered.countDown(); check(finish.await(5, java.util.concurrent.TimeUnit.SECONDS)) }
      override fun update(title: String, body: String?) {}
    }
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" }
    val owner = Any()
    val lease = registry.acquireOwned(owner, "first")
    val thread = Thread { registry.releaseOwned(owner, lease) }
    thread.start()
    assertTrue(entered.await(5, java.util.concurrent.TimeUnit.SECONDS))
    assertTrue(registry.fenceOwner(owner, "fence"))
    assertEquals(1, registry.snapshotOwner(owner).inFlight)
    assertEquals(setOf(lease), registry.snapshotOwner(owner).leases)
    finish.countDown()
    thread.join(5000)
    assertEquals(0, registry.snapshotOwner(owner).inFlight)
    assertTrue(registry.snapshotOwner(owner).leases.isEmpty())
  }
  @Test fun `fence retains a late accepted start and exact retry releases it`() {
    val entered = java.util.concurrent.CountDownLatch(1)
    val finish = java.util.concurrent.CountDownLatch(1)
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      var stops = 0
      override fun start(reason: String) { entered.countDown(); check(finish.await(5, java.util.concurrent.TimeUnit.SECONDS)) }
      override fun stop() { stops++ }
      override fun update(title: String, body: String?) {}
    }
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "late" }
    val owner = Any()
    val retirement = "retirement"
    var result: Result<String>? = null
    val thread = Thread { result = runCatching { registry.acquireOwned(owner, "start") } }
    thread.start()
    assertTrue(entered.await(5, java.util.concurrent.TimeUnit.SECONDS))
    assertTrue(registry.fenceOwner(owner, retirement))
    assertEquals(1, registry.snapshotOwner(owner).inFlight)
    assertEquals("late", registry.snapshotOwner(owner).pendingStartId)
    assertFalse(registry.snapshotOwner(owner).pendingStartCleanup)
    assertFalse(registry.fenceOwner(owner, "other"))
    assertTrue(runCatching { registry.acquireOwned(owner, "after fence") }.isFailure)
    finish.countDown()
    thread.join(5000)
    assertEquals("late", result!!.getOrThrow())
    assertEquals(setOf("late"), registry.snapshotOwner(owner).leases)
    assertTrue(runCatching { registry.retryOwner(Any(), retirement) }.isFailure)
    assertTrue(runCatching { registry.retryOwner(owner, "other") }.isFailure)
    registry.retryOwner(owner, retirement)
    assertTrue(registry.snapshotOwner(owner).leases.isEmpty())
    assertEquals(1, driver.stops)
  }

  @Test fun `fenced cleanup retries failed stop without touching a shared owner`() {
    val driver = RecordingServiceDriver(failFirstStop = true)
    var id = 0
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-${++id}" }
    val first = Any()
    val second = Any()
    val firstLease = registry.acquireOwned(first, "first")
    val secondLease = registry.acquireOwned(second, "second")
    assertTrue(registry.fenceOwner(first, "fence-1"))
    registry.retryOwner(first, "fence-1")
    assertFalse(registry.hasLease(firstLease))
    assertTrue(registry.hasLease(secondLease))
    assertEquals(0, driver.stopCount)
    assertTrue(registry.fenceOwner(second, "fence-2"))
    assertTrue(runCatching { registry.retryOwner(second, "fence-2") }.isFailure)
    assertEquals(setOf(secondLease), registry.snapshotOwner(second).leases)
    assertEquals(1, registry.snapshotOwner(second).failures.size)
    registry.retryOwner(second, "fence-2")
    assertTrue(registry.snapshotOwner(second).leases.isEmpty())
    assertEquals(2, driver.stopCount)
  }

  @Test fun `failed accepted start is retained for only its fenced owner`() {
    var stops = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) { throw ForegroundServiceControlException("interrupted", "accepted", InterruptedException(), true) }
      override fun stop() { if (++stops < 3) throw IllegalStateException("refused $stops") }
      override fun update(title: String, body: String?) {}
    }
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" }
    val owner = Any()
    val foreign = Any()
    assertTrue(runCatching { registry.acquireOwned(owner, "first") }.isFailure)
    assertTrue(registry.snapshotOwner(owner).pendingStartCleanup)
    assertEquals("lease", registry.snapshotOwner(owner).pendingStartId)
    assertFalse(registry.snapshotOwner(foreign).pendingStartCleanup)
    assertTrue(registry.fenceOwner(owner, "fence"))
    assertTrue(runCatching { registry.retryOwner(owner, "fence") }.isFailure)
    assertTrue(registry.snapshotOwner(owner).pendingStartCleanup)
    assertEquals(2, registry.snapshotOwner(owner).failures.size)
    registry.retryOwner(owner, "fence")
    assertFalse(registry.snapshotOwner(owner).pendingStartCleanup)
  }
  @Test fun `another owner cannot clean up a failed accepted start`() {
    var stops = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) {
        throw ForegroundServiceControlException("interrupted", "accepted", InterruptedException(), true)
      }
      override fun stop() { stops++; if (stops == 1) throw IllegalStateException("refused") }
      override fun update(title: String, body: String?) {}
    }
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" }
    val first = ForegroundServiceBackgroundPort(registry)
    val second = ForegroundServiceBackgroundPort(registry)
    assertTrue(runCatching { first.acquire("connected-device", "first") }.isFailure)
    assertTrue(runCatching { second.acquire("connected-device", "second") }.isFailure)
    assertEquals(1, stops)
    assertTrue(registry.hasPendingStartCleanup())
  }
  @Test fun `colliding lease IDs cannot replace another owner's lease`() {
    val driver = RecordingServiceDriver()
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "collision" }
    val first = ForegroundServiceBackgroundPort(registry)
    val second = ForegroundServiceBackgroundPort(registry)
    val lease = first.acquire("connected-device", "first")
    assertTrue(runCatching { second.acquire("connected-device", "second") }.isFailure)
    assertTrue(runCatching { second.release(lease) }.isFailure)
    assertTrue(registry.hasLease(lease))
    assertEquals(0, driver.stopCount)
    first.release(lease)
    assertEquals(1, driver.stopCount)
  }

  @Test fun `background ports cannot release or update another owner's lease`() {
    val driver = RecordingServiceDriver()
    var id = 0
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-${++id}" }
    val first = ForegroundServiceBackgroundPort(registry)
    val second = ForegroundServiceBackgroundPort(registry)
    val lease = first.acquire("connected-device", "first")

    assertTrue(runCatching { second.release(lease) }.isFailure)
    assertTrue(runCatching { second.update(lease, "wrong owner", null) }.isFailure)
    assertTrue(registry.hasLease(lease))
    assertEquals(0, driver.stopCount)
    first.release(lease)
    assertEquals(1, driver.stopCount)
  }
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
