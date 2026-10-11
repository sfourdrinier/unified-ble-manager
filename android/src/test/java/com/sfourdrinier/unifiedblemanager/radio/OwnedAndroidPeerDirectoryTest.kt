package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.*
import android.content.Context
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.*

class OwnedAndroidPeerDirectoryTest {
  @Test fun originAddressResolvesAnUnbondedPeerWithoutCreatingRadioState() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    val device = mock(BluetoothDevice::class.java)
    val address = "AA:BB:CC:DD:EE:01"
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
    `when`(manager.getConnectedDevices(BluetoothProfile.GATT)).thenReturn(emptyList())
    `when`(adapter.bondedDevices).thenReturn(emptySet())
    `when`(adapter.getRemoteDevice(address)).thenReturn(device)
    `when`(device.name).thenReturn(null)
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })

    val resolved = radio.resolveDirectoryPeer(address)

    assertEquals(ConnectedPeerSnapshot(address, null), resolved)
    verify(adapter).getRemoteDevice(address)
    verify(manager).getConnectedDevices(BluetoothProfile.GATT)
    verify(adapter).bondedDevices
    verify(adapter, never()).startDiscovery()
    verify(device, never()).createBond()
    verify(device, never()).connectGatt(any(), anyBoolean(), any(), anyInt())
  }

  @Test fun originAddressKeepsConnectedAndBondedInventoryPrecedence() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    val connected = mock(BluetoothDevice::class.java)
    val bonded = mock(BluetoothDevice::class.java)
    val address = "AA:BB:CC:DD:EE:02"
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
    `when`(connected.address).thenReturn(address)
    `when`(connected.name).thenReturn("connected")
    `when`(bonded.address).thenReturn(address)
    `when`(bonded.name).thenReturn("bonded")
    doReturn(listOf(connected), emptyList<BluetoothDevice>()).`when`(manager).getConnectedDevices(BluetoothProfile.GATT)
    `when`(adapter.bondedDevices).thenReturn(setOf(bonded))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })

    assertEquals(ConnectedPeerSnapshot(address, "connected"), radio.resolveDirectoryPeer(address))
    assertEquals(ConnectedPeerSnapshot(address, "bonded"), radio.resolveDirectoryPeer(address))
    verify(adapter, never()).getRemoteDevice(anyString())
  }

  @Test fun originAddressPreservesAnOsCachedName() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    val device = mock(BluetoothDevice::class.java)
    val address = "AA:BB:CC:DD:EE:04"
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
    `when`(manager.getConnectedDevices(BluetoothProfile.GATT)).thenReturn(emptyList())
    `when`(adapter.bondedDevices).thenReturn(emptySet())
    `when`(adapter.getRemoteDevice(address)).thenReturn(device)
    `when`(device.name).thenReturn("cached sensor")
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })

    assertEquals(ConnectedPeerSnapshot(address, "cached sensor"), radio.resolveDirectoryPeer(address))
    verify(device).name
    val original = SecurityException("cached name denied")
    `when`(device.name).thenThrow(original)
    assertSame(original, assertThrows(SecurityException::class.java) { radio.resolveDirectoryPeer(address) })
    verify(adapter, never()).startDiscovery()
    verify(device, never()).createBond()
    verify(device, never()).connectGatt(any(), anyBoolean(), any(), anyInt())
  }

  @Test fun originAddressRejectsInvalidIdsBeforeAnyAndroidLookup() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })

    assertThrows(IllegalArgumentException::class.java) { radio.resolveDirectoryPeer("not-an-address") }
    verifyNoInteractions(manager, adapter)
  }

  @Test fun originAddressPreservesPermissionAndAdapterReadinessFailures() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    val address = "AA:BB:CC:DD:EE:03"
    val permission = SecurityException("connect permission denied")
    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
    `when`(manager.getConnectedDevices(BluetoothProfile.GATT)).thenThrow(permission)
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    assertSame(permission, assertThrows(SecurityException::class.java) { radio.resolveDirectoryPeer(address) })

    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_OFF)
    assertThrows(IllegalStateException::class.java) { radio.resolveDirectoryPeer(address) }
    verify(adapter, never()).getRemoteDevice(anyString())
  }

  @Test fun originAddressPreservesAdapterLookupFailureIdentity() {
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    val address = "AA:BB:CC:DD:EE:05"
    val original = SecurityException("adapter lookup denied")
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
    `when`(manager.adapter).thenReturn(adapter)
    `when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
    `when`(manager.getConnectedDevices(BluetoothProfile.GATT)).thenReturn(emptyList())
    `when`(adapter.bondedDevices).thenReturn(emptySet())
    `when`(adapter.getRemoteDevice(address)).thenThrow(original)
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })

    assertSame(original, assertThrows(SecurityException::class.java) { radio.resolveDirectoryPeer(address) })
    verify(adapter).getRemoteDevice(address)
    verify(adapter, never()).startDiscovery()
  }

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
