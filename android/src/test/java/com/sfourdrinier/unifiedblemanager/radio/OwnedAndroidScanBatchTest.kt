package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanRecord
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test
import org.mockito.Mockito

class OwnedAndroidScanBatchTest {
  @Test
  fun nativeBatchOwnsEveryPayloadAndRejectsLateCallbacksAfterStop() {
    exerciseBatch(false)
  }

  @Test
  fun synchronousStartupBatchBelongsToTheAdmittedScan() {
    exerciseBatch(true)
  }

  @Test
  fun api26NonlegacyScanForwardsCodedPhyAndBatchDelayToTheNativeBuilder() {
    exerciseBatch(false, 26, BluetoothDevice.PHY_LE_CODED, false)
  }

  @Test
  fun api26NonlegacyScanForwardsAllSupportedPhysWithoutInventingASelectedPhy() {
    exerciseBatch(false, 26, ScanSettings.PHY_LE_ALL_SUPPORTED, false)
  }

  private fun exerciseBatch(duringStart: Boolean, scanSdkInt: Int = 0, phy: Int? = null, legacy: Boolean = true) {
    val context = Mockito.mock(Context::class.java)
    val manager = Mockito.mock(BluetoothManager::class.java)
    val adapter = Mockito.mock(BluetoothAdapter::class.java)
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val device = Mockito.mock(BluetoothDevice::class.java)
    val record = Mockito.mock(ScanRecord::class.java)
    val first = Mockito.mock(ScanResult::class.java)
    val second = Mockito.mock(ScanResult::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    Mockito.`when`(manager.adapter).thenReturn(adapter)
    Mockito.`when`(adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.`when`(adapter.isLeCodedPhySupported).thenReturn(true)
    Mockito.`when`(device.address).thenReturn("A0:9E:1A:00:00:01")
    Mockito.`when`(device.name).thenReturn("batch peer")
    val bytes = byteArrayOf(2, 1, 6)
    Mockito.`when`(record.bytes).thenReturn(bytes)
    listOf(first, second).forEach { result ->
      Mockito.`when`(result.device).thenReturn(device)
      Mockito.`when`(result.scanRecord).thenReturn(record)
    }
    Mockito.`when`(first.rssi).thenReturn(-40)
    Mockito.`when`(second.rssi).thenReturn(-50)
    val received = mutableListOf<OwnedAndroidProtocolAdvertisement>()
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true }, scanSdkInt = scanSdkInt)
    radio.onProtocolScanResult = { received.add(it) }
    var callback: ScanCallback? = null
    Mockito.doAnswer { invocation ->
      val admitted = invocation.getArgument<ScanCallback>(2)
      callback = admitted
      if (duringStart) admitted.onBatchScanResults(mutableListOf(first, second))
      null
    }.`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
      Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
      Mockito.`when`(builder.setLegacy(Mockito.anyBoolean())).thenReturn(builder)
      Mockito.`when`(builder.setPhy(Mockito.anyInt())).thenReturn(builder)
      Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
    }.use { builders ->
      radio.startScan(null, ScanSettings.SCAN_MODE_BALANCED, legacyScan = legacy, reportDelayMs = 500, scanPhy = phy)
      Mockito.verify(builders.constructed().single()).setReportDelay(500)
      if (scanSdkInt >= 26) Mockito.verify(builders.constructed().single()).setLegacy(legacy)
      if (phy != null) Mockito.verify(builders.constructed().single()).setPhy(phy)
      val admitted = requireNotNull(callback)
      if (!duringStart) admitted.onBatchScanResults(mutableListOf(first, second))
      assertEquals(listOf(-40, -50), received.map { it.rssi })
      bytes[2] = 99
      received.forEach { assertArrayEquals(byteArrayOf(2, 1, 6), it.rawRecord) }
      assertEquals(null, radio.stopScan())
      admitted.onBatchScanResults(mutableListOf(first, second))
      admitted.onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, first)
      assertEquals(2, received.size)
      Mockito.verify(scanner).stopScan(admitted)
    }
  }
}
