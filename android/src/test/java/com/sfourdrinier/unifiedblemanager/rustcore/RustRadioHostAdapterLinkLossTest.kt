// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustRadioHostAdapterLinkLossTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothProfile
import android.content.Context
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import org.junit.Assert.assertEquals
import org.junit.Test
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.mock
import java.util.UUID

/**
 * Finding 132: an operation in flight when the link goes down fails as a link
 * loss, as legacy's dispatcher failed every pending command with
 * `connectionLost`. Driven through the REAL [OwnedAndroidGattRadio] teardown
 * (`onConnectionStateChange(DISCONNECTED)` / an app disconnect) and the real
 * [OwnedRadioPort] and [RustRadioHostAdapter]; only `BluetoothGatt` is mocked.
 */
class RustRadioHostAdapterLinkLossTest {
  private val core = FakeCore()
  private val context = mock(Context::class.java)
  private val device = mock(android.bluetooth.BluetoothDevice::class.java)
  private val gatt = mock(BluetoothGatt::class.java)
  private val service = mock(BluetoothGattService::class.java)
  private val characteristic = mock(BluetoothGattCharacteristic::class.java)
  private val radio = OwnedAndroidGattRadio(
    context,
    post = { action ->
      action()
      true
    },
    scheduleDelayed = { _, _ -> true }
  )
  private val adapter = RustRadioHostAdapter(
    core,
    OwnedRadioPort(radio) { },
    FakeBackground(),
    { null },
    { null },
    DirectExecutor,
    DirectExecutor
  ) { }

  init {
    doReturn(DEVICE_ID).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(SERVICE_UUID).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(CHAR_UUID).`when`(characteristic).uuid
    doReturn(0x02).`when`(characteristic).getProperties()
    doReturn(service).`when`(characteristic).service
    doReturn(true).`when`(gatt).readCharacteristic(characteristic)
    radio.attachConnectedGatt(DEVICE_ID, gatt, listOf(service))
    adapter.statusCounts()
    radio.nativeGattCallback().onConnectionStateChange(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
  }

  private fun readInFlight(requestId: Long) {
    adapter.read(requestId, DEVICE_ID, SERVICE_UUID.toString(), 0, CHAR_UUID.toString(), 0)
    assertEquals("the read is in flight", null, core.failures[requestId])
  }

  @Test
  fun aPeerLinkLossFailsTheOperationInFlightAsALinkLossWithTheAndroidStatus() {
    readInFlight(7)
    radio.nativeGattCallback().onConnectionStateChange(gatt, 8, BluetoothProfile.STATE_DISCONNECTED)
    val failure = core.failures.getValue(7)
    assertEquals(RadioFailureKind.NOT_CONNECTED, failure.kind)
    assertEquals(8, failure.gattStatus)
    assertEquals(true, failure.dispatched)
  }

  @Test
  fun anAppDisconnectFailsTheOperationInFlightAsALinkLoss() {
    readInFlight(7)
    adapter.disconnect(8, DEVICE_ID)
    assertEquals(RadioFailureKind.NOT_CONNECTED, core.failures.getValue(7).kind)
  }

  @Test
  fun anOperationQueuedBehindTheInFlightOneWasNeverSent() {
    readInFlight(7)
    adapter.read(9, DEVICE_ID, SERVICE_UUID.toString(), 0, CHAR_UUID.toString(), 0)
    radio.nativeGattCallback().onConnectionStateChange(gatt, 19, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals(RadioFailureKind.NOT_CONNECTED, core.failures.getValue(9).kind)
    assertEquals(false, core.failures.getValue(9).dispatched)
  }

  /**
   * Physical run (Samsung, Polar H10): the link dropped (status 22) while
   * service discovery was pending. The discovery failed as a plain platform
   * failure, so the public layer reported `platform.failure: gatt.discover`
   * and the supervisor stopped. It is a link loss, as every other operation
   * in flight at a disconnect is.
   */
  @Test
  fun aLinkLossDuringDiscoveryFailsTheDiscoveryAsALinkLossWithTheAndroidStatus() {
    doReturn(true).`when`(gatt).discoverServices()
    adapter.discover(11, DEVICE_ID)
    assertEquals("the discovery is in flight", null, core.failures[11])
    radio.nativeGattCallback().onConnectionStateChange(gatt, 22, BluetoothProfile.STATE_DISCONNECTED)
    val failure = core.failures.getValue(11)
    assertEquals(RadioFailureKind.NOT_CONNECTED, failure.kind)
    assertEquals(22, failure.gattStatus)
    assertEquals(true, failure.dispatched)
  }

  @Test
  fun aDiscoveryQueuedBehindAReadWhenTheLinkDropsWasNeverSent() {
    readInFlight(7)
    adapter.discover(12, DEVICE_ID)
    radio.nativeGattCallback().onConnectionStateChange(gatt, 8, BluetoothProfile.STATE_DISCONNECTED)
    val failure = core.failures.getValue(12)
    assertEquals(RadioFailureKind.NOT_CONNECTED, failure.kind)
    assertEquals(false, failure.dispatched)
  }

  @Test
  fun aDiscoveryTheStackFailsWithoutALinkLossStaysAPlatformFailure() {
    doReturn(true).`when`(gatt).discoverServices()
    adapter.discover(13, DEVICE_ID)
    radio.nativeGattCallback().onServicesDiscovered(gatt, 129)
    assertEquals(RadioFailureKind.PLATFORM, core.failures.getValue(13).kind)
  }

  companion object {
    private const val DEVICE_ID = "AA:BB:CC:DD:EE:FF"
    private val SERVICE_UUID: UUID = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    private val CHAR_UUID: UUID = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
  }
}
