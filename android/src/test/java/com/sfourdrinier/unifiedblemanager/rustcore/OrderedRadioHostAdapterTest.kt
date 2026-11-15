// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/OrderedRadioHostAdapterTest.kt
package com.sfourdrinier.unifiedblemanager.rustcore

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattService
import android.content.Context
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanSettings
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.os.ParcelUuid
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedOpenSlots
import com.ubm.core.MobileCoreBridge
import java.util.UUID
import java.util.concurrent.Executor
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.mock
import org.mockito.Mockito

class OrderedRadioHostAdapterTest {
  @Test fun callbackIngressWithoutOrderedGroupPortRemainsSupported() {
    val radio = FakeRadio()
    var advertisements = 0
    val core = object : MobileCorePort by FakeCore() {
      override fun ingestAdvertisement(advertisement: AdvertisementFacts): Int {
        advertisements++
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, radio, FakeBackground(), { null }, { null }, direct, direct) {}
    radio.events.onAdvertisement(AdvertisementFacts("peer", "peer", null, null, null, emptyList(), emptyList(), emptyList(), null, null))
    assertEquals(1, advertisements)
    assertEquals(0L, adapter.statusCounts()["ingest:advertisement:retired-owner"] ?: 0L)
  }

  @Test fun statusReportsCleanupFailureEvenWhenItsRetrySucceedsImmediately() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val radio = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    var stops = 0
    val driver = object : com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) {}
      override fun stop() { if (++stops == 1) throw IllegalStateException("first stop failed") }
      override fun update(title: String, body: String?) {}
    }
    val port = ForegroundServiceBackgroundPort(com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" })
    val queue = mutableListOf<Runnable>()
    var immediate = false
    val executor = Executor { if (immediate) it.run() else queue.add(it) }
    val receipts = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int { receipts[requestId] = result; return 0 }
    }
    val adapter = RustRadioHostAdapter(core, radio, port, { null }, { null }, Executor { it.run() }, executor) {}
    adapter.acquireBackground(1, "connected-device", "reason")
    queue.removeAt(0).run()
    val retirement = UUID.randomUUID().toString()
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement)
    queue.removeAt(0).run()
    immediate = true
    adapter.ownerStatus(3, adapter.orderedAdmissionOwner(3), retirement)
    assertTrue(receipts.getValue(3).failures.any { it.detail.contains("first stop failed") })
    assertTrue(port.snapshot().leases.isEmpty())
    native.destroy()
  }

  @Test fun deliveredUnsupportedAcquisitionsDoNotBecomeCleanupDebt() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val radio = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val driver = object : com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) { error("unsupported kind must not start") }
      override fun stop() { error("no cleanup") }
      override fun update(title: String, body: String?) {}
    }
    val port = ForegroundServiceBackgroundPort(com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" })
    val receipts = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int { receipts[requestId] = result; return 0 }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, radio, port, { null }, { null }, direct, direct) {}
    repeat(10) { adapter.acquireBackground(it.toLong() + 1, "unsupported", "reason") }
    adapter.ownerFence(20, adapter.orderedAdmissionOwner(3), UUID.randomUUID().toString())
    assertTrue(adapter.backgroundOwnershipSnapshot().acquireFailures.isEmpty())
    assertTrue(receipts.getValue(20).failures.isEmpty())
    native.destroy()
  }
  @Test fun backgroundSnapshotIncludesRegistryWorkAdmittedOutsideAdapter() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val radio = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val entered = CountDownLatch(1)
    val finish = CountDownLatch(1)
    val driver = object : com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) { entered.countDown(); check(finish.await(5, TimeUnit.SECONDS)) }
      override fun stop() {}
      override fun update(title: String, body: String?) {}
    }
    val registry = com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "external" }
    val port = ForegroundServiceBackgroundPort(registry)
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(FakeCore(), radio, port, { null }, { null }, direct, direct) {}
    val retirement = UUID.randomUUID().toString()
    val worker = Thread { port.acquire("connected-device", "external") }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      assertTrue("registry work remains owned even without an adapter request", adapter.backgroundOwnershipSnapshot().inFlight > 0)
      adapter.ownerFence(1, adapter.orderedAdmissionOwner(3), retirement)
      assertTrue("the fence must not hide an admitted registry start", adapter.backgroundOwnershipSnapshot().inFlight > 0)
    } finally {
      finish.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
    assertEquals(setOf("external"), adapter.backgroundOwnershipSnapshot().leases)
    adapter.ownerStatus(2, adapter.orderedAdmissionOwner(3), retirement)
    assertTrue(adapter.backgroundOwnershipSnapshot().leases.isEmpty())
    native.destroy()
  }
  @Test fun productionBackgroundExecutorRejectionRemainsVisibleUntilExactRetry() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val radio = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val driver = object : com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceDriver {
      var stops = 0
      override fun start(reason: String) {}
      override fun stop() { stops++ }
      override fun update(title: String, body: String?) {}
    }
    val registry = com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease" }
    val queue = mutableListOf<Runnable>()
    var reject = false
    val executor = Executor { if (reject) throw java.util.concurrent.RejectedExecutionException("busy") else queue.add(it) }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(FakeCore(), radio, ForegroundServiceBackgroundPort(registry), { null }, { null }, direct, executor) {}
    adapter.acquireBackground(1, "connected-device", "reason")
    queue.removeAt(0).run()
    val retirement = UUID.randomUUID().toString()
    reject = true
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement)
    assertEquals(setOf("lease"), adapter.backgroundOwnershipSnapshot().leases)
    assertEquals(1, adapter.backgroundOwnershipSnapshot().registryFailures.size)
    reject = false
    adapter.ownerStatus(3, adapter.orderedAdmissionOwner(3), retirement)
    queue.removeAt(0).run()
    assertTrue(adapter.backgroundOwnershipSnapshot().leases.isEmpty())
    assertTrue(adapter.backgroundOwnershipSnapshot().registryFailures.isEmpty())
    assertEquals(1, driver.stops)
    adapter.ownerStatus(4, adapter.orderedAdmissionOwner(3), retirement)
    assertTrue(queue.isEmpty())
    native.destroy()
  }
  @Test fun productionBackgroundLateStartWaitsForCompletionAndRetriesFailedCleanup() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val radio = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val startEntered = CountDownLatch(1)
    val startFinish = CountDownLatch(1)
    val completionEntered = CountDownLatch(1)
    val completionFinish = CountDownLatch(1)
    val driver = object : com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceDriver {
      var stops = 0
      override fun start(reason: String) { startEntered.countDown(); check(startFinish.await(5, TimeUnit.SECONDS)) }
      override fun stop() { if (++stops == 1) throw IllegalStateException("stop refused") }
      override fun update(title: String, body: String?) {}
    }
    val registry = com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "late" }
    val queue = java.util.concurrent.ConcurrentLinkedQueue<Runnable>()
    val direct = Executor { it.run() }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeLease(requestId: Long, leaseId: String): Int {
        completionEntered.countDown()
        check(completionFinish.await(5, TimeUnit.SECONDS))
        return 0
      }
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
    }
    val adapter = RustRadioHostAdapter(core, radio, ForegroundServiceBackgroundPort(registry), { null }, { null }, direct, Executor { queue.add(it) }) {}
    val retirement = UUID.randomUUID().toString()
    adapter.acquireBackground(1, "connected-device", "late")
    val thread = Thread { queue.remove().run() }
    thread.start()
    assertTrue(startEntered.await(5, TimeUnit.SECONDS))
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement)
    assertTrue(queue.isEmpty())
    startFinish.countDown()
    assertTrue(completionEntered.await(5, TimeUnit.SECONDS))
    assertEquals(setOf("late"), adapter.backgroundOwnershipSnapshot().leases)
    assertEquals(1, adapter.backgroundOwnershipSnapshot().inFlight)
    assertTrue(queue.isEmpty())
    completionFinish.countDown()
    thread.join(5000)
    assertFalse(thread.isAlive)
    queue.remove().run()
    assertEquals(setOf("late"), adapter.backgroundOwnershipSnapshot().leases)
    assertEquals(1, adapter.backgroundOwnershipSnapshot().registryFailures.size)
    adapter.ownerStatus(3, adapter.orderedAdmissionOwner(3), UUID.randomUUID().toString())
    assertTrue(queue.isEmpty())
    adapter.ownerStatus(4, adapter.orderedAdmissionOwner(3), retirement)
    queue.remove().run()
    assertTrue(adapter.backgroundOwnershipSnapshot().leases.isEmpty())
    assertTrue(adapter.backgroundOwnershipSnapshot().registryFailures.isEmpty())
    assertEquals(2, driver.stops)
    native.destroy()
  }
  @Test fun productionBackgroundFenceRetriesOnlyItsSharedRegistryLease() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val radio = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val driver = object : com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceDriver {
      var stops = 0
      override fun start(reason: String) {}
      override fun stop() { stops++ }
      override fun update(title: String, body: String?) {}
    }
    var id = 0
    val registry = com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry(driver) { "lease-${++id}" }
    val firstPort = ForegroundServiceBackgroundPort(registry)
    val otherPort = ForegroundServiceBackgroundPort(registry)
    val otherLease = otherPort.acquire("connected-device", "other")
    val queue = mutableListOf<Runnable>()
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(FakeCore(), radio, firstPort, { null }, { null }, direct, Executor { queue.add(it) }) {}
    assertFalse(adapter.backgroundOwnershipSnapshot().unknownOwnership)
    adapter.acquireBackground(1, "connected-device", "first")
    queue.removeAt(0).run()
    val lease = adapter.backgroundOwnershipSnapshot().leases.single()
    val retirement = UUID.randomUUID().toString()
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement)
    assertTrue(queue.isNotEmpty())
    assertEquals(setOf(lease), adapter.backgroundOwnershipSnapshot().leases)
    queue.removeAt(0).run()
    assertTrue(adapter.backgroundOwnershipSnapshot().leases.isEmpty())
    assertTrue(registry.hasLease(otherLease))
    assertEquals(0, driver.stops)
    assertTrue(runCatching { firstPort.release(otherLease) }.isFailure)
    otherPort.release(otherLease)
    assertEquals(1, driver.stops)
    native.destroy()
  }
  @Test fun failedOrdinaryStopKeepsDeliveringUntilRetrySucceeds() = failedOrdinaryStop(false)
  @Test fun failedOrdinaryStopStillDeliversNativeFailureAndRetriesCleanup() = failedOrdinaryStop(true)

  private fun failedOrdinaryStop(failureAfterStop: Boolean) {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    val device = mock(BluetoothDevice::class.java)
    val result = mock(ScanResult::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.state).thenReturn(BluetoothAdapter.STATE_ON)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    Mockito.`when`(device.address).thenReturn("AA:BB:CC:DD:EE:01")
    Mockito.`when`(result.device).thenReturn(device)
    var callback: ScanCallback? = null
    Mockito.doAnswer { callback = it.getArgument(2); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    var stops = 0
    Mockito.doAnswer { if (++stops == 1) throw SecurityException("stop denied"); null }
      .`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val core = FakeCore()
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, OwnedRadioPort(radio) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
    }.use {
      adapter.startScan(1, null, null, null, null, 1, 0, null)
      adapter.stopScan(2)
      assertTrue(core.failures.containsKey(2))
      requireNotNull(callback).onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
      assertTrue(core.advertisements.size == 1)
      if (failureAfterStop) {
        requireNotNull(callback).onScanFailed(4)
        assertTrue(core.calls.any { it.startsWith("scan-failed:") })
      }
      adapter.stopScan(3)
      requireNotNull(callback).onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
      requireNotNull(callback).onScanFailed(5)
      assertEquals(1, core.advertisements.size)
      assertEquals(if (failureAfterStop) 1 else 0, core.calls.count { it.startsWith("scan-failed:") })
      assertEquals(2, stops)
    }
  }
  @Test fun failedStartRetainsCompensatingStopFailureForRetirementRetry() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.state).thenReturn(BluetoothAdapter.STATE_ON)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    var callback: ScanCallback? = null
    Mockito.doAnswer { invocation ->
      callback = invocation.getArgument(2)
      throw IllegalArgumentException("start failed")
    }.`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    var stops = 0
    Mockito.doAnswer {
      if (++stops == 1) throw SecurityException("cleanup denied")
      null
    }.`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val fake = FakeCore()
    val retirements = mutableListOf<MobileCoreBridge.OwnerRetirementResult>()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements.add(result)
        return 0
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, OwnedRadioPort(radio) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
    }.use {
      adapter.startScan(1, null, null, null, null, 1, 0, null)
      assertEquals(RadioFailureKind.UNSUPPORTED, fake.failures.getValue(1).kind)
      assertEquals(1, stops)
      adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), UUID.randomUUID().toString())
      assertEquals(2, stops)
      Mockito.verify(scanner, Mockito.times(2)).stopScan(requireNotNull(callback))
      assertEquals("cleanup denied", retirements.single().failures.single().detail)
      assertEquals("permission-denied", retirements.single().failures.single().failureKind)
    }
  }
  @Test fun productionFenceJoinsAnAdvertisementRelay() = productionFenceJoinsScanRelay(false)
  @Test fun productionFenceJoinsAFailureRelay() = productionFenceJoinsScanRelay(true)

  @Test fun heldAdvertisementKeepsItsScanIdentityAcrossRestart() = heldScanEventKeepsIdentity(false)
  @Test fun heldFailureKeepsItsScanIdentityAcrossRestart() = heldScanEventKeepsIdentity(true)

  private fun heldScanEventKeepsIdentity(failure: Boolean) {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    val device = mock(BluetoothDevice::class.java)
    val result = mock(ScanResult::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.state).thenReturn(BluetoothAdapter.STATE_ON)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    Mockito.`when`(device.address).thenReturn("AA:BB:CC:DD:EE:01")
    Mockito.`when`(result.device).thenReturn(device)
    val callbacks = mutableListOf<ScanCallback>()
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val starts = mutableListOf<String>()
    val observed = mutableListOf<String?>()
    val failureCodes = mutableListOf<Int>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeScanStarted(requestId: Long, scanIdentity: String): Int { starts.add(scanIdentity); return 0 }
      private fun record(identity: String?): Int {
        if (entered.count > 0) { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
        synchronized(observed) { observed.add(identity) }
        return 0
      }
      override fun ingestAdvertisement(advertisement: AdvertisementFacts) = record(advertisement.scanIdentity)
      override fun ingestScanFailed(detail: String, scanIdentity: String) = record(scanIdentity)
      override fun ingestScanFailed(detail: String, scanIdentity: String, errorCode: Int): Int {
        synchronized(failureCodes) { failureCodes.add(errorCode) }
        return record(scanIdentity)
      }
    }
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, OwnedRadioPort(radio) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
    }.use {
      adapter.startScan(1, null, null, null, null, 1, 0, null)
      val worker = Thread {
        if (failure) callbacks[0].onScanFailed(1)
        else callbacks[0].onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
      }
      worker.start()
      try {
        assertTrue(entered.await(5, TimeUnit.SECONDS))
        adapter.stopScan(2)
        adapter.startScan(3, null, null, null, null, 1, 0, null)
        assertEquals(listOf("1", "3"), starts)
        assertNotEquals(starts[0], starts[1])
        release.countDown()
        worker.join(5000)
        assertFalse(worker.isAlive)
        if (failure) callbacks[1].onScanFailed(2)
        else callbacks[1].onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
        assertEquals(starts, observed)
        if (failure) assertEquals(listOf(1, 2), failureCodes)
        callbacks[0].onScanFailed(3)
        assertEquals(2, observed.size)
      } finally { release.countDown(); worker.join(5000) }
    }
  }

  private fun productionFenceJoinsScanRelay(failure: Boolean) {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    val device = mock(BluetoothDevice::class.java)
    val result = mock(ScanResult::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.state).thenReturn(BluetoothAdapter.STATE_ON)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    Mockito.`when`(device.address).thenReturn("AA:BB:CC:DD:EE:01")
    Mockito.`when`(result.device).thenReturn(device)
    var callback: ScanCallback? = null
    Mockito.doAnswer { invocation -> callback = invocation.getArgument(2); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val port = OwnedRadioPort(radio) {}
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val finished = CountDownLatch(1)
    var deliveries = 0
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
      private fun relay(): Int {
        deliveries++
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
        return 0
      }
      override fun ingestAdvertisement(advertisement: AdvertisementFacts) = relay()
      override fun ingestScanFailed(detail: String) = relay()
      override fun ingestScanFailed(detail: String, scanIdentity: String) = relay()
      override fun ingestScanFailed(detail: String, scanIdentity: String, errorCode: Int) = relay()
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
    }.use {
      adapter.startScan(1, null, null, null, null, 1, 0, null)
      val admitted = requireNotNull(callback)
      val worker = Thread {
        try {
          if (failure) admitted.onScanFailed(2) else admitted.onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
        } finally { finished.countDown() }
      }
      worker.start()
      try {
        assertTrue(entered.await(5, TimeUnit.SECONDS))
        val retirement = UUID.randomUUID()
        adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
        assertEquals(1, port.orderedSlots().ownerStatus(port.orderedSlots().incarnation, retirement).effectWork)
        if (failure) admitted.onScanFailed(3) else admitted.onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
        assertEquals(1, deliveries)
        Mockito.verify(scanner, Mockito.times(1)).stopScan(admitted)
        release.countDown()
        assertTrue(finished.await(5, TimeUnit.SECONDS))
        assertEquals(0, port.orderedSlots().ownerStatus(port.orderedSlots().incarnation, retirement).effectWork)
      } finally {
        release.countDown()
        worker.join(5000)
      }
    }
  }
  @Test fun separateNativeScansKeepEachOwnersFiltersAndSettings() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    Mockito.`when`(bluetooth.isLeCodedPhySupported).thenReturn(true)
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true }, scanSdkInt = 26)
    val parcel = mock(ParcelUuid::class.java)
    val service = "0000180d-0000-1000-8000-00805f9b34fb"
    val firstOwner = Any()
    val secondOwner = Any()
    Mockito.mockStatic(ParcelUuid::class.java).use { parcels ->
      parcels.`when`<ParcelUuid> { ParcelUuid.fromString(service) }.thenReturn(parcel)
      Mockito.mockConstruction(ScanFilter.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setDeviceAddress(Mockito.anyString())).thenReturn(builder)
        Mockito.`when`(builder.setServiceUuid(Mockito.any(ParcelUuid::class.java))).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(mock(ScanFilter::class.java))
      }.use { filters ->
        Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
          Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
          Mockito.`when`(builder.setLegacy(Mockito.anyBoolean())).thenReturn(builder)
          Mockito.`when`(builder.setPhy(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
        }.use { settings ->
          radio.startOwnedScan(firstOwner, arrayOf(service), ScanSettings.SCAN_MODE_BALANCED,
            ScanSettings.CALLBACK_TYPE_ALL_MATCHES, false, true, arrayOf("AA:BB:CC:DD:EE:01"),
            500, BluetoothDevice.PHY_LE_CODED, {}, { _, _ -> })
          radio.startOwnedScan(secondOwner, null, ScanSettings.SCAN_MODE_LOW_POWER,
            ScanSettings.CALLBACK_TYPE_FIRST_MATCH, true, true, arrayOf("AA:BB:CC:DD:EE:02"),
            0, null, {}, { _, _ -> })
          Mockito.verify(filters.constructed()[0]).setDeviceAddress("AA:BB:CC:DD:EE:01")
          Mockito.verify(filters.constructed()[0]).setServiceUuid(parcel)
          Mockito.verify(filters.constructed()[1]).setDeviceAddress("AA:BB:CC:DD:EE:02")
          Mockito.verify(filters.constructed()[1], Mockito.never()).setServiceUuid(Mockito.any(ParcelUuid::class.java))
          Mockito.verify(settings.constructed()[0]).setLegacy(false)
          Mockito.verify(settings.constructed()[0]).setPhy(BluetoothDevice.PHY_LE_CODED)
          Mockito.verify(settings.constructed()[0]).setReportDelay(500)
          Mockito.verify(settings.constructed()[1]).setCallbackType(ScanSettings.CALLBACK_TYPE_FIRST_MATCH)
          Mockito.verify(settings.constructed()[1]).setLegacy(true)
          Mockito.verify(settings.constructed()[1], Mockito.never()).setPhy(Mockito.anyInt())
          assertNull(radio.stopOwnedScan(firstOwner))
          assertNull(radio.stopOwnedScan(secondOwner))
        }
      }
    }
  }
  @Test fun delayedFailureCleanupCannotStopAReplacementSession() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    val callbacks = mutableListOf<ScanCallback>()
    Mockito.doAnswer { invocation -> callbacks.add(invocation.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = Any()
    var delayedStop: (() -> Pair<Boolean, com.sfourdrinier.unifiedblemanager.radio.OwnedRadioTeardownFailure?>)? = null
    fun start() = radio.startOwnedScan(owner, null, ScanSettings.SCAN_MODE_BALANCED,
      ScanSettings.CALLBACK_TYPE_ALL_MATCHES, true, true, emptyArray(), 0, null,
      {}, { _, stop -> delayedStop = stop })
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
    }.use {
      start()
      callbacks[0].onScanFailed(1)
      assertNotNull(delayedStop)
      assertNull(radio.stopOwnedScan(owner))
      start()
      assertTrue(requireNotNull(delayedStop).invoke().first)
      Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[1])
      assertNull(radio.stopOwnedScan(owner))
      Mockito.verify(scanner).stopScan(callbacks[1])
    }
  }
  @Test fun lostStopCompletionDoesNotStopAReplacementOrAnotherOwner() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.state).thenReturn(BluetoothAdapter.STATE_ON)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    val callbacks = mutableListOf<ScanCallback>()
    Mockito.doAnswer { invocation -> callbacks.add(invocation.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    val native = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val base = FakeCore()
    val core = object : MobileCorePort by base {
      override fun completeUnit(requestId: Long): Int =
        if (requestId == 3L) MobileCoreBridge.STATUS_LATE else base.completeUnit(requestId)
    }
    val direct = Executor { it.run() }
    val first = RustRadioHostAdapter(core, OwnedRadioPort(native) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    val second = RustRadioHostAdapter(FakeCore(), OwnedRadioPort(native) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
    }.use {
      first.startScan(1, null, null, null, null, 1, 0, null)
      second.startScan(2, null, null, null, null, 1, 0, null)
      first.stopScan(3)
      first.stopScan(4)
      Mockito.verify(scanner, Mockito.times(1)).stopScan(callbacks[0])
      Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[1])
      first.startScan(5, null, null, null, null, 1, 0, null)
      callbacks[0].onScanFailed(9)
      Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[2])
      first.stopScan(6)
      second.stopScan(7)
      Mockito.verify(scanner).stopScan(callbacks[2])
      Mockito.verify(scanner).stopScan(callbacks[1])
    }
  }
  @Test fun productionRetirementRetriesTheExactFailedStopWithoutStoppingTheOtherOwner() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.state).thenReturn(BluetoothAdapter.STATE_ON)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    val callbacks = mutableListOf<ScanCallback>()
    Mockito.doAnswer { invocation -> callbacks.add(invocation.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    val stops = mutableListOf<ScanCallback>()
    val stopFailure = IllegalStateException("stop failed")
    Mockito.doAnswer { invocation ->
      val callback = invocation.getArgument<ScanCallback>(0)
      stops.add(callback)
      if (stops.size == 1) throw stopFailure
      null
    }.`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    val native = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val firstCore = FakeCore()
    val secondCore = FakeCore()
    val retirements = mutableListOf<MobileCoreBridge.OwnerRetirementResult>()
    val core = object : MobileCorePort by firstCore {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements.add(result)
        return 0
      }
    }
    val direct = Executor { it.run() }
    val first = RustRadioHostAdapter(core, OwnedRadioPort(native) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    val second = RustRadioHostAdapter(secondCore, OwnedRadioPort(native) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
    }.use {
      first.startScan(1, null, null, null, null, 1, 0, null)
      second.startScan(2, null, null, null, null, 1, 0, null)
      val owner = first.orderedAdmissionOwner(3)
      val retirement = UUID.randomUUID().toString()
      first.ownerFence(3, owner, retirement)
      assertEquals(listOf(callbacks[0]), stops)
      assertEquals("stop failed", retirements.single().failures.single().detail)
      assertEquals("platform", retirements.single().failures.single().failureKind)
      callbacks[0].onScanFailed(7)
      assertTrue(firstCore.calls.none { it.startsWith("scan-failed:") })
      first.ownerStatus(4, owner, retirement)
      assertEquals(listOf(callbacks[0], callbacks[0]), stops)
      assertEquals(listOf("fenced-pending", "fenced-pending"), retirements.map { it.state })
      assertEquals("stop failed", retirements.last().failures.single().detail)
      first.ownerStatus(5, owner, retirement)
      assertTrue(retirements.last().failures.isEmpty())
      callbacks[1].onScanFailed(8)
      assertTrue(secondCore.calls.any { it.contains("scan failed code=8") })
      assertEquals(callbacks[1], stops.last())
    }
  }
  @Test fun productionScanOwnersStopOnlyTheirOwnCallbackAndRejectStaleFailures() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val bluetooth = mock(BluetoothAdapter::class.java)
    val scanner = mock(BluetoothLeScanner::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(bluetooth)
    Mockito.`when`(bluetooth.state).thenReturn(BluetoothAdapter.STATE_ON)
    Mockito.`when`(bluetooth.bluetoothLeScanner).thenReturn(scanner)
    val callbacks = mutableListOf<ScanCallback>()
    Mockito.doAnswer { invocation -> callbacks.add(invocation.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    val native = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val firstCore = FakeCore()
    val secondCore = FakeCore()
    val direct = Executor { it.run() }
    val first = RustRadioHostAdapter(firstCore, OwnedRadioPort(native) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    val second = RustRadioHostAdapter(secondCore, OwnedRadioPort(native) {}, FakeBackground(), { null }, { null }, direct, direct) {}
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(mock(ScanSettings::class.java))
    }.use { builders ->
      first.startScan(1, null, null, "balanced", null, 1, 0, null)
      second.startScan(2, null, null, "low-power", "first-match", 1, 250, null)
      assertEquals(2, callbacks.size)
      Mockito.verify(builders.constructed()[0]).setScanMode(ScanSettings.SCAN_MODE_BALANCED)
      Mockito.verify(builders.constructed()[0]).setCallbackType(ScanSettings.CALLBACK_TYPE_ALL_MATCHES)
      Mockito.verify(builders.constructed()[1]).setScanMode(ScanSettings.SCAN_MODE_LOW_POWER)
      Mockito.verify(builders.constructed()[1]).setCallbackType(ScanSettings.CALLBACK_TYPE_FIRST_MATCH)
      Mockito.verify(builders.constructed()[1]).setReportDelay(250)
      first.ownerFence(3, first.orderedAdmissionOwner(3), UUID.randomUUID().toString())
      Mockito.verify(scanner).stopScan(callbacks[0])
      Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[1])
      callbacks[0].onScanFailed(4)
      callbacks[1].onScanFailed(5)
      assertTrue(firstCore.calls.none { it.startsWith("scan-failed:") })
      assertTrue(secondCore.calls.any { it.contains("scan failed code=5") })
      Mockito.verify(scanner).stopScan(callbacks[1])
    }
  }
  @Test fun ownerFenceJoinsScanFailureRelayAndRejectsLateFailures() {
    val nativeRadio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(nativeRadio)
    val driver = FakeRadio()
    val port = object : AndroidRadioPort by driver, OrderedGroupPort by owner {}
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val finished = CountDownLatch(1)
    var deliveries = 0
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
      override fun ingestScanFailed(detail: String): Int {
        deliveries++
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
        return 0
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    Thread { try { driver.events.onScanFailed(2) } finally { finished.countDown() } }.start()
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    val retirement = UUID.randomUUID()
    adapter.ownerFence(1, adapter.orderedAdmissionOwner(3), retirement.toString())
    val slots = owner.orderedSlots()
    val outstanding = slots.ownerStatus(slots.incarnation, retirement).effectWork
    release.countDown()
    assertTrue(finished.await(5, TimeUnit.SECONDS))
    driver.events.onScanFailed(3)
    assertEquals(1, outstanding)
    assertEquals(1, deliveries)
    assertEquals(1, driver.calls.count { it == "stopScan" })
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    assertEquals(1L, adapter.statusCounts()["ingest:scan-failed:retired-owner"])
  }
  @Test fun ownerFenceJoinsNotificationSourceFailureRelayAndRejectsLateFailures() {
    val nativeRadio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(nativeRadio)
    val driver = FakeRadio()
    val port = object : AndroidRadioPort by driver, OrderedGroupPort by owner {}
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val finished = CountDownLatch(1)
    var deliveries = 0
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
      override fun ingestNotificationSourceFailed(instance: CharacteristicInstance, epoch: Long, failure: RadioFailure): Int {
        deliveries++
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
        return 0
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val characteristic = CharacteristicInstance("peer", "service", 0, "characteristic", 0)
    val failure = RadioFailure(RadioFailureKind.PLATFORM, null, "failure")
    Thread { try { driver.events.onNotificationSourceFailed(characteristic, 1, failure) } finally { finished.countDown() } }.start()
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    val retirement = UUID.randomUUID()
    adapter.ownerFence(1, adapter.orderedAdmissionOwner(3), retirement.toString())
    val slots = owner.orderedSlots()
    val outstanding = slots.ownerStatus(slots.incarnation, retirement).effectWork
    release.countDown()
    assertTrue(finished.await(5, TimeUnit.SECONDS))
    driver.events.onNotificationSourceFailed(characteristic, 1, failure)
    assertEquals(1, outstanding)
    assertEquals(1, deliveries)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    assertEquals(1L, adapter.statusCounts()["ingest:notification-source-failed:retired-owner"])
  }
  @Test fun ownerFenceJoinsNotificationRelayAndRejectsLateNotifications() {
    val nativeRadio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(nativeRadio)
    val driver = FakeRadio()
    val port = object : AndroidRadioPort by driver, OrderedGroupPort by owner {}
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val finished = CountDownLatch(1)
    var deliveries = 0
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
      override fun ingestNotification(instance: CharacteristicInstance, epoch: Long, value: ByteArray): Int {
        deliveries++
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
        return 0
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val characteristic = CharacteristicInstance("peer", "service", 0, "characteristic", 0)
    Thread { try { driver.events.onNotification(characteristic, 1, byteArrayOf(1)) } finally { finished.countDown() } }.start()
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    val retirement = UUID.randomUUID()
    adapter.ownerFence(1, adapter.orderedAdmissionOwner(3), retirement.toString())
    val slots = owner.orderedSlots()
    val outstanding = slots.ownerStatus(slots.incarnation, retirement).effectWork
    release.countDown()
    assertTrue(finished.await(5, TimeUnit.SECONDS))
    driver.events.onNotification(characteristic, 1, byteArrayOf(2))
    assertEquals(1, outstanding)
    assertEquals(1, deliveries)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    assertEquals(1L, adapter.statusCounts()["ingest:notification:retired-owner"])
  }
  @Test fun ownerFenceJoinsAdvertisementRelaysAndRejectsLateAdvertisements() {
    val nativeRadio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(nativeRadio)
    val driver = FakeRadio()
    val port = object : AndroidRadioPort by driver, OrderedGroupPort by owner {}
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val finished = CountDownLatch(1)
    var deliveries = 0
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
      override fun ingestAdvertisement(advertisement: AdvertisementFacts): Int {
        deliveries++
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
        return 0
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val advertisement = AdvertisementFacts("peer", "peer", null, null, null, emptyList(), emptyList(), emptyList(), null, null)
    Thread { try { driver.events.onAdvertisement(advertisement) } finally { finished.countDown() } }.start()
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    val retirement = UUID.randomUUID()
    adapter.ownerFence(1, adapter.orderedAdmissionOwner(3), retirement.toString())
    val slots = owner.orderedSlots()
    val outstanding = slots.ownerStatus(slots.incarnation, retirement).effectWork
    release.countDown()
    assertTrue(finished.await(5, TimeUnit.SECONDS))
    driver.events.onAdvertisement(advertisement)
    assertEquals(1, outstanding)
    assertEquals(1, deliveries)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    assertEquals(1L, adapter.statusCounts()["ingest:advertisement:retired-owner"])
  }
  @Test fun ownerFenceStopsAnAlreadyStartedScan() {
    val nativeRadio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(nativeRadio)
    val driver = FakeRadio()
    val port = object : AndroidRadioPort by driver, OrderedGroupPort by owner {}
    val tasks = mutableListOf<Runnable>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    adapter.startScan(1, null, null, null, null, 1, 0, null)
    tasks.removeAt(0).run()
    val retirement = UUID.randomUUID().toString()
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement)
    while (tasks.isNotEmpty()) tasks.removeAt(0).run()
    assertEquals(1, driver.calls.count { it == "stopScan" })
  }
  @Test fun ownerFenceTracksConnectWhileDriverCallIsExecuting() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val finished = CountDownLatch(1)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun connect(peerId: String, autoConnect: Boolean, phyMask: Int, attempt: com.sfourdrinier.unifiedblemanager.radio.GattConnectAttempt) {
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
      }
    }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null },
      Executor { task -> Thread { try { task.run() } finally { finished.countDown() } }.start() }, Executor { it.run() }) {}
    val retirement = UUID.randomUUID()
    adapter.connect(1, "AA:BB:CC:DD:EE:FF", false, emptyArray(), "generation-1")
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    release.countDown()
    assertTrue(finished.await(5, TimeUnit.SECONDS))
  }

  @Test fun cancelledQueuedConnectDoesNotDispatchAcrossOwnerFence() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val driver = FakeRadio()
    val port = object : AndroidRadioPort by driver, OrderedGroupPort by owner {}
    val tasks = mutableListOf<Runnable>()
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val retirement = UUID.randomUUID()
    adapter.connect(1, "AA:BB:CC:DD:EE:FF", false, emptyArray(), "generation-1")
    adapter.cancel(1)
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
    tasks.single().run()
    assertTrue(driver.calls.none { it.startsWith("connect:") })
    assertEquals(false, fake.failures.getValue(1).dispatched)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
  }

  @Test fun ownerFencePreventsQueuedAndNewConnectEffects() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val driver = FakeRadio()
    val port = object : AndroidRadioPort by driver, OrderedGroupPort by owner {}
    val tasks = mutableListOf<Runnable>()
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val retirement = UUID.randomUUID()
    adapter.connect(1, "AA:BB:CC:DD:EE:FF", false, emptyArray(), "generation-1")
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    tasks.removeAt(0).run()
    assertTrue(driver.calls.none { it.startsWith("connect:") })
    assertEquals(false, fake.failures.getValue(1).dispatched)
    adapter.connect(3, "AA:BB:CC:DD:EE:FF", false, emptyArray(), "generation-2")
    assertTrue(tasks.isEmpty())
    assertEquals(false, fake.failures.getValue(3).dispatched)
  }
  @Test fun ownerFenceRetainsExecutingScanThroughItsCompletion() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val finished = CountDownLatch(1)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun startScan(serviceUuids: List<String>, deviceAddresses: List<String>, mode: Int, callbackType: Int,
                             legacy: Boolean, reportDelayMs: Long, phy: Int?) {
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
      }
    }
    val retirement = UUID.randomUUID()
    var workDuringCompletion = -1
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
      override fun completeUnit(requestId: Long): Int {
        workDuringCompletion = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
        return 0
      }
      override fun completeScanStarted(requestId: Long, scanIdentity: String) = completeUnit(requestId)
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { task -> Thread { try { task.run() } finally { finished.countDown() } }.start() }, Executor { it.run() }) {}
    adapter.startScan(1, null, null, null, null, 1, 0, null)
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    release.countDown()
    assertTrue(finished.await(5, TimeUnit.SECONDS))
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    assertEquals(1, workDuringCompletion)
  }
  @Test fun ownerFencePreventsQueuedAndNewScanEffects() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    var starts = 0
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun startScan(serviceUuids: List<String>, deviceAddresses: List<String>, mode: Int, callbackType: Int,
                             legacy: Boolean, reportDelayMs: Long, phy: Int?) { starts++ }
    }
    val tasks = mutableListOf<Runnable>()
    val fake = FakeCore()
    var workDuringCompletion = -1
    val retirementId = UUID.randomUUID()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
      override fun completeFailure(requestId: Long, failure: RadioFailure): Int {
        if (requestId == 1L) workDuringCompletion = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirementId).effectWork
        return fake.completeFailure(requestId, failure)
      }
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val ownerId = adapter.orderedAdmissionOwner(3)
    val retirement = retirementId.toString()
    adapter.startScan(1, null, null, null, null, 1, 0, null)
    adapter.ownerFence(2, ownerId, retirement)
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirementId).effectWork)
    tasks.removeAt(0).run()
    assertEquals(0, starts)
    assertEquals(1, workDuringCompletion)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirementId).effectWork)
    assertEquals(false, fake.failures.getValue(1).dispatched)
    adapter.startScan(3, null, null, null, null, 1, 0, null)
    assertEquals(0, tasks.size)
    assertEquals(false, fake.failures.getValue(3).dispatched)
  }

  @Test fun ownerFencePreventsQueuedGraphAuthorityAdmission() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val tasks = mutableListOf<Runnable>()
    val core = FakeCore()
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val retirement = UUID.randomUUID()
    val ownerId = adapter.orderedAdmissionOwner(3)

    adapter.orderedGraphAuthority(1, "reserve", 91)
    adapter.ownerFence(2, ownerId, retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(UUID.fromString(ownerId), retirement).effectWork)
    assertEquals(1, tasks.size)
    tasks.removeAt(0).run()

    assertEquals(0, owner.orderedSlots().ownerStatus(UUID.fromString(ownerId), retirement).effectWork)
    assertEquals(false, core.failures.getValue(1).dispatched)
    val status = owner.orderedGraphAuthority("reserve-status", 91uL) as GraphAuthorityPortResponse.Reservation
    assertEquals(com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus.Pending, status.status)
    native.destroy()
  }

  @Test fun ownerFencePreventsMarkerReservationBeforeQueuedDispatch() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    var reservations = 0
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedReserveMarkerWrite(
        generation: ULong, group: ULong, markerId: ULong, connectionGeneration: String,
        graph: com.sfourdrinier.unifiedblemanager.radio.AndroidNativeGraphIdentity,
        instance: CharacteristicInstance, value: ByteArray, withResponse: Boolean,
        cancellation: com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation
      ): com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.Reservation {
        reservations++
        error("a fenced owner must not reserve a marker")
      }
    }
    val fake = FakeCore()
    val tasks = mutableListOf<Runnable>()
    val adapter = RustRadioHostAdapter(fake, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val retirement = UUID.randomUUID()
    adapter.orderedMarkerWrite(1, "write", "counter", "1", 1, 1, "connection", "owner", 1, 1,
      "peer", UUID.randomUUID().toString(), 0, UUID.randomUUID().toString(), 0, byteArrayOf(1), true)
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, tasks.size)
    tasks.single().run()

    assertEquals(0, reservations)
    assertEquals(false, fake.failures.getValue(1).dispatched)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    native.destroy()
  }

  @Test fun ownerFencePreventsOrderedEnableAdmissionBeforeQueuedDispatch() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    var claimChecks = 0
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedHasClaim(generation: ULong, group: ULong, member: OrderedPortRegistration, instance: CharacteristicInstance): Boolean {
        claimChecks++
        error("a fenced owner must not admit an ordered enable")
      }
    }
    val fake = FakeCore()
    val tasks = mutableListOf<Runnable>()
    val adapter = RustRadioHostAdapter(fake, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val retirement = UUID.randomUUID()
    adapter.orderedEnable(1, "enable", "peer", UUID.randomUUID().toString(), 0, UUID.randomUUID().toString(), 0,
      "connection", "counter", "1", 1, 1, 1, 1, 1, "notification", null)
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, tasks.size)
    tasks.single().run()

    assertEquals(0, claimChecks)
    val release = fake.orderedEnableReleases.getValue(1)
    assertEquals(false, release.dispatched)
    assertEquals(RadioFailureKind.CANCELLED, release.kind)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    native.destroy()
  }

  @Test fun ownerFencePreventsLegacyOrderedOpenAdmissionBeforeQueuedDispatch() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    var admissions = 0
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedAdmitOpen(group: ULong, request: ULong, peer: String): com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner.OpenAdmission {
        admissions++
        error("a fenced owner must not reserve a legacy ordered open")
      }
    }
    val fake = FakeCore()
    val tasks = mutableListOf<Runnable>()
    val adapter = RustRadioHostAdapter(fake, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val retirement = UUID.randomUUID()
    val service = UUID.randomUUID().toString()
    val characteristic = UUID.randomUUID().toString()
    adapter.orderedOpen(1, 1, longArrayOf(1), arrayOf("peer"), arrayOf(service), longArrayOf(0), arrayOf(characteristic), longArrayOf(0),
      8, 32, "connection", "database", "owner", 1, 1)
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, tasks.size)
    tasks.single().run()

    assertEquals(0, admissions)
    assertEquals(false, fake.failures.getValue(1).dispatched)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    native.destroy()
  }

  @Test fun ownerFencePreventsQueuedOrderedRegistrationMutation() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    var registrations = 0
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedRegister(generation: ULong, group: ULong, member: OrderedPortRegistration): OrderedPortRegistration {
        registrations++
        error("a fenced owner must not register a source member")
      }
    }
    val fake = FakeCore()
    val tasks = mutableListOf<Runnable>()
    val adapter = RustRadioHostAdapter(fake, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val retirement = UUID.randomUUID()
    adapter.orderedRequest(1, "register", "counter", "1", 1, 1, 1, 1, 0)
    adapter.ownerFence(2, adapter.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, tasks.size)
    tasks.single().run()

    assertEquals(0, registrations)
    assertEquals(false, fake.failures.getValue(1).dispatched)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    native.destroy()
  }

  @Test fun ownerControlRejectsNonRfcUuidWithoutFencingSlots() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = 0
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    adapter.ownerFence(99, adapter.orderedAdmissionOwner(3), "00000000-0000-4000-0000-000000000000")
    assertTrue(fake.failures.containsKey(99))
    val retirement = UUID.randomUUID()
    assertEquals(null, owner.orderedSlots().ownerControl(owner.orderedSlots().incarnation, retirement, true))
  }
  @Test fun backgroundAcquireFailurePreservesUnknownOwnershipAndOriginalFailure() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val failure = IllegalStateException("start outcome unknown")
    val background = object : BackgroundPort {
      override fun acquire(kind: String, reason: String): String { throw failure }
      override fun release(leaseId: String) {}
      override fun update(leaseId: String, title: String, body: String?) {}
    }
    val direct = Executor { it.run() }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeFailure(requestId: Long, failure: RadioFailure) = MobileCoreBridge.STATUS_LATE
    }
    val adapter = RustRadioHostAdapter(core, port, background, { null }, { null }, direct, direct) {}
    adapter.acquireBackground(1, "connected-device", "reason")
    val snapshot = adapter.backgroundOwnershipSnapshot()
    assertTrue(snapshot.unknownOwnership)
    assertEquals(1L, snapshot.acquireFailures.single().first)
    assertSame(failure, snapshot.acquireFailures.single().second)
    assertEquals(0, snapshot.inFlight)
    native.destroy()
  }
  @Test fun backgroundSnapshotRetainsLateAcquireAndRetryFailureForExactOwner() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val firstOwner = OwnedOrderedGroupPort(radio)
    val secondOwner = OwnedOrderedGroupPort(radio)
    val firstPort = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by firstOwner {}
    val secondPort = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by secondOwner {}
    val entered = java.util.concurrent.CountDownLatch(1)
    val finish = java.util.concurrent.CountDownLatch(1)
    val finished = java.util.concurrent.CountDownLatch(1)
    var failRelease = true
    val background = object : BackgroundPort {
      override fun acquire(kind: String, reason: String): String {
        entered.countDown()
        check(finish.await(5, java.util.concurrent.TimeUnit.SECONDS))
        return "same-lease"
      }
      override fun release(leaseId: String) { if (failRelease) throw IllegalStateException("retry release") }
      override fun update(leaseId: String, title: String, body: String?) {}
    }
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    lateinit var first: RustRadioHostAdapter
    var inFlightDuringCompletion = -1
    val core = object : MobileCorePort by FakeCore() {
      override fun completeLease(requestId: Long, leaseId: String): Int {
        inFlightDuringCompletion = first.backgroundOwnershipSnapshot().inFlight
        return 0
      }
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return 0
      }
    }
    val direct = Executor { it.run() }
    first = RustRadioHostAdapter(core, firstPort, background, { null }, { null }, direct,
      Executor { task -> if (entered.count == 0L) task.run() else Thread { try { task.run() } finally { finished.countDown() } }.start() }) {}
    val second = RustRadioHostAdapter(FakeCore(), secondPort, background, { null }, { null }, direct, direct) {}
    // The shared port exposes no authoritative per-owner registry snapshot.
    assertTrue(first.backgroundOwnershipSnapshot().unknownOwnership)
    first.acquireBackground(1, "connected-device", "reason")
    assertTrue(entered.await(5, java.util.concurrent.TimeUnit.SECONDS))
    assertEquals(firstOwner.orderedSlots().incarnation, first.backgroundOwnershipSnapshot().ownerIncarnation)
    assertEquals(secondOwner.orderedSlots().incarnation, second.backgroundOwnershipSnapshot().ownerIncarnation)
    assertEquals(1, first.backgroundOwnershipSnapshot().inFlight)
    assertEquals(0, second.backgroundOwnershipSnapshot().inFlight)
    val retirement = UUID.randomUUID().toString()
    first.ownerFence(2, first.orderedAdmissionOwner(3), retirement)
    finish.countDown()
    assertTrue(finished.await(5, java.util.concurrent.TimeUnit.SECONDS))
    assertEquals(1, inFlightDuringCompletion)
    assertEquals(setOf("same-lease"), first.backgroundOwnershipSnapshot().leases)
    assertTrue(second.backgroundOwnershipSnapshot().leases.isEmpty())
    second.acquireBackground(6, "connected-device", "other owner")
    assertEquals(setOf("same-lease"), second.backgroundOwnershipSnapshot().leases)
    first.releaseBackground(3, "same-lease")
    assertEquals(setOf("same-lease"), first.backgroundOwnershipSnapshot().leases)
    assertEquals(1, first.backgroundOwnershipSnapshot().releaseFailures.size)
    first.ownerStatus(5, first.orderedAdmissionOwner(3), retirement)
    assertEquals("fenced-pending", retirements.getValue(5).state)
    assertEquals(1, retirements.getValue(5).failures.size)
    failRelease = false
    first.releaseBackground(4, "same-lease")
    assertTrue(first.backgroundOwnershipSnapshot().leases.isEmpty())
    assertTrue(first.backgroundOwnershipSnapshot().releaseFailures.isEmpty())
    assertEquals(setOf("same-lease"), second.backgroundOwnershipSnapshot().leases)
    radio.destroy()
  }
  @Test fun ownerControlFencesQueuedOpenAndRemainsPendingThroughDeliveryAndAfterDrain() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val tasks = mutableListOf<Runnable>()
    val receipts = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val retirement = UUID.randomUUID().toString()
    lateinit var adapter: RustRadioHostAdapter
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        receipts[requestId] = result
        return MobileCoreBridge.STATUS_DROPPED_CONTROL
      }
      override fun completeFailure(requestId: Long, failure: RadioFailure): Int {
        if (requestId == 10L) {
          assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, UUID.fromString(retirement)).adapterWork)
          adapter.ownerStatus(13, owner.orderedSlots().incarnation.toString(), retirement)
        }
        return MobileCoreBridge.STATUS_DROPPED_CONTROL
      }
      override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult) = MobileCoreBridge.STATUS_DROPPED_CONTROL
    }
    adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val own = adapter.orderedAdmissionOwner(3)
    val id = MobileCoreBridge.OrderedOpenIdentity(2, own, 0, 1, 10, 20, "peer", "connection", "database", "snapshot", 1, 2,
      longArrayOf(3), arrayOf("peer"), arrayOf("service"), longArrayOf(0), arrayOf("characteristic"), longArrayOf(0), 8, 32)
    adapter.orderedOpenV2(10, id)
    adapter.ownerFence(11, own, retirement)
    adapter.orderedAcknowledgeOpenV2(12, id)
    assertEquals(1, owner.orderedSlots().ownerStatus(UUID.fromString(own), UUID.fromString(retirement)).adapterWork)
    tasks.single().run()
    assertEquals("fenced-pending", receipts.getValue(13).state)
    assertEquals(0, owner.orderedSlots().ownerStatus(UUID.fromString(own), UUID.fromString(retirement)).adapterWork)
    adapter.ownerStatus(14, own, retirement)
    assertEquals("fenced-pending", receipts.getValue(14).state)
  }

  @Test fun ownerFenceRejectsLateNativeOrderedSourceWake() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    var wake: ((ULong, ULong) -> Unit)? = null
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedOpenSlot(
        identity: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedOpenSlots.Identity,
        onWakeFailure: (ULong, ULong, Throwable) -> Unit,
        wakeCallback: (ULong, ULong) -> Unit
      ): com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedOpenSlots.Result {
        wake = wakeCallback
        return com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedOpenSlots.Result.Failed(
          IllegalStateException("test source did not install"),
          com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedOpenSlots.Ownership.NotCreated
        )
      }
    }
    val wakes = mutableListOf<Pair<Long, Long>>()
    val core = object : MobileCorePort by FakeCore() {
      override fun ingestOrderedWake(generation: Long, group: Long): Int {
        wakes += generation to group
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val ownerId = adapter.orderedAdmissionOwner(3)
    val identity = MobileCoreBridge.OrderedOpenIdentity(2, ownerId, 0, 1, 10, 20, "peer", "connection", "database", "snapshot", 1, 2,
      longArrayOf(3), arrayOf("peer"), arrayOf("service"), longArrayOf(0), arrayOf("characteristic"), longArrayOf(0), 8, 32)

    adapter.orderedOpenV2(10, identity)
    assertNotNull(wake)
    val retirement = UUID.randomUUID()
    adapter.ownerFence(11, ownerId, retirement.toString())
    wake!!.invoke(7u, 20u)

    assertTrue(wakes.isEmpty())
    assertEquals(0, owner.orderedSlots().ownerStatus(UUID.fromString(ownerId), retirement).effectWork)
    native.destroy()
  }

  @Test fun v4FenceLinearizesActualOrderedWakeCoordinatorAndSlotAdmission() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    var wake: ((ULong, ULong) -> Unit)? = null
    val coordinatorAdmitted = CountDownLatch(1)
    val allowSlotReservation = CountDownLatch(1)
    val coreEntered = CountDownLatch(1)
    val allowCore = CountDownLatch(1)
    val fenceStarted = CountDownLatch(1)
    val fenceReturned = CountDownLatch(1)
    val blockNextSlotLookup = AtomicBoolean(false)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedSlots(): AndroidOrderedOpenSlots {
        if (blockNextSlotLookup.compareAndSet(true, false)) {
          coordinatorAdmitted.countDown()
          check(allowSlotReservation.await(5, TimeUnit.SECONDS)) { "ordered wake slot reservation did not resume" }
        }
        return owner.orderedSlots()
      }

      override fun orderedOpenSlot(
        identity: AndroidOrderedOpenSlots.Identity,
        onWakeFailure: (ULong, ULong, Throwable) -> Unit,
        wakeCallback: (ULong, ULong) -> Unit
      ): AndroidOrderedOpenSlots.Result {
        wake = wakeCallback
        return AndroidOrderedOpenSlots.Result.Failed(
          IllegalStateException("test source did not install"),
          AndroidOrderedOpenSlots.Ownership.NotCreated
        )
      }
    }
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val wakes = mutableListOf<Pair<Long, Long>>()
    val core = object : MobileCorePort by FakeCore() {
      override fun ingestOrderedWake(generation: Long, group: Long): Int {
        wakes += generation to group
        coreEntered.countDown()
        check(allowCore.await(5, TimeUnit.SECONDS)) { "ordered wake core ingress did not resume" }
        return MobileCoreBridge.STATUS_ACCEPTED
      }

      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return MobileCoreBridge.STATUS_ACCEPTED
      }

      override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult): Int =
        MobileCoreBridge.STATUS_ACCEPTED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    // The ordered-open slot remains the v3 slot owner. The installed native
    // source callback is nevertheless admitted by the independent v4 owner
    // coordinator that this test fences below.
    val slotOwner = adapter.orderedAdmissionOwner(3)
    val v4Owner = adapter.orderedAdmissionOwner(4)
    val identity = MobileCoreBridge.OrderedOpenIdentity(2, slotOwner, 0, 1, 10, 20, "peer", "connection", "database", "snapshot", 1, 2,
      longArrayOf(3), arrayOf("peer"), arrayOf("service"), longArrayOf(0), arrayOf("characteristic"), longArrayOf(0), 8, 32)
    adapter.orderedOpenV2(10, identity)
    assertNotNull(wake)
    // The synthetic open has deliberately failed after handing us the native
    // source callback. Retire that terminal slot before testing callback
    // debt, so it cannot mask the callback's eventual quiescence.
    adapter.orderedAcknowledgeOpenV2(9, identity)
    val retirement = UUID.randomUUID().toString()
    blockNextSlotLookup.set(true)
    val callback = Thread { wake!!.invoke(7u, 20u) }
    val fence = Thread {
      fenceStarted.countDown()
      adapter.ownerFence(11, v4Owner, retirement)
      fenceReturned.countDown()
    }
    callback.start()
    try {
      assertTrue(coordinatorAdmitted.await(5, TimeUnit.SECONDS))
      fence.start()
      assertTrue(fenceStarted.await(5, TimeUnit.SECONDS))
      assertTrue("fence must block behind the coordinator-to-slot admission transaction", awaitBlocked(fence))
      allowSlotReservation.countDown()
      assertTrue(coreEntered.await(5, TimeUnit.SECONDS))
      assertTrue(fenceReturned.await(5, TimeUnit.SECONDS))
      assertEquals("fenced-pending", retirements.getValue(11).state)
      allowCore.countDown()
      callback.join(5_000)
      assertFalse(callback.isAlive)
      adapter.ownerStatus(12, v4Owner, retirement)
      assertEquals("quiescent", retirements.getValue(12).state)
      wake!!.invoke(8u, 20u)
      assertEquals(listOf(7L to 20L), wakes)
      assertEquals(1L, adapter.statusCounts()["ingest:ordered-wake:retired-owner"])
    } finally {
      allowSlotReservation.countDown()
      allowCore.countDown()
      callback.join(5_000)
      fence.join(5_000)
      native.destroy()
    }
  }

  @Test fun v4FenceLinearizesActualOrderedWakeFailureCoordinatorAndSlotAdmission() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    var wakeFailure: ((ULong, ULong, Throwable) -> Unit)? = null
    val coordinatorAdmitted = CountDownLatch(1)
    val allowSlotReservation = CountDownLatch(1)
    val coreEntered = CountDownLatch(1)
    val allowCore = CountDownLatch(1)
    val fenceStarted = CountDownLatch(1)
    val fenceReturned = CountDownLatch(1)
    val blockNextSlotLookup = AtomicBoolean(false)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedSlots(): AndroidOrderedOpenSlots {
        if (blockNextSlotLookup.compareAndSet(true, false)) {
          coordinatorAdmitted.countDown()
          check(allowSlotReservation.await(5, TimeUnit.SECONDS)) { "ordered wake failure slot reservation did not resume" }
        }
        return owner.orderedSlots()
      }

      override fun orderedOpenSlot(
        identity: AndroidOrderedOpenSlots.Identity,
        onWakeFailure: (ULong, ULong, Throwable) -> Unit,
        wakeCallback: (ULong, ULong) -> Unit
      ): AndroidOrderedOpenSlots.Result {
        wakeFailure = onWakeFailure
        return AndroidOrderedOpenSlots.Result.Failed(
          IllegalStateException("test source did not install"),
          AndroidOrderedOpenSlots.Ownership.NotCreated
        )
      }
    }
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val failures = mutableListOf<Pair<Long, Long>>()
    val core = object : MobileCorePort by FakeCore() {
      override fun ingestOrderedWakeFailure(generation: Long, group: Long): Int {
        failures += generation to group
        coreEntered.countDown()
        check(allowCore.await(5, TimeUnit.SECONDS)) { "ordered wake failure core ingress did not resume" }
        return MobileCoreBridge.STATUS_ACCEPTED
      }

      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return MobileCoreBridge.STATUS_ACCEPTED
      }

      override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult): Int =
        MobileCoreBridge.STATUS_ACCEPTED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val slotOwner = adapter.orderedAdmissionOwner(3)
    val v4Owner = adapter.orderedAdmissionOwner(4)
    val identity = MobileCoreBridge.OrderedOpenIdentity(2, slotOwner, 0, 1, 10, 20, "peer", "connection", "database", "snapshot", 1, 2,
      longArrayOf(3), arrayOf("peer"), arrayOf("service"), longArrayOf(0), arrayOf("characteristic"), longArrayOf(0), 8, 32)
    adapter.orderedOpenV2(10, identity)
    assertNotNull(wakeFailure)
    adapter.orderedAcknowledgeOpenV2(9, identity)
    val retirement = UUID.randomUUID().toString()
    blockNextSlotLookup.set(true)
    val callback = Thread { wakeFailure!!.invoke(7u, 20u, IllegalStateException("native wake failure")) }
    val fence = Thread {
      fenceStarted.countDown()
      adapter.ownerFence(11, v4Owner, retirement)
      fenceReturned.countDown()
    }
    callback.start()
    try {
      assertTrue(coordinatorAdmitted.await(5, TimeUnit.SECONDS))
      fence.start()
      assertTrue(fenceStarted.await(5, TimeUnit.SECONDS))
      assertTrue("fence must block behind the coordinator-to-slot admission transaction", awaitBlocked(fence))
      allowSlotReservation.countDown()
      assertTrue(coreEntered.await(5, TimeUnit.SECONDS))
      assertTrue(fenceReturned.await(5, TimeUnit.SECONDS))
      assertEquals("fenced-pending", retirements.getValue(11).state)
      allowCore.countDown()
      callback.join(5_000)
      assertFalse(callback.isAlive)
      adapter.ownerStatus(12, v4Owner, retirement)
      assertEquals("quiescent", retirements.getValue(12).state)
      wakeFailure!!.invoke(8u, 20u, IllegalStateException("late native wake failure"))
      assertEquals(listOf(7L to 20L), failures)
      assertEquals(1L, adapter.statusCounts()["ingest:ordered-wake-failure:retired-owner"])
    } finally {
      allowSlotReservation.countDown()
      allowCore.countDown()
      callback.join(5_000)
      fence.join(5_000)
      native.destroy()
    }
  }

  @Test fun ownerControlRetainsFenceAndRejectsStaleOrConflictingIdentities() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val captured = mutableMapOf<Long, Any>()
    val fake = FakeCore()
    val core = java.lang.reflect.Proxy.newProxyInstance(MobileCorePort::class.java.classLoader, arrayOf(MobileCorePort::class.java)) { _, method, args ->
      if (method.name == "completeOwnerRetirement") { captured[args[0] as Long] = args[1]; MobileCoreBridge.STATUS_DROPPED_CONTROL }
      else method.invoke(fake, *(args ?: emptyArray()))
    } as MobileCorePort
    val tasks = mutableListOf<Runnable>()
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val own = adapter.orderedAdmissionOwner(3)
    val retirement = UUID.randomUUID().toString()
    val other = UUID.randomUUID().toString()
    val type = MobileCoreBridge.RadioHost::class.java
    fun call(name: String, id: Long, ownerId: String, retirementId: String) = type.getMethod(name, java.lang.Long.TYPE, String::class.java, String::class.java).invoke(adapter, id, ownerId, retirementId)
    fun field(id: Long, name: String) = captured.getValue(id).javaClass.getField(name).get(captured.getValue(id))
    call("ownerFence", 1, other, retirement)
    assertEquals("wrong-owner", field(1, "reason"))
    assertEquals(other, field(1, "ownerIncarnation"))
    call("ownerFence", 2, own, retirement)
    call("ownerStatus", 3, own, retirement)
    call("ownerFence", 4, own, retirement)
    for (id in 2L..4L) {
      assertEquals("fenced-pending", field(id, "state"))
      assertEquals(own, field(id, "ownerIncarnation"))
      assertEquals(retirement, field(id, "retirementId"))
      assertEquals(setOf("attachments", "reservations", "native-work", "callbacks", "cleanup"), (field(id, "outstandingClasses") as Array<*>).toSet())
    }
    call("ownerFence", 5, own, other)
    call("ownerStatus", 6, own, other)
    assertEquals("conflicting-retirement", field(5, "reason"))
    assertEquals("conflicting-retirement", field(6, "reason"))
    assertNotEquals("", adapter.orderedAdmissionOwner(4))
    assertNotEquals(own, adapter.orderedAdmissionOwner(4))
    assertTrue(tasks.isEmpty())
  }

  @Test fun v4OwnerFencesEveryAdapterAdmissionAndConfirmsAnIdleOwner() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}

    val ownerId = adapter.orderedAdmissionOwner(4)
    assertNotEquals(owner.orderedSlots().incarnation.toString(), ownerId)
    val retirement = UUID.randomUUID().toString()
    adapter.ownerFence(1, ownerId, retirement)
    adapter.ownerStatus(2, ownerId, retirement)

    assertEquals("quiescent", retirements.getValue(1).state)
    assertEquals("quiescent", retirements.getValue(2).state)
    assertTrue(retirements.getValue(2).failures.isEmpty())
    native.destroy()
  }

  @Test fun v4FenceRejectsAQueuedEffectBeforeNativeAdmissionThenBecomesQuiescent() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val failures = mutableListOf<Long>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return MobileCoreBridge.STATUS_ACCEPTED
      }
      override fun completeFailure(requestId: Long, failure: RadioFailure): Int {
        failures += requestId
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val tasks = mutableListOf<Runnable>()
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks += it }, direct) {}
    val ownerId = adapter.orderedAdmissionOwner(4)
    val retirement = UUID.randomUUID().toString()

    adapter.startScan(10, null, null, null, null, 0, 0, null)
    assertEquals(1, tasks.size)
    adapter.ownerFence(1, ownerId, retirement)
    assertEquals("fenced-pending", retirements.getValue(1).state)
    adapter.startScan(11, null, null, null, null, 0, 0, null)
    assertEquals(listOf(11L), failures)
    tasks.removeAt(0).run()
    assertEquals(listOf(11L, 10L), failures)
    adapter.ownerStatus(2, ownerId, retirement)
    assertEquals("quiescent", retirements.getValue(2).state)
    native.destroy()
  }

  @Test fun v4FenceLinearizesOrderedOpenCoordinatorAndAdapterWorkReservation() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val coordinatorAdmitted = CountDownLatch(1)
    val allowSlotReservation = CountDownLatch(1)
    val fenceReturned = CountDownLatch(1)
    val firstSlotLookup = AtomicBoolean(true)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedSlots(): AndroidOrderedOpenSlots {
        if (firstSlotLookup.compareAndSet(true, false)) {
          coordinatorAdmitted.countDown()
          check(allowSlotReservation.await(5, TimeUnit.SECONDS)) { "ordered open slot reservation did not resume" }
        }
        return owner.orderedSlots()
      }
    }
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val completions = mutableListOf<Long>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return MobileCoreBridge.STATUS_ACCEPTED
      }
      override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult): Int {
        completions += requestId
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val tasks = mutableListOf<Runnable>()
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks += it }, direct) {}
    val slotOwner = adapter.orderedAdmissionOwner(3)
    val v4Owner = adapter.orderedAdmissionOwner(4)
    val identity = MobileCoreBridge.OrderedOpenIdentity(2, slotOwner, 0, 1, 10, 20, "peer", "connection", "database", "snapshot", 1, 2,
      longArrayOf(3), arrayOf("peer"), arrayOf("service"), longArrayOf(0), arrayOf("characteristic"), longArrayOf(0), 8, 32)
    val retirement = UUID.randomUUID().toString()
    val open = Thread { adapter.orderedOpenV2(10, identity) }
    val fence = Thread {
      adapter.ownerFence(1, v4Owner, retirement)
      fenceReturned.countDown()
    }
    open.start()
    try {
      assertTrue(coordinatorAdmitted.await(5, TimeUnit.SECONDS))
      fence.start()
      assertFalse("fence must not close slots after coordinator admitted ordered open", fenceReturned.await(100, TimeUnit.MILLISECONDS))
      allowSlotReservation.countDown()
      open.join(5_000)
      assertFalse(open.isAlive)
      assertTrue(fenceReturned.await(5, TimeUnit.SECONDS))
      assertEquals("fenced-pending", retirements.getValue(1).state)
      assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, UUID.fromString(retirement)).adapterWork)
      tasks.single().run()
      assertEquals(listOf(10L), completions)
      adapter.orderedAcknowledgeOpenV2(3, identity)
      adapter.ownerStatus(2, v4Owner, retirement)
      assertEquals("quiescent", retirements.getValue(2).state)
    } finally {
      allowSlotReservation.countDown()
      open.join(5_000)
      fence.join(5_000)
      native.destroy()
    }
  }

  @Test fun v4FenceLinearizesEffectCoordinatorAndSlotReservation() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val coordinatorAdmitted = CountDownLatch(1)
    val allowSlotReservation = CountDownLatch(1)
    val fenceReturned = CountDownLatch(1)
    val firstSlotLookup = AtomicBoolean(true)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun orderedSlots(): AndroidOrderedOpenSlots {
        if (firstSlotLookup.compareAndSet(true, false)) {
          coordinatorAdmitted.countDown()
          check(allowSlotReservation.await(5, TimeUnit.SECONDS)) { "owner effect slot reservation did not resume" }
        }
        return owner.orderedSlots()
      }
    }
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val failures = mutableListOf<Long>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return MobileCoreBridge.STATUS_ACCEPTED
      }
      override fun completeFailure(requestId: Long, failure: RadioFailure): Int {
        failures += requestId
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val tasks = mutableListOf<Runnable>()
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks += it }, direct) {}
    val ownerId = adapter.orderedAdmissionOwner(4)
    val retirement = UUID.randomUUID().toString()
    val effect = Thread { adapter.startScan(10, null, null, null, null, 0, 0, null) }
    val fence = Thread {
      adapter.ownerFence(1, ownerId, retirement)
      fenceReturned.countDown()
    }
    effect.start()
    try {
      assertTrue(coordinatorAdmitted.await(5, TimeUnit.SECONDS))
      fence.start()
      assertFalse("fence must not close slots after coordinator admitted host effect", fenceReturned.await(100, TimeUnit.MILLISECONDS))
      allowSlotReservation.countDown()
      effect.join(5_000)
      assertFalse(effect.isAlive)
      assertTrue(fenceReturned.await(5, TimeUnit.SECONDS))
      assertEquals("fenced-pending", retirements.getValue(1).state)
      assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, UUID.fromString(retirement)).effectWork)
      tasks.single().run()
      assertEquals(listOf(10L), failures)
      adapter.ownerStatus(2, ownerId, retirement)
      assertEquals("quiescent", retirements.getValue(2).state)
    } finally {
      allowSlotReservation.countDown()
      effect.join(5_000)
      fence.join(5_000)
      native.destroy()
    }
  }

  @Test fun v4FenceLinearizesCallbackCoordinatorAndSlotAdmissionWithoutDroppingPreFenceIngress() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val driver = FakeRadio()
    val coordinatorAdmitted = CountDownLatch(1)
    val allowSlotReservation = CountDownLatch(1)
    val coreEntered = CountDownLatch(1)
    val allowCore = CountDownLatch(1)
    val fenceStarted = CountDownLatch(1)
    val fenceReturned = CountDownLatch(1)
    val firstSlotLookup = AtomicBoolean(true)
    val port = object : AndroidRadioPort by driver, OrderedGroupPort by owner {
      override fun orderedSlots(): AndroidOrderedOpenSlots {
        if (firstSlotLookup.compareAndSet(true, false)) {
          coordinatorAdmitted.countDown()
          check(allowSlotReservation.await(5, TimeUnit.SECONDS)) { "slot reservation did not resume" }
        }
        return owner.orderedSlots()
      }
    }
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    var advertisements = 0
    val core = object : MobileCorePort by FakeCore() {
      override fun ingestAdvertisement(advertisement: AdvertisementFacts): Int {
        advertisements++
        coreEntered.countDown()
        check(allowCore.await(5, TimeUnit.SECONDS)) { "advertisement ingress did not resume" }
        return MobileCoreBridge.STATUS_ACCEPTED
      }
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val ownerId = adapter.orderedAdmissionOwner(4)
    val retirement = UUID.randomUUID().toString()
    val advertisement = AdvertisementFacts("peer", "peer", null, null, null, emptyList(), emptyList(), emptyList(), null, null)
    val callback = Thread { driver.events.onAdvertisement(advertisement) }
    val fence = Thread {
      fenceStarted.countDown()
      adapter.ownerFence(1, ownerId, retirement)
      fenceReturned.countDown()
    }
    callback.start()
    try {
      assertTrue(coordinatorAdmitted.await(5, TimeUnit.SECONDS))
      fence.start()
      assertTrue(fenceStarted.await(5, TimeUnit.SECONDS))
      assertTrue("fence must block behind the coordinator-to-slot admission transaction", awaitBlocked(fence))
      allowSlotReservation.countDown()
      assertTrue(coreEntered.await(5, TimeUnit.SECONDS))
      assertTrue(fenceReturned.await(5, TimeUnit.SECONDS))
      assertEquals("fenced-pending", retirements.getValue(1).state)
      allowCore.countDown()
      callback.join(5_000)
      assertFalse(callback.isAlive)
      adapter.ownerStatus(2, ownerId, retirement)
      assertEquals("quiescent", retirements.getValue(2).state)
      driver.events.onAdvertisement(advertisement)
      assertEquals(1, advertisements)
      assertEquals(1L, adapter.statusCounts()["ingest:advertisement:retired-owner"])
    } finally {
      allowSlotReservation.countDown()
      allowCore.countDown()
      callback.join(5_000)
      fence.join(5_000)
      native.destroy()
    }
  }

  @Test fun v4FenceRefusesDurablePresenceRestoreWithoutConsumingIt() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val restored = mutableListOf<List<PresenceRestoredPeer>>()
    val core = object : MobileCorePort by FakeCore() {
      override fun ingestRestored(peers: List<PresenceRestoredPeer>): Int {
        restored += peers
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val ownerId = adapter.orderedAdmissionOwner(4)
    adapter.ownerFence(1, ownerId, UUID.randomUUID().toString())

    assertFalse(adapter.ingestPresenceRestored(listOf(PresenceRestoredPeer("AA:BB:CC:DD:EE:FF", null, false))))
    assertTrue(restored.isEmpty())
    native.destroy()
  }

  @Test fun v4StatusWaitsForAnAlreadyAdmittedPresenceRestore() {
    val native = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(native)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val retirements = mutableMapOf<Long, MobileCoreBridge.OwnerRetirementResult>()
    val core = object : MobileCorePort by FakeCore() {
      override fun ingestRestored(peers: List<PresenceRestoredPeer>): Int {
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS)) { "restore did not release" }
        return MobileCoreBridge.STATUS_ACCEPTED
      }
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        retirements[requestId] = result
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val restore = Thread { adapter.ingestPresenceRestored(listOf(PresenceRestoredPeer("AA:BB:CC:DD:EE:FF", null, false))) }
    restore.start()
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    val ownerId = adapter.orderedAdmissionOwner(4)
    val retirement = UUID.randomUUID().toString()
    adapter.ownerFence(1, ownerId, retirement)
    assertEquals("fenced-pending", retirements.getValue(1).state)
    release.countDown()
    restore.join(5_000)
    assertFalse(restore.isAlive)
    adapter.ownerStatus(2, ownerId, retirement)
    assertEquals("quiescent", retirements.getValue(2).state)
    native.destroy()
  }
  @Test fun ownerSlotDebtIncludesQueuedClosureAndItsCompletionAfterAcknowledgement() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val tasks = mutableListOf<Runnable>()
    val retirement = UUID.randomUUID()
    var duringCompletion = -1
    val core = object : MobileCorePort by FakeCore() {
      override fun completeFailure(requestId: Long, failure: RadioFailure): Int {
        duringCompletion = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).adapterWork
        return MobileCoreBridge.STATUS_DROPPED_CONTROL
      }
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    val id = MobileCoreBridge.OrderedOpenIdentity(2, adapter.orderedAdmissionOwner(3), 0, 1, 10, 20, "peer",
      "connection", "database", "snapshot", 1, 2, longArrayOf(3), arrayOf("peer"), arrayOf("service"), longArrayOf(0),
      arrayOf("characteristic"), longArrayOf(0), 8, 32)
    adapter.orderedOpenV2(10, id)
    val slots = owner.orderedSlots()
    assertEquals(1, slots.fenceOwner(slots.incarnation, retirement).adapterWork)
    adapter.orderedAcknowledgeOpenV2(11, id)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).terminal)
    assertEquals(1, slots.ownerStatus(slots.incarnation, retirement).adapterWork)
    tasks.single().run()
    assertEquals(1, duringCompletion)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).adapterWork)
  }
  @Test fun v2FenceOvertakesQueuedOpenAndRetiredClosureCannotReviveSlot() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owned = OwnedOrderedGroupPort(radio)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owned {}
    val results = mutableMapOf<Long, MobileCoreBridge.OrderedResult>()
    val failures = mutableListOf<Long>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult): Int { results[requestId] = result; return 0 }
      override fun completeFailure(requestId: Long, failure: RadioFailure): Int { failures.add(requestId); return 0 }
    }
    val tasks = mutableListOf<Runnable>()
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { tasks.add(it) }, Executor { it.run() }) {}
    fun identity(generation: Long, original: Long, group: Long, service: String = "0000180d-0000-1000-8000-00805f9b34fb") =
      MobileCoreBridge.OrderedOpenIdentity(2, adapter.orderedAdmissionOwner(3), 0, generation, original, group, "peer",
        "connection", "database", "snapshot", 1, 2, longArrayOf(3), arrayOf("peer"), arrayOf(service), longArrayOf(0),
        arrayOf("00002a37-0000-1000-8000-00805f9b34fb"), longArrayOf(0), 8, 32)
    val submitted = identity(1, 10, 20)
    adapter.orderedOpenV2(10, submitted)
    submitted.members[0] = 999
    val first = identity(1, 10, 20)
    assertEquals(1, tasks.size)
    adapter.orderedAcknowledgeOpenV2(9, first)
    assertTrue(9L in failures)
    adapter.orderedRecoverOpenV2(11, identity(1, 10, 20, "different"))
    assertTrue(11L in failures)
    adapter.orderedRecoverOpenV2(12, first)
    assertEquals("failed", results.getValue(12).status)
    assertEquals("not-created", results.getValue(12).ownershipKind)
    adapter.orderedAcknowledgeOpenV2(13, first)
    assertEquals("open-retired", results.getValue(13).kind)
    val next = identity(2, 14, 21)
    adapter.orderedOpenV2(14, next)
    tasks.removeAt(0).run()
    adapter.orderedRecoverOpenV2(15, next)
    assertEquals("failed", results.getValue(15).status)
    assertEquals(2L, results.getValue(15).slotGeneration)
    adapter.orderedAcknowledgeOpenV2(16, first)
    assertEquals("open-retired", results.getValue(16).kind)
    adapter.orderedAcknowledgeOpenV2(17, next)
    assertEquals("open-retired", results.getValue(17).kind)
    tasks.removeAt(0).run()
    val rejecting = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null },
      Executor { throw java.util.concurrent.RejectedExecutionException("rejected") }, Executor { it.run() }) {}
    val rejected = identity(3, 18, 22)
    rejecting.orderedOpenV2(18, rejected)
    assertEquals("failed", results.getValue(18).status)
    assertEquals("not-created", results.getValue(18).ownershipKind)
    assertFalse(results.getValue(18).dispatched)
    rejecting.orderedRecoverOpenV2(19, rejected)
    assertEquals(results.getValue(18).detail, results.getValue(19).detail)
  }
  @Test fun v2ExecutingFenceIsNonblockingAndPartialFailureRetainsCreatedOwnership() {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    doReturn(peer).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(service).`when`(characteristic).service
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val graph = radio.withNativeGraph(peer) { Unit }.first
    val owned = OwnedOrderedGroupPort(radio)
    val entered = java.util.concurrent.CountDownLatch(1)
    val resume = java.util.concurrent.CountDownLatch(1)
    radio.onOrderedOpenHandoffForTest = { entered.countDown(); check(resume.await(5, java.util.concurrent.TimeUnit.SECONDS)); error("handoff failed") }
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owned {}
    val results = java.util.concurrent.ConcurrentHashMap<Long, MobileCoreBridge.OrderedResult>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult): Int {
        results[requestId] = result
        return if (requestId == 20L) MobileCoreBridge.STATUS_DROPPED_CONTROL else 0
      }
    }
    var thread: Thread? = null
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { task -> thread = kotlin.concurrent.thread { task.run() } }, Executor { it.run() }) {}
    val identity = MobileCoreBridge.OrderedOpenIdentity(2, adapter.orderedAdmissionOwner(3), 0, 1, 20, 30, peer,
      "connection", "database", graph.owner, graph.attachment, graph.revision.toLong(), longArrayOf(5), arrayOf(peer),
      arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32)
    adapter.orderedOpenV2(20, identity)
    assertTrue(entered.await(5, java.util.concurrent.TimeUnit.SECONDS))
    val retirement = UUID.randomUUID()
    try {
      val slotStatus = owned.orderedSlots().fenceOwner(owned.orderedSlots().incarnation, retirement)
      assertEquals(1, slotStatus.executing)
      assertEquals(1, slotStatus.adapterWork)
      adapter.orderedRecoverOpenV2(21, identity)
      assertEquals("pending", results.getValue(21).status)
    } finally { resume.countDown() }
    thread!!.join(5000)
    assertFalse(thread!!.isAlive)
    assertEquals(0, owned.orderedSlots().ownerStatus(owned.orderedSlots().incarnation, retirement).adapterWork)
    assertEquals("failed", results.getValue(20).status)
    assertEquals("created", results.getValue(20).ownershipKind)
    assertEquals("handoff failed", results.getValue(20).detail)
    assertEquals(5L, results.getValue(20).members.single().member)
    adapter.orderedRecoverOpenV2(22, identity)
    assertEquals("handoff failed", results.getValue(22).detail)
    adapter.orderedAcknowledgeOpenV2(23, identity)
    assertEquals("open-retired", results.getValue(23).kind)
  }
  @Test fun adaptersObserveTheSameAdmissionOwnerAndReplacementIsFresh() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val sharedOwner = OwnedOrderedGroupPort(radio)
    fun adapter(owner: OrderedGroupPort): RustRadioHostAdapter {
      val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
      val direct = Executor { it.run() }
      return RustRadioHostAdapter(FakeCore(), port, FakeBackground(), { null }, { null }, direct, direct) {}
    }
    val first = adapter(sharedOwner)
    val second = adapter(sharedOwner)
    val replacement = adapter(OwnedOrderedGroupPort(radio))
    assertEquals("", first.orderedAdmissionOwner(1))
    assertEquals("", first.orderedAdmissionOwner(2))
    assertEquals(first.orderedAdmissionOwner(3), second.orderedAdmissionOwner(3))
    assertNotEquals("", first.orderedAdmissionOwner(3))
    assertNotEquals(first.orderedAdmissionOwner(3), replacement.orderedAdmissionOwner(3))
  }
  @Test fun requestsDriveRealSourceAndRetainBatchesAcrossCompletion() {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    doReturn(peer).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(service).`when`(characteristic).service
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val graph = radio.withNativeGraph(peer) { Unit }.first
    val fakeRadio = FakeRadio()
    val physicalCallbacks = mutableListOf<Pair<Boolean, (Result<Unit>) -> Unit>>()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by OwnedOrderedGroupPort(radio), OrderedPhysicalPort {
      override fun setOrderedNotification(instance: CharacteristicInstance, enable: Boolean, mode: String?, epoch: Long, admission: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedAdmission?,
                                          onResult: (Result<Unit>) -> Unit): Long {
        physicalCallbacks.add(enable to onResult)
        return 900L
      }
    }
    val completed = mutableListOf<MobileCoreBridge.OrderedResult>()
    val wakes = mutableListOf<Pair<Long, Long>>()
    val released = mutableListOf<Pair<Long, RadioFailure>>()
    val failureWakes = mutableListOf<Pair<Long, Long>>()
    var rejectedGroup: Long? = null
    var completionStatus = 0
    val baseCore = FakeCore()
    val core = object : MobileCorePort by baseCore {
      override fun completeOrdered(requestId: Long, result: MobileCoreBridge.OrderedResult): Int { completed.add(result); return completionStatus }
      override fun completeOrderedEnableReleased(requestId: Long, failure: RadioFailure): Int { released.add(requestId to failure); return 0 }
      override fun ingestOrderedWake(generation: Long, group: Long): Int {
        wakes.add(generation to group)
        return if (group == rejectedGroup) MobileCoreBridge.STATUS_DROPPED_CONTROL else 0
      }
      override fun ingestOrderedWakeFailure(generation: Long, group: Long): Int { failureWakes.add(generation to group); return 0 }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val instance = CharacteristicInstance(peer, serviceId.toString(), 0, charId.toString(), 0)
    fakeRadio.characteristics[instance] = CharacteristicFacts(0x10, true)
    adapter.connect(100, peer, false, emptyArray(), "connection")
    fakeRadio.events.onConnection(peer, true, 0, fakeRadio.attempts[peer])
    adapter.orderedGraphAuthority(291, "reserve", 888L)
    assertEquals("graph-authority-reserved", completed.last().kind)
    val authorityReceipt = completed.last().authorityId
    adapter.orderedGraphAuthority(292, "status", authorityReceipt)
    assertEquals("pending", completed.last().status)
    adapter.orderedGraphAuthority(293, "open", authorityReceipt)
    assertEquals("graph-authority-opened", completed.last().kind)
    val graphAuthority = completed.last().authorityId.toULong()
    adapter.orderedGraphAuthority(294, "status", authorityReceipt)
    assertEquals("opened", completed.last().status)
    fun reservation(id: Long, operation: String, localRequest: Long) = adapter.orderedGraphPermitReservation(
      id, operation, graphAuthority.toLong(), localRequest, peer, "connection", graph.owner, graph.attachment, graph.revision.toLong())
    reservation(301, "status", 1)
    assertEquals("pending", completed.last().status)
    reservation(302, "reserve", 1)
    assertEquals("graph-permit-reserved", completed.last().kind)
    assertEquals(1L, completed.last().authorityId)
    reservation(303, "fence", 1)
    assertEquals("issued", completed.last().status)
    assertEquals(1L, completed.last().authorityId)
    val permitKey = OwnedAndroidGattRadio.GraphPermitKey(graphAuthority, peer, "connection", graph, 1uL)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.FORGOTTEN, port.orderedGraphPermit("forget", permitKey) { true })
    reservation(304, "forget", 1)
    reservation(305, "fence", 2)
    assertEquals("not-created", completed.last().status)
    reservation(306, "reserve", 2)
    assertEquals("not-created", completed.last().status)
    reservation(307, "forget", 2)
    adapter.orderedGraphAuthority(308, "close", graphAuthority.toLong())
    adapter.orderedGraphAuthority(309, "status", authorityReceipt)
    assertEquals("closed", completed.last().status)
    adapter.orderedGraphAuthority(310, "forget", graphAuthority.toLong())
    adapter.orderedGraphAuthority(311, "reserve-fence", 888L)
    assertEquals("issued", completed.last().status)
    adapter.orderedGraphAuthority(312, "forget-open", authorityReceipt)
    adapter.orderedGraphAuthority(313, "reserve-forget", 888L)
    completed.clear()
    // Recovery may run ahead of an accepted open, even after waiter cancellation.
    val reorderedTasks = mutableListOf<Runnable>()
    val originalEvents = fakeRadio.events
    val reordered = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, Executor { reorderedTasks.add(it) }, direct) {}
    reordered.orderedOpen(700, 7000, longArrayOf(1), arrayOf(peer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    reordered.cancel(700)
    reordered.orderedRecoverOpen(701, 7000, 700, peer)
    reorderedTasks.removeAt(1).run()
    assertEquals("not-created", completed.last().ownershipKind)
    assertEquals(OrderedPortOpenRecovery.NotCreated, port.orderedRecoverOpenExact(7000uL, 700uL, peer))
    port.orderedAcknowledgeOpen(7000uL, 700uL, peer)
    port.orderedAcknowledgeOpen(7000uL, 700uL, peer)
    reorderedTasks.removeAt(0).run()
    assertEquals("not-created", completed.last().ownershipKind)
    val rejecting = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null },
      Executor { throw java.util.concurrent.RejectedExecutionException("test") }, direct) {}
    rejecting.orderedOpen(702, 7001, longArrayOf(1), arrayOf(peer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    assertEquals("not-created", completed.last().ownershipKind)
    assertFalse(completed.last().dispatched)
    assertTrue(port.orderedRecoverOpenExact(7001uL, 702uL, peer) is OrderedPortOpenRecovery.Failed)
    port.orderedAcknowledgeOpen(7001uL, 702uL, peer)
    adapter.orderedOpen(703, 7002, longArrayOf(1), arrayOf(peer), emptyArray(), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    assertEquals("not-created", completed.last().ownershipKind)
    val validationFailure = completed.last()
    adapter.orderedRecoverOpen(704, 7002, 703, peer)
    assertEquals(validationFailure.kind, completed.last().kind)
    assertEquals(validationFailure.detail, completed.last().detail)
    assertEquals("not-created", completed.last().ownershipKind)
    adapter.orderedAcknowledgeOpen(705, 2, 7002, 703, peer)
    assertFalse(baseCore.calls.contains("unit:705"))
    adapter.orderedAcknowledgeOpen(706, 1, 7002, 999, peer)
    assertFalse(baseCore.calls.contains("unit:706"))
    adapter.orderedAcknowledgeOpen(707, 1, 7002, 703, peer)
    adapter.orderedAcknowledgeOpen(708, 1, 7002, 703, peer)
    assertTrue(baseCore.calls.contains("unit:707"))
    assertTrue(baseCore.calls.contains("unit:708"))
    port.orderedAcknowledgeOpen(7002uL, 703uL, peer)
    val absentPeer = "AA:BB:CC:DD:EE:00"
    adapter.orderedOpen(716, 7004, longArrayOf(1), arrayOf(absentPeer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    val absentFailure = completed.last()
    assertEquals("open-failed", absentFailure.kind)
    assertEquals("not-created", absentFailure.ownershipKind)
    assertEquals("connection is not current", absentFailure.detail)
    assertFalse(absentFailure.dispatched)
    adapter.orderedRecoverOpen(717, 7004, 716, absentPeer)
    assertEquals(absentFailure.detail, completed.last().detail)
    assertEquals(absentFailure.dispatched, completed.last().dispatched)
    adapter.orderedAcknowledgeOpen(718, 1, 7004, 716, absentPeer)
    radio.onOrderedOpenHandoffForTest = { error("partial handoff failure") }
    adapter.orderedOpen(709, 7003, longArrayOf(1), arrayOf(peer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    radio.onOrderedOpenHandoffForTest = null
    val partialFailure = completed.last()
    assertEquals("open-failed", partialFailure.kind)
    assertEquals("created", partialFailure.ownershipKind)
    assertEquals("partial handoff failure", partialFailure.detail)
    adapter.orderedRecoverOpen(710, 7003, 709, peer)
    val replayedFailure = completed.last()
    assertEquals("open-failed", replayedFailure.kind)
    assertEquals(partialFailure.detail, replayedFailure.detail)
    assertEquals(partialFailure.failureKind, replayedFailure.failureKind)
    assertEquals(partialFailure.ownershipKind, replayedFailure.ownershipKind)
    assertEquals(partialFailure.generationValue, replayedFailure.generationValue)
    assertEquals(partialFailure.members.single().member, replayedFailure.members.single().member)
    adapter.orderedAcknowledgeOpen(711, 1, 7003, 709, peer)
    assertTrue(baseCore.calls.contains("unit:711"))
    val partialGeneration = partialFailure.generationValue.toULong()
    port.orderedClose(partialGeneration, 7003uL)
    val partialBatch = port.orderedDrain(partialGeneration, 7003uL)
    port.orderedAcknowledge(partialGeneration, 7003uL, requireNotNull(partialBatch.id))
    port.orderedRemove(partialGeneration, 7003uL)
    port.orderedForgetRemovalReceipt(partialGeneration, 7003uL)
    wakes.clear()
    fakeRadio.setEvents(originalEvents)
    completed.clear()
    adapter.orderedOpen(1, -1, longArrayOf(-1), arrayOf(peer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    val opened = completed.single()
    assertEquals("opened", opened.kind)
    assertEquals(-1L, opened.members.single().member)
    adapter.orderedRecoverOpen(712, -1, 1, peer)
    assertEquals("opened", completed.last().kind)
    assertEquals(opened.generationValue, completed.last().generationValue)
    adapter.orderedRecoverOpen(713, -1, 2, peer)
    assertTrue(baseCore.failures.containsKey(713))
    adapter.orderedRecoverOpen(714, -1, 1, "wrong-peer")
    assertTrue(baseCore.failures.containsKey(714))
    adapter.orderedAcknowledgeOpen(715, 1, -1, 1, peer)
    assertTrue(baseCore.calls.contains("unit:715"))
    val registration = opened.members.single()
    adapter.orderedRequest(101, "retain-claim", "counter", opened.generationValue, -1, registration.member, 0, 0, 0)
    // Exact membership is required by the retain operation.
    adapter.orderedRequest(102, "retain-claim", "counter", opened.generationValue, -1, registration.member, registration.sourceMember, registration.epoch, 0)
    adapter.orderedEnable(103, "enable", peer, serviceId.toString(), 0, charId.toString(), 0, "connection", "counter", opened.generationValue,
      -1, registration.member, registration.sourceMember, registration.epoch, 7, "notification", null)
    adapter.cancel(103)
    assertTrue(fakeRadio.cancelled.isEmpty())
    assertTrue(physicalCallbacks.single().first)
    physicalCallbacks.removeAt(0).second(Result.success(Unit))
    assertTrue(baseCore.calls.contains("notify:103:notification"))
    adapter.orderedEnable(104, "disable", peer, serviceId.toString(), 0, charId.toString(), 0, "connection", "counter", opened.generationValue,
      -1, registration.member, registration.sourceMember, registration.epoch, 7, null, null)
    assertFalse(physicalCallbacks.single().first)
    physicalCallbacks.removeAt(0).second(Result.success(Unit))
    adapter.orderedEnable(105, "settlement", peer, serviceId.toString(), 0, charId.toString(), 0, "connection", "counter", opened.generationValue,
      -1, registration.member, registration.sourceMember, registration.epoch, 7, null, null)
    assertEquals("released", completed.last().status)
    adapter.orderedRequest(106, "confirm-cleanup", "counter", opened.generationValue, -1, registration.member, registration.sourceMember, registration.epoch, 0)
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(5))
    assertEquals(opened.generationValue.toULong().toLong() to -1L, wakes.single())
    adapter.orderedRequest(2, "drain", "counter", opened.generationValue, -1, 0, 0, 0, 0)
    val batch = completed.last()
    assertTrue(batch.hasBatch)
    assertEquals("value", batch.records.single().kind)
    assertEquals(-1L, batch.records.single().member.member)
    adapter.orderedRequest(3, "drain", "counter", opened.generationValue, -1, 0, 0, 0, 0)
    assertEquals(batch.batch, completed.last().batch)
    adapter.orderedRequest(4, "acknowledge-batch", "counter", opened.generationValue, -1, 0, 0, 0, batch.batch)
    adapter.orderedRequest(5, "close", "counter", opened.generationValue, -1, 0, 0, 0, 0)
    adapter.orderedRequest(6, "drain", "counter", opened.generationValue, -1, 0, 0, 0, 0)
    assertEquals("closed", completed.last().records.single().reason)
    val terminalBatch = completed.last().batch
    adapter.orderedRequest(7, "acknowledge-batch", "counter", opened.generationValue, -1, 0, 0, 0, terminalBatch)
    adapter.orderedRequest(8, "remove", "counter", opened.generationValue, -1, 0, 0, 0, 0)
    completionStatus = MobileCoreBridge.STATUS_LATE
    adapter.orderedOpen(9, -2, longArrayOf(-1), arrayOf(peer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    val late = completed.last()
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(7))
    completionStatus = MobileCoreBridge.STATUS_DELIVERED
    adapter.orderedRequest(10, "drain", "counter", late.generationValue, -2, 0, 0, 0, 0)
    assertArrayEquals(byteArrayOf(7), completed.last().records.single().value)
    adapter.orderedOpen(11, 77, longArrayOf(1), arrayOf(peer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    val rejected = completed.last()
    val otherMember = rejected.members.single()
    adapter.orderedRequest(110, "retain-claim", "counter", rejected.generationValue, 77, otherMember.member, otherMember.sourceMember, otherMember.epoch, 0)
    adapter.orderedEnable(111, "enable", peer, serviceId.toString(), 0, charId.toString(), 0, "connection", "counter", rejected.generationValue,
      77, otherMember.member, otherMember.sourceMember, otherMember.epoch, 8, "notification", null)
    val lateCallback = physicalCallbacks.removeAt(0).second
    fakeRadio.events.onOrderedLinkFence(peer, fakeRadio.attempts[peer])
    lateCallback(Result.failure(IllegalStateException("late callback")))
    adapter.orderedEnable(112, "settlement", peer, serviceId.toString(), 0, charId.toString(), 0, "connection", "counter", rejected.generationValue,
      77, otherMember.member, otherMember.sourceMember, otherMember.epoch, 8, null, null)
    assertEquals("released", completed.last().status)
    adapter.orderedOpen(12, 88, longArrayOf(1), arrayOf(peer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    rejectedGroup = 77
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(8))
    assertEquals(listOf(rejected.generationValue.toULong().toLong() to 77L), failureWakes)
    adapter.orderedRequest(13, "drain", "counter", rejected.generationValue, 77, 0, 0, 0, 0)
    assertEquals("wake-failed", completed.last().records.last().reason)

    val queued = QueuedExecutor()
    var reject = false
    val switchable = Executor { command ->
      if (reject) throw java.util.concurrent.RejectedExecutionException("test")
      queued.execute(command)
    }
    val cancellable = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, switchable, direct) {}
    cancellable.connect(140, peer, false, emptyArray(), "connection")
    queued.runAll()
    fakeRadio.events.onConnection(peer, true, 0, fakeRadio.attempts[peer])
    cancellable.orderedOpen(14, 99, longArrayOf(1), arrayOf(peer), arrayOf(serviceId.toString()), longArrayOf(0), arrayOf(charId.toString()), longArrayOf(0), 8, 32, "connection", "database", graph.owner, graph.attachment, graph.revision.toLong())
    queued.runAll()
    val barrierGroup = completed.last()
    val queuedMember = barrierGroup.members.single()
    cancellable.orderedMarkerWrite(201, "write", "counter", barrierGroup.generationValue, 99, 0, "connection", graph.owner,
      graph.attachment, graph.revision.toLong(), peer, serviceId.toString(), 0, charId.toString(), 0, byteArrayOf(1), true)
    cancellable.orderedRequest(206, "cancel-marker-write", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 0)
    assertEquals("not-admitted", completed.last().admission)
    try { port.orderedForgetMarkerWriteReceipt(barrierGroup.generationValue.toULong(), 99uL, 0uL); fail("pending adapter cell forgotten") }
    catch (_: IllegalStateException) { }
    queued.runAll()
    assertEquals("marker-write-status", completed.last().kind)
    assertEquals("not-admitted", completed.last().admission)
    assertEquals("cancelled", completed.last().failureKind)
    cancellable.orderedRequest(202, "marker-write-settlement", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 0)
    queued.runAll()
    assertEquals("not-admitted", completed.last().admission)
    cancellable.orderedRequest(203, "forget-marker-write-receipt", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 0)
    queued.runAll()
    reject = true
    cancellable.orderedMarkerWrite(204, "write", "counter", barrierGroup.generationValue, 99, 1, "connection", graph.owner,
      graph.attachment, graph.revision.toLong(), peer, serviceId.toString(), 0, charId.toString(), 0, byteArrayOf(1), true)
    reject = false
    assertEquals("not-admitted", completed.last().admission)
    assertEquals("platform", completed.last().failureKind)
    cancellable.orderedRequest(205, "forget-marker-write-receipt", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 1)
    queued.runAll()
    cancellable.orderedRequest(141, "retain-claim", "counter", barrierGroup.generationValue, 99, queuedMember.member, queuedMember.sourceMember, queuedMember.epoch, 0)
    queued.runAll()
    cancellable.orderedEnable(142, "enable", peer, serviceId.toString(), 0, charId.toString(), 0, "connection", "counter", barrierGroup.generationValue,
      99, queuedMember.member, queuedMember.sourceMember, queuedMember.epoch, 9, "notification", null)
    cancellable.cancel(142)
    queued.runAll()
    assertEquals(142L, released.last().first)
    assertEquals(RadioFailureKind.CANCELLED, released.last().second.kind)
    cancellable.orderedEnable(143, "enable", peer, serviceId.toString(), 0, charId.toString(), 0, "connection", "counter", barrierGroup.generationValue,
      99, queuedMember.member, queuedMember.sourceMember, queuedMember.epoch, 10, "indication", null)
    queued.runAll()
    assertEquals(143L, released.last().first)
    assertEquals(RadioFailureKind.UNSUPPORTED, released.last().second.kind)
    reject = true
    cancellable.orderedEnable(144, "enable", peer, serviceId.toString(), 0, charId.toString(), 0, "connection", "counter", barrierGroup.generationValue,
      99, queuedMember.member, queuedMember.sourceMember, queuedMember.epoch, 11, "notification", null)
    reject = false
    assertEquals(144L, released.last().first)
    assertEquals(RadioFailureKind.PLATFORM, released.last().second.kind)
    cancellable.orderedRequest(15, "barrier", "counter", barrierGroup.generationValue, 99, 0, 0, 0, -1)
    cancellable.cancel(15)
    queued.runAll()
    cancellable.orderedRequest(16, "drain", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 0)
    queued.runAll()
    assertTrue(completed.last().records.isEmpty())
    cancellable.orderedRequest(17, "barrier", "counter", barrierGroup.generationValue, 99, 0, 0, 0, -1)
    queued.runAll()
    cancellable.orderedRequest(18, "drain", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 0)
    queued.runAll()
    assertEquals("barrier", completed.last().records.single().kind)
    assertEquals(-1L, completed.last().records.single().barrierId)
    val barrierBatch = completed.last().batch
    cancellable.orderedRequest(19, "acknowledge-batch", "counter", barrierGroup.generationValue, 99, 0, 0, 0, barrierBatch)
    queued.runAll()
    cancellable.orderedRequest(20, "close", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 0)
    queued.runAll()
    cancellable.orderedRequest(21, "seal", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 0)
    cancellable.cancel(21)
    queued.runAll()
    cancellable.orderedRequest(22, "seal", "counter", barrierGroup.generationValue, 99, 0, 0, 0, 0)
    queued.runAll()
    assertTrue(baseCore.calls.contains("unit:22"))
  }

  private fun awaitBlocked(thread: Thread): Boolean {
    val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
    while (System.nanoTime() < deadline) {
      if (thread.state == Thread.State.BLOCKED) return true
      Thread.sleep(1)
    }
    return thread.state == Thread.State.BLOCKED
  }
}
