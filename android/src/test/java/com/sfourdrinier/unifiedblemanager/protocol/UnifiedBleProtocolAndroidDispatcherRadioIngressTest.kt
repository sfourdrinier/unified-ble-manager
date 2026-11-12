// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcherRadioIngressTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothProfile
import android.content.Context
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattOperationFailure
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.classifyAndroidNotificationRegistrationFailure
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito.doAnswer
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.doThrow
import org.mockito.Mockito.mock
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

class UnifiedBleProtocolAndroidDispatcherRadioIngressTest {
  @Test
  fun notificationArrivingBetweenWriteDescriptorAndSuccessIsDelivered() {
    val fixture = RadioFixture()
    val results = fixture.subscribe()
    assertTrue("subscribe settled before CCCD callback: $results", results.isEmpty())

    fixture.deliverChanged(byteArrayOf(0x11, 0x22))
    assertTrue(fixture.notifications.isEmpty())

    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)

    assertTrue(results.single().isSuccess)
    assertTrue(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))
    assertEquals(1, fixture.notifications.size)
    assertArrayEquals(byteArrayOf(0x11, 0x22), fixture.notifications.single())
  }

  @Test
  fun deferredCccd133ThenSameGenerationSuccessActivatesAndDelivers() {
    val fixture = RadioFixture()
    val results = fixture.subscribe()

    fixture.deliverDescriptorWrite(133)
    assertTrue(results.isEmpty())
    assertEquals(1, fixture.delayed.size)
    assertFalse(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))

    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)

    assertTrue(results.single().isSuccess)
    assertTrue(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))

    fixture.delayed.single().invoke()
    assertTrue(results.single().isSuccess)

    fixture.deliverChanged(byteArrayOf(0x42))
    assertEquals(1, fixture.notifications.size)
    assertArrayEquals(byteArrayOf(0x42), fixture.notifications.single())
  }

  @Test
  fun registrationFalseWithoutDisconnectIsNotConnectionLostAndLaterReadSucceeds() {
    val fixture = RadioFixture(registrationAccepted = false)
    val results = fixture.subscribe()
    assertTrue(results.isEmpty())
    assertEquals(1, fixture.delayed.size)

    fixture.delayed.single().invoke()

    val error = results.single().exceptionOrNull()
    assertTrue(error is AndroidGattOperationFailure)
    if (error !is AndroidGattOperationFailure) throw AssertionError("expected typed registration failure")
    assertFalse(error.isLinkLoss)
    assertEquals("subscriptionFailed", androidGattOperationFailureCode(error, "subscriptionFailed"))
    assertFalse(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))

    doReturn(true).`when`(fixture.gatt).readCharacteristic(fixture.characteristic)
    val reads = ArrayList<Result<ByteArray?>>()
    fixture.radio.readCharacteristicExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0) { reads.add(it) }
    fixture.radio.nativeGattCallback().onCharacteristicRead(
      fixture.gatt,
      fixture.characteristic,
      byteArrayOf(0x09),
      BluetoothGatt.GATT_SUCCESS
    )
    assertArrayEquals(byteArrayOf(0x09), reads.single().getOrNull())
  }

  @Test
  fun registrationFalseThenStatus19IsConnectionLost() {
    val fixture = RadioFixture(registrationAccepted = false)
    val results = fixture.subscribe()
    assertTrue(results.isEmpty())

    fixture.deliverDisconnect(ANDROID_STATUS_19)

    val error = results.single().exceptionOrNull()
    assertTrue(error is AndroidGattOperationFailure)
    if (error !is AndroidGattOperationFailure) throw AssertionError("expected typed link-loss failure")
    assertTrue(error.isLinkLoss)
    assertEquals(ANDROID_STATUS_19, error.gattStatus)
    assertEquals("connectionLost", androidGattOperationFailureCode(error, "subscriptionFailed"))

    fixture.delayed.single().invoke()
    assertEquals(1, results.size)
    assertEquals("connectionLost", androidGattOperationFailureCode(results.single().exceptionOrNull()!!, "subscriptionFailed"))
  }

  @Test
  fun registrationFalseRacesInFlightStatus19AndDoesNotSettleAsPlatformFailure() {
    val fixture = RadioFixture(registrationAccepted = false)
    val results = fixture.subscribe()
    val isolated = classifyAndroidNotificationRegistrationFailure("setCharacteristicNotification")
    assertFalse(isolated.isLinkLoss)
    assertEquals("subscriptionFailed", androidGattOperationFailureCode(isolated, "subscriptionFailed"))

    assertTrue(results.isEmpty())
    fixture.deliverDisconnect(ANDROID_STATUS_19)
    fixture.delayed.single().invoke()

    val error = results.single().exceptionOrNull()
    assertTrue(error is AndroidGattOperationFailure)
    if (error !is AndroidGattOperationFailure) throw AssertionError("expected typed link-loss failure")
    assertTrue(error.isLinkLoss)
    assertEquals("connectionLost", androidGattOperationFailureCode(error, "subscriptionFailed"))
  }

  @Test
  fun cancelledEnableWaitsForRemoteDisableBeforeReplacement() {
    val fixture = RadioFixture()
    val cancelled = mutableListOf<Result<Unit>>()
    val operation = fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification") {
      cancelled.add(it)
    }
    assertTrue(fixture.radio.cancelOperation(operation))
    assertTrue(cancelled.single().isFailure)
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)

    val replacement = fixture.subscribe()
    assertTrue(replacement.isEmpty())
    // The first callback settles the compensating disable, not the replacement enable.
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(replacement.isEmpty())
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(replacement.single().isSuccess)
    assertTrue(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))
  }

  @Test
  fun failedCompensationRetainsOwnerAndRetryPrecedesReplacement() {
    val fixture = RadioFixture()
    val operation = fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification") { }
    assertTrue(fixture.radio.cancelOperation(operation))
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    fixture.deliverDescriptorWrite(1)
    fixture.delayed.last().invoke()

    val blocked = fixture.subscribe()
    assertTrue(blocked.single().isFailure)
    // The blocked acquisition retried the retained cleanup, without enabling a new owner.
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    val replacement = fixture.subscribe()
    assertTrue(replacement.isEmpty())
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(replacement.single().isSuccess)
  }

  @Test
  fun notificationDuringLocalRegistrationIsStagedBeforeDescriptorCompletion() {
    val fixture = RadioFixture()
    doAnswer {
      fixture.deliverChanged(byteArrayOf(7))
      true
    }.`when`(fixture.gatt).setCharacteristicNotification(fixture.characteristic, true)
    val results = fixture.subscribe()
    assertTrue(fixture.notifications.isEmpty())
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(results.single().isSuccess)
    assertArrayEquals(byteArrayOf(7), fixture.notifications.single())
  }

  @Test
  fun enablementOverflowReportsExactLossAfterAcceptedValues() {
    val fixture = RadioFixture()
    val results = fixture.subscribe()
    repeat(18) { fixture.deliverChanged(byteArrayOf(it.toByte())) }
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(results.single().isSuccess)
    assertEquals(16, fixture.notifications.size)
    assertEquals(listOf(2L to 2L), fixture.losses)
  }

  @Test
  fun failedLocalRollbackIsRetriedBeforeAnotherEnable() {
    val fixture = RadioFixture()
    doReturn(false).`when`(fixture.gatt).setCharacteristicNotification(fixture.characteristic, false)
    val first = fixture.subscribe()
    fixture.deliverDescriptorWrite(1)
    fixture.delayed.last().invoke()
    assertTrue(first.single().isFailure)

    val blocked = fixture.subscribe()
    assertTrue(blocked.single().isFailure)
    doReturn(true).`when`(fixture.gatt).setCharacteristicNotification(fixture.characteristic, false)
    val replacement = fixture.subscribe()
    assertTrue(replacement.isEmpty())
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(replacement.single().isSuccess)
  }

  @Test
  fun failedEnableReportsStagedObservationsAsLossInsteadOfValues() {
    val fixture = RadioFixture()
    val results = fixture.subscribe()
    fixture.deliverChanged(byteArrayOf(1, 2))
    fixture.deliverChanged(byteArrayOf(3))
    fixture.deliverDescriptorWrite(1)
    fixture.delayed.last().invoke()
    assertTrue(results.single().isFailure)
    assertTrue(fixture.notifications.isEmpty())
    assertEquals(listOf(2L to 3L), fixture.losses)
  }

  @Test
  fun cancellationAccountsStagedLossBeforePublicFailureAndLateCleanup() {
    val fixture = RadioFixture()
    val seen = mutableListOf<List<Pair<Long, Long>>>()
    val operation = fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification") {
      assertTrue(it.isFailure)
      seen.add(fixture.losses.toList())
    }
    fixture.deliverChanged(byteArrayOf(1, 2))
    assertTrue(fixture.radio.cancelOperation(operation))
    assertEquals(listOf(listOf(1L to 2L)), seen)
    assertTrue(fixture.notifications.isEmpty())
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
  }

  @Test
  fun failedDeliveryRetainsOriginalPathAfterServiceInventoryChanges() {
    val fixture = RadioFixture()
    var failedPath: com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidNotificationPath? = null
    var failedEpoch: Long? = null
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    fixture.radio.onOwnedProtocolNotification = { _, _, _, _ ->
      entered.countDown()
      check(release.await(5, TimeUnit.SECONDS))
      throw IllegalStateException("delivery failed")
    }
    fixture.radio.onProtocolNotificationFailure = { _, _, _, epoch, path, _ ->
      failedEpoch = epoch
      failedPath = path
    }
    fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification", sourceEpoch = 42L) { }
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    val delivery = Thread { fixture.deliverChanged(byteArrayOf(1)) }
    delivery.start()
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    doReturn(emptyList<BluetoothGattCharacteristic>()).`when`(fixture.service).characteristics
    fixture.radio.nativeGattCallback().onServiceChanged(fixture.gatt)
    release.countDown()
    delivery.join(5000)
    assertFalse(delivery.isAlive)
    assertEquals(42L, failedEpoch)
    assertEquals(SERVICE_UUID, failedPath?.serviceUuid)
    assertEquals(CHAR_UUID, failedPath?.characteristicUuid)
    assertEquals(0, failedPath?.serviceOccurrence)
    assertEquals(0, failedPath?.characteristicOccurrence)
  }

  @Test
  fun cancellationDuringSuccessfulCallbackCannotPublishSecondTerminal() {
    val fixture = RadioFixture()
    val results = mutableListOf<Result<Unit>>()
    var operation = 0L
    operation = fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification") { result ->
      results.add(result)
      fixture.radio.cancelOperation(operation)
    }
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertEquals(1, results.size)
    assertTrue(results.single().isSuccess)
    assertTrue(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))
  }

  @Test
  fun descriptorSubmissionExceptionRetiresClaimAndAllowsReplacement() {
    val fixture = RadioFixture()
    doThrow(SecurityException("permission revoked")).`when`(fixture.gatt).writeDescriptor(fixture.cccd)
    doThrow(SecurityException("permission revoked")).`when`(fixture.gatt)
      .writeDescriptor(eq(fixture.cccd), any(ByteArray::class.java))
    val failed = fixture.subscribe()
    assertTrue(failed.single().isFailure)
    assertFalse(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))
    doReturn(true).`when`(fixture.gatt).writeDescriptor(fixture.cccd)
    doReturn(BluetoothGatt.GATT_SUCCESS).`when`(fixture.gatt)
      .writeDescriptor(eq(fixture.cccd), any(ByteArray::class.java))
    val replacement = fixture.subscribe()
    assertTrue(replacement.isEmpty())
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(replacement.single().isSuccess)
  }

  @Test
  fun descriptorSubmissionExceptionRetainsFailedRollbackUntilRetry() {
    val fixture = RadioFixture()
    doReturn(false).`when`(fixture.gatt).setCharacteristicNotification(fixture.characteristic, false)
    doThrow(SecurityException("permission revoked")).`when`(fixture.gatt).writeDescriptor(fixture.cccd)
    doThrow(SecurityException("permission revoked")).`when`(fixture.gatt)
      .writeDescriptor(eq(fixture.cccd), any(ByteArray::class.java))
    assertTrue(fixture.subscribe().single().isFailure)
    assertTrue(fixture.subscribe().single().isFailure)
    doReturn(true).`when`(fixture.gatt).setCharacteristicNotification(fixture.characteristic, false)
    doReturn(true).`when`(fixture.gatt).writeDescriptor(fixture.cccd)
    doReturn(BluetoothGatt.GATT_SUCCESS).`when`(fixture.gatt)
      .writeDescriptor(eq(fixture.cccd), any(ByteArray::class.java))
    val replacement = fixture.subscribe()
    assertTrue(replacement.isEmpty())
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(replacement.single().isSuccess)
  }

  @Test
  fun legacyDeliveryRetainsClaimPathWhenServiceInventoryChanges() {
    val fixture = RadioFixture()
    var observedPath: com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidNotificationPath? = null
    fixture.radio.onProtocolNotification = { _, _, path, _ -> observedPath = path }
    val results = fixture.subscribe()
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(results.single().isSuccess)
    doReturn(emptyList<BluetoothGattCharacteristic>()).`when`(fixture.service).characteristics
    fixture.deliverChanged(byteArrayOf(1))
    assertEquals(SERVICE_UUID, observedPath?.serviceUuid)
    assertEquals(CHAR_UUID, observedPath?.characteristicUuid)
    assertEquals(0, observedPath?.characteristicOccurrence)
  }

  @Test
  fun staleDisableDoesNotTouchReplacementOnSameCharacteristic() {
    val fixture = RadioFixture()
    val first = mutableListOf<Result<Unit>>()
    fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification", sourceEpoch = 41L) { first.add(it) }
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(first.single().isSuccess)
    val firstRelease = mutableListOf<Result<Unit>>()
    fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, false, sourceEpoch = 41L) { firstRelease.add(it) }
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(firstRelease.single().isSuccess)
    val second = mutableListOf<Result<Unit>>()
    fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification", sourceEpoch = 42L) { second.add(it) }
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    assertTrue(second.single().isSuccess)
    val staleRelease = mutableListOf<Result<Unit>>()
    fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, false, sourceEpoch = 41L) { staleRelease.add(it) }
    assertTrue(staleRelease.single().isSuccess)
    org.mockito.Mockito.verify(fixture.gatt, org.mockito.Mockito.times(1))
      .setCharacteristicNotification(fixture.characteristic, false)
    assertTrue(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))
    fixture.deliverChanged(byteArrayOf(42))
    assertArrayEquals(byteArrayOf(42), fixture.notifications.single())
  }

  @Test
  fun delayedServiceChangeCleanupCannotDisableReplacementOnSameGatt() {
    val fixture = RadioFixture()
    fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification", sourceEpoch = 41L) { assertTrue(it.isSuccess) }
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)
    doReturn(false).`when`(fixture.gatt).setCharacteristicNotification(fixture.characteristic, false)
    fixture.radio.nativeGattCallback().onServiceChanged(fixture.gatt)
    doReturn(true).`when`(fixture.gatt).setCharacteristicNotification(fixture.characteristic, false)
    doReturn(listOf(fixture.service)).`when`(fixture.gatt).services
    fixture.radio.nativeGattCallback().onServicesDiscovered(fixture.gatt, BluetoothGatt.GATT_SUCCESS)
    fixture.radio.setNotifyExact(DEVICE_ID, SERVICE_UUID, 0, CHAR_UUID, 0, true, "notification", sourceEpoch = 42L) { assertTrue(it.isSuccess) }
    fixture.deliverDescriptorWrite(BluetoothGatt.GATT_SUCCESS)

    val retry = fixture.radio.javaClass.getDeclaredMethod("retryCleanupLedger")
    retry.isAccessible = true
    retry.invoke(fixture.radio)
    org.mockito.Mockito.verify(fixture.gatt, org.mockito.Mockito.times(1))
      .setCharacteristicNotification(fixture.characteristic, false)
    assertTrue(fixture.radio.isNativeSubscriptionActive(DEVICE_ID, fixture.generation, fixture.characteristic))
    fixture.deliverChanged(byteArrayOf(42))
    assertArrayEquals(byteArrayOf(42), fixture.notifications.single())
  }

  private class RadioFixture(
    registrationAccepted: Boolean = true
  ) {
    val delayed = mutableListOf<() -> Unit>()
    val notifications = mutableListOf<ByteArray>()
    val losses = mutableListOf<Pair<Long, Long>>()
    val context = mock(Context::class.java)
    val device = mock(android.bluetooth.BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val cccd = mock(BluetoothGattDescriptor::class.java)
    val radio = OwnedAndroidGattRadio(
      context,
      post = { action ->
        action()
        true
      },
      scheduleDelayed = { _, action ->
        delayed.add(action)
        true
      }
    )
    val generation: Long

    init {
      doReturn(DEVICE_ID).`when`(device).address
      doReturn(device).`when`(gatt).device
      doReturn(SERVICE_UUID).`when`(service).uuid
      doReturn(listOf(characteristic)).`when`(service).characteristics
      doReturn(CHAR_UUID).`when`(characteristic).uuid
      doReturn(0x10).`when`(characteristic).getProperties()
      doReturn(service).`when`(characteristic).service
      doReturn(cccd).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
      doReturn(OwnedAndroidGattRadio.CCCD_UUID).`when`(cccd).uuid
      doReturn(characteristic).`when`(cccd).characteristic
      doReturn(registrationAccepted).`when`(gatt).setCharacteristicNotification(characteristic, true)
      doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, false)
      doReturn(true).`when`(gatt).writeDescriptor(cccd)
      doReturn(BluetoothGatt.GATT_SUCCESS).`when`(gatt).writeDescriptor(eq(cccd), any(ByteArray::class.java))
      check(characteristic.getProperties() == 0x10) {
        "properties stub failed: ${characteristic.getProperties()}"
      }
      generation = radio.attachConnectedGatt(DEVICE_ID, gatt, listOf(service))
      radio.onProtocolNotification = { _, _, _, value -> notifications.add(value.copyOf()) }
      radio.onProtocolNotificationLoss = { _, _, _, items, bytes -> losses.add(items to bytes) }
    }

    fun subscribe(): ArrayList<Result<Unit>> {
      val results = ArrayList<Result<Unit>>()
      radio.setNotifyExact(
        DEVICE_ID,
        SERVICE_UUID,
        0,
        CHAR_UUID,
        0,
        true,
        "notification"
      ) { results.add(it) }
      return results
    }

    fun deliverChanged(value: ByteArray) {
      radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, value)
    }

    fun deliverDescriptorWrite(status: Int) {
      radio.nativeGattCallback().onDescriptorWrite(gatt, cccd, status)
    }

    fun deliverDisconnect(status: Int) {
      radio.nativeGattCallback().onConnectionStateChange(
        gatt,
        status,
        BluetoothProfile.STATE_DISCONNECTED
      )
    }
  }

  companion object {
    private const val DEVICE_ID = "AA:BB:CC:DD:EE:FF"
    private const val ANDROID_STATUS_19 = 19
    private val SERVICE_UUID: UUID = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    private val CHAR_UUID: UUID = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
  }
}
