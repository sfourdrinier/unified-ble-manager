// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadioTestFixture.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.content.Context
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.mock
import org.mockito.Mockito.mockingDetails

/**
 * The real [OwnedAndroidGattRadio] over a mocked adapter, device and GATT with an injected
 * `post`/`scheduleDelayed`, shared by the disconnect-owner and connect-ownership suites.
 */
internal class GattRadioTimer(val dueAt: Long, val delayMs: Long, val action: () -> Unit)

internal class GattRadioFixture {
  val peer = "A0:9E:1A:00:00:01"
  val context: Context = mock(Context::class.java)
  val manager: BluetoothManager = mock(BluetoothManager::class.java)
  val adapter: BluetoothAdapter = mock(BluetoothAdapter::class.java)
  val device: BluetoothDevice = mock(BluetoothDevice::class.java)
  val timers = mutableListOf<GattRadioTimer>()
  var now = 0L

  /** Close deadlines are refused (`false`) or rejected (thrown) instead of armed. */
  var refuseDeadline = false
  var rejectDeadline: Throwable? = null

  /** Runs when a close deadline is requested, before the scheduler accepts, refuses or rejects it. */
  var onCloseDeadlineRequested: (() -> Unit)? = null

  init {
    doReturn(manager).`when`(context).getSystemService(Context.BLUETOOTH_SERVICE)
    doReturn(adapter).`when`(manager).adapter
    doReturn(BluetoothAdapter.STATE_ON).`when`(adapter).state
    doReturn(device).`when`(adapter).getRemoteDevice(peer)
    doReturn(peer).`when`(device).address
  }

  val radio = OwnedAndroidGattRadio(
    context,
    post = { action -> action(); true },
    scheduleDelayed = { delayMs, action ->
      val isCloseDeadline = delayMs == OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS
      if (isCloseDeadline) onCloseDeadlineRequested?.invoke()
      if (isCloseDeadline) rejectDeadline?.let { throw it }
      if (isCloseDeadline && refuseDeadline) {
        false
      } else {
        timers.add(GattRadioTimer(now + delayMs, delayMs, action))
        true
      }
    }
  )

  fun gatt(): BluetoothGatt {
    val gatt = mock(BluetoothGatt::class.java)
    doReturn(device).`when`(gatt).device
    return gatt
  }

  fun connected(gatt: BluetoothGatt = gatt()): BluetoothGatt {
    radio.attachConnectedGatt(peer, gatt, emptyList())
    radio.nativeGattCallback().onConnectionStateChange(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    return gatt
  }

  fun nativeDisconnected(gatt: BluetoothGatt, status: Int = BluetoothGatt.GATT_SUCCESS) {
    radio.nativeGattCallback().onConnectionStateChange(gatt, status, BluetoothProfile.STATE_DISCONNECTED)
  }

  /** Close-deadline timers armed so far (other radio timers are not counted). */
  fun closeDeadlines(): List<GattRadioTimer> =
    timers.filter { it.delayMs == OwnedAndroidGattRadio.GATT_CLOSE_TIMEOUT_MS }

  /** Advance virtual time, running every due timer once in due order. */
  fun advanceTo(ms: Long) {
    now = ms
    while (true) {
      val due = timers.filter { it.dueAt <= ms }.minByOrNull { it.dueAt } ?: return
      timers.remove(due)
      due.action()
    }
  }
}

internal class GattDisconnectResults {
  val values = mutableListOf<OwnedRadioTeardownFailure?>()
  val callback: (OwnedRadioTeardownFailure?) -> Unit = { values.add(it) }
}

/** Makes `connectGatt` on the fixture device return [gatt]; [connectGattCalls] counts what was opened. */
internal fun connectGattReturns(f: GattRadioFixture, gatt: BluetoothGatt) {
  doReturn(gatt).`when`(f.device)
    .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
}

internal fun connectGattCalls(f: GattRadioFixture): Int {
  var calls = 0
  mockingDetails(f.device).invocations.forEach { if (it.method.name == "connectGatt") calls += 1 }
  return calls
}

internal fun thrownBy(block: () -> Unit): Throwable? =
  try {
    block()
    null
  } catch (error: Throwable) {
    error
  }

internal class Boom(message: String) : RuntimeException(message)
