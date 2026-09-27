package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.background.*
import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

class PresenceForegroundContinuationTest {
  @Test fun `foreground service permission wrapper keeps the same public classification as ordinary leases`() {
    val failure = continuationPlatformFailure(ContinuationStrategy.FOREGROUND_SERVICE, ForegroundServiceControlException("foregroundServicePermissionDenied", "Bluetooth permission refused", SecurityException("binder denied")))
    assertEquals("permission.denied", failure.code)
    assertEquals("Bluetooth permission refused", failure.reason)
    assertTrue(failure.platform!!.contains("foregroundServicePermissionDenied"))
  }
  private val peer = "AA:BB:CC:DD:EE:FF"
  private val declaration = BackgroundContinuationDeclaration.recordOnly().copy(
    strategy = ContinuationStrategy.FOREGROUND_SERVICE,
    foregroundService = ContinuationForegroundService(ContinuationNotification("ble", "BLE", "Recording", null, null))
  )
  @Test fun `same peer shares its lease and failed release remains retryable`() {
    var starts = 0
    var stops = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) { error("exact configuration required") }
      override fun start(reason: String, configuration: ForegroundServiceNotificationConfiguration) { starts++ }
      override fun stop() { if (++stops == 1) throw IllegalStateException("stop refused") }
      override fun update(title: String, body: String?) {}
    }
    var id = 0
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-${++id}" }
    val owner = PresenceForegroundContinuation(registry)
    assertEquals("foreground-service-started", (owner.execute(peer, declaration) as ContinuationOutcome.Completed).stage)
    assertTrue(owner.execute(peer, declaration) is ContinuationOutcome.Completed)
    assertEquals(1, starts)
    assertTrue(owner.release(peer) is ContinuationOutcome.Failed)
    assertEquals(1, registry.activeLeaseCount())
    assertNull(owner.release(peer))
    assertEquals(0, registry.activeLeaseCount())
  }
  @Test fun `explicit presence cleanup retries an accepted start with no returned lease`() {
    var stops = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) { error("exact configuration required") }
      override fun start(reason: String, configuration: ForegroundServiceNotificationConfiguration) {
        throw ForegroundServiceControlException("foregroundServiceStartInterrupted", "accepted", null, true)
      }
      override fun stop() { if (++stops == 1) throw IllegalStateException("stop refused") }
      override fun update(title: String, body: String?) {}
    }
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" }
    val owner = PresenceForegroundContinuation(registry)
    val failed = owner.execute(peer, declaration)
    assertTrue(failed is ContinuationOutcome.Failed)
    assertTrue((failed as ContinuationOutcome.Failed).platform!!.contains("cleanupPending"))
    val detail = com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson.parse(failed.platform!!) as Map<*, *>
    val metadata = detail["metadata"] as Map<*, *>
    assertTrue("canonical platform metadata contains scalar values only", metadata.values.all { it is String || it is Number || it is Boolean })
    assertEquals(1L, metadata["cleanupFailureCount"])
    assertEquals("stop refused", metadata["cleanupFailure0Message"])
    val fixture = generateSequence(java.io.File(System.getProperty("user.dir"))) { it.parentFile }
      .map { java.io.File(it, "__tests__/fixtures/android-continuation-cleanup-wake.json") }
      .first { it.isFile }
    val emitted = ContinuationWakeRecord(1, failed.event, failed.strategy, peer, failed.code, failed.reason, platform = failed.platform).wire()
    assertEquals(com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson.parse(fixture.readText()), emitted)
    assertTrue(registry.hasPendingStartCleanup())
    assertNull(owner.release(peer))
    assertFalse(registry.hasPendingStartCleanup())
  }
  @Test fun `disappearance fences a held start and releases its late lease`() {
    val entered = CountDownLatch(1)
    val finish = CountDownLatch(1)
    var stops = 0
    val driver = object : ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) { error("exact configuration required") }
      override fun start(reason: String, configuration: ForegroundServiceNotificationConfiguration) { entered.countDown(); check(finish.await(5, TimeUnit.SECONDS)) }
      override fun stop() { stops++ }
      override fun update(title: String, body: String?) {}
    }
    val registry = ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" }
    val owner = PresenceForegroundContinuation(registry)
    var result: ContinuationOutcome? = null
    val thread = Thread { result = owner.execute(peer, declaration) }.also { it.start() }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    assertNull(owner.release(peer))
    finish.countDown(); thread.join(5000)
    assertTrue(result is ContinuationOutcome.Failed)
    assertEquals(1, stops)
    assertEquals(0, registry.activeLeaseCount())
  }
}
