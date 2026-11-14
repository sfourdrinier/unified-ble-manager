// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OrderedMarkerRadioTest.kt
package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.*
import android.content.Context
import java.util.UUID
import org.junit.Assert.*
import org.junit.Test
import org.mockito.ArgumentMatchers.*
import org.mockito.Mockito.*

class OrderedMarkerRadioTest {
  private class Fixture(capacity: Int, post: ((() -> Unit) -> Boolean) = { it(); true }) {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device: BluetoothDevice = mock(BluetoothDevice::class.java)
    val gatt: BluetoothGatt = mock(BluetoothGatt::class.java)
    val service: BluetoothGattService = mock(BluetoothGattService::class.java)
    val characteristic: BluetoothGattCharacteristic = mock(BluetoothGattCharacteristic::class.java)
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = post, scheduleDelayed = { _, _ -> true })
    val owner = AndroidOrderedGroupOwner(radio)
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    val graph: AndroidNativeGraphIdentity
    val generation: ULong
    init {
      doReturn(peer).`when`(device).address; doReturn(device).`when`(gatt).device
      doReturn(serviceId).`when`(service).uuid; doReturn(listOf(characteristic)).`when`(service).characteristics
      doReturn(charId).`when`(characteristic).uuid; doReturn(0x18).`when`(characteristic).properties
      doReturn(service).`when`(characteristic).service
      radio.attachConnectedGatt(peer, gatt, listOf(service))
      graph = radio.withNativeGraph(peer) { Unit }.first
      generation = owner.open(3uL, peer, mapOf(0uL to path), capacity, 64, expectedGraph = graph) { _, _ ->
        assertFalse(radio.admissionLocksHeldByCurrentThread())
      }.generation
    }
    fun value(byte: Byte) = radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(byte))
    fun start(result: (OrderedMarkerTransactions.State) -> Unit): Long? =
      owner.markerWrite(generation, 3uL, 0uL, peer, graph, "connection", path, byteArrayOf(42), true, result = result)
  }

  @Test fun nativeWritePlacesMarkerBetweenCallbacksAndRetainsReplay() {
    val f = Fixture(8); var writes = 0
    val result = mutableListOf<OrderedMarkerTransactions.State>()
    f.value(1)
    doAnswer { writes++; f.value(2); f.radio.nativeGattCallback().onCharacteristicWrite(f.gatt, f.characteristic, BluetoothGatt.GATT_SUCCESS); assertTrue(result.isEmpty()); true }
      .`when`(f.gatt).writeCharacteristic(f.characteristic)
    doAnswer { writes++; f.value(2); f.radio.nativeGattCallback().onCharacteristicWrite(f.gatt, f.characteristic, BluetoothGatt.GATT_SUCCESS); assertTrue(result.isEmpty()); BluetoothGatt.GATT_SUCCESS }
      .`when`(f.gatt).writeCharacteristic(eq(f.characteristic), any(ByteArray::class.java), eq(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT))
    f.start { state -> assertFalse(f.radio.admissionLocksHeldByCurrentThread()); result.add(state) }
    assertEquals(1, writes); assertTrue(result.single() is OrderedMarkerTransactions.State.Settled)
    val replay = mutableListOf<OrderedMarkerTransactions.State>()
    assertNull(f.start { replay.add(it) }); assertTrue(replay.single() is OrderedMarkerTransactions.State.Settled); assertEquals(1, writes)
    val disconnectedReplay = mutableListOf<OrderedMarkerTransactions.State>()
    assertNull(f.owner.markerWrite(f.generation, 3uL, 0uL, f.peer, f.graph, "connection", f.path, byteArrayOf(42), true,
      validate = { error("old connection is no longer current") }) { disconnectedReplay.add(it) })
    assertTrue(disconnectedReplay.single() is OrderedMarkerTransactions.State.Settled)
    val batch = f.owner.drain(f.generation, 3uL)
    assertEquals(0uL, (batch.events[0] as OrderedNotificationSource.Event.Value).ordinal)
    val marker = batch.events[1] as OrderedNotificationSource.Event.Marker
    assertEquals(0uL, marker.markerId); assertEquals(1uL, marker.ordinal)
    assertEquals(2uL, (batch.events[2] as OrderedNotificationSource.Event.Value).ordinal)
    assertFalse(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
    f.owner.acknowledge(f.generation, 3uL, requireNotNull(batch.id))
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
  }

  @Test fun cancellationAfterWriteStartKeepsLateCompletionForSettlement() {
    val f = Fixture(8)
    var writes = 0
    doAnswer { writes++; true }.`when`(f.gatt).writeCharacteristic(f.characteristic)
    doAnswer { writes++; BluetoothGatt.GATT_SUCCESS }.`when`(f.gatt).writeCharacteristic(eq(f.characteristic), any(ByteArray::class.java), eq(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT))
    val results = mutableListOf<OrderedMarkerTransactions.State>()
    val operation = requireNotNull(f.start { results.add(it) })
    assertTrue(f.radio.cancelOperation(operation))
    assertTrue(results.single() is OrderedMarkerTransactions.State.Started)
    f.radio.nativeGattCallback().onCharacteristicWrite(f.gatt, f.characteristic, BluetoothGatt.GATT_SUCCESS)
    assertTrue(f.owner.markerWriteSettlement(f.generation, 3uL, 0uL) is OrderedMarkerTransactions.State.Settled)
    assertEquals(1, results.size)
    val replay = mutableListOf<OrderedMarkerTransactions.State>()
    assertNull(f.start { replay.add(it) })
    assertTrue(replay.single() is OrderedMarkerTransactions.State.Settled)
    assertEquals(1, writes)
    val batch = f.owner.drain(f.generation, 3uL)
    f.owner.acknowledge(f.generation, 3uL, requireNotNull(batch.id))
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
  }

  @Test fun nativeWriteInitiationCanBeQueriedWhileDriverCallIsBlocked() {
    val f = Fixture(8)
    val inside = java.util.concurrent.CountDownLatch(1)
    val finish = java.util.concurrent.CountDownLatch(1)
    fun pause() { inside.countDown(); check(finish.await(5, java.util.concurrent.TimeUnit.SECONDS)) }
    doAnswer { pause(); true }.`when`(f.gatt).writeCharacteristic(f.characteristic)
    doAnswer { pause(); BluetoothGatt.GATT_SUCCESS }.`when`(f.gatt).writeCharacteristic(eq(f.characteristic), any(ByteArray::class.java), eq(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT))
    val worker = kotlin.concurrent.thread { f.start {} }
    assertTrue(inside.await(5, java.util.concurrent.TimeUnit.SECONDS))
    assertEquals(OrderedMarkerTransactions.State.Initiating(0uL), f.owner.markerWriteSettlement(f.generation, 3uL, 0uL))
    assertEquals(OrderedMarkerTransactions.State.Initiating(0uL), f.owner.cancelMarkerWriteExact(f.generation, 3uL, 0uL))
    finish.countDown(); worker.join(5000); assertFalse(worker.isAlive)
    f.radio.nativeGattCallback().onCharacteristicWrite(f.gatt, f.characteristic, BluetoothGatt.GATT_SUCCESS)
    val batch = f.owner.drain(f.generation, 3uL)
    f.owner.acknowledge(f.generation, 3uL, requireNotNull(batch.id))
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
  }

  @Test fun thrownWriteRetainsUncertaintyUntilNativeCompletion() {
    val f = Fixture(8)
    doAnswer { throw IllegalStateException("driver threw") }.`when`(f.gatt).writeCharacteristic(f.characteristic)
    doAnswer { throw IllegalStateException("driver threw") }.`when`(f.gatt).writeCharacteristic(eq(f.characteristic), any(ByteArray::class.java), eq(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT))
    val results = mutableListOf<OrderedMarkerTransactions.State>()
    f.start { results.add(it) }
    val uncertain = f.owner.markerWriteSettlement(f.generation, 3uL, 0uL) as OrderedMarkerTransactions.State.Started
    assertEquals("driver threw", uncertain.uncertainty?.message)
    assertFalse(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
    f.radio.nativeGattCallback().onCharacteristicWrite(f.gatt, f.characteristic, BluetoothGatt.GATT_SUCCESS)
    assertTrue(f.owner.markerWriteSettlement(f.generation, 3uL, 0uL) is OrderedMarkerTransactions.State.Settled)
    val batch = f.owner.drain(f.generation, 3uL)
    f.owner.acknowledge(f.generation, 3uL, requireNotNull(batch.id))
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
  }

  @Test fun thrownWriteWithoutCallbackDoesNotPinQueuedGattWork() {
    val f = Fixture(8)
    val results = mutableListOf<OrderedMarkerTransactions.State>()
    val mtuResults = mutableListOf<Result<Int>>()
    doAnswer {
      f.radio.requestMtu(f.peer, 100) { mtuResults.add(it) }
      throw IllegalStateException("driver threw before any callback")
    }.`when`(f.gatt).writeCharacteristic(f.characteristic)
    doAnswer {
      f.radio.requestMtu(f.peer, 100) { mtuResults.add(it) }
      throw IllegalStateException("driver threw before any callback")
    }.`when`(f.gatt).writeCharacteristic(eq(f.characteristic), any(ByteArray::class.java), eq(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT))
    doReturn(true).`when`(f.gatt).requestMtu(100)
    f.start { results.add(it) }
    val uncertain = f.owner.markerWriteSettlement(f.generation, 3uL, 0uL) as OrderedMarkerTransactions.State.Started
    assertEquals("driver threw before any callback", uncertain.uncertainty?.message)
    assertSame(uncertain, results.single())
    verify(f.gatt).requestMtu(100)
    f.radio.nativeGattCallback().onMtuChanged(f.gatt, 100, BluetoothGatt.GATT_SUCCESS)
    assertEquals(100, mtuResults.single().getOrThrow())
    assertFalse(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
  }

  @Test fun lateThrownWriteCallbackCannotSettleLaterSameCharacteristicWrite() {
    val f = Fixture(8)
    var writes = 0
    doAnswer { writes++; throw IllegalStateException("ambiguous submission") }.`when`(f.gatt).writeCharacteristic(f.characteristic)
    doAnswer { writes++; throw IllegalStateException("ambiguous submission") }.`when`(f.gatt).writeCharacteristic(eq(f.characteristic), any(ByteArray::class.java), eq(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT))
    f.start {}
    val later = mutableListOf<Result<ByteArray?>>()
    f.radio.writeCharacteristicExact(f.peer, f.serviceId, 0, f.charId, 0, byteArrayOf(99), true) { later.add(it) }
    assertEquals(1, later.size)
    assertTrue(later.single().isFailure)
    f.radio.nativeGattCallback().onCharacteristicWrite(f.gatt, f.characteristic, BluetoothGatt.GATT_SUCCESS)
    assertTrue(f.owner.markerWriteSettlement(f.generation, 3uL, 0uL) is OrderedMarkerTransactions.State.Settled)
    assertEquals(1, later.size)
    doReturn(true).`when`(f.gatt).writeCharacteristic(f.characteristic)
    doReturn(BluetoothGatt.GATT_SUCCESS).`when`(f.gatt).writeCharacteristic(eq(f.characteristic), any(ByteArray::class.java), eq(BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT))
    f.radio.writeCharacteristicExact(f.peer, f.serviceId, 0, f.charId, 0, byteArrayOf(100), true) { later.add(it) }
    assertEquals(1, later.size)
    f.radio.nativeGattCallback().onCharacteristicWrite(f.gatt, f.characteristic, BluetoothGatt.GATT_SUCCESS)
    assertTrue(later.last().isSuccess)
  }

  @Test fun cancelledStartedMarkerReleasesQueueButKeepsItsCallbackOwner() {
    val f = Fixture(8)
    doReturn(true).`when`(f.gatt).writeCharacteristic(f.characteristic)
    doReturn(BluetoothGatt.GATT_SUCCESS).`when`(f.gatt).writeCharacteristic(eq(f.characteristic), any(ByteArray::class.java), anyInt())
    doReturn(true).`when`(f.gatt).requestMtu(100)
    val result = mutableListOf<OrderedMarkerTransactions.State>()
    val operation = requireNotNull(f.start { result.add(it) })
    assertTrue(f.radio.cancelOperation(operation))
    val mtu = mutableListOf<Result<Int>>()
    f.radio.requestMtu(f.peer, 100) { mtu.add(it) }
    verify(f.gatt).requestMtu(100)
    f.radio.nativeGattCallback().onMtuChanged(f.gatt, 100, BluetoothGatt.GATT_SUCCESS)
    assertEquals(100, mtu.single().getOrThrow())
    val blocked = mutableListOf<Result<ByteArray?>>()
    f.radio.writeCharacteristicExact(f.peer, f.serviceId, 0, f.charId, 0, byteArrayOf(9), true) { blocked.add(it) }
    assertTrue(blocked.single().isFailure)
    f.radio.nativeGattCallback().onCharacteristicWrite(f.gatt, f.characteristic, BluetoothGatt.GATT_SUCCESS)
    assertTrue(f.owner.markerWriteSettlement(f.generation, 3uL, 0uL) is OrderedMarkerTransactions.State.Settled)
    assertEquals(1, result.size)
    f.radio.destroy()
  }

  @Test fun fullSourceRejectsMarkerWithoutWriting() {
    val f = Fixture(1); f.value(1)
    val results = mutableListOf<OrderedMarkerTransactions.State>(); f.start { results.add(it) }
    assertTrue(results.single() is OrderedMarkerTransactions.State.Rejected)
    verify(f.gatt, never()).writeCharacteristic(any(BluetoothGattCharacteristic::class.java))
    verify(f.gatt, never()).writeCharacteristic(any(BluetoothGattCharacteristic::class.java), any(ByteArray::class.java), anyInt())
    val batch = f.owner.drain(f.generation, 3uL)
    assertEquals(0uL, (batch.events.last() as OrderedNotificationSource.Event.Terminal).rejectedMarker)
    assertFalse(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
    f.owner.acknowledge(f.generation, 3uL, requireNotNull(batch.id))
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
  }

  @Test fun serviceChangeBeforeQueuedAdmissionNeverWritesOldGraph() {
    val tasks = ArrayDeque<() -> Unit>()
    val f = Fixture(8) { action -> tasks.addLast(action); true }
    val results = mutableListOf<OrderedMarkerTransactions.State>()
    f.start { results.add(it) }
    f.radio.nativeGattCallback().onServiceChanged(f.gatt)
    while (tasks.isNotEmpty()) tasks.removeFirst().invoke()
    assertTrue(results.single() is OrderedMarkerTransactions.State.NotAdmitted)
    verify(f.gatt, never()).writeCharacteristic(any(BluetoothGattCharacteristic::class.java))
    verify(f.gatt, never()).writeCharacteristic(any(BluetoothGattCharacteristic::class.java), any(ByteArray::class.java), anyInt())
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
  }

  @Test fun exactCancelRemovesQueuedCellBeforeReceiptRetirement() {
    val tasks = ArrayDeque<() -> Unit>()
    val f = Fixture(8) { action -> tasks.addLast(action); true }
    val results = mutableListOf<OrderedMarkerTransactions.State>()
    f.start { results.add(it) }
    assertTrue(f.owner.cancelMarkerWriteExact(f.generation, 3uL, 0uL) is OrderedMarkerTransactions.State.NotAdmitted)
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
    while (tasks.isNotEmpty()) tasks.removeFirst().invoke()
    assertTrue(f.owner.drain(f.generation, 3uL).events.isEmpty())
    verify(f.gatt, never()).writeCharacteristic(any(BluetoothGattCharacteristic::class.java))
    verify(f.gatt, never()).writeCharacteristic(any(BluetoothGattCharacteristic::class.java), any(ByteArray::class.java), anyInt())
  }

  @Test fun cancellationBeforeQueuedAdmissionNeverWrites() {
    val tasks = ArrayDeque<() -> Unit>()
    val f = Fixture(8) { action -> tasks.addLast(action); true }
    val results = mutableListOf<OrderedMarkerTransactions.State>()
    val operation = requireNotNull(f.start { results.add(it) })
    assertTrue(f.radio.cancelOperation(operation))
    while (tasks.isNotEmpty()) tasks.removeFirst().invoke()
    assertTrue(results.single() is OrderedMarkerTransactions.State.NotAdmitted)
    assertTrue(f.owner.drain(f.generation, 3uL).events.isEmpty())
    verify(f.gatt, never()).writeCharacteristic(any(BluetoothGattCharacteristic::class.java))
    assertTrue(f.owner.forgetMarkerWriteReceipt(f.generation, 3uL, 0uL))
  }
}
