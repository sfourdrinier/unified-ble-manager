package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.*
import android.content.Context
import android.content.BroadcastReceiver
import android.content.Intent
import android.content.IntentFilter
import com.sfourdrinier.unifiedblemanager.rustcore.CharacteristicInstance
import com.sfourdrinier.unifiedblemanager.rustcore.OrderedGroupBridgeMapper
import com.sfourdrinier.unifiedblemanager.rustcore.OrderedPortRecord
import com.sfourdrinier.unifiedblemanager.rustcore.OwnedOrderedGroupPort
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlin.concurrent.thread
import org.junit.Assert.*
import org.junit.Test
import org.mockito.ArgumentMatchers.*
import org.mockito.Mockito.*

/** Android callbacks have no operation ID: an abandoned owner must not be replaced. */
class OwnedAndroidGattPendingCallbackTest {
  private enum class Operation { READ, WRITE, DESCRIPTOR_READ, DESCRIPTOR_WRITE }

  private class Fixture(post: ((() -> Unit) -> Boolean) = { it(); true }) {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId: UUID = UUID.randomUUID()
    val charId: UUID = UUID.randomUUID()
    val descId: UUID = UUID.randomUUID()
    val device = mock(BluetoothDevice::class.java)
    var gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val descriptor = mock(BluetoothGattDescriptor::class.java)
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    val scheduled = mutableListOf<() -> Unit>()
    val radio = OwnedAndroidGattRadio(context, post = post, scheduleDelayed = { _, action -> scheduled.add(action); true })
    init {
      doReturn(manager).`when`(context).getSystemService(Context.BLUETOOTH_SERVICE)
      doReturn(adapter).`when`(manager).adapter
      doReturn(BluetoothAdapter.STATE_ON).`when`(adapter).state
      doReturn(peer).`when`(device).address
      doReturn(serviceId).`when`(service).uuid
      doReturn(listOf(characteristic)).`when`(service).characteristics
      doReturn(charId).`when`(characteristic).uuid
      doReturn(service).`when`(characteristic).service
      doReturn(listOf(descriptor)).`when`(characteristic).descriptors
      doReturn(descId).`when`(descriptor).uuid
      doReturn(characteristic).`when`(descriptor).characteristic
      attach(gatt)
    }
    fun attach(next: BluetoothGatt) {
      gatt = next
      doReturn(device).`when`(gatt).device
      doReturn(listOf(service)).`when`(gatt).services
      radio.attachConnectedGatt(peer, gatt, listOf(service))
    }
    fun start(op: Operation, results: MutableList<Result<*>>): Long = when (op) {
      Operation.READ -> radio.readCharacteristicExact(peer, serviceId, 0, charId, 0) { results.add(it) }
      Operation.WRITE -> radio.writeCharacteristicExact(peer, serviceId, 0, charId, 0, byteArrayOf(1), true) { results.add(it) }
      Operation.DESCRIPTOR_READ -> radio.readDescriptorExact(peer, serviceId, 0, charId, 0, descId, 0) { results.add(it) }
      Operation.DESCRIPTOR_WRITE -> radio.writeDescriptorExact(peer, serviceId, 0, charId, 0, descId, 0, byteArrayOf(1)) { results.add(it) }
    }
    fun configure(op: Operation, throws: Boolean, beforeThrow: () -> Unit = {}) {
      fun fail(): Nothing { beforeThrow(); throw IllegalStateException("ambiguous submission") }
      when (op) {
        Operation.READ -> if (throws) doAnswer { fail() }.`when`(gatt).readCharacteristic(characteristic) else doReturn(true).`when`(gatt).readCharacteristic(characteristic)
        Operation.WRITE -> {
          if (throws) {
            doAnswer { fail() }.`when`(gatt).writeCharacteristic(characteristic)
            doAnswer { fail() }.`when`(gatt).writeCharacteristic(eq(characteristic), any(ByteArray::class.java), anyInt())
          } else {
            doReturn(true).`when`(gatt).writeCharacteristic(characteristic)
            doReturn(BluetoothGatt.GATT_SUCCESS).`when`(gatt).writeCharacteristic(eq(characteristic), any(ByteArray::class.java), anyInt())
          }
        }
        Operation.DESCRIPTOR_READ -> if (throws) doAnswer { fail() }.`when`(gatt).readDescriptor(descriptor) else doReturn(true).`when`(gatt).readDescriptor(descriptor)
        Operation.DESCRIPTOR_WRITE -> {
          if (throws) {
            doAnswer { fail() }.`when`(gatt).writeDescriptor(descriptor)
            doAnswer { fail() }.`when`(gatt).writeDescriptor(eq(descriptor), any(ByteArray::class.java))
          } else {
            doReturn(true).`when`(gatt).writeDescriptor(descriptor)
            doReturn(BluetoothGatt.GATT_SUCCESS).`when`(gatt).writeDescriptor(eq(descriptor), any(ByteArray::class.java))
          }
        }
      }
    }
    fun callback(op: Operation, source: BluetoothGatt = gatt) {
      when (op) {
        Operation.READ -> radio.nativeGattCallback().onCharacteristicRead(source, characteristic, byteArrayOf(9), BluetoothGatt.GATT_SUCCESS)
        Operation.WRITE -> radio.nativeGattCallback().onCharacteristicWrite(source, characteristic, BluetoothGatt.GATT_SUCCESS)
        Operation.DESCRIPTOR_READ -> radio.nativeGattCallback().onDescriptorRead(source, descriptor, BluetoothGatt.GATT_SUCCESS, byteArrayOf(9))
        Operation.DESCRIPTOR_WRITE -> radio.nativeGattCallback().onDescriptorWrite(source, descriptor, BluetoothGatt.GATT_SUCCESS)
      }
    }
  }

  @Test fun thrownInvocationKeepsOwnerUntilLateCallbackAndDoesNotBlockUnrelatedWork() {
    for (op in Operation.values()) {
      val f = Fixture()
      val mtu = mutableListOf<Result<Int>>()
      doReturn(true).`when`(f.gatt).requestMtu(100)
      f.configure(op, true) { f.radio.requestMtu(f.peer, 100) { mtu.add(it) } }
      val first = mutableListOf<Result<*>>(); val blocked = mutableListOf<Result<*>>()
      f.start(op, first)
      assertEquals(op.name, "ambiguous submission", first.single().exceptionOrNull()?.message)
      verify(f.gatt).requestMtu(100)
      f.radio.nativeGattCallback().onMtuChanged(f.gatt, 100, BluetoothGatt.GATT_SUCCESS)
      assertEquals(op.name, 100, mtu.single().getOrThrow())
      f.start(op, blocked)
      assertTrue(op.name, blocked.single().isFailure)
      f.callback(op)
      assertEquals(op.name, 1, first.size)
      f.configure(op, false)
      val next = mutableListOf<Result<*>>()
      f.start(op, next)
      assertTrue("${op.name}: $next", next.isEmpty())
      f.callback(op)
      assertTrue(op.name, next.single().isSuccess)
      f.radio.destroy()
    }
  }

  @Test fun destroyRetiresUnsettledOwnersWithoutDeliveringASecondResult() {
    for (op in Operation.values()) {
      val f = Fixture(); f.configure(op, true)
      val first = mutableListOf<Result<*>>(); f.start(op, first)
      f.radio.destroy()
      f.callback(op)
      assertEquals(op.name, 1, first.size)
    }
  }

