package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.*
import android.content.Context
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.*

class OwnedAndroidPeerDirectoryTest {
  @Test fun systemGattInventoryIncludesAnUnbondedForeignLinkWithoutCreatingGatt() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    val device = mock(BluetoothDevice::class.java)
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
    `when`(device.address).thenReturn("AA:BB:CC:DD:EE:FF")
    `when`(device.name).thenReturn("foreign LE link")
    `when`(manager.getConnectedDevices(BluetoothProfile.GATT)).thenReturn(listOf(device))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val peers = radio.connectedPeerSnapshots()
    assertEquals("AA:BB:CC:DD:EE:FF", peers.single().nativePeerId)
    assertEquals("foreign LE link", peers.single().displayName)
    verify(manager).getConnectedDevices(BluetoothProfile.GATT)
    verify(adapter, never()).bondedDevices
    verify(adapter, never()).getRemoteDevice(anyString())
    `when`(manager.getConnectedDevices(BluetoothProfile.GATT)).thenReturn(emptyList())
    assertTrue(radio.connectedPeerSnapshots().isEmpty())
  }

  @Test fun originalSystemInventoryPermissionFailureIsNotConvertedToAnEmptyList() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
    val original = SecurityException("host CONNECT permission was revoked")
    `when`(manager.getConnectedDevices(BluetoothProfile.GATT)).thenThrow(original)
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    try { radio.connectedPeerSnapshots(); fail("permission failure must survive") } catch (failure: SecurityException) { assertSame(original, failure) }
  }
}
