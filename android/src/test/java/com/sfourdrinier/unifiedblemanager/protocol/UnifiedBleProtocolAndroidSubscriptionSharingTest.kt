// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidSubscriptionSharingTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.content.Context
import com.sfourdrinier.unifiedblemanager.protocol.generated.RecordKind
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import java.util.UUID
import org.junit.Assert.assertEquals
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito

class UnifiedBleProtocolAndroidSubscriptionSharingTest {
  @Test
  fun pendingOwnersShareOneCccdAndOnlyFinalReleaseDisablesIt() {
    val context = Mockito.mock(Context::class.java)
    Mockito.`when`(context.applicationContext).thenReturn(context)
    val emitted = mutableListOf<ByteArray>()
    val jsi = Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java)
    JsiStaticStubs.stubEmitRecord(jsi, emitted)
    JsiStaticStubs.stubEmitDiagnostic(jsi, mutableListOf())
    val dispatcher = UnifiedBleProtocolAndroidDispatcher(context, 123L, { _, _ -> null }, java.util.concurrent.Executor { it.run() })
    val delayed = mutableListOf<() -> Unit>()
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, action -> delayed.add(action); true })
    val radioField = dispatcher.javaClass.getDeclaredField("radio").also { it.isAccessible = true }
    val original = radioField.get(dispatcher)
    val originalRadio = original as OwnedAndroidGattRadio
    radio.onProtocolNotification = originalRadio.onProtocolNotification
    radio.onProtocolNotificationLoss = originalRadio.onProtocolNotificationLoss
    radio.onProtocolNotificationFailure = originalRadio.onProtocolNotificationFailure
    radioField.set(dispatcher, radio)
    try {
      val peer = "AA:BB:CC:DD:EE:FF"
      val serviceUuid = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
      val charUuid = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
      val device = Mockito.mock(BluetoothDevice::class.java)
      val gatt = Mockito.mock(BluetoothGatt::class.java)
      val service = Mockito.mock(BluetoothGattService::class.java)
      val characteristic = Mockito.mock(BluetoothGattCharacteristic::class.java)
      val cccd = Mockito.mock(BluetoothGattDescriptor::class.java)
      Mockito.`when`(device.address).thenReturn(peer)
      Mockito.`when`(gatt.device).thenReturn(device)
      Mockito.`when`(service.uuid).thenReturn(serviceUuid)
      Mockito.`when`(service.characteristics).thenReturn(listOf(characteristic))
      Mockito.`when`(characteristic.uuid).thenReturn(charUuid)
      Mockito.`when`(characteristic.properties).thenReturn(0x30)
      Mockito.`when`(characteristic.service).thenReturn(service)
      Mockito.`when`(characteristic.getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)).thenReturn(cccd)
      Mockito.`when`(cccd.uuid).thenReturn(OwnedAndroidGattRadio.CCCD_UUID)
      Mockito.`when`(cccd.characteristic).thenReturn(characteristic)
      Mockito.`when`(gatt.setCharacteristicNotification(eq(characteristic), Mockito.anyBoolean())).thenReturn(true)
      Mockito.`when`(gatt.writeDescriptor(cccd)).thenReturn(true)
      Mockito.`when`(gatt.writeDescriptor(eq(cccd), any(ByteArray::class.java))).thenReturn(BluetoothGatt.GATT_SUCCESS)
      val generation = radio.attachConnectedGatt(peer, gatt, listOf(service))
      fun rec(kind: RecordKind, vararg fields: Pair<Int, ProtocolWireValue>) = ProtocolWireRecord(kind, mapOf(*fields))
      fun str(value: String) = ProtocolWireValue.StringValue(value)
      fun nested(value: ProtocolWireRecord) = ProtocolWireValue.RecordValue(value)
      val attachment = rec(RecordKind.ATTACHMENT, 1 to str("a"), 2 to str("b"), 3 to str("bg"), 4 to str("d"), 5 to str("dg"))
      val connection = rec(RecordKind.CONNECTION_PATH, 1 to nested(attachment), 2 to str(peer), 3 to str("c"), 4 to str("l"), 5 to str("cg"))
      val database = rec(RecordKind.DATABASE_PATH, 1 to nested(connection), 2 to str("db"), 3 to str("dbgen"))
      val svc = rec(RecordKind.SERVICE_PATH, 1 to nested(database), 2 to str(serviceUuid.toString()), 3 to str("0"))
      val path = rec(RecordKind.CHARACTERISTIC_PATH, 1 to nested(svc), 2 to str(charUuid.toString()), 3 to str("0"))
      val pendingField = dispatcher.javaClass.getDeclaredField("pendingCommands").also { it.isAccessible = true }
      @Suppress("UNCHECKED_CAST")
      val pending = pendingField.get(dispatcher) as MutableMap<String, ProtocolWireRecord>
      val subscribe = dispatcher.javaClass.getDeclaredMethod("subscribe", ProtocolWireRecord::class.java, Boolean::class.javaPrimitiveType).also { it.isAccessible = true }
      fun execute(kind: String, epoch: Long, id: String, constraint: String? = null, mode: String = "notification") {
        val correlation = rec(RecordKind.OPERATION_CORRELATION, 1 to nested(attachment), 2 to ProtocolWireValue.UnsignedIntegerValue(epoch), 3 to str("op-$epoch"))
        val fields = mutableMapOf<Int, ProtocolWireValue>(1 to ProtocolWireValue.UnsignedIntegerValue(2), 2 to nested(correlation), 3 to str(kind), 4 to nested(path), 7 to str(id), 21 to str(mode))
        if (constraint != null) fields[22] = str(constraint)
        val command = ProtocolWireRecord(RecordKind.COMMAND, fields)
        pending["$epoch:op-$epoch"] = command
        subscribe.invoke(dispatcher, command, kind == "subscribe")
      }
      execute("subscribe", 1, "one", "preferNotification")
      execute("subscribe", 2, "two", "preferIndication", "indication")
      Mockito.verify(gatt, Mockito.times(1)).setCharacteristicNotification(characteristic, true)
      assertEquals(0, emitted.size)
      try {
        execute("subscribe", 3, "conflict", "requireIndication", "indication")
        throw AssertionError("Conflicting hard mode must be rejected")
      } catch (error: java.lang.reflect.InvocationTargetException) {
        check(error.cause is IllegalArgumentException)
        pending.remove("3:op-3")
      }
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(9))
      dispatcher.cancelPendingOperation(2, "op-2")
      assertEquals(1, emitted.size)
      jsi.verify({ UnifiedBleProtocolJsiBinding.emitNotificationLoss(123L, "one", 1L, 1L) }, Mockito.never())
      Mockito.verify(gatt, Mockito.never()).setCharacteristicNotification(characteristic, false)
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      assertEquals(2, emitted.size)
      jsi.verify { UnifiedBleProtocolJsiBinding.emitNotification(123L, "one", byteArrayOf(9)) }
      jsi.verify({ UnifiedBleProtocolJsiBinding.emitNotification(123L, "two", byteArrayOf(9)) }, Mockito.never())
      execute("subscribe", 4, "three", "preferIndication", "indication")
      assertEquals(3, emitted.size)
      radio.onProtocolNotificationFailure?.invoke(
        peer, generation, characteristic, null,
        com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidNotificationPath(serviceUuid, 0, charUuid, 0),
        com.sfourdrinier.unifiedblemanager.radio.NotificationOwnershipFifo.Stop.DeliveryUncertain(
          SecurityException("private platform detail"), 1, 1
        )
      )
      jsi.verify {
        UnifiedBleProtocolJsiBinding.emitNotificationTerminated(
          123L, "one", "permission-denied", "NOTIFICATION_DELIVERY_UNCERTAIN", null,
          "Android notification ingress delivery failed"
        )
      }
      jsi.verify {
        UnifiedBleProtocolJsiBinding.emitNotificationTerminated(
          123L, "three", "permission-denied", "NOTIFICATION_DELIVERY_UNCERTAIN", null,
          "Android notification ingress delivery failed"
        )
      }
      execute("unsubscribe", 5, "one")
      assertEquals(4, emitted.size)
      Mockito.verify(gatt, Mockito.never()).setCharacteristicNotification(characteristic, false)
      execute("unsubscribe", 6, "three")
      Mockito.verify(gatt, Mockito.times(1)).setCharacteristicNotification(characteristic, false)
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      assertEquals(5, emitted.size)

      val lossOrder = mutableListOf<Int>()
      jsi.`when`<Unit> { UnifiedBleProtocolJsiBinding.emitNotificationLoss(123L, "cancelled", 2L, 3L) }
        .thenAnswer { lossOrder.add(emitted.size); Unit }
      execute("subscribe", 7, "cancelled", "preferNotification")
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(1, 2))
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(3))
      val beforeCancellation = emitted.size
      dispatcher.cancelPendingOperation(7, "op-7")
      assertEquals(listOf(beforeCancellation), lossOrder)
      assertEquals(beforeCancellation + 1, emitted.size)
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      Mockito.verify(gatt, Mockito.times(2)).setCharacteristicNotification(characteristic, false)
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, 1)
      delayed.last().invoke()
      val beforeRetry = emitted.size
      execute("subscribe", 8, "retry", "preferNotification")
      assertEquals(beforeRetry + 1, emitted.size)
      check(!pending.containsKey("8:op-8"))
      Mockito.verify(gatt, Mockito.times(2)).setCharacteristicNotification(characteristic, true)
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      execute("subscribe", 9, "replacement", "preferNotification")
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      Mockito.verify(gatt, Mockito.times(3)).setCharacteristicNotification(characteristic, true)

      val groupsField = dispatcher.javaClass.getDeclaredField("subscriptionGroups").also { it.isAccessible = true }
      @Suppress("UNCHECKED_CAST")
      val groups = groupsField.get(dispatcher) as MutableMap<ProtocolWireRecord, Any>
      val group = groups.getValue(path)
      val state = group.javaClass.getDeclaredField("state").also { it.isAccessible = true }
      val routesField = dispatcher.javaClass.getDeclaredField("activeSubscriptions").also { it.isAccessible = true }
      @Suppress("UNCHECKED_CAST")
      val routes = routesField.get(dispatcher) as MutableMap<String, Any>
      val route = routes.getValue("replacement")
      val failures = mutableListOf<String>()
      fun rejectingRadio(): OwnedAndroidGattRadio = OwnedAndroidGattRadio(
        context,
        post = { throw IllegalStateException("synchronous queue failure") },
        scheduleDelayed = { _, _ -> true }
      ).also { it.onCleanupFailure = { failure -> failures.add(failure.operation) } }
      val cleanup = dispatcher.javaClass.getDeclaredMethod("cleanupUnownedSubscription", group.javaClass).also { it.isAccessible = true }
      radioField.set(dispatcher, rejectingRadio())
      cleanup.invoke(dispatcher, group)
      assertEquals("cleanup-failed", state.get(group))
      assertEquals(listOf("cancelledSubscriptionDisable"), failures)
      assertEquals(group, groups[path])

      val restore = dispatcher.javaClass.getDeclaredMethod("restoreCancelledSubscription", group.javaClass, route.javaClass).also { it.isAccessible = true }
      radioField.set(dispatcher, rejectingRadio())
      state.set(group, "restoring")
      restore.invoke(dispatcher, group, route)
      assertEquals("cleanup-failed", state.get(group))
      assertEquals(listOf("cancelledSubscriptionDisable", "cancelledUnsubscribeRestore"), failures)
      assertEquals(route, routes["replacement"])
      jsi.verify { UnifiedBleProtocolJsiBinding.emitNotificationTerminated(123L, "replacement", "platform", "NOTIFICATION_RESTORE_FAILED", null, "Android notification restoration failed after cancellation") }

      radioField.set(dispatcher, radio)
      execute("unsubscribe", 10, "replacement")
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      check(!groups.containsKey(path))

      execute("subscribe", 20, "new-generation", "preferNotification")
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      val replacementGroup = groups.getValue(path)
      val epoch = group.javaClass.getDeclaredField("epoch").also { it.isAccessible = true }
      check(epoch.getLong(replacementGroup) > epoch.getLong(group))
      val stopsBeforeStaleCleanup = Mockito.mockingDetails(gatt).invocations.count {
        it.method.name == "setCharacteristicNotification" && it.arguments[1] == false
      }
      cleanup.invoke(dispatcher, group)
      val stopsAfterStaleCleanup = Mockito.mockingDetails(gatt).invocations.count {
        it.method.name == "setCharacteristicNotification" && it.arguments[1] == false
      }
      assertEquals(stopsBeforeStaleCleanup, stopsAfterStaleCleanup)
      assertEquals(replacementGroup, groups[path])
      execute("unsubscribe", 21, "new-generation")
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, BluetoothGatt.GATT_SUCCESS)
      check(!groups.containsKey(path))

      val entered = java.util.concurrent.CountDownLatch(1)
      val release = java.util.concurrent.CountDownLatch(1)
      val throwingRadio = OwnedAndroidGattRadio(
        context,
        post = {
          entered.countDown()
          check(release.await(5, java.util.concurrent.TimeUnit.SECONDS))
          throw IllegalStateException("held start failed")
        },
        scheduleDelayed = { _, _ -> true }
      )
      throwingRadio.attachConnectedGatt(peer, gatt, listOf(service))
      radioField.set(dispatcher, throwingRadio)
      val emissionsBefore = emitted.size
      val workerError = java.util.concurrent.atomic.AtomicReference<Throwable?>()
      val worker = Thread {
        val workerJsi = Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java)
        JsiStaticStubs.stubEmitRecord(workerJsi, emitted)
        JsiStaticStubs.stubEmitDiagnostic(workerJsi, mutableListOf())
        try { execute("subscribe", 11, "throwing", "preferNotification") }
        catch (error: Throwable) { workerError.set(error) }
        finally { workerJsi.close() }
      }
      worker.start()
      check(entered.await(5, java.util.concurrent.TimeUnit.SECONDS))
      execute("subscribe", 12, "joining", "preferNotification")
      release.countDown()
      worker.join(5000)
      check(!worker.isAlive)
      check(workerError.get() == null) { "Start failure escaped: ${workerError.get()}" }
      assertEquals(emissionsBefore + 2, emitted.size)
      check(!pending.containsKey("11:op-11"))
      check(!pending.containsKey("12:op-12"))
      check(!groups.containsKey(path))
      radioField.set(dispatcher, radio)
      val nextEpoch = dispatcher.javaClass.getDeclaredField("nextSubscriptionEpoch").also { it.isAccessible = true }
      nextEpoch.setLong(dispatcher, Long.MAX_VALUE)
      try {
        execute("subscribe", 22, "exhausted", "preferNotification")
        throw AssertionError("Epoch exhaustion must reject a new owner")
      } catch (error: java.lang.reflect.InvocationTargetException) {
        check(error.cause is IllegalStateException)
        pending.remove("22:op-22")
      }
      check(!groups.containsKey(path))
    } finally {
      radioField.set(dispatcher, original)
      jsi.close()
    }
  }
}