  @Test fun cancellationWithoutCallbackDoesNotLetLateCallbackCompleteReplacement() {
    for (op in Operation.values()) {
      val f = Fixture(); f.configure(op, false)
      val first = mutableListOf<Result<*>>(); val blocked = mutableListOf<Result<*>>()
      val id = f.start(op, first)
      assertTrue(op.name, f.radio.cancelOperation(id))
      assertTrue(op.name, first.single().isFailure)
      val mtu = mutableListOf<Result<Int>>()
      doReturn(true).`when`(f.gatt).requestMtu(100)
      f.radio.requestMtu(f.peer, 100) { mtu.add(it) }
      f.radio.nativeGattCallback().onMtuChanged(f.gatt, 100, BluetoothGatt.GATT_SUCCESS)
      assertEquals(op.name, 100, mtu.single().getOrThrow())
      f.start(op, blocked)
      assertTrue(op.name, blocked.single().isFailure)
      f.callback(op)
      assertEquals(op.name, 1, first.size)
      val next = mutableListOf<Result<*>>()
      f.start(op, next)
      assertTrue(op.name, next.isEmpty())
      f.callback(op)
      assertTrue(op.name, next.single().isSuccess)
      f.radio.destroy()
    }
  }

  @Test fun reconnectRetiresOldOwnerAndOldCallbackCannotCompleteNewOwner() {
    for (op in Operation.values()) {
      val f = Fixture(); f.configure(op, true)
      val first = mutableListOf<Result<*>>(); f.start(op, first)
      val old = f.gatt
      f.radio.nativeGattCallback().onConnectionStateChange(old, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
      f.attach(mock(BluetoothGatt::class.java)); f.configure(op, false)
      val next = mutableListOf<Result<*>>(); f.start(op, next)
      f.callback(op, old)
      assertTrue(op.name, next.isEmpty())
      f.callback(op)
      assertTrue(op.name, next.single().isSuccess)
      assertEquals(op.name, 1, first.size)
      f.radio.destroy()
    }
  }
  @Test fun synchronousCallbackBeforeInvocationThrowsSettlesExactlyOnce() {
    for (op in Operation.values()) {
      val f = Fixture()
      val mtu = mutableListOf<Result<Int>>()
      doReturn(true).`when`(f.gatt).requestMtu(100)
      f.configure(op, true) {
        f.radio.requestMtu(f.peer, 100) { mtu.add(it) }
        f.callback(op)
        verify(f.gatt, never()).requestMtu(100)
      }
      val first = mutableListOf<Result<*>>(); f.start(op, first)
      assertTrue(op.name, first.single().isSuccess)
      verify(f.gatt).requestMtu(100)
      f.radio.nativeGattCallback().onMtuChanged(f.gatt, 100, BluetoothGatt.GATT_SUCCESS)
      assertEquals(op.name, 100, mtu.single().getOrThrow())
      f.configure(op, false)
      val next = mutableListOf<Result<*>>(); f.start(op, next)
      assertTrue(op.name, next.isEmpty())
      f.callback(op)
      assertTrue(op.name, next.single().isSuccess)
      assertEquals(op.name, 1, first.size)
      f.radio.destroy()
    }
  }

  @Test fun serviceChangeDoesNotRetireCallbackDebtOnTheSameGatt() {
    for (op in Operation.values()) {
      val f = Fixture(); f.configure(op, true)
      val first = mutableListOf<Result<*>>(); f.start(op, first)
      f.radio.nativeGattCallback().onServiceChanged(f.gatt)
      doReturn(true).`when`(f.gatt).discoverServices()
      val discovery = mutableListOf<Result<Unit>>()
      f.radio.discover(f.peer) { discovery.add(it) }
      f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
      assertTrue(discovery.single().isSuccess)
      val blocked = mutableListOf<Result<*>>(); f.start(op, blocked)
      assertTrue(op.name, blocked.single().isFailure)
      f.callback(op)
      assertEquals(op.name, 1, first.size)
      f.configure(op, false)
      val next = mutableListOf<Result<*>>(); f.start(op, next)
      assertTrue("${op.name}: $next", next.isEmpty())
      f.callback(op)
      assertTrue(op.name, next.single().isSuccess)
      f.radio.destroy()
    }
  }

  @Test fun cancellationDoesNotReleaseQueueWhilePlatformInvocationIsRunning() {
    val token = OwnedAndroidGattRadio.GattSerialQueue.GattOperationToken()
    val entered = CountDownLatch(1)
    val finish = CountDownLatch(1)
    val releases = AtomicInteger()
    val worker = thread {
      token.invokePhysical({ releases.incrementAndGet() }) {
        entered.countDown()
        check(finish.await(5, TimeUnit.SECONDS))
      }
    }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    token.cancel()
    token.releaseCancelled()
    assertEquals(0, releases.get())
    finish.countDown()
    worker.join(5000)
    assertFalse(worker.isAlive)
    assertEquals(1, releases.get())
  }

  @Test fun deferredActionsRunInOrderAndReleaseEvenWhenOneFails() {
    val token = OwnedAndroidGattRadio.GattSerialQueue.GattOperationToken()
    val actions = mutableListOf<String>()
    val failure = IllegalStateException("cleanup failed")
    val secondFailure = IllegalArgumentException("second cleanup failed")
    try {
      token.invokePhysical({ actions.add("release") }) {
        token.completeAfterInvocation { actions.add("first"); throw failure }
        token.completeAfterInvocation {
          actions.add("second")
          token.completeAfterInvocation { actions.add("third") }
          throw secondFailure
        }
        token.cancel()
      }
      fail("deferred failure must be reported")
    } catch (error: IllegalStateException) {
      assertSame(failure, error)
      assertEquals(listOf(secondFailure), error.suppressed.toList())
    }
    assertEquals(listOf("first", "second", "third", "release"), actions)
  }

  @Test fun physicalFailureRemainsPrimaryWhenDeferredCleanupAlsoFails() {
    val token = OwnedAndroidGattRadio.GattSerialQueue.GattOperationToken()
    val physicalFailure = IllegalStateException("native call failed")
    val cleanupFailure = IllegalStateException("cleanup failed")
    var released = false
    try {
      token.invokePhysical({ released = true }) {
        token.completeAfterInvocation { throw cleanupFailure }
        token.cancel()
        throw physicalFailure
      }
      fail("native failure must be reported")
    } catch (error: IllegalStateException) {
      assertSame(physicalFailure, error)
      assertEquals(listOf(cleanupFailure), error.suppressed.toList())
    }
    assertTrue(released)
  }

  @Test fun cancellationCleanupPrecedesQueueReleaseWhenCallbackWinsTheSchedulingRace() {
    val tasks = ConcurrentLinkedQueue<() -> Unit>()
    val queue = OwnedAndroidGattRadio.GattSerialQueue(post = { tasks.add(it); true })
    val nativeEntered = CountDownLatch(1)
    val resumeNative = CountDownLatch(1)
    val cancellationEntered = CountDownLatch(1)
    val resumeCancellation = CountDownLatch(1)
    val events = java.util.Collections.synchronizedList(mutableListOf<String>())
    lateinit var token: OwnedAndroidGattRadio.GattSerialQueue.GattOperationToken
    val id = queue.submitCancellable({ current, done ->
      token = current
      current.invokePhysical(done) {
        nativeEntered.countDown()
        check(resumeNative.await(5, TimeUnit.SECONDS))
        done() // the reentrant callback arrives before cancellation schedules cleanup
      }
    }, {
      cancellationEntered.countDown()
      check(resumeCancellation.await(5, TimeUnit.SECONDS))
      token.completeAfterInvocation { events.add("cleanup") }
    })
    queue.submit({ done -> events.add("next"); done() }, {})
    val worker = thread { checkNotNull(tasks.poll()).invoke() }
    assertTrue(nativeEntered.await(5, TimeUnit.SECONDS))
    val canceller = thread { assertTrue(queue.cancel(id)) }
    try {
      assertTrue(cancellationEntered.await(5, TimeUnit.SECONDS))
      resumeNative.countDown()
      worker.join(5000)
      assertFalse(worker.isAlive)
      assertTrue("queue must stay occupied until cancellation cleanup is scheduled", tasks.isEmpty())
      resumeCancellation.countDown()
      canceller.join(5000)
      assertFalse(canceller.isAlive)
      checkNotNull(tasks.poll()).invoke()
      assertEquals(listOf("cleanup", "next"), events)
    } finally {
      resumeNative.countDown()
      resumeCancellation.countDown()
      worker.join(5000)
      canceller.join(5000)
    }
  }

  @Test fun cancelledInFlightDisableWithReentrantSuccessReportsBridgeUncertaintyAndRestores() {
    val tasks = ConcurrentLinkedQueue<() -> Unit>()
    val f = Fixture(post = { tasks.add(it); true })
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val graph = f.radio.withNativeGraph(f.peer) { Unit }.first
    val port = OwnedOrderedGroupPort(f.radio)
    val instance = CharacteristicInstance(f.peer, f.serviceId.toString(), 0, f.charId.toString(), 0)
    val opened = port.orderedOpen(42u, graph, mapOf(77uL to instance), 8, 64) { _, _ -> }
    val member = opened.members.single()
    port.orderedRetainClaim(opened.generation, 42u, member)
    val admission = port.orderedAdmission(opened.generation, 42u, member, instance)
    val enabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, true, "notification", 7, admission) { enabled.add(it) }
    checkNotNull(tasks.poll()).invoke()
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(enabled.single().isSuccess)
    checkNotNull(tasks.poll()).invoke()
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(3))

    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    doAnswer {
      entered.countDown()
      check(resume.await(5, TimeUnit.SECONDS))
      f.callback(Operation.DESCRIPTOR_WRITE)
      true
    }.`when`(f.gatt).writeDescriptor(f.descriptor)
    clearInvocations(f.gatt)
    val disabled = java.util.Collections.synchronizedList(mutableListOf<Result<Unit>>())
    val id = f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, false, null, 7, null) { disabled.add(it) }
    val worker = thread { checkNotNull(tasks.poll()).invoke() }
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      assertTrue(f.radio.cancelOperation(id))
      assertTrue(disabled.single().isFailure)
      verify(f.gatt, never()).setCharacteristicNotification(f.characteristic, true)
      resume.countDown()
      worker.join(5000)
      assertFalse(worker.isAlive)

      val batch = port.orderedDrain(opened.generation, 42u)
      assertTrue(batch.records.first() is OrderedPortRecord.Value)
      val terminal = batch.records.last() as OrderedPortRecord.Terminal
      assertEquals("source-failed", terminal.reason)
      assertEquals(member, terminal.failureMember)
      assertTrue(terminal.uncertainTail)
      val wire = OrderedGroupBridgeMapper.records(batch)
      assertEquals("source-failed", wire.records.last().reason)
      assertTrue(wire.records.last().uncertainTail)
      verify(f.gatt).setCharacteristicNotification(f.characteristic, true)

      // The late success must still trigger remote restoration after local rollback.
      f.configure(Operation.DESCRIPTOR_WRITE, false)
      checkNotNull(tasks.poll()).invoke()
      val order = inOrder(f.gatt)
      order.verify(f.gatt, times(2)).setCharacteristicNotification(f.characteristic, true)
      order.verify(f.gatt).writeDescriptor(f.descriptor)
      f.callback(Operation.DESCRIPTOR_WRITE)
      assertEquals(1, disabled.size)
    } finally {
      resume.countDown()
      worker.join(5000)
      f.radio.destroy()
    }
  }

  @Test fun reentrantDisableRetainsBothFailedRestorationObligations() {
    val tasks = ConcurrentLinkedQueue<() -> Unit>()
    val f = Fixture(post = { tasks.add(it); true })
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { }
    checkNotNull(tasks.poll()).invoke()
    f.callback(Operation.DESCRIPTOR_WRITE)
    checkNotNull(tasks.poll()).invoke()
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    doAnswer { entered.countDown(); check(resume.await(5, TimeUnit.SECONDS)); f.callback(Operation.DESCRIPTOR_WRITE); true }
      .`when`(f.gatt).writeDescriptor(f.descriptor)
    doReturn(false).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { }
    val worker = thread { checkNotNull(tasks.poll()).invoke() }
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      assertTrue(f.radio.cancelOperation(id))
      resume.countDown()
      worker.join(5000)
      assertFalse(worker.isAlive)
      doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
      f.configure(Operation.DESCRIPTOR_WRITE, false)
      checkNotNull(tasks.poll()).invoke() // remote restore starts
      f.radio.nativeGattCallback().onDescriptorWrite(f.gatt, f.descriptor, BluetoothGatt.GATT_FAILURE)
      f.scheduled.removeAt(0).invoke() // arbitrate failure
      clearInvocations(f.gatt)
      f.scheduled.removeAt(0).invoke() // autonomous retry
      checkNotNull(tasks.poll()).invoke()
      verify(f.gatt).writeDescriptor(f.descriptor)
      f.callback(Operation.DESCRIPTOR_WRITE) // remote obligation succeeds first
      doReturn(false).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
      clearInvocations(f.gatt)
      val blocked = mutableListOf<Result<Unit>>()
      f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { blocked.add(it) }
      checkNotNull(tasks.poll()).invoke()
      assertTrue("local cleanup must still fence after remote success", blocked.single().isFailure)
      verify(f.gatt).setCharacteristicNotification(f.characteristic, true)
      verify(f.gatt, never()).writeDescriptor(f.descriptor)
      doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
      val retry = mutableListOf<Result<Unit>>()
      f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { retry.add(it) }
      checkNotNull(tasks.poll()).invoke()
      f.callback(Operation.DESCRIPTOR_WRITE)
      assertTrue(retry.single().isSuccess)
    } finally {
      resume.countDown()
      worker.join(5000)
      f.radio.destroy()
    }
  }

  @Test fun pendingDescriptorWriteBlocksNotificationCccdBeforeLocalRegistration() {
    val f = Fixture()
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    f.configure(Operation.DESCRIPTOR_WRITE, true)
    val first = mutableListOf<Result<*>>(); f.start(Operation.DESCRIPTOR_WRITE, first)
    val notification = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { notification.add(it) }
    assertTrue(notification.single().isFailure)
    verify(f.gatt, never()).setCharacteristicNotification(f.characteristic, true)
    f.callback(Operation.DESCRIPTOR_WRITE)
    f.radio.destroy()
  }

  @Test fun cancelledDisableRestoresTheRetainedRemoteSubscription() = assertDisableRestored(false)

  @Test fun thrownDisableRestoresTheRetainedRemoteSubscription() = assertDisableRestored(true)

  @Test fun cancelledDisableWithoutCallbackRestoresLocalDeliveryAndReportsUncertainTail() =
    assertAbandonedDisableWithoutCallback(false)

  @Test fun thrownDisableWithoutCallbackRestoresLocalDeliveryAndReportsUncertainTail() =
    assertAbandonedDisableWithoutCallback(true)

  private fun assertAbandonedDisableWithoutCallback(throws: Boolean) {
      val f = Fixture()
      doReturn(0x10).`when`(f.characteristic).properties
      doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
      doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
      doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
      f.configure(Operation.DESCRIPTOR_WRITE, false)
      val failures = mutableListOf<NotificationOwnershipFifo.Stop>()
      val failureEpochs = mutableListOf<Long?>()
      val failurePaths = mutableListOf<OwnedAndroidNotificationPath?>()
      val values = mutableListOf<Int>()
      f.radio.onProtocolNotificationFailure = { _, _, _, epoch, path, reason ->
        failureEpochs.add(epoch); failurePaths.add(path); failures.add(reason)
      }
      f.radio.onOwnedProtocolNotification = { _, _, _, value -> values.add(value[0].toInt()) }
      f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true, sourceEpoch = 7) { }
      f.callback(Operation.DESCRIPTOR_WRITE)
      f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(1))
      if (throws) f.configure(Operation.DESCRIPTOR_WRITE, true)
      clearInvocations(f.gatt)
      val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { }
      if (!throws) assertTrue(f.radio.cancelOperation(id))
      verify(f.gatt).setCharacteristicNotification(f.characteristic, true)
      f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(2))
      assertTrue("a restored callback must be delivered or the owner terminated", values == listOf(1, 2) || failures.isNotEmpty())
      assertTrue("unconfirmed disable must report one uncertain tail", failures.size == 1 && failures[0] is NotificationOwnershipFifo.Stop.DeliveryUncertain)
      assertEquals(listOf(7L), failureEpochs)
      assertEquals(OwnedAndroidNotificationPath(f.serviceId, 0, f.charId, 0), failurePaths.single())
      f.radio.destroy()
  }

  @Test fun failedLocalRestoreReportsTheRetainedOwnerWithoutAnotherRequest() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val failures = mutableListOf<NotificationOwnershipFifo.Stop>()
    f.radio.onProtocolNotificationFailure = { _, _, _, _, _, reason -> failures.add(reason) }
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true, sourceEpoch = 7) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    doReturn(false).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { }
    assertTrue(f.radio.cancelOperation(id))
    assertTrue("failed local recovery must reach the exact retained owner", failures.size == 1 && failures[0] is NotificationOwnershipFifo.Stop.DeliveryUncertain)
    f.radio.destroy()
  }

  @Test fun abandonedOrderedDisableTerminatesAffectedGroupWithUncertainTail() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val path = OwnedAndroidNotificationPath(f.serviceId, 0, f.charId, 0)
    val graph = f.radio.withNativeGraph(f.peer) { Unit }.first
    val opened = f.radio.openOrderedSourceForPeer(f.peer, setOf(path), 8, 64) { }
    val member = opened.members.getValue(path)
    opened.source.retainPhysicalClaim(opened.group, member.first, member.second)
    val admission = AndroidOrderedAdmission(graph, opened.source, opened.group, member.first, member.second)
    f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, true, "notification", 7, admission) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    val id = f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, false, null, 7, null) { }
    assertTrue(f.radio.cancelOperation(id))
    try {
      val terminal = opened.source.drain(opened.group).filterIsInstance<OrderedNotificationSource.Event.Terminal<*>>().singleOrNull()
      assertTrue("ordered owner must receive an uncertain terminal without a descriptor callback", terminal?.uncertainTail == true)
    } finally { f.radio.destroy() }
  }

  @Test fun cancelledDisableThenFailedCallbackRetainsBothCausesAcrossBatchAcknowledgment() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val graph = f.radio.withNativeGraph(f.peer) { Unit }.first
    val port = OwnedOrderedGroupPort(f.radio)
    val instance = CharacteristicInstance(f.peer, f.serviceId.toString(), 0, f.charId.toString(), 0)
    val opened = port.orderedOpen(42u, graph, mapOf(77uL to instance), 8, 64) { _, _ -> }
    val source = f.radio.openOrderedSourceForPeer(f.peer,
      setOf(OwnedAndroidNotificationPath(f.serviceId, 0, f.charId, 0)), 8, 64) { }.source
    val targets = source.activeMemberships(f.characteristic)
    val member = opened.members.single()
    port.orderedRetainClaim(opened.generation, 42u, member)
    val admission = port.orderedAdmission(opened.generation, 42u, member, instance)
    f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, true, "notification", 7, admission) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(3))
    val id = f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, false, null, 7, null) { }
    assertTrue(f.radio.cancelOperation(id))
    val first = port.orderedDrain(opened.generation, 42u)
    assertTrue(first.records.first() is OrderedPortRecord.Value)
    val terminal = first.records.last() as OrderedPortRecord.Terminal
    assertEquals("notification delivery may have been interrupted by an abandoned disable", terminal.failure?.detail)
    f.radio.nativeGattCallback().onDescriptorWrite(f.gatt, f.descriptor, BluetoothGatt.GATT_FAILURE)
    f.scheduled.removeAt(0).invoke()
    assertEquals(first, port.orderedDrain(opened.generation, 42u))
    port.orderedAcknowledge(opened.generation, 42u, checkNotNull(first.id))
    val laterBatch = port.orderedDrain(opened.generation, 42u)
    val later = OrderedGroupBridgeMapper.records(laterBatch).records.single()
    assertEquals("subsequent-source-failure", later.kind)
    assertEquals("gatt-status", later.failureKind)
    assertEquals(BluetoothGatt.GATT_FAILURE, later.gattStatus)
    assertEquals("Android notification source GATT failure", later.detail)
    port.orderedAcknowledge(opened.generation, 42u, checkNotNull(laterBatch.id))
    source.failCurrentMembers(f.characteristic, targets, IllegalStateException("another failure"))
    val thirdBatch = port.orderedDrain(opened.generation, 42u)
    val third = OrderedGroupBridgeMapper.records(thirdBatch).records.single()
    assertEquals("subsequent-source-failure", third.kind)
    assertEquals("platform", third.failureKind)
    assertEquals("Android notification source failure", third.detail)
    assertEquals(thirdBatch, port.orderedDrain(opened.generation, 42u))
    port.orderedAcknowledge(opened.generation, 42u, checkNotNull(thirdBatch.id))
    repeat(9) { source.failCurrentMembers(f.characteristic, targets, IllegalStateException("overflow $it")) }
    val overflowBatch = port.orderedDrain(opened.generation, 42u)
    val overflow = OrderedGroupBridgeMapper.records(overflowBatch).records
    assertEquals(9, overflow.size)
    assertTrue(overflow.take(8).all { it.kind == "subsequent-source-failure" })
    assertEquals("failure-observations-omitted", overflow.last().kind)
    assertEquals(overflowBatch, port.orderedDrain(opened.generation, 42u))
    port.orderedSeal(opened.generation, 42u)
    port.orderedConfirmCleanup(opened.generation, 42u, member)
    try { port.orderedRemove(opened.generation, 42u); fail("unacknowledged failure debt must prevent removal") }
    catch (_: IllegalStateException) { }
    f.radio.destroy()
  }

  @Test fun abandonedDisableFencesOnlyCapturedCurrentMembershipsAndPreservesFifo() {
    val f = Fixture()
    val other = mock(BluetoothGattCharacteristic::class.java)
    val otherId = UUID.randomUUID()
    doReturn(otherId).`when`(other).uuid
    doReturn(f.service).`when`(other).service
    doReturn(listOf(f.characteristic, other)).`when`(f.service).characteristics
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val path = OwnedAndroidNotificationPath(f.serviceId, 0, f.charId, 0)
    val otherPath = OwnedAndroidNotificationPath(f.serviceId, 0, otherId, 0)
    val graph = f.radio.withNativeGraph(f.peer) { Unit }.first
    val wakesUnderLock = mutableListOf<Boolean>()
    fun open(p: OwnedAndroidNotificationPath) = f.radio.openOrderedSourceForPeer(f.peer, setOf(p), 16, 64) {
      wakesUnderLock.add(f.radio.admissionLocksHeldByCurrentThread())
    }
    val first = open(path); val second = open(path); val stale = open(path); val unrelated = open(otherPath)
    fun claim(opened: OrderedAndroidSourceOpen) = opened.members.getValue(path).also {
      opened.source.retainPhysicalClaim(opened.group, it.first, it.second)
    }
    val a = claim(first); val b = claim(second); val old = claim(stale)
    val admission = AndroidOrderedAdmission(graph, first.source, first.group, a.first, a.second)
    f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, true, "notification", 7, admission) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(1))
    val id = f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, false, null, 7, null) { }
    assertTrue(stale.source.retire(stale.group, old.first, old.second))
    val replacement = stale.source.register(stale.group, old.first, old.second)
    stale.source.retainPhysicalClaim(stale.group, old.first, replacement)
    doAnswer {
      f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(2))
      true
    }.`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    assertTrue(f.radio.cancelOperation(id))
    for ((opened, captured) in listOf(first to a, second to b)) {
      val events = opened.source.drain(opened.group)
      assertEquals(2, events.size)
      assertTrue(events.first() is OrderedNotificationSource.Event.Value)
      val terminal = events.last() as OrderedNotificationSource.Event.Terminal
      assertEquals(OrderedNotificationSource.Reason.SOURCE_FAILED, terminal.reason)
      assertTrue(terminal.uncertainTail)
      assertEquals(opened.source.sourceGeneration, terminal.sourceFailure?.sourceGeneration)
      assertEquals(captured.second, terminal.sourceFailure?.membership)
      assertSame(captured.first, terminal.sourceFailure?.member)
      assertTrue(opened.group.cleanupPending.isNotEmpty())
    }
    val staleEvents = stale.source.drain(stale.group)
    assertTrue(staleEvents.none { it is OrderedNotificationSource.Event.Terminal })
    assertTrue(staleEvents.filterIsInstance<OrderedNotificationSource.Event.Value<*>>().any { it.epoch == replacement.epoch })
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, other, byteArrayOf(9))
    assertTrue(unrelated.source.drain(unrelated.group).any { it is OrderedNotificationSource.Event.Value })
    assertFalse(wakesUnderLock.any { it })
    f.radio.destroy()
  }

  @Test fun orderedFailureWakeAdmissionOverflowIsReportedAndTerminalRemainsDrainable() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val path = OwnedAndroidNotificationPath(f.serviceId, 0, f.charId, 0)
    val graph = f.radio.withNativeGraph(f.peer) { Unit }.first
    val wakeLocks = mutableListOf<Boolean>(); val overflows = mutableListOf<Long>()
    f.radio.onAdmissionOverflow = { _, _, controls -> overflows.add(controls) }
    val opened = f.radio.openOrderedSourceForPeer(f.peer, setOf(path), 8, 64) {
      wakeLocks.add(f.radio.admissionLocksHeldByCurrentThread())
    }
    val member = opened.members.getValue(path)
    opened.source.retainPhysicalClaim(opened.group, member.first, member.second)
    val admission = AndroidOrderedAdmission(graph, opened.source, opened.group, member.first, member.second)
    f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, true, "notification", 7, admission) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    val id = f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, false, null, 7, null) { }
    f.radio.admissionWakeCapacityForTest = 0
    assertTrue(f.radio.cancelOperation(id))
    val terminal = opened.source.drain(opened.group).filterIsInstance<OrderedNotificationSource.Event.Terminal<*>>().single()
    assertEquals(OrderedNotificationSource.Reason.SOURCE_FAILED, terminal.reason)
    assertNotNull(terminal.rejectedLoss)
    assertEquals(listOf(1L), overflows)
    assertEquals(listOf(false), wakeLocks)
    assertTrue(opened.group.cleanupPending.isNotEmpty())
    f.radio.destroy()
  }

  @Test fun orderedFailureWakeExceptionIsReportedWithoutLosingSourceTerminal() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val path = OwnedAndroidNotificationPath(f.serviceId, 0, f.charId, 0)
    val graph = f.radio.withNativeGraph(f.peer) { Unit }.first
    val reported = mutableListOf<String>(); val signaled = mutableListOf<Throwable>()
    f.radio.onCleanupFailure = { reported.add(it.operation) }
    val opened = f.radio.openOrderedSourceForPeer(f.peer, setOf(path), 8, 64,
      wakeFailureId = 44uL, onWakeFailure = { _, _, error -> signaled.add(error) }) {
      assertFalse(f.radio.admissionLocksHeldByCurrentThread())
      throw IllegalStateException("wake failed")
    }
    val member = opened.members.getValue(path)
    opened.source.retainPhysicalClaim(opened.group, member.first, member.second)
    val admission = AndroidOrderedAdmission(graph, opened.source, opened.group, member.first, member.second)
    f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, true, "notification", 7, admission) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    val id = f.radio.setNotifyExactRetained(f.peer, f.serviceId, 0, f.charId, 0, false, null, 7, null) { }
    assertTrue(f.radio.cancelOperation(id))
    val terminal = opened.source.drain(opened.group).filterIsInstance<OrderedNotificationSource.Event.Terminal<*>>().single()
    assertEquals(OrderedNotificationSource.Reason.SOURCE_FAILED, terminal.reason)
    assertTrue(terminal.wakeFailed)
    assertEquals("wake failed", signaled.single().message)
    assertTrue(reported.contains("orderedSource.failureWake"))
    assertTrue(opened.group.cleanupPending.isNotEmpty())
    f.radio.destroy()
  }

  @Test fun failedRemoteRestoreRetriesWithoutAnotherClientOperation() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { }
    assertTrue(f.radio.cancelOperation(id))
    f.callback(Operation.DESCRIPTOR_WRITE)
    f.radio.nativeGattCallback().onDescriptorWrite(f.gatt, f.descriptor, BluetoothGatt.GATT_FAILURE)
    f.scheduled.last().invoke()
    clearInvocations(f.gatt)
    f.scheduled.last().invoke()
    verify(f.gatt).writeDescriptor(f.descriptor)
    f.radio.destroy()
  }

  @Test fun autonomousRemoteRestoreRetriesAreBoundedAndRemainRetained() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { }
    assertTrue(f.radio.cancelOperation(id))
    f.callback(Operation.DESCRIPTOR_WRITE)
    clearInvocations(f.gatt)
    repeat(3) { index ->
      f.radio.nativeGattCallback().onDescriptorWrite(f.gatt, f.descriptor, BluetoothGatt.GATT_FAILURE)
      f.scheduled.removeAt(0).invoke() // terminal arbitration
      if (index < 2) f.scheduled.removeAt(0).invoke() // autonomous retry
    }
    assertTrue("automatic retries must stop", f.scheduled.isEmpty())
    verify(f.gatt, times(2)).writeDescriptor(f.descriptor)
    assertNotNull("unresolved restoration remains owned by teardown", f.radio.disconnect(f.peer))
    f.radio.destroy()
  }

  @Test fun admittedNotificationCannotRegisterAfterGattRetirement() {
    for (retirement in listOf("disconnect", "adapter", "destroy")) assertAdmittedDisableFenced(retirement)
  }

  private fun assertAdmittedDisableFenced(retirement: String) {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { }
    f.callback(Operation.DESCRIPTOR_WRITE)
    clearInvocations(f.gatt)
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    doAnswer { entered.countDown(); check(resume.await(5, TimeUnit.SECONDS)); f.descriptor }
      .`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    val results = mutableListOf<Result<Unit>>()
    val worker = thread { f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { results.add(it) } }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    when (retirement) {
      "disconnect" -> {
        f.radio.disconnect(f.peer)
        f.radio.nativeGattCallback().onConnectionStateChange(f.gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
      }
      "adapter" -> {
        var receiver: BroadcastReceiver? = null
        doAnswer { call -> receiver = call.getArgument(0); null }
          .`when`(f.context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
        f.radio.registerAdapterStateReceiver()
        val intent = mock(Intent::class.java)
        doReturn(BluetoothAdapter.ACTION_STATE_CHANGED).`when`(intent).action
        doReturn(BluetoothAdapter.STATE_OFF).`when`(intent).getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)
        checkNotNull(receiver).onReceive(f.context, intent)
      }
      else -> f.radio.destroy()
    }
    resume.countDown()
    worker.join(5000)
    assertFalse(worker.isAlive)
    verify(f.gatt, never()).setCharacteristicNotification(f.characteristic, false)
    f.radio.destroy()
  }

  @Test fun retirementWaitsForLocalRegistrationAlreadyInProgress() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val retiring = CountDownLatch(1)
    doAnswer { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)); true }
      .`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    val worker = thread { f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { } }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    val teardown = thread { retiring.countDown(); f.radio.destroy() }
    assertTrue(retiring.await(5, TimeUnit.SECONDS))
    try {
      teardown.join(100)
      assertTrue("retirement must wait for the in-progress native call", teardown.isAlive)
      verify(f.gatt, never()).close()
    } finally {
      release.countDown()
      worker.join(5000)
      teardown.join(5000)
    }
    assertFalse(worker.isAlive)
    assertFalse(teardown.isAlive)
    verify(f.gatt).close()
  }

  @Test fun failedRestorationStaysOwnedAndIsRetriedBeforeAnotherDisable() {
    val f = Fixture()
    doReturn(0x30).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val cleanupFailures = mutableListOf<OwnedRadioTeardownFailure>()
    f.radio.onCleanupFailure = { cleanupFailures.add(it) }
    val enabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true, "indication") { enabled.add(it) }
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(enabled.single().isSuccess)
    val disabled = mutableListOf<Result<Unit>>()
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { disabled.add(it) }
    assertTrue(f.radio.cancelOperation(id))
    assertTrue(disabled.single().isFailure)
    f.callback(Operation.DESCRIPTOR_WRITE) // late successful disable starts restoration
    f.radio.nativeGattCallback().onDescriptorWrite(f.gatt, f.descriptor, BluetoothGatt.GATT_FAILURE)
    // Ordinary CCCD failures wait briefly for same-generation disconnect evidence.
    f.scheduled.lastOrNull()?.invoke()
    assertTrue("failed restoration must be reported", cleanupFailures.isNotEmpty())
    clearInvocations(f.gatt, f.descriptor)
    val blocked = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { blocked.add(it) }
    assertTrue("restoration must remain pending", blocked.single().isFailure)
    verify(f.gatt).writeDescriptor(f.descriptor)
    verify(f.descriptor).setValue(ANDROID_CCCD_ENABLE_INDICATION)
    f.callback(Operation.DESCRIPTOR_WRITE)
    val retry = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { retry.add(it) }
    assertTrue(retry.isEmpty())
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(retry.single().isSuccess)
    assertEquals(1, disabled.size)
    f.radio.destroy()
  }

  @Test fun failedCloseKeepsCallbackFenceUntilCloseRetrySucceeds() {
    val f = Fixture()
    f.configure(Operation.READ, false)
    val first = mutableListOf<Result<*>>()
    f.start(Operation.READ, first)
    doThrow(IllegalStateException("close failed")).doNothing().`when`(f.gatt).close()
    f.radio.disconnect(f.peer)
    f.radio.nativeGattCallback().onConnectionStateChange(f.gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    assertTrue(first.single().isFailure)
    val blocked = mutableListOf<Result<*>>()
    f.start(Operation.READ, blocked)
    f.callback(Operation.READ)
    assertTrue("failed close cannot admit a replacement", blocked.none { it.isSuccess })
    f.radio.disconnect(f.peer)
    val old = f.gatt
    f.attach(mock(BluetoothGatt::class.java))
    f.configure(Operation.READ, false)
    val next = mutableListOf<Result<*>>()
    f.start(Operation.READ, next)
    f.callback(Operation.READ, old)
    assertTrue(next.isEmpty())
    f.callback(Operation.READ)
    assertTrue(next.single().isSuccess)
    f.radio.destroy()
  }

  @Test fun thrownRestorationKeepsDescriptorDebtUntilItsLateSuccess() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val failures = mutableListOf<OwnedRadioTeardownFailure>()
    f.radio.onCleanupFailure = { failures.add(it) }
    val enabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { enabled.add(it) }
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(enabled.single().isSuccess)
    val disabled = mutableListOf<Result<Unit>>()
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { disabled.add(it) }
    assertTrue(f.radio.cancelOperation(id))
    f.configure(Operation.DESCRIPTOR_WRITE, true)
    f.callback(Operation.DESCRIPTOR_WRITE) // late disable success; restoration throws ambiguously
    assertTrue(failures.isNotEmpty())
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    clearInvocations(f.gatt)
    val blocked = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { blocked.add(it) }
    assertTrue(blocked.single().isFailure)
    verify(f.gatt, never()).writeDescriptor(f.descriptor)
    f.callback(Operation.DESCRIPTOR_WRITE) // the original restoration's late success
    val retry = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { retry.add(it) }
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(retry.single().isSuccess)
    assertEquals(1, disabled.size)
    f.radio.destroy()
  }

  @Test fun thrownLocalDisableKeepsFailedRollbackRetryable() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val cleanup = mutableListOf<OwnedRadioTeardownFailure>()
    f.radio.onCleanupFailure = { cleanup.add(it) }
    val enabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { enabled.add(it) }
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(enabled.single().isSuccess)
    doThrow(IllegalStateException("ambiguous local disable")).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    doReturn(false).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    val disabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { disabled.add(it) }
    assertTrue(disabled.single().isFailure)
    assertTrue(cleanup.isNotEmpty())
    clearInvocations(f.gatt)
    val blocked = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { blocked.add(it) }
    assertTrue(blocked.single().isFailure)
    verify(f.gatt, never()).setCharacteristicNotification(f.characteristic, false)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    val retry = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { retry.add(it) }
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(retry.single().isSuccess)
    f.radio.destroy()
  }

  @Test fun cancellationDuringLocalDisableRestoresRegistrationBeforeReturningQueue() {
    val posted = mutableListOf<() -> Unit>()
    val f = Fixture { action -> posted.add(action); true }
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val enabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { enabled.add(it) }
    posted.removeAt(0).invoke()
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(enabled.single().isSuccess)
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    doAnswer { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)); true }
      .`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    clearInvocations(f.gatt)
    val disabled = mutableListOf<Result<Unit>>()
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { disabled.add(it) }
    val worker = thread { posted.removeAt(0).invoke() }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    assertTrue(f.radio.cancelOperation(id))
    release.countDown()
    worker.join(5000)
    assertFalse(worker.isAlive)
    assertTrue(disabled.single().isFailure)
    verify(f.gatt).setCharacteristicNotification(f.characteristic, true)
    verify(f.gatt, never()).writeDescriptor(f.descriptor)
    f.radio.destroy()
  }

  @Test fun failedRestorationRemainsReportedAcrossFailedCloseAndTeardownRetry() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val enabled = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { enabled.add(it) }
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(enabled.single().isSuccess)
    val disabled = mutableListOf<Result<Unit>>()
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { disabled.add(it) }
    assertTrue(f.radio.cancelOperation(id))
    f.callback(Operation.DESCRIPTOR_WRITE)
    f.radio.nativeGattCallback().onDescriptorWrite(f.gatt, f.descriptor, BluetoothGatt.GATT_FAILURE)
    f.scheduled.last().invoke()
    doThrow(IllegalStateException("close failed")).doNothing().`when`(f.gatt).close()
    assertNotNull("unresolved restoration must reach teardown", f.radio.disconnect(f.peer))
    f.radio.nativeGattCallback().onConnectionStateChange(f.gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    val blocked = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { blocked.add(it) }
    assertTrue(blocked.single().isFailure)
    assertNotNull("cleanup stays owned while close is retried", f.radio.disconnect(f.peer))
    verify(f.gatt, times(2)).close()
    assertTrue("closed attachment releases its retained cleanup", f.radio.destroy().isSuccessful)
  }

  private fun assertDisableRestored(throws: Boolean) {
      val f = Fixture()
      doReturn(0x30).`when`(f.characteristic).properties
      doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
      doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
      doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
      f.configure(Operation.DESCRIPTOR_WRITE, false)
      val enabled = mutableListOf<Result<Unit>>()
      f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true, "indication") { enabled.add(it) }
      f.callback(Operation.DESCRIPTOR_WRITE)
      assertTrue(enabled.single().isSuccess)
      if (throws) f.configure(Operation.DESCRIPTOR_WRITE, true)
      val disabled = mutableListOf<Result<Unit>>()
      val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, false) { disabled.add(it) }
      if (!throws) assertTrue(f.radio.cancelOperation(id))
      assertTrue(disabled.single().isFailure)
      f.configure(Operation.DESCRIPTOR_WRITE, false)
      clearInvocations(f.gatt, f.descriptor)
      f.callback(Operation.DESCRIPTOR_WRITE)
      // The successful late disable must be followed by a remote enable using
      // the original indication mode, not just a local Android registration.
      verify(f.gatt).writeDescriptor(f.descriptor)
      verify(f.descriptor).setValue(ANDROID_CCCD_ENABLE_INDICATION)
      f.callback(Operation.DESCRIPTOR_WRITE)
      assertEquals(1, disabled.size)
      f.radio.destroy()
  }

  @Test fun disconnectFencesReentrantReplacementBeforeRetiringOldOwner() = assertClosingFenced(false)

  @Test fun reconnectFencesReentrantReplacementBeforeRetiringOldOwner() = assertClosingFenced(true)

  @Test fun adapterLossFencesReentrantReplacementBeforeRetiringOldOwner() {
    val f = Fixture()
    var receiver: BroadcastReceiver? = null
    doAnswer { call -> receiver = call.getArgument(0); null }
      .`when`(f.context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    f.radio.registerAdapterStateReceiver()
    f.configure(Operation.READ, false)
    val replacement = mutableListOf<Result<*>>()
    val first = mutableListOf<Result<*>>()
    f.radio.readCharacteristicExact(f.peer, f.serviceId, 0, f.charId, 0) {
      first.add(it)
      f.callback(Operation.READ)
      f.start(Operation.READ, replacement)
      f.callback(Operation.READ)
    }
    val intent = mock(Intent::class.java)
    doReturn(BluetoothAdapter.ACTION_STATE_CHANGED).`when`(intent).action
    doReturn(BluetoothAdapter.STATE_OFF).`when`(intent).getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)
    checkNotNull(receiver).onReceive(f.context, intent)
    assertTrue(first.single().isFailure)
    assertTrue("adapter loss must fence the retiring attachment", replacement.none { it.isSuccess })
    f.radio.destroy()
  }

  private fun assertClosingFenced(reconnect: Boolean) {
      val f = Fixture()
      val newGatt = mock(BluetoothGatt::class.java)
      doReturn(f.device).`when`(f.adapter).getRemoteDevice(f.peer)
      doReturn(newGatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
      f.configure(Operation.READ, false)
      val replacement = mutableListOf<Result<*>>()
      val first = mutableListOf<Result<*>>()
      f.radio.readCharacteristicExact(f.peer, f.serviceId, 0, f.charId, 0) {
        first.add(it)
        f.callback(Operation.READ)
        f.start(Operation.READ, replacement)
        f.callback(Operation.READ)
      }
      if (reconnect) f.radio.connect(f.peer, false) else f.radio.disconnect(f.peer)
      assertTrue(first.single().isFailure)
      assertTrue("replacement must not succeed on a closing generation", replacement.none { it.isSuccess })
      val old = f.gatt
      f.radio.nativeGattCallback().onConnectionStateChange(old, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
      f.attach(newGatt)
      f.configure(Operation.READ, false)
      val next = mutableListOf<Result<*>>()
      f.start(Operation.READ, next)
      f.callback(Operation.READ, old)
      assertTrue(next.isEmpty())
      f.callback(Operation.READ)
      assertTrue(next.single().isSuccess)
      f.radio.destroy()
  }

  @Test fun cancelledCccdReleasesQueueAndKeepsDescriptorFenceUntilCallback() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val notifications = mutableListOf<Result<Unit>>()
    val id = f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { notifications.add(it) }
    assertTrue(f.radio.cancelOperation(id))
    assertTrue(notifications.single().isFailure)
    doReturn(true).`when`(f.gatt).requestMtu(100)
    val mtu = mutableListOf<Result<Int>>()
    f.radio.requestMtu(f.peer, 100) { mtu.add(it) }
    verify(f.gatt).requestMtu(100)
    f.radio.nativeGattCallback().onMtuChanged(f.gatt, 100, BluetoothGatt.GATT_SUCCESS)
    assertEquals(100, mtu.single().getOrThrow())
    val blocked = mutableListOf<Result<*>>()
    f.start(Operation.DESCRIPTOR_WRITE, blocked)
    assertTrue(blocked.single().isFailure)
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertEquals(1, notifications.size)
    f.radio.destroy()
  }

  @Test fun ambiguousCccdWriteFencesDirectDescriptorWriteUntilCallback() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, true)
    val first = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { first.add(it) }
    assertTrue(first.single().isFailure)
    val blocked = mutableListOf<Result<*>>()
    f.start(Operation.DESCRIPTOR_WRITE, blocked)
    assertTrue(blocked.single().isFailure)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    f.callback(Operation.DESCRIPTOR_WRITE) // late enable; compensation writes a disable
    f.callback(Operation.DESCRIPTOR_WRITE) // compensation completes
    val next = mutableListOf<Result<*>>()
    f.start(Operation.DESCRIPTOR_WRITE, next)
    assertTrue(next.isEmpty())
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(next.single().isSuccess)
    assertEquals(1, first.size)
    f.radio.destroy()
  }

  @Test fun clearSettlesActiveQueueCellBeforeItsOwnerIsRegistered() {
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    val failures = mutableListOf<Throwable>()
    val queue = OwnedAndroidGattRadio.GattSerialQueue(post = { task -> thread { task() }; true })
    queue.submitCancellable({ token, done ->
      entered.countDown()
      check(resume.await(5, TimeUnit.SECONDS))
      if (!token.invokePhysical(done) { fail("cancelled cell reached platform") }) done()
    }, { failures.add(IllegalStateException("cancelled")) }, { failures.add(it) }, settleOnClearBeforeOwner = true)
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    queue.clear(IllegalStateException("database changed"))
    resume.countDown()
    assertEquals("database changed", failures.single().message)
  }

  @Test fun serviceChangeRetainsCccdFenceAgainstDirectDescriptorWrite() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    f.configure(Operation.DESCRIPTOR_WRITE, false)
    val first = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { first.add(it) }
    f.radio.nativeGattCallback().onServiceChanged(f.gatt)
    assertTrue(first.single().isFailure)
    doReturn(true).`when`(f.gatt).discoverServices()
    f.radio.discover(f.peer) { assertTrue(it.isSuccess) }
    f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
    val blocked = mutableListOf<Result<*>>()
    f.start(Operation.DESCRIPTOR_WRITE, blocked)
    assertTrue(blocked.single().isFailure)
    f.callback(Operation.DESCRIPTOR_WRITE)
    val next = mutableListOf<Result<*>>()
    f.start(Operation.DESCRIPTOR_WRITE, next)
    assertTrue(next.isEmpty())
    f.callback(Operation.DESCRIPTOR_WRITE)
    assertTrue(next.single().isSuccess)
    f.radio.destroy()
  }

  @Test fun admissionOverflowPreservesExistingCallbackDebt() {
    val f = Fixture()
    f.configure(Operation.WRITE, true)
    val first = mutableListOf<Result<*>>()
    f.start(Operation.WRITE, first)
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(true).`when`(f.gatt).setCharacteristicNotification(f.characteristic, false)
    doAnswer {
      repeat(70) { f.radio.nativeGattCallback().onMtuChanged(f.gatt, 23, BluetoothGatt.GATT_SUCCESS) }
      true
    }.`when`(f.gatt).setCharacteristicNotification(f.characteristic, true)
    val notify = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { notify.add(it) }
    val blocked = mutableListOf<Result<*>>()
    f.start(Operation.WRITE, blocked)
    assertTrue(blocked.single().isFailure)
    f.callback(Operation.WRITE)
    f.configure(Operation.WRITE, false)
    val next = mutableListOf<Result<*>>()
    f.start(Operation.WRITE, next)
    assertTrue(next.isEmpty())
    f.callback(Operation.WRITE)
    assertTrue(next.single().isSuccess)
    f.radio.destroy()
  }

  @Test fun callbackDebtIsBoundedAndCapacityReturnsWhenOldCallbackArrives() {
    val f = Fixture()
    f.radio.callbackOwnerCapacityForTest = 2
    val attributes = mutableListOf<BluetoothGattCharacteristic>()
    repeat(3) { index ->
      val ch = mock(BluetoothGattCharacteristic::class.java)
      attributes.add(ch)
      doReturn(f.charId).`when`(ch).uuid
      doReturn(f.service).`when`(ch).service
      doReturn(listOf(ch)).`when`(f.service).characteristics
      doAnswer { throw IllegalStateException("ambiguous $index") }.`when`(f.gatt).readCharacteristic(ch)
      val result = mutableListOf<Result<*>>()
      f.start(Operation.READ, result)
      assertTrue(result.single().isFailure)
      if (index == 2) verify(f.gatt, never()).readCharacteristic(ch)
      if (index < 2) {
        f.radio.nativeGattCallback().onServiceChanged(f.gatt)
        doReturn(true).`when`(f.gatt).discoverServices()
        f.radio.discover(f.peer) { assertTrue(it.isSuccess) }
        f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
      }
    }
    f.radio.nativeGattCallback().onCharacteristicRead(f.gatt, attributes[0], byteArrayOf(1), BluetoothGatt.GATT_SUCCESS)
    doReturn(true).`when`(f.gatt).readCharacteristic(attributes[2])
    val next = mutableListOf<Result<*>>()
    f.start(Operation.READ, next)
    assertTrue(next.isEmpty())
    f.radio.nativeGattCallback().onCharacteristicRead(f.gatt, attributes[2], byteArrayOf(2), BluetoothGatt.GATT_SUCCESS)
    assertTrue(next.single().isSuccess)
    f.radio.destroy()
  }

  @Test fun serviceChangeSettlesExactReadPausedBeforeOwnerRegistration() {
    val f = Fixture()
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    doAnswer {
      entered.countDown()
      check(resume.await(5, TimeUnit.SECONDS))
      listOf(f.characteristic)
    }.`when`(f.service).characteristics
    val results = java.util.Collections.synchronizedList(mutableListOf<Result<*>>())
    val worker = thread { f.start(Operation.READ, results) }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    f.radio.nativeGattCallback().onServiceChanged(f.gatt)
    resume.countDown()
    worker.join(5000)
    assertFalse(worker.isAlive)
    assertEquals(1, results.size)
    assertTrue(results.single().isFailure)
    verify(f.gatt, never()).readCharacteristic(f.characteristic)
    f.radio.destroy()
  }

  @Test fun serviceChangeRetainsDescriptorFenceAgainstCccdEnable() {
    val f = Fixture()
    doReturn(0x10).`when`(f.characteristic).properties
    doReturn(f.descriptor).`when`(f.characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    f.configure(Operation.DESCRIPTOR_WRITE, true)
    val first = mutableListOf<Result<*>>()
    f.start(Operation.DESCRIPTOR_WRITE, first)
    f.radio.nativeGattCallback().onServiceChanged(f.gatt)
    doReturn(true).`when`(f.gatt).discoverServices()
    f.radio.discover(f.peer) { assertTrue(it.isSuccess) }
    f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
    val blocked = mutableListOf<Result<Unit>>()
    f.radio.setNotifyExact(f.peer, f.serviceId, 0, f.charId, 0, true) { blocked.add(it) }
    assertTrue(blocked.single().isFailure)
    verify(f.gatt, never()).setCharacteristicNotification(f.characteristic, true)
    f.callback(Operation.DESCRIPTOR_WRITE)
    f.radio.destroy()
  }

}
