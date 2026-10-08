package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.*
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.*

class OwnedAndroidEncryptionTest {
  private val peer = "A0:9E:1A:00:00:01"
  private val context = mock(Context::class.java)
  private val manager = mock(BluetoothManager::class.java)
  private val adapter = mock(BluetoothAdapter::class.java)
  private val device = mock(BluetoothDevice::class.java)

  init {
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    `when`(adapter.getRemoteDevice(peer)).thenReturn(device)
    `when`(device.address).thenReturn(peer)
    `when`(device.bondState).thenReturn(BluetoothDevice.BOND_BONDED)
  }

  private fun radio(sdk: Int) = OwnedAndroidGattRadio(context, post = { it(); true },
    scheduleDelayed = { _, _ -> true }, securitySdkInt = sdk, securityFullSdkInt = sdk * 100000)

  @Test fun alreadyPairedResultUsesTheRuntimeSecurityProjectionOnBothCallbackRoutes() {
    for ((sdk, fullSdk) in listOf(35 to 3500000, 36 to 3600000, 36 to 3600001)) {
      for (scheduled in listOf(true, false)) {
        val current = OwnedAndroidGattRadio(context,
          post = { if (scheduled) { it(); true } else false },
          scheduleDelayed = { _, _ -> true }, securitySdkInt = sdk, securityFullSdkInt = fullSdk)
        val expected = current.securityState(peer)
        val results = mutableListOf<Pair<String, OwnedAndroidSecurityState>>()
        assertEquals(0L, current.pair(peer, "platformDefault") { outcome, state -> results.add(outcome to state) })
        assertEquals(listOf("alreadyPaired" to expected), results)
        assertEquals(if (sdk < 36) "unsupported" else "unknown", results.single().second.encryption)
        verify(device, never()).createBond()
      }
    }
  }

  @Test fun api35DoesNotInventEncryptionAndApi36EventsNeedNoBondTransition() {
    val old = radio(35)
    val states = mutableListOf<OwnedAndroidSecurityState>()
    old.onSecurityState = { _, state -> states.add(state) }
    old.receiveEncryptionChange(device, BluetoothDevice.TRANSPORT_LE, 0, true)
    assertTrue(states.isEmpty())
    assertEquals("unsupported", old.securityState(peer).encryption)
    val current = radio(36)
    current.onSecurityState = { _, state -> states.add(state) }
    current.receiveEncryptionChange(device, BluetoothDevice.TRANSPORT_BREDR, 0, true)
    assertTrue(states.isEmpty())
    current.receiveEncryptionChange(device, BluetoothDevice.TRANSPORT_LE, 0, true)
    assertEquals("encrypted", states.single().encryption)
    assertEquals("bonded", states.single().bond)
  }

  @Test fun addressGetterPermissionFailureRemainsAnExplicitUnattributedSourceFailure() {
    val current = radio(36)
    val refusal = SecurityException("device address denied")
    `when`(device.address).thenThrow(refusal)
    val failures = mutableListOf<Pair<String?, Throwable>>()
    val states = mutableListOf<OwnedAndroidSecurityState>()
    current.onSecurityFailure = { peerId, error -> failures.add(Pair(peerId, error)) }
    current.onSecurityState = { _, state -> states.add(state) }
    current.receiveEncryptionChange(device, BluetoothDevice.TRANSPORT_LE, 0, true)
    assertTrue(states.isEmpty())
    assertEquals(1, failures.size)
    assertNull(failures.single().first)
    assertSame(refusal, failures.single().second)
  }

