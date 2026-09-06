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
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.mock
import java.util.UUID

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

  private class RadioFixture(
    registrationAccepted: Boolean = true
  ) {
    val delayed = mutableListOf<() -> Unit>()
    val notifications = mutableListOf<ByteArray>()
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
      radio.onProtocolNotification = { _, _, value -> notifications.add(value.copyOf()) }
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
