// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OrderedNotificationRadioTest.kt
package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothProfile
import android.content.Context
import org.junit.Assert.*
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.doAnswer
import org.mockito.Mockito.mock
import org.mockito.Mockito.verify
import org.mockito.Mockito.times
import org.mockito.Mockito.never
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlin.concurrent.thread

class OrderedNotificationRadioTest {
  @Test fun saturatedControlQueueStillDeliversDisconnectAndClosesGatt() {
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
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val graph = radio.withNativeGraph(peer) { Unit }.first
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    val owner = AndroidOrderedGroupOwner(radio)
    val opened = owner.open(1uL, peer, mapOf(1uL to path), 4, 8, expectedGraph = graph) { _, _ -> }
    val member = owner.retainClaim(opened.generation, 1uL, opened.members.single())
    val admission = owner.admission(opened.generation, 1uL, member, peer, path)
    var disconnected = false
    var controlsLost = 0L
    radio.onConnectionState = { _, connected, _ -> if (!connected) disconnected = true }
    radio.onAdmissionOverflow = { _, _, controls -> controlsLost += controls }
    doAnswer {
      repeat(70) { radio.nativeGattCallback().onMtuChanged(gatt, 23, BluetoothGatt.GATT_SUCCESS) }
      radio.nativeGattCallback().onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_DISCONNECTED)
      true
    }.`when`(gatt).setCharacteristicNotification(characteristic, true)
    radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, true, "notification", 7, admission) { }
    assertTrue(disconnected)
    assertEquals(6L, controlsLost)
    verify(gatt).close()
    val records = owner.drain(opened.generation, 1uL).events
    assertEquals(OrderedNotificationSource.Reason.CONNECTION_LOST, (records.last() as OrderedNotificationSource.Event.Terminal).reason)
  }
  @Test fun droppedWakeThenLifecycleRecoversDetachedSourceBeforeSeal() {
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
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    radio.admissionWakeCapacityForTest = 0
    val graph = radio.withNativeGraph(peer) { Unit }.first
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    val owner = AndroidOrderedGroupOwner(radio)
    var wakes = 0
    val opened = owner.open(1uL, peer, mapOf(1uL to path), 4, 8, expectedGraph = graph) { _, _ -> wakes++ }
    val member = owner.retainClaim(opened.generation, 1uL, opened.members.single())
    val admission = owner.admission(opened.generation, 1uL, member, peer, path)
    doAnswer {
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(4))
      radio.nativeGattCallback().onServiceChanged(gatt)
      true
    }.`when`(gatt).setCharacteristicNotification(characteristic, true)
    radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, true, "notification", 7, admission) { }
    assertTrue(wakes > 0)
    val records = owner.drain(opened.generation, 1uL)
    assertEquals(OrderedNotificationSource.Reason.DATABASE_CHANGED, (records.events.last() as OrderedNotificationSource.Event.Terminal).reason)
    owner.acknowledge(opened.generation, 1uL, requireNotNull(records.id))
    owner.seal(opened.generation, 1uL)
  }
  @Test fun synchronousServiceCleanupCallbackRunsAfterBothLocksRelease() {
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
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val results = mutableListOf<Result<Unit>>()
    radio.setNotifyExact(peer, serviceId, 0, charId, 0, true, "notification", 7) { results.add(it) }
    assertTrue(results.single().isSuccess)
    val acquired = CountDownLatch(1)
    radio.onConnectionState = { _, connected, _ ->
      if (!connected) {
        val probe = thread { radio.probeAdmissionLocksForTest(); acquired.countDown() }
        assertTrue(acquired.await(5, TimeUnit.SECONDS))
        probe.join(5000)
      }
    }
    doAnswer {
      radio.nativeGattCallback().onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_DISCONNECTED)
      true
    }.`when`(gatt).setCharacteristicNotification(characteristic, false)
    radio.nativeGattCallback().onServiceChanged(gatt)
    assertEquals(0L, acquired.count)
  }
  @Test fun synchronousRollbackCallbackRunsAfterBothLocksRelease() {
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
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val graph = radio.withNativeGraph(peer) { Unit }.first
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    val owner = AndroidOrderedGroupOwner(radio)
    val acquired = CountDownLatch(1)
    val opened = owner.open(1uL, peer, mapOf(1uL to path), 4, 8, expectedGraph = graph) { _, _ ->
      val probe = thread { radio.probeAdmissionLocksForTest(); acquired.countDown() }
      assertTrue(acquired.await(5, TimeUnit.SECONDS))
      probe.join(5000)
    }
    val member = owner.retainClaim(opened.generation, 1uL, opened.members.single())
    val admission = owner.admission(opened.generation, 1uL, member, peer, path)
    doAnswer { throw IllegalStateException("enable failed") }.`when`(gatt).setCharacteristicNotification(characteristic, true)
    doAnswer {
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(1))
      true
    }.`when`(gatt).setCharacteristicNotification(characteristic, false)
    val results = mutableListOf<Result<Unit>>()
    radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, true, "notification", 7, admission) { results.add(it) }
    assertTrue(results.single().isFailure)
    assertEquals(0L, acquired.count)
  }
  @Test fun reentrantLifecycleControlFencesLaterValueAtAdmission() {
    for (disconnect in listOf(false, true)) {
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
      doReturn(0x10).`when`(characteristic).properties
      doReturn(service).`when`(characteristic).service
      doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
      val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
      radio.attachConnectedGatt(peer, gatt, listOf(service))
      val graph = radio.withNativeGraph(peer) { Unit }.first
      val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
      val owner = AndroidOrderedGroupOwner(radio)
      val opened = owner.open(1uL, peer, mapOf(1uL to path), 4, 8, expectedGraph = graph) { _, _ -> }
      val member = owner.retainClaim(opened.generation, 1uL, opened.members.single())
      val admission = owner.admission(opened.generation, 1uL, member, peer, path)
      doAnswer {
        if (disconnect) radio.nativeGattCallback().onConnectionStateChange(gatt, 0, BluetoothProfile.STATE_DISCONNECTED)
        else radio.nativeGattCallback().onServiceChanged(gatt)
        radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(9))
        true
      }.`when`(gatt).setCharacteristicNotification(characteristic, true)
      radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, true, "notification", 7, admission) { }
      val records = owner.drain(opened.generation, 1uL).events
      assertTrue(records.none { it is OrderedNotificationSource.Event.Value })
      val terminal = records.single() as OrderedNotificationSource.Event.Terminal
      assertEquals(if (disconnect) OrderedNotificationSource.Reason.CONNECTION_LOST else OrderedNotificationSource.Reason.DATABASE_CHANGED, terminal.reason)
    }
  }
  @Test fun synchronousCallbackOverflowTerminatesSourceAndReportsLoss() {
    val deviceId = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    doReturn(deviceId).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val graph = radio.withNativeGraph(deviceId) { Unit }.first
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    val owner = AndroidOrderedGroupOwner(radio)
    val opened = owner.open(1uL, deviceId, mapOf(1uL to path), 100, 200, expectedGraph = graph) { _, _ -> }
    val member = owner.retainClaim(opened.generation, 1uL, opened.members.single())
    val admission = owner.admission(opened.generation, 1uL, member, deviceId, path)
    var dropped = 0L
    radio.onAdmissionOverflow = { _, notifications, controls -> dropped += notifications + controls }
    doAnswer {
      repeat(70) { radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(1)) }
      true
    }.`when`(gatt).setCharacteristicNotification(characteristic, true)
    val results = mutableListOf<Result<Unit>>()
    radio.setNotifyExactRetained(deviceId, serviceId, 0, charId, 0, true, "notification", 7, admission) { results.add(it) }
    assertTrue(results.single().isFailure)
    assertEquals(6L, dropped)
    val events = owner.drain(opened.generation, 1uL).events
    assertEquals(OrderedNotificationSource.Reason.OVERFLOW, (events.last() as OrderedNotificationSource.Event.Terminal).reason)
  }
  @Test fun synchronousGattCallbacksAreAdmittedButDeliveredAfterLocksRelease() {
    val deviceId = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val cccd = mock(BluetoothGattDescriptor::class.java)
    doReturn(deviceId).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    doReturn(cccd).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(OwnedAndroidGattRadio.CCCD_UUID).`when`(cccd).uuid
    doReturn(characteristic).`when`(cccd).characteristic
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val graph = radio.withNativeGraph(deviceId) { Unit }.first
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    val owner = AndroidOrderedGroupOwner(radio)
    var woke = false
    lateinit var source: OrderedNotificationSource<BluetoothGattCharacteristic>
    val opened = owner.open(1uL, deviceId, mapOf(1uL to path), 4, 8, expectedGraph = graph) { _, _ ->
      assertFalse(radio.admissionLocksHeldByCurrentThread())
      assertFalse(source.lockHeldByCurrentThread())
      woke = true
    }
    val member = owner.retainClaim(opened.generation, 1uL, opened.members.single())
    val admission = owner.admission(opened.generation, 1uL, member, deviceId, path)
    source = admission.source
    val results = mutableListOf<Result<Unit>>()
    val ordinary = mutableListOf<ByteArray>()
    radio.onProtocolNotification = { _, _, _, bytes ->
      assertFalse(radio.admissionLocksHeldByCurrentThread())
      ordinary.add(bytes)
    }
    lateinit var concurrent: Thread
    val concurrentStarted = CountDownLatch(1)
    doAnswer {
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(9))
      concurrent = thread {
        concurrentStarted.countDown()
        radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(11))
      }
      check(concurrentStarted.await(5, TimeUnit.SECONDS))
      assertFalse(woke)
      true
    }.`when`(gatt).setCharacteristicNotification(characteristic, true)
    doAnswer {
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(10))
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      assertTrue(results.isEmpty())
      true
    }.`when`(gatt).writeDescriptor(cccd)
    doAnswer {
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(10))
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      assertTrue(results.isEmpty())
      BluetoothGatt.GATT_SUCCESS
    }.`when`(gatt).writeDescriptor(eq(cccd), any(ByteArray::class.java))
    radio.setNotifyExactRetained(deviceId, serviceId, 0, charId, 0, true, "notification", 7, admission) {
      assertFalse(radio.admissionLocksHeldByCurrentThread())
      results.add(it)
    }
    concurrent.join(5000)
    assertFalse(concurrent.isAlive)
    assertTrue(woke)
    assertTrue(results.single().isSuccess)
    assertEquals(3, ordinary.size)
    val values = owner.drain(opened.generation, 1uL).events.map { it as OrderedNotificationSource.Event.Value }
    assertEquals(listOf(0uL, 1uL, 2uL), values.map { it.ordinal })
    assertArrayEquals(byteArrayOf(9), values[0].bytes)
    assertEquals(setOf(10, 11), values.drop(1).map { it.bytes.single().toInt() }.toSet())
    assertEquals(values.map { it.bytes.single() }, ordinary.map { it.single() })
  }
  @Test fun missingGattBeforeOrderedEnableIsAuthoritativelyNotApplied() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val results = mutableListOf<Result<Unit>>()
    radio.setNotifyExactRetained("AA:BB:CC:DD:EE:FF", UUID.randomUUID(), 0, UUID.randomUUID(), 0,
      true, "notification", 7, null) { results.add(it) }
    assertTrue(results.single().exceptionOrNull() is OrderedEnableNotApplied)
  }
  @Test fun staleGraphCannotDispatchQueuedOrderedEnable() {
    val deviceId = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    doReturn(deviceId).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val graph = radio.withNativeGraph(deviceId) { Unit }.first
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    val owner = AndroidOrderedGroupOwner(radio)
    val opened = owner.open(1uL, deviceId, mapOf(1uL to path), 4, 8, expectedGraph = graph) { _, _ -> }
    val member = owner.retainClaim(opened.generation, 1uL, opened.members.single())
    val admission = owner.admission(opened.generation, 1uL, member, deviceId, path)
    radio.nativeGattCallback().onServiceChanged(gatt)
    radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val results = mutableListOf<Result<Unit>>()
    radio.setNotifyExactRetained(deviceId, serviceId, 0, charId, 0, true, "notification", 7, admission) { results.add(it) }
    assertTrue(results.single().exceptionOrNull() is OrderedEnableNotApplied)
    verify(gatt, never()).setCharacteristicNotification(characteristic, true)
  }
  @Test fun failedOrderedEnableRetainsOwnerUntilConfirmedDisable() {
    val deviceId = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val cccd = mock(BluetoothGattDescriptor::class.java)
    doReturn(deviceId).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    doReturn(cccd).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(OwnedAndroidGattRadio.CCCD_UUID).`when`(cccd).uuid
    doReturn(characteristic).`when`(cccd).characteristic
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
    doReturn(true).`when`(gatt).writeDescriptor(cccd)
    doReturn(BluetoothGatt.GATT_SUCCESS).`when`(gatt).writeDescriptor(eq(cccd), any(ByteArray::class.java))
    val deferred = mutableListOf<() -> Unit>()
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, action -> deferred.add(action); true })
    radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val enabled = mutableListOf<Result<Unit>>()
    radio.setNotifyExactRetained(deviceId, serviceId, 0, charId, 0, true, "notification", 7L, null) { enabled.add(it) }
    radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_FAILURE)
    deferred.toList().forEach { it() }
    assertTrue(enabled.single().isFailure)
    val disabled = mutableListOf<Result<Unit>>()
    radio.setNotifyExactRetained(deviceId, serviceId, 0, charId, 0, false, null, 7L, null) { disabled.add(it) }
    assertTrue(disabled.isEmpty())
    radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
    assertTrue(disabled.single().isSuccess)
  }
  @Test fun preDispatchLocalRollbackCanBeRetriedForExactAttachment() {
    val deviceId = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    doReturn(deviceId).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    doReturn(listOf(service)).`when`(gatt).services
    doAnswer { throw IllegalStateException("local enable failed") }.`when`(gatt).setCharacteristicNotification(characteristic, true)
    var rollbacks = 0
    doAnswer { ++rollbacks > 1 }.`when`(gatt).setCharacteristicNotification(characteristic, false)
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val attachment = radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val results = mutableListOf<Result<Unit>>()
    radio.setNotifyExact(deviceId, serviceId, 0, charId, 0, true, "notification", 7L) { results.add(it) }
    assertTrue(results.single().isFailure)
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    assertEquals(OrderedLocalCleanupStatus.UNKNOWN, radio.retryOrderedLocalCleanup(deviceId, path, attachment + 1, 7))
    assertEquals(OrderedLocalCleanupStatus.UNKNOWN, radio.retryOrderedLocalCleanup(deviceId, path, attachment, 8))
    assertEquals(OrderedLocalCleanupStatus.RELEASED, radio.retryOrderedLocalCleanup(deviceId, path, attachment, 7))
    rollbacks = 0
    radio.setNotifyExact(deviceId, serviceId, 0, charId, 0, true, "notification", 8L) { }
    val replacementAttachment = radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    assertEquals(OrderedLocalCleanupStatus.UNKNOWN, radio.retryOrderedLocalCleanup(deviceId, path, replacementAttachment, 8))
  }
  @Test fun oldGraphTokenCannotOpenAfterServiceChangeAndRediscovery() {
    val deviceId = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    doReturn(deviceId).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(service).`when`(characteristic).service
    doReturn(listOf(service)).`when`(gatt).services
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val old = radio.withNativeGraph(deviceId) { it.size }.first
    doReturn(true).`when`(gatt).discoverServices()
    radio.discover(deviceId) {}
    radio.nativeGattCallback().onServiceChanged(gatt)
    val blocked = mutableListOf<Result<Unit>>()
    radio.discover(deviceId) { blocked.add(it) }
    assertTrue(blocked.single().isFailure)
    verify(gatt, times(1)).discoverServices()
    radio.nativeGattCallback().onServicesDiscovered(gatt, BluetoothGatt.GATT_SUCCESS)
    try { radio.withNativeGraph(deviceId) { it.size }; fail("late discovery restored a stale graph") }
    catch (_: IllegalStateException) { }
    val retried = mutableListOf<Result<Unit>>()
    radio.discover(deviceId) { retried.add(it) }
    verify(gatt, times(2)).discoverServices()
    radio.nativeGattCallback().onServicesDiscovered(gatt, BluetoothGatt.GATT_SUCCESS)
    assertTrue(retried.single().isSuccess)
    val current = radio.withNativeGraph(deviceId) { it.size }.first
    assertNotEquals(old, current)
    val paths = setOf(OwnedAndroidNotificationPath(serviceId, 0, charId, 0))
    try { radio.openOrderedSourceForPeer(deviceId, paths, 4, 8, expectedGraph = old) {}; fail("stale graph accepted") }
    catch (_: OrderedRegistrationStale) { }
    radio.openOrderedSourceForPeer(deviceId, paths, 4, 8, expectedGraph = current) {}
    radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    try { radio.openOrderedSourceForPeer(deviceId, paths, 4, 8, expectedGraph = current) {}; fail("old attachment accepted") }
    catch (_: OrderedRegistrationStale) { }
  }
  @Test fun callbackDuringPhysicalSetupReachesGroupAndOrdinarySubscriber() {
    val deviceId = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val cccd = mock(BluetoothGattDescriptor::class.java)
    doReturn(deviceId).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(0x10).`when`(characteristic).properties
    doReturn(service).`when`(characteristic).service
    doReturn(cccd).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(OwnedAndroidGattRadio.CCCD_UUID).`when`(cccd).uuid
    doReturn(characteristic).`when`(cccd).characteristic
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
    doReturn(true).`when`(gatt).writeDescriptor(cccd)
    doReturn(BluetoothGatt.GATT_SUCCESS).`when`(gatt).writeDescriptor(eq(cccd), any(ByteArray::class.java))
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val generation = radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    try {
      radio.openOrderedSourceGroup(gatt, generation + 1, setOf(characteristic), 8, 32) {}
      fail("stale generation accepted")
    } catch (_: IllegalStateException) { }
    val ordinary = mutableListOf<ByteArray>()
    val failures = mutableListOf<OwnedRadioTeardownFailure>()
    radio.onCleanupFailure = { failures.add(it) }
    radio.onProtocolNotification = { _, _, _, value -> ordinary.add(value) }
    var throwWake = false
    val opened = radio.openOrderedSourceForPeer(deviceId, setOf(OwnedAndroidNotificationPath(serviceId, 0, charId, 0)), 8, 32) {
      if (throwWake) error("wake failure")
    }
    val source = opened.source
    val group = opened.group
    val membership = opened.members.values.single().second
    val result = mutableListOf<Result<Unit>>()
    radio.setNotifyExact(deviceId, serviceId, 0, charId, 0, true, "notification") { result.add(it) }
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(7))
    assertTrue(result.isEmpty()); assertTrue(ordinary.isEmpty())
    assertArrayEquals(byteArrayOf(7), (source.drain(group).single() as OrderedNotificationSource.Event.Value).bytes)
    radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
    assertTrue(result.single().isSuccess)
    assertArrayEquals(byteArrayOf(7), ordinary.single())
    assertTrue(source.retire(group, characteristic, membership))
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(8))
    assertTrue(source.drain(group).single() is OrderedNotificationSource.Event.Fence)
    assertArrayEquals(byteArrayOf(8), ordinary.last())
    source.register(group, characteristic, membership)
    throwWake = true
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(9))
    assertArrayEquals(byteArrayOf(9), ordinary.last())
    assertEquals("orderedSource.valueWake", failures.last().operation)
    assertEquals(group.id, (failures.last().throwable as OrderedNotificationSource.WakeFailure).groupId)
    assertEquals(OrderedNotificationSource.Reason.WAKE_FAILED, (source.drain(group).last() as OrderedNotificationSource.Event.Terminal).reason)
    val serviceGroup = source.open(setOf(characteristic), 8, 32) { error("wake failure") }
    source.register(serviceGroup, characteristic)
    var serviceChanged = false
    radio.onServicesChanged = { serviceChanged = true }
    radio.nativeGattCallback().onServiceChanged(gatt)
    assertTrue(serviceChanged)
    assertEquals("orderedSource.serviceChangedWake", failures.last().operation)
    val terminal = source.drain(serviceGroup).single() as OrderedNotificationSource.Event.Terminal
    assertEquals(OrderedNotificationSource.Reason.DATABASE_CHANGED, terminal.reason)
    assertTrue(terminal.uncertainTail)
    try {
      radio.openOrderedSourceGroup(gatt, generation, setOf(characteristic), 8, 32) {}
      fail("invalidated database accepted")
    } catch (_: IllegalStateException) { }
    val emptyGeneration = radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val detached = CountDownLatch(1)
    val resumeInvalidation = CountDownLatch(1)
    radio.onOrderedSourceDetachedForTest = { detached.countDown(); check(resumeInvalidation.await(5, TimeUnit.SECONDS)) }
    val invalidationFailure = AtomicReference<Throwable?>()
    val emptyInvalidator = thread {
      try { radio.nativeGattCallback().onServiceChanged(gatt) }
      catch (error: Throwable) { invalidationFailure.set(error) }
    }
    assertTrue(detached.await(5, TimeUnit.SECONDS))
    try {
      radio.openOrderedSourceGroup(gatt, emptyGeneration, setOf(characteristic), 8, 32) {}
      fail("open raced past a null-source invalidation")
    } catch (_: IllegalStateException) { }
    resumeInvalidation.countDown(); emptyInvalidator.join(5000)
    assertFalse(emptyInvalidator.isAlive); assertNull(invalidationFailure.get())
    radio.onOrderedSourceDetachedForTest = null
    val racedGeneration = radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    doAnswer { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)); listOf(characteristic) }.`when`(service).characteristics
    var raced: Pair<OrderedNotificationSource<BluetoothGattCharacteristic>, OrderedNotificationSource<BluetoothGattCharacteristic>.Group>? = null
    val threadFailure = AtomicReference<Throwable?>()
    val opener = thread {
      try { raced = radio.openOrderedSourceGroup(gatt, racedGeneration, setOf(characteristic), 8, 32) {} }
      catch (error: Throwable) { threadFailure.compareAndSet(null, error) }
    }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    val invalidator = thread {
      try { radio.nativeGattCallback().onServiceChanged(gatt) }
      catch (error: Throwable) { threadFailure.compareAndSet(null, error) }
    }
    release.countDown(); opener.join(5000); invalidator.join(5000)
    assertFalse(opener.isAlive); assertFalse(invalidator.isAlive)
    assertNull(threadFailure.get())
    val (racedSource, racedGroup) = checkNotNull(raced)
    assertEquals(OrderedNotificationSource.Reason.DATABASE_CHANGED, (racedSource.drain(racedGroup).single() as OrderedNotificationSource.Event.Terminal).reason)
    doReturn(listOf(characteristic)).`when`(service).characteristics
    val nextGeneration = radio.attachConnectedGatt(deviceId, gatt, listOf(service))
    val (nextSource, nextGroup) = radio.openOrderedSourceGroup(gatt, nextGeneration, setOf(characteristic), 8, 32) { error("disconnect wake failure") }
    nextSource.register(nextGroup, characteristic)
    radio.nativeGattCallback().onConnectionStateChange(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    verify(gatt).close()
    assertEquals("orderedSource.disconnectWake", failures.last().operation)
    assertEquals(OrderedNotificationSource.Reason.CONNECTION_LOST, (nextSource.drain(nextGroup).single() as OrderedNotificationSource.Event.Terminal).reason)
  }
}