  @Test fun linkLossAndOldGattCallbacksCannotRenewEncryptionInTheNewGeneration() {
    val current = radio(36)
    val first = mock(BluetoothGatt::class.java)
    `when`(first.device).thenReturn(device)
    current.attachConnectedGatt(peer, first, emptyList())
    current.nativeGattCallback().onConnectionStateChange(first, 0, BluetoothProfile.STATE_CONNECTED)
    current.receiveEncryptionChange(device, BluetoothDevice.TRANSPORT_LE, 0, true)
    assertEquals("unknown", current.securityState(peer).encryption)
    current.nativeGattCallback().onConnectionStateChange(first, 8, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals("unknown", current.securityState(peer).encryption)
    val second = mock(BluetoothGatt::class.java)
    `when`(second.device).thenReturn(device)
    current.attachConnectedGatt(peer, second, emptyList())
    current.nativeGattCallback().onConnectionStateChange(second, 0, BluetoothProfile.STATE_CONNECTED)
    current.nativeGattCallback().onConnectionStateChange(first, 0, BluetoothProfile.STATE_CONNECTED)
    assertEquals("unknown", current.securityState(peer).encryption)
    current.receiveEncryptionChange(device, BluetoothDevice.TRANSPORT_LE, 0, false)
    assertEquals("unknown", current.securityState(peer).encryption)
  }

  @Test fun delayedPeerBroadcastCannotBecomeAReplacementLinksSnapshot() {
    val current = radio(36)
    val states = mutableListOf<OwnedAndroidSecurityState>()
    current.onSecurityState = { _, state -> states.add(state) }
    val first = mock(BluetoothGatt::class.java)
    `when`(first.device).thenReturn(device)
    current.attachConnectedGatt(peer, first, emptyList())
    current.nativeGattCallback().onConnectionStateChange(first, 0, BluetoothProfile.STATE_CONNECTED)
    current.nativeGattCallback().onConnectionStateChange(first, 8, BluetoothProfile.STATE_DISCONNECTED)
    val replacement = mock(BluetoothGatt::class.java)
    `when`(replacement.device).thenReturn(device)
    current.attachConnectedGatt(peer, replacement, emptyList())
    current.nativeGattCallback().onConnectionStateChange(replacement, 0, BluetoothProfile.STATE_CONNECTED)
    states.clear()
    // Android's peer broadcast has no GATT generation; this may be from the old link.
    current.receiveEncryptionChange(device, BluetoothDevice.TRANSPORT_LE, 0, true)
    assertEquals("encrypted", states.single().encryption)
    assertEquals("unknown", current.securityState(peer).encryption)
    verify(manager, never()).getConnectionState(device, BluetoothProfile.GATT)
  }

  @Test fun failedReceiverCleanupKeepsRetryDebtButRetiresCallbacksImmediately() {
    val current = radio(36)
    var receiver: BroadcastReceiver? = null
    doAnswer { call -> receiver = call.getArgument(0); null }.`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    val states = mutableListOf<OwnedAndroidSecurityState>()
    current.onSecurityState = { _, state -> states.add(state) }
    current.registerBondStateReceiver()
    val admitted = checkNotNull(receiver)
    val intent = mock(Intent::class.java)
    `when`(intent.action).thenReturn(BluetoothDevice.ACTION_ENCRYPTION_CHANGE)
    `when`(intent.getParcelableExtra<BluetoothDevice>(BluetoothDevice.EXTRA_DEVICE)).thenReturn(device)
    `when`(intent.hasExtra(BluetoothDevice.EXTRA_TRANSPORT)).thenReturn(true)
    `when`(intent.getIntExtra(BluetoothDevice.EXTRA_TRANSPORT, BluetoothDevice.TRANSPORT_AUTO)).thenReturn(BluetoothDevice.TRANSPORT_LE)
    `when`(intent.getIntExtra(BluetoothDevice.EXTRA_ENCRYPTION_STATUS, BluetoothDevice.ERROR)).thenReturn(0)
    `when`(intent.hasExtra(BluetoothDevice.EXTRA_ENCRYPTION_ENABLED)).thenReturn(true)
    `when`(intent.getBooleanExtra(BluetoothDevice.EXTRA_ENCRYPTION_ENABLED, false)).thenReturn(true)
    admitted.onReceive(context, intent)
    assertEquals(1, states.size)
    doThrow(IllegalStateException("unregister failed")).doNothing().`when`(context).unregisterReceiver(admitted)
    assertNotNull(current.unregisterBondStateReceiver())
    admitted.onReceive(context, intent)
    assertEquals(1, states.size)
    assertNull(current.unregisterBondStateReceiver())
    verify(context, times(2)).unregisterReceiver(admitted)
  }
}
