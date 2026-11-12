// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/OwnedOrderedGroupPortTest.kt
package com.sfourdrinier.unifiedblemanager.rustcore

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.content.Context
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import java.util.UUID
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.mock

class OwnedOrderedGroupPortTest {
  @Test fun orderedStoreSnapshotsAreExactToEachOwnerAndRetainAdmissionReceipts() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val first = OwnedOrderedGroupPort(radio)
    val second = OwnedOrderedGroupPort(radio)
    assertEquals(0, first.orderedStoreSnapshot().openAdmissions)
    first.orderedAdmitOpen(42u, 7u, "peer")
    second.orderedAdmitOpen(42u, 7u, "peer")
    assertEquals(1, first.orderedStoreSnapshot().openAdmissions)
    assertEquals(1, second.orderedStoreSnapshot().openAdmissions)
    first.orderedRecoverOpenExact(42u, 7u, "peer")
    first.orderedAcknowledgeOpen(42u, 7u, "peer")
    assertEquals(0, first.orderedStoreSnapshot().openAdmissions)
    assertEquals(1, first.orderedStoreSnapshot().acknowledgedOpens)
    assertEquals(1, second.orderedStoreSnapshot().openAdmissions)
    assertEquals(0, second.orderedStoreSnapshot().acknowledgedOpens)
    radio.destroy()
  }
  @Test fun failedPhysicalDisableReachesBridgeWithExactMemberAndPlatformStatus() {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.randomUUID(); val charId = UUID.randomUUID()
    val device = mock(BluetoothDevice::class.java); val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java); val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val descriptor = mock(BluetoothGattDescriptor::class.java)
    doReturn(peer).`when`(device).address; doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid; doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid; doReturn(service).`when`(characteristic).service
    doReturn(0x10).`when`(characteristic).properties
    doReturn(descriptor).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(characteristic).`when`(descriptor).characteristic
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
    doReturn(true).`when`(gatt).writeDescriptor(descriptor)
    val scheduled = mutableListOf<() -> Unit>()
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, action -> scheduled.add(action); true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val graph = radio.withNativeGraph(peer) { Unit }.first
    val port = OwnedOrderedGroupPort(radio)
    val instance = CharacteristicInstance(peer, serviceId.toString(), 0, charId.toString(), 0)
    val opened = port.orderedOpen(42u, graph, mapOf(77uL to instance), 8, 64) { _, _ -> }
    val registration = opened.members.single()
    assertEquals(1, port.orderedStoreSnapshot().routes)
    assertEquals(1, port.orderedStoreSnapshot().removalReceipts)
    port.orderedReserveMarkerWrite(opened.generation, 42u, 0u, "connection", graph, instance, byteArrayOf(1), true,
      com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation())
    assertEquals(1, port.orderedStoreSnapshot().markerReceipts)
    port.orderedCancelMarkerWrite(opened.generation, 42u, 0u, IllegalStateException("cancelled"))
    port.orderedForgetMarkerWriteReceipt(opened.generation, 42u, 0u)
    assertEquals(0, port.orderedStoreSnapshot().markerReceipts)
    port.orderedRetainClaim(opened.generation, 42u, registration)
    val admission = port.orderedAdmission(opened.generation, 42u, registration, instance)
    radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, true, "notification", 7, admission) { }
    radio.nativeGattCallback().onDescriptorWrite(gatt, descriptor, BluetoothGatt.GATT_SUCCESS)
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(3))
    radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, false, null, 7, null) { }
    radio.nativeGattCallback().onDescriptorWrite(gatt, descriptor, 42)
    scheduled.removeAt(0).invoke()
    val batch = port.orderedDrain(opened.generation, 42u)
    assertEquals(1, port.orderedStoreSnapshot().batches)
    assertTrue(batch.records.first() is OrderedPortRecord.Value)
    val terminal = batch.records.last() as OrderedPortRecord.Terminal
    assertEquals("source-failed", terminal.reason)
    assertEquals(registration, terminal.failureMember)
    assertEquals("android", terminal.nativeDomain)
    assertEquals("42", terminal.nativeCode)
    assertTrue(terminal.uncertainTail)
    val wire = OrderedGroupBridgeMapper.records(batch)
    assertEquals("source-failed", wire.records.last().reason)
    assertEquals("42", wire.records.last().nativeCode)
    assertEquals("gatt-status", wire.records.last().failureKind)
    assertEquals(42, wire.records.last().gattStatus)
    assertEquals(77L, wire.records.last().failureMember.member)
    assertEquals(registration.sourceMember.toLong(), wire.records.last().failureMember.sourceMember)
    assertEquals(registration.epoch.toLong(), wire.records.last().failureMember.epoch)
    // The original terminal is retained until ACK while later native observations arrive.
    val target = mapOf(admission.group to admission.membership)
    val failures = listOf(SecurityException("private"),
      RadioPortFailure(RadioFailureKind.GATT_STATUS, "private status", 257),
      com.sfourdrinier.unifiedblemanager.radio.AndroidCccdSubmissionFailure(17)) +
      List(6) { SecurityException("private $it") }
    failures.forEach { cause -> assertTrue(admission.source.failMember(characteristic, target, cause)) }
    assertEquals(batch.id, port.orderedDrain(opened.generation, 42u).id)
    port.orderedAcknowledge(opened.generation, 42u, requireNotNull(batch.id))
    port.orderedSeal(opened.generation, 42u)
    val later = port.orderedDrain(opened.generation, 42u)
    val laterWire = OrderedGroupBridgeMapper.records(later)
    assertEquals(9, laterWire.records.size)
    assertEquals("subsequent-source-failure", laterWire.records.first().kind)
    assertEquals("permission-denied", laterWire.records.first().failureKind)
    assertEquals("Bluetooth permission denied while maintaining notification source", laterWire.records.first().detail)
    assertEquals("gatt-status", laterWire.records[1].failureKind)
    assertEquals(257, laterWire.records[1].gattStatus)
    assertEquals("Android notification source GATT failure", laterWire.records[1].detail)
    assertEquals("17", laterWire.records[2].nativeName)
    assertEquals("failure-observations-omitted", laterWire.records.last().kind)
    assertEquals(later.id, port.orderedDrain(opened.generation, 42u).id)
    try { port.orderedRemove(opened.generation, 42u); fail("unacknowledged failure batch must remain retained") }
    catch (_: IllegalStateException) { }
    port.orderedAcknowledge(opened.generation, 42u, requireNotNull(later.id))
    try { port.orderedRemove(opened.generation, 42u); fail("physical claim must remain retained") }
    catch (_: IllegalStateException) { }
    port.orderedConfirmCleanup(opened.generation, 42u, registration)
    port.orderedRemove(opened.generation, 42u)
    assertEquals(0, port.orderedStoreSnapshot().routes)
    assertEquals(1, port.orderedStoreSnapshot().removalReceipts)
    port.orderedForgetRemovalReceipt(opened.generation, 42u)
    assertEquals(0, port.orderedStoreSnapshot().removalReceipts)
    radio.destroy()
  }
  @Test fun permissionFailureDuringDisableReachesBridgeWithoutLeakingExceptionText() {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.randomUUID(); val charId = UUID.randomUUID()
    val device = mock(BluetoothDevice::class.java); val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java); val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val descriptor = mock(BluetoothGattDescriptor::class.java)
    doReturn(peer).`when`(device).address; doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid; doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid; doReturn(service).`when`(characteristic).service
    doReturn(0x10).`when`(characteristic).properties
    doReturn(descriptor).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(characteristic).`when`(descriptor).characteristic
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    org.mockito.Mockito.doThrow(SecurityException("private exception detail")).`when`(gatt).setCharacteristicNotification(characteristic, false)
    doReturn(true).`when`(gatt).writeDescriptor(descriptor)
    val scheduled = mutableListOf<() -> Unit>()
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, action -> scheduled.add(action); true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val graph = radio.withNativeGraph(peer) { Unit }.first
    val port = OwnedOrderedGroupPort(radio)
    val instance = CharacteristicInstance(peer, serviceId.toString(), 0, charId.toString(), 0)
    val opened = port.orderedOpen(42u, graph, mapOf(77uL to instance), 8, 64) { _, _ -> }
    val registration = opened.members.single()
    port.orderedRetainClaim(opened.generation, 42u, registration)
    val admission = port.orderedAdmission(opened.generation, 42u, registration, instance)
    radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, true, "notification", 7, admission) { }
    radio.nativeGattCallback().onDescriptorWrite(gatt, descriptor, BluetoothGatt.GATT_SUCCESS)
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(3))
    radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, false, null, 7, null) { }
    val batch = port.orderedDrain(opened.generation, 42u)
    assertTrue(batch.records.first() is OrderedPortRecord.Value)
    val terminal = batch.records.last() as OrderedPortRecord.Terminal
    assertEquals("source-failed", terminal.reason)
    assertEquals(registration, terminal.failureMember)
    assertNull(terminal.nativeDomain)
    assertNull(terminal.nativeCode)
    assertTrue(terminal.uncertainTail)
    val wire = OrderedGroupBridgeMapper.records(batch)
    assertEquals("source-failed", wire.records.last().reason)
    assertNull(wire.records.last().nativeCode)
    assertEquals("permission-denied", wire.records.last().failureKind)
    assertEquals("Bluetooth permission denied while maintaining notification source", wire.records.last().detail)
    assertEquals(77L, wire.records.last().failureMember.member)
    assertEquals(registration.sourceMember.toLong(), wire.records.last().failureMember.sourceMember)
    assertEquals(registration.epoch.toLong(), wire.records.last().failureMember.epoch)
    port.orderedAcknowledge(opened.generation, 42u, requireNotNull(batch.id))
    try { port.orderedRemove(opened.generation, 42u); fail("physical claim must remain retained") }
    catch (_: IllegalStateException) { }
    port.orderedConfirmCleanup(opened.generation, 42u, registration)
    port.orderedRemove(opened.generation, 42u)
    radio.destroy()
  }
  @Test fun abandonedDisableReachesBridgeWithoutInventingPlatformStatus() {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.randomUUID(); val charId = UUID.randomUUID()
    val device = mock(BluetoothDevice::class.java); val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java); val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val descriptor = mock(BluetoothGattDescriptor::class.java)
    doReturn(peer).`when`(device).address; doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid; doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid; doReturn(service).`when`(characteristic).service
    doReturn(0x10).`when`(characteristic).properties
    doReturn(descriptor).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(characteristic).`when`(descriptor).characteristic
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
    doReturn(true).`when`(gatt).writeDescriptor(descriptor)
    val scheduled = mutableListOf<() -> Unit>()
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, action -> scheduled.add(action); true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val graph = radio.withNativeGraph(peer) { Unit }.first
    val port = OwnedOrderedGroupPort(radio)
    val instance = CharacteristicInstance(peer, serviceId.toString(), 0, charId.toString(), 0)
    val opened = port.orderedOpen(42u, graph, mapOf(77uL to instance), 8, 64) { _, _ -> }
    val registration = opened.members.single()
    port.orderedRetainClaim(opened.generation, 42u, registration)
    val admission = port.orderedAdmission(opened.generation, 42u, registration, instance)
    radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, true, "notification", 7, admission) { }
    radio.nativeGattCallback().onDescriptorWrite(gatt, descriptor, BluetoothGatt.GATT_SUCCESS)
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(3))
    val disable = radio.setNotifyExactRetained(peer, serviceId, 0, charId, 0, false, null, 7, null) { }
    assertTrue(radio.cancelOperation(disable))
    val batch = port.orderedDrain(opened.generation, 42u)
    assertTrue(batch.records.first() is OrderedPortRecord.Value)
    val terminal = batch.records.last() as OrderedPortRecord.Terminal
    assertEquals("source-failed", terminal.reason)
    assertEquals(registration, terminal.failureMember)
    assertNull(terminal.nativeDomain)
    assertNull(terminal.nativeCode)
    assertTrue(terminal.uncertainTail)
    val wire = OrderedGroupBridgeMapper.records(batch)
    assertEquals("source-failed", wire.records.last().reason)
    assertNull(wire.records.last().nativeCode)
    assertEquals("platform", wire.records.last().failureKind)
    assertEquals("notification delivery may have been interrupted by an abandoned disable", wire.records.last().detail)
    assertEquals(77L, wire.records.last().failureMember.member)
    assertEquals(registration.sourceMember.toLong(), wire.records.last().failureMember.sourceMember)
    assertEquals(registration.epoch.toLong(), wire.records.last().failureMember.epoch)
    port.orderedAcknowledge(opened.generation, 42u, requireNotNull(batch.id))
    try { port.orderedRemove(opened.generation, 42u); fail("physical claim must remain retained") }
    catch (_: IllegalStateException) { }
    port.orderedConfirmCleanup(opened.generation, 42u, registration)
    port.orderedRemove(opened.generation, 42u)
    radio.destroy()
  }
  @Test fun admissionIdentityBelongsToThePortOwner() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val replacement = OwnedOrderedGroupPort(radio)
    assertEquals("", owner.orderedAdmissionOwner(1))
    assertEquals("", owner.orderedAdmissionOwner(2))
    val slotOwner = owner.orderedAdmissionOwner(3)
    assertEquals(slotOwner, owner.orderedAdmissionOwner(3))
    assertNotEquals(slotOwner, replacement.orderedAdmissionOwner(3))
    assertEquals(4, UUID.fromString(slotOwner).version())
    val v4Owner = owner.orderedAdmissionOwner(4)
    assertNotEquals(slotOwner, v4Owner)
    assertNotEquals(v4Owner, replacement.orderedAdmissionOwner(4))
    assertEquals(4, UUID.fromString(v4Owner).version())
  }
  @Test fun mapsValueFenceAndOverflowWithoutLosingMembership() {
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
    val port = OwnedOrderedGroupPort(radio)
    val id = ULong.MAX_VALUE
    val opened = port.orderedOpen(id, graph, mapOf(id to CharacteristicInstance(peer, serviceId.toString(), 0, charId.toString(), 0)), 1, 8) { _, _ -> }
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(1))
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(2, 3))
    val batch = port.orderedDrain(opened.generation, id)
    val value = batch.records[0] as OrderedPortRecord.Value
    val terminal = batch.records[1] as OrderedPortRecord.Terminal
    assertEquals(opened.members.single(), value.member)
    assertArrayEquals(byteArrayOf(1), value.bytes)
    assertEquals("overflow", terminal.reason)
    assertEquals(OrderedPortRejected.Value(value.member, 2uL), terminal.rejected)
    assertEquals(batch, port.orderedDrain(opened.generation, id))
    port.orderedAcknowledge(opened.generation, id, requireNotNull(batch.id))
    port.orderedRemove(opened.generation, id)
    val secondId = id - 1uL
    val second = port.orderedOpen(secondId, graph, mapOf(id to CharacteristicInstance(peer, serviceId.toString(), 0, charId.toString(), 0)), 2, 8) { _, _ -> }
    port.orderedRetire(second.generation, secondId, second.members.single())
    val fence = port.orderedDrain(second.generation, secondId)
    assertEquals(OrderedPortRecord.Fence(second.members.single(), true), fence.records.single())
    port.orderedAcknowledge(second.generation, secondId, requireNotNull(fence.id))
    port.orderedClose(second.generation, secondId)
    val closed = port.orderedDrain(second.generation, secondId)
    assertEquals("closed", (closed.records.single() as OrderedPortRecord.Terminal).reason)
    port.orderedAcknowledge(second.generation, secondId, requireNotNull(closed.id))
    port.orderedRemove(second.generation, secondId)
  }
}
