// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/AndroidGraphPermitTest.kt
package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothProfile
import android.content.Context
import android.content.BroadcastReceiver
import android.content.Intent
import android.content.IntentFilter
import com.sfourdrinier.unifiedblemanager.rustcore.OwnedOrderedGroupPort
import com.sfourdrinier.unifiedblemanager.rustcore.OwnedRadioPort
import com.sfourdrinier.unifiedblemanager.rustcore.GraphAuthorityPortResponse
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.*
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

class AndroidGraphPermitTest {
  @Test fun sharedGattDiscoveryAndGraphAreAttributedOnlyToTheAdmittingOwner() {
    val f = Fixture()
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    val firstIdle = first.graphOwnerStatus()
    val secondIdle = second.graphOwnerStatus()
    doReturn(true).`when`(f.gatt).discoverServices()
    doReturn(listOf(f.service)).`when`(f.gatt).services
    doReturn(BluetoothGattService.SERVICE_TYPE_PRIMARY).`when`(f.service).type
    var result: Result<com.sfourdrinier.unifiedblemanager.rustcore.ScopedGattGraph>? = null
    first.discoverScoped(f.peer) { result = it }
    verify(f.gatt).discoverServices()
    assertNotEquals("admitted discovery must appear in its owner's snapshot", firstIdle, first.graphOwnerStatus())
    assertEquals(1, first.graphOwnerStatus().discoveryWork)
    assertEquals("a different owner must remain idle", secondIdle, second.graphOwnerStatus())
    f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
    assertTrue(checkNotNull(result).isSuccess)
    assertNotEquals("published physical graph remains owned after discovery", firstIdle, first.graphOwnerStatus())
    assertEquals(0, first.graphOwnerStatus().discoveryWork)
    assertEquals(1, first.graphOwnerStatus().nativeGraphs)
    assertEquals(secondIdle, second.graphOwnerStatus())
    f.radio.nativeGattCallback().onServiceChanged(f.gatt)
    assertEquals(0, first.graphOwnerStatus().nativeGraphs)
    assertEquals(secondIdle, second.graphOwnerStatus())
  }

  @Test fun cancellingQueuedForeignDiscoveryDoesNotRemoveTheActiveOwnerCallback() {
    val f = Fixture()
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    val secondIdle = second.graphOwnerStatus()
    doReturn(true).`when`(f.gatt).discoverServices()
    doReturn(listOf(f.service)).`when`(f.gatt).services
    doReturn(BluetoothGattService.SERVICE_TYPE_PRIMARY).`when`(f.service).type
    var firstResult: Result<com.sfourdrinier.unifiedblemanager.rustcore.ScopedGattGraph>? = null
    var secondResult: Result<com.sfourdrinier.unifiedblemanager.rustcore.ScopedGattGraph>? = null
    first.discoverScoped(f.peer) { firstResult = it }
    val queued = second.discoverScoped(f.peer) { secondResult = it }
    assertNotEquals(secondIdle, second.graphOwnerStatus())
    assertEquals(1, second.graphOwnerStatus().discoveryWork)
    assertTrue(second.cancel(queued))
    assertTrue(checkNotNull(secondResult).isFailure)
    assertEquals(secondIdle, second.graphOwnerStatus())
    f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
    assertTrue(checkNotNull(firstResult).isSuccess)
    verify(f.gatt, times(1)).discoverServices()
  }

  @Test fun cancelledDiscoveryRetainsItsOwnerUntilLateCallbackReturnsAfterFence() {
    val f = Fixture()
    val first = OwnedRadioPort(f.radio) {}
    val foreign = OwnedRadioPort(f.radio) {}
    val idle = first.graphOwnerStatus()
    val foreignIdle = foreign.graphOwnerStatus()
    doReturn(true).`when`(f.gatt).discoverServices()
    val operation = first.discoverScoped(f.peer) { }
    assertTrue(first.cancel(operation))
    assertNotEquals(idle, first.graphOwnerStatus())
    assertEquals(1, first.graphOwnerStatus().discoveryWork)
    assertEquals(foreignIdle, foreign.graphOwnerStatus())
    first.fenceGraphOwner()
    val fencedWithDebt = first.graphOwnerStatus()
    f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
    assertNotEquals(fencedWithDebt, first.graphOwnerStatus())
    assertEquals(0, first.graphOwnerStatus().discoveryWork)
    assertEquals(0, first.graphOwnerStatus().nativeGraphs)
    assertEquals(foreignIdle, foreign.graphOwnerStatus())
  }

  @Test fun discoveryCallbackRemainsChargedUntilTheConsumerReturns() {
    val f = Fixture()
    val first = OwnedRadioPort(f.radio) {}
    val foreign = OwnedRadioPort(f.radio) {}
    doReturn(true).`when`(f.gatt).discoverServices()
    doReturn(listOf(f.service)).`when`(f.gatt).services
    doReturn(BluetoothGattService.SERVICE_TYPE_PRIMARY).`when`(f.service).type
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    first.discoverScoped(f.peer) { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
    val callback = thread { f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS) }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    first.fenceGraphOwner()
    val during = first.graphOwnerStatus()
    assertEquals(1, during.discoveryWork)
    val foreignBefore = foreign.graphOwnerStatus()
    release.countDown()
    callback.join(5000)
    assertFalse(callback.isAlive)
    assertNotEquals(during, first.graphOwnerStatus())
    assertEquals(0, first.graphOwnerStatus().discoveryWork)
    assertEquals(1, first.graphOwnerStatus().nativeGraphs)
    assertEquals(foreignBefore, foreign.graphOwnerStatus())
  }

  @Test fun synchronousCallbackRemainsChargedUntilPhysicalInvocationReturns() {
    val f = Fixture()
    val port = OwnedRadioPort(f.radio) {}
    doReturn(listOf(f.service)).`when`(f.gatt).services
    doReturn(BluetoothGattService.SERVICE_TYPE_PRIMARY).`when`(f.service).type
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    doAnswer {
      f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
      entered.countDown()
      check(release.await(5, TimeUnit.SECONDS))
      true
    }.`when`(f.gatt).discoverServices()
    val worker = thread { port.discoverScoped(f.peer) { assertTrue(it.isSuccess) } }
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      port.fenceGraphOwner()
      assertEquals(1, port.graphOwnerStatus().discoveryWork)
    } finally {
      release.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
    assertEquals(0, port.graphOwnerStatus().discoveryWork)
    assertEquals(1, port.graphOwnerStatus().nativeGraphs)
  }

  @Test fun fencingQueuedDiscoveryPreventsItsLaterPhysicalAdmission() {
    val f = Fixture()
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    doReturn(true).`when`(f.gatt).discoverServices()
    doReturn(listOf(f.service)).`when`(f.gatt).services
    doReturn(BluetoothGattService.SERVICE_TYPE_PRIMARY).`when`(f.service).type
    var result: Result<com.sfourdrinier.unifiedblemanager.rustcore.ScopedGattGraph>? = null
    first.discoverScoped(f.peer) { }
    second.discoverScoped(f.peer) { result = it }
    assertEquals(1, second.graphOwnerStatus().discoveryWork)
    second.fenceGraphOwner()
    f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
    assertTrue(checkNotNull(result).isFailure)
    assertEquals(0, second.graphOwnerStatus().discoveryWork)
    assertEquals(0, second.graphOwnerStatus().nativeGraphs)
    assertEquals(1, first.graphOwnerStatus().nativeGraphs)
    verify(f.gatt, times(1)).discoverServices()
  }

  @Test fun foreignDiscoveryCannotClaimCancelledOwnersDelayedCallback() {
    val f = Fixture()
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    doReturn(true).`when`(f.gatt).discoverServices()
    val operation = first.discoverScoped(f.peer) { }
    assertTrue(first.cancel(operation))
    var result: Result<com.sfourdrinier.unifiedblemanager.rustcore.ScopedGattGraph>? = null
    second.discoverScoped(f.peer) { result = it }
    assertTrue(checkNotNull(result).isFailure)
    assertEquals(1, first.graphOwnerStatus().discoveryWork)
    assertEquals(0, second.graphOwnerStatus().discoveryWork)
    f.radio.nativeGattCallback().onServicesDiscovered(f.gatt, BluetoothGatt.GATT_SUCCESS)
    assertEquals(0, first.graphOwnerStatus().discoveryWork)
    assertEquals(0, second.graphOwnerStatus().nativeGraphs)
    verify(f.gatt, times(1)).discoverServices()
  }

  @Test fun identicalAuthorityTokensBelongToTheirPortOwners() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val first = OwnedOrderedGroupPort(radio)
    val second = OwnedOrderedGroupPort(radio)
    fun open(port: OwnedOrderedGroupPort): Pair<ULong, ULong> {
      val reservation = port.orderedGraphAuthority("reserve", 1uL) as GraphAuthorityPortResponse.Reservation
      val receipt = (reservation.status as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
      val opened = port.orderedGraphAuthority("open", receipt) as GraphAuthorityPortResponse.Open
      return receipt to (opened.status as OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Opened).authority
    }
    val (firstReceipt, firstAuthority) = open(first)
    val (secondReceipt, secondAuthority) = open(second)
    first.orderedGraphAuthority("close", firstAuthority)
    assertEquals(OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Closed(firstAuthority),
      (first.orderedGraphAuthority("status", firstReceipt) as GraphAuthorityPortResponse.Open).status)
    assertEquals(OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Opened(secondAuthority),
      (second.orderedGraphAuthority("status", secondReceipt) as GraphAuthorityPortResponse.Open).status)
  }

  @Test fun fencingOneOwnerKeepsAnotherOwnerAndExistingReceiptsUsable() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val first = OwnedRadioPort(radio) {}
    val second = OwnedRadioPort(radio) {}
    val existing = first.orderedGraphAuthority("reserve", 9uL)
    first.fenceGraphOwner()
    assertEquals(existing, first.orderedGraphAuthority("reserve", 9uL))
    try { first.orderedGraphAuthority("reserve", 10uL); fail("late admission must be refused") }
    catch (_: IllegalStateException) { }
    assertTrue(second.orderedGraphAuthority("reserve", 10uL) is GraphAuthorityPortResponse.Reservation)
    val firstStatus = first.graphOwnerStatus()
    val secondStatus = second.graphOwnerStatus()
    assertEquals(UUID.fromString(first.orderedAdmissionOwner(3)), firstStatus.incarnation)
    assertEquals(1, firstStatus.authorityReservations)
    assertEquals(existing, GraphAuthorityPortResponse.Reservation(firstStatus.reservationReceipts.getValue(9uL).status))
    assertFalse(firstStatus.reservationReceipts.getValue(9uL).fenced)
    assertFalse(secondStatus.fenced)
    first.orderedGraphAuthority("reserve-fence", 10uL)
    val retried = first.graphOwnerStatus()
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.NotCreated, retried.reservationReceipts.getValue(10uL).status)
    assertTrue(retried.reservationReceipts.getValue(10uL).fenced)
    assertEquals(1, firstStatus.reservationReceipts.size) // Earlier observations are immutable snapshots.
    assertEquals(1, secondStatus.reservationReceipts.size)
  }

  @Test fun legacyGraphOwnerFenceDoesNotFenceASeparatePortOwner() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val port = OwnedOrderedGroupPort(radio)
    radio.fenceGraphOwner()
    assertTrue(port.orderedGraphAuthority("reserve", 1uL) is GraphAuthorityPortResponse.Reservation)
  }

  @Test fun graphOwnerSnapshotCountsCallsWaitingOnGraphLock() {
    val f = Fixture()
    val owner = f.radio.newGraphOwner(UUID.randomUUID())
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val started = CountDownLatch(1)
    var snapshot: OwnedAndroidGattRadio.GraphOwnerStatus? = null
    var outcome: OwnedAndroidGattRadio.GraphPermitResult? = null
    val holder = thread {
      f.radio.withGraphOwner(owner) {
        f.radio.reserveGraphAuthority(1uL)
        val receipt = (f.radio.statusGraphAuthorityReservation(1uL) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
        val authority = (f.radio.openGraphAuthority(receipt) as OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Opened).authority
        val request = OwnedAndroidGattRadio.GraphPermitRequest(authority, 1uL, f.peer, "connection", f.graph)
        val permit = (f.radio.reserveGraphPermit(request) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
        val key = OwnedAndroidGattRadio.GraphPermitKey(authority, f.peer, "connection", f.graph, permit)
        outcome = f.radio.acquireGraphPermit(key) {
          entered.countDown()
          check(started.await(5, TimeUnit.SECONDS))
          check(release.await(5, TimeUnit.SECONDS))
          f.radio.fenceGraphOwner()
          snapshot = f.radio.graphOwnerStatus(owner)
          true
        }
      }
    }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    var refused = false
    val waiter = thread {
      f.radio.withGraphOwner(owner) {
        started.countDown()
        try { f.radio.reserveGraphAuthority(2uL) } catch (_: IllegalStateException) { refused = true }
      }
    }
    try {
      assertTrue(started.await(5, TimeUnit.SECONDS))
      release.countDown()
      holder.join(5000)
      waiter.join(5000)
      assertFalse(holder.isAlive)
      assertFalse(waiter.isAlive)
      assertEquals(2, snapshot?.callsInFlight)
      assertEquals(true, snapshot?.fenced)
      assertEquals(1, snapshot?.authorities)
      assertTrue(refused)
      assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, outcome)
      assertEquals(1, f.radio.graphOwnerStatus(owner).authorityReservations)
    } finally { release.countDown(); holder.join(5000); waiter.join(5000) }
  }

  @Test fun reentrantReservationRetirementCannotPublishAnOrphanPermit() {
    val f = Fixture()
    val outcome = f.radio.acquireGraphPermit(f.key) {
      assertEquals(OwnedAndroidGattRadio.GraphPermitResult.FORGOTTEN, f.radio.forgetGraphPermit(f.key))
      f.radio.fenceGraphPermit(f.request(f.key.authority, 1uL))
      f.radio.forgetGraphPermitReservation(f.request(f.key.authority, 1uL))
      true
    }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, outcome)
    assertEquals(0, f.radio.graphStoreStatus().activePermits)
    assertEquals(0, f.radio.graphStoreStatus().permitReceipts)
  }

  @Test fun reentrantDuplicateAcquireRetainsTheOriginalReceipt() {
    val f = Fixture()
    val outcome = f.radio.acquireGraphPermit(f.key) {
      assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
      true
    }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, outcome)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.COMMITTED, f.radio.commitGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
  }

  @Test fun collidingPermitKeysCannotReleaseAnotherOwnersGate() {
    val f = Fixture()
    val other = OwnedOrderedGroupPort(f.radio)
    val reservation = other.orderedGraphAuthority("reserve", 101uL) as GraphAuthorityPortResponse.Reservation
    val receipt = (reservation.status as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
    val opened = other.orderedGraphAuthority("open", receipt) as GraphAuthorityPortResponse.Open
    val authority = (opened.status as OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Opened).authority
    val request = OwnedAndroidGattRadio.GraphPermitRequest(authority, 1uL, f.peer, "connection", f.graph)
    val permit = (other.orderedGraphPermitReservation("reserve", request) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
    val key = OwnedAndroidGattRadio.GraphPermitKey(authority, f.peer, "connection", f.graph, permit)
    assertEquals(f.key, key)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, other.orderedGraphPermit("commit", key) { true })
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, other.orderedGraphPermit("release", key) { true })
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.COMMITTED, f.radio.commitGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
  }

  @Test fun graphOwnerIdentityIsUniquePerRadio() {
    val first = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val second = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val identity = UUID.randomUUID()
    val firstOwner = first.newGraphOwner(identity)
    second.newGraphOwner(identity)
    try { first.newGraphOwner(identity); fail("duplicate owner must be rejected") } catch (_: IllegalStateException) { }
    try { second.graphOwnerStatus(firstOwner); fail("owner from another radio must be rejected") } catch (_: IllegalArgumentException) { }
  }

  @Test fun graphStoreObservationCountsCallbackWhileItIsExecutingOutsideTheLock() {
    val f = Fixture()
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    f.onWake = { entered.countDown(); check(resume.await(5, TimeUnit.SECONDS)) }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(1))
    val worker = thread { f.radio.releaseGraphPermit(f.key) }
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      val active = f.radio.graphStoreStatus()
      assertEquals(1, active.drainingGates)
      assertEquals(1, active.admissionGates)
    } finally {
      resume.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
    assertEquals(0, f.radio.graphStoreStatus().drainingGates)
  }

  @Test fun graphStoreObservationRetainsReceiptsAndCountsDeferredWork() {
    val f = Fixture()
    val initial = f.radio.graphStoreStatus()
    assertEquals(1, initial.authorities)
    assertEquals(1, initial.authorityReservations)
    assertEquals(1, initial.authorityOpenReceipts)
    assertEquals(1, initial.permitReservations)
    assertEquals(1, initial.nativeGraphs)
    assertEquals(0, initial.admissionGates)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(1))
    val active = f.radio.graphStoreStatus()
    assertEquals(1, active.permitReceipts)
    assertEquals(1, active.admissionGates)
    assertEquals(1, active.activePermits)
    assertTrue(active.deferredCallbacks > 0)
    try { f.radio.closeGraphAuthority(f.key.authority); fail("authority closed with retained permit") }
    catch (_: IllegalStateException) { }
    assertEquals(1, f.radio.graphStoreStatus().authorities)
    assertEquals(1, f.radio.graphStoreStatus().permitReceipts)
    f.radio.releaseGraphPermit(f.key)
    val released = f.radio.graphStoreStatus()
    assertEquals(0, released.admissionGates)
    assertEquals(1, released.permitReceipts)
    f.retire(f.key, 1uL)
    f.retireAuthority(101uL, f.key.authority)
    val retired = f.radio.graphStoreStatus()
    assertEquals(0, retired.authorities)
    assertEquals(0, retired.authorityReservations)
    assertEquals(0, retired.authorityOpenReceipts)
    assertEquals(1, retired.nativeGraphs)
  }

  private class Fixture(ownedConnection: Boolean = false) {
    val peer = "AA:BB:CC:DD:EE:FF"
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val characteristicId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val context = mock(Context::class.java)
    val manager = mock(BluetoothManager::class.java)
    val adapter = mock(BluetoothAdapter::class.java)
    val scheduled = mutableListOf<() -> Unit>()
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, action -> scheduled.add(action); true })
    val receiverOwner = OwnedAndroidGattRadio.ReceiverOwner()
    val connectionEvents = mutableListOf<Boolean>()
    val graph: AndroidNativeGraphIdentity
    val owner = AndroidOrderedGroupOwner(radio)
    var onWake: () -> Unit = {}
    val opened: AndroidOrderedGroupOwner.Opened
    val key: OwnedAndroidGattRadio.GraphPermitKey
    fun openAuthority(token: ULong): ULong {
      val receipt = (radio.reserveGraphAuthority(token) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
      return (radio.openGraphAuthority(receipt) as OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Opened).authority
    }
    fun retireAuthority(token: ULong, authority: ULong) {
      val receipt = (radio.statusGraphAuthorityReservation(token) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
      radio.closeGraphAuthority(authority)
      radio.forgetGraphAuthority(authority)
      radio.fenceGraphAuthorityReservation(token)
      radio.forgetGraphAuthorityOpen(receipt)
      radio.forgetGraphAuthorityReservation(token)
    }
    fun request(authority: ULong, id: ULong) = OwnedAndroidGattRadio.GraphPermitRequest(authority, id, peer, "connection", graph)
    fun reserve(authority: ULong, id: ULong): OwnedAndroidGattRadio.GraphPermitKey {
      val issued = radio.reserveGraphPermit(request(authority, id)) as OwnedAndroidGattRadio.GraphReservationStatus.Issued
      return OwnedAndroidGattRadio.GraphPermitKey(authority, peer, "connection", graph, issued.permit)
    }
    fun retire(key: OwnedAndroidGattRadio.GraphPermitKey, requestId: ULong) {
      radio.forgetGraphPermit(key)
      radio.fenceGraphPermit(request(key.authority, requestId))
      radio.forgetGraphPermitReservation(request(key.authority, requestId))
    }
    init {
      doReturn(manager).`when`(context).getSystemService(Context.BLUETOOTH_SERVICE)
      doReturn(adapter).`when`(manager).adapter
      doReturn(BluetoothAdapter.STATE_ON).`when`(adapter).state
      doReturn(peer).`when`(device).address
      doReturn(device).`when`(gatt).device
      doReturn(serviceId).`when`(service).uuid
      doReturn(listOf(characteristic)).`when`(service).characteristics
      doReturn(characteristicId).`when`(characteristic).uuid
      doReturn(service).`when`(characteristic).service
      if (ownedConnection) {
        doReturn(device).`when`(adapter).getRemoteDevice(peer)
        doReturn(gatt).`when`(device).connectGatt(eq(context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
        doReturn(true).`when`(gatt).discoverServices()
        doReturn(listOf(service)).`when`(gatt).services
        radio.connectOwned(peer, false, 0, GattConnectAttempt(), receiverOwner,
          { _, connected, _, _ -> connectionEvents.add(connected) }, {})
        radio.discover(peer) { check(it.isSuccess) }
        radio.nativeGattCallback().onServicesDiscovered(gatt, BluetoothGatt.GATT_SUCCESS)
      } else radio.attachConnectedGatt(peer, gatt, listOf(service))
      graph = radio.withNativeGraph(peer) { Unit }.first
      opened = owner.open(1uL, peer, mapOf(1uL to OwnedAndroidNotificationPath(serviceId, 0, characteristicId, 0)), 8, 32, expectedGraph = graph) { _, _ -> onWake() }
      key = reserve(openAuthority(101uL), 1uL)
    }
  }

  @Test fun serviceChangeBeforeCommitInvalidatesAndLaterValueCannotCrossFence() {
    val f = Fixture()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    f.radio.nativeGattCallback().onServiceChanged(f.gatt)
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(9))
    assertTrue(f.owner.drain(f.opened.generation, 1uL).events.isEmpty())
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.INVALIDATED, f.radio.commitGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    val events = f.owner.drain(f.opened.generation, 1uL).events
    assertEquals(1, events.size)
    assertEquals(OrderedNotificationSource.Reason.DATABASE_CHANGED, (events.single() as OrderedNotificationSource.Event.Terminal).reason)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.acquireGraphPermit(f.key))
  }

  @Test fun disconnectAfterCommitIsOrderedAheadOfLaterValue() {
    val f = Fixture()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.COMMITTED, f.radio.commitGraphPermit(f.key))
    f.radio.nativeGattCallback().onConnectionStateChange(f.gatt, 0, BluetoothProfile.STATE_DISCONNECTED)
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(2))
    verify(f.gatt, never()).close()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.COMMITTED, f.radio.commitGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    verify(f.gatt).close()
    val events = f.owner.drain(f.opened.generation, 1uL).events
    assertEquals(OrderedNotificationSource.Reason.CONNECTION_LOST, (events.single() as OrderedNotificationSource.Event.Terminal).reason)
  }
  @Test fun explicitDisconnectWaiterRemainsOwnedUntilRelease() {
    val f = Fixture()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    var completionCount = 0
    var deferredFailure: OwnedRadioTeardownFailure? = null
    f.radio.onCleanupFailure = { deferredFailure = it }
    f.radio.disconnect(f.peer) { completionCount++ }
    verify(f.gatt, never()).disconnect()
    assertEquals(0, completionCount)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.INVALIDATED, f.radio.commitGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    f.radio.nativeGattCallback().onConnectionStateChange(f.gatt, 0, BluetoothProfile.STATE_DISCONNECTED)
    assertNull(deferredFailure?.throwable?.toString(), deferredFailure)
    assertEquals(1, completionCount)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
  }

  @Test fun saturatedPermitControlsDrainBeforeCloseDeadlineIsArmed() {
    val f = Fixture()
    val results = mutableListOf<OwnedRadioTeardownFailure?>()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    repeat(64) { f.radio.disconnect(f.peer) { results.add(it) } }
    assertTrue(f.scheduled.isEmpty())
    verify(f.gatt, never()).close()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    assertEquals(1, f.scheduled.size)
    f.scheduled.single().invoke()
    verify(f.gatt, times(1)).close()
    assertEquals(64, results.size)
    assertTrue(results.all { it == null })
  }

  @Test fun disconnectFencePreventsANewPermitBeforeItsDeadline() {
    val f = Fixture()
    val results = mutableListOf<OwnedRadioTeardownFailure?>()
    val cleanup = mutableListOf<OwnedRadioTeardownFailure>()
    val refusal = IllegalStateException("close refused")
    doThrow(refusal).doNothing().`when`(f.gatt).close()
    f.radio.onCleanupFailure = { cleanup.add(it) }
    f.radio.disconnect(f.peer) { results.add(it) }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.acquireGraphPermit(f.key))
    f.scheduled.single().invoke()
    assertEquals(1, results.size)
    assertSame(refusal, results.single()?.throwable)
    assertSame(refusal, cleanup.single().throwable)
    val retry = mutableListOf<OwnedRadioTeardownFailure?>()
    assertNull(f.radio.disconnect(f.peer) { retry.add(it) })
    assertEquals(listOf<OwnedRadioTeardownFailure?>(null), retry)
    verify(f.gatt, times(2)).close()
  }

  @Test fun deferredConnectionDeliveryRemainsChargedToRetiredAttemptOwner() {
    val f = Fixture(ownedConnection = true)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    f.radio.nativeGattCallback().onConnectionStateChange(f.gatt, 0, BluetoothProfile.STATE_CONNECTED)
    val retirement = f.radio.retireReceiverOwner(f.receiverOwner)
    assertTrue("the route and deferred delivery both remain owned", retirement.callbacks >= 2)
    assertTrue(f.connectionEvents.isEmpty())
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    assertEquals(listOf(true), f.connectionEvents)
    assertEquals(1, f.radio.retireReceiverOwner(f.receiverOwner).callbacks)
    f.radio.nativeGattCallback().onConnectionStateChange(f.gatt, 0, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals(0, f.radio.retireReceiverOwner(f.receiverOwner).callbacks)
  }

  @Test fun overflowInvalidatesBeforeCommitAndIsReportedOnRelease() {
    val f = Fixture()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    repeat(70) { f.radio.nativeGattCallback().onMtuChanged(f.gatt, 23, 0) }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.INVALIDATED, f.radio.commitGraphPermit(f.key))
    var overflow = 0L
    f.radio.onAdmissionOverflow = { _, _, controls -> overflow += controls }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    assertEquals(6L, overflow)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
  }

  @Test fun adapterLossRetainsExactTeardownWhenOrdinaryPermitControlsAreFull() {
    val f = Fixture()
    var receiver: BroadcastReceiver? = null
    doAnswer { call -> receiver = call.getArgument(0); null }
      .`when`(f.context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    f.radio.registerAdapterStateReceiver()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    repeat(64) { f.radio.nativeGattCallback().onMtuChanged(f.gatt, 23, 0) }
    val intent = mock(Intent::class.java)
    doReturn(BluetoothAdapter.ACTION_STATE_CHANGED).`when`(intent).action
    doReturn(BluetoothAdapter.STATE_OFF).`when`(intent).getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)
    checkNotNull(receiver).onReceive(f.context, intent)
    verify(f.gatt, never()).close()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    verify(f.gatt).close()
  }

  @Test fun overflowAfterQueuedLifecycleIsRecordedBeforeTerminalWake() {
    val f = Fixture()
    var terminal: OrderedNotificationSource.Event.Terminal<BluetoothGattCharacteristic>? = null
    f.onWake = {
      val records = f.owner.drain(f.opened.generation, 1uL).events
      records.filterIsInstance<OrderedNotificationSource.Event.Terminal<BluetoothGattCharacteristic>>().firstOrNull()?.let { terminal = it }
    }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.COMMITTED, f.radio.commitGraphPermit(f.key))
    f.radio.nativeGattCallback().onServiceChanged(f.gatt)
    repeat(70) { f.radio.nativeGattCallback().onMtuChanged(f.gatt, 23, 0) }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    assertEquals(OrderedNotificationSource.Reason.DATABASE_CHANGED, terminal?.reason)
    assertNotNull(terminal?.rejectedLoss)
  }

  @Test fun staleConnectionAndWrongKeyCannotClaimOrRelease() {
    val f = Fixture()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.acquireGraphPermit(f.key) { false })
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.acquireGraphPermit(f.key) { true })
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.FORGOTTEN, f.radio.forgetGraphPermit(f.key))
    f.radio.fenceGraphPermit(f.request(f.key.authority, 1uL))
    f.radio.forgetGraphPermitReservation(f.request(f.key.authority, 1uL))
    val next = f.reserve(f.key.authority, 2uL)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(next) { true })
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.COMMITTED, f.radio.commitGraphPermit(next))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.COMMITTED, f.radio.commitGraphPermit(next))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.releaseGraphPermit(next.copy(connection = "other")))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.BUSY, f.radio.forgetGraphPermit(next))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(next))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(next))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.FORGOTTEN, f.radio.forgetGraphPermit(next))
  }

  @Test fun outOfOrderRetirementCannotEvictUnresolvedOldestPermit() {
    val f = Fixture()
    val oldest = f.key.copy(id = 1uL)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(oldest))
    for (id in 2uL..256uL) {
      val key = f.reserve(f.key.authority, id)
      assertEquals(OwnedAndroidGattRadio.GraphPermitResult.BUSY, f.radio.acquireGraphPermit(key))
      assertEquals(OwnedAndroidGattRadio.GraphPermitResult.FORGOTTEN, f.radio.forgetGraphPermit(key))
      f.radio.fenceGraphPermit(f.request(key.authority, id))
      f.radio.forgetGraphPermitReservation(f.request(key.authority, id))
      assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.acquireGraphPermit(key))
    }
    val beyondWindow = f.key.copy(id = 257uL)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.acquireGraphPermit(beyondWindow))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(oldest))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(oldest))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.FORGOTTEN, f.radio.forgetGraphPermit(oldest))
    f.radio.fenceGraphPermit(f.request(oldest.authority, 1uL))
    f.radio.forgetGraphPermitReservation(f.request(oldest.authority, 1uL))
    assertEquals(beyondWindow, f.reserve(f.key.authority, 257uL))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(beyondWindow))
  }

  @Test fun disconnectDeadlineKeepsWaiterUntilPermitReleases() {
    val f = Fixture()
    var completions = 0
    f.radio.disconnect(f.peer) { completions++ }
    assertTrue(f.scheduled.isNotEmpty())
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.acquireGraphPermit(f.key))
    // An already accepted disconnect retires the graph before its deadline; it
    // cannot be claimed by a new handoff while the close remains pending.
    f.scheduled.first().invoke()
    assertEquals(1, completions)
    verify(f.gatt).close()
  }

  @Test fun destroyWaitsForPermitAndResumesAfterRelease() {
    val f = Fixture()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    assertFalse(f.radio.destroy().isSuccessful)
    verify(f.gatt, never()).close()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.INVALIDATED, f.radio.commitGraphPermit(f.key))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    verify(f.gatt).close()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
  }

  @Test fun ownerFenceInvalidatesPermitButRetainsDeferredValueAndExactReceipt() {
    val f = Fixture()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(7))
    f.radio.fenceGraphOwner()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.INVALIDATED, f.radio.commitGraphPermit(f.key))
    assertTrue(f.owner.drain(f.opened.generation, 1uL).events.isEmpty())
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    val events = f.owner.drain(f.opened.generation, 1uL).events
    assertArrayEquals(byteArrayOf(7), (events.single() as OrderedNotificationSource.Event.Value).bytes)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
    f.retire(f.key, 1uL)
    f.retireAuthority(101uL, f.key.authority)
  }

  @Test fun destroyWaitsForDeferredGraphCallbackToReturn() {
    val f = Fixture()
    val entered = CountDownLatch(1)
    val leave = CountDownLatch(1)
    f.onWake = { entered.countDown(); check(leave.await(5, TimeUnit.SECONDS)) }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    f.radio.nativeGattCallback().onCharacteristicChanged(f.gatt, f.characteristic, byteArrayOf(7))
    val worker = thread { f.radio.releaseGraphPermit(f.key) }
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      assertFalse("callback still owns the graph", f.radio.destroy().isSuccessful)
    } finally {
      leave.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
    assertTrue(f.radio.destroy().isSuccessful)
  }

  @Test fun destroyFencesNewPermitReservationsButRetainsExistingReceipt() {
    val f = Fixture()
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.key))
    assertFalse(f.radio.destroy().isSuccessful)
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.Issued(f.key.id),
      f.radio.reserveGraphPermit(f.request(f.key.authority, 1uL)))
    assertThrows(IllegalStateException::class.java) {
      f.radio.reserveGraphPermit(f.request(f.key.authority, 2uL))
    }
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(f.key))
  }

  @Test fun authorityNamespacesIsolateSamePermitIdAndRetireSafely() {
    val f = Fixture()
    val first = f.key.copy(id = 1uL)
    assertEquals(first.authority, f.openAuthority(101uL))
    val secondAuthority = f.openAuthority(102uL)
    val second = f.reserve(secondAuthority, 1uL)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(first))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.BUSY, f.radio.acquireGraphPermit(second))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.FORGOTTEN, f.radio.forgetGraphPermit(second))
    f.radio.fenceGraphPermit(f.request(secondAuthority, 1uL))
    f.radio.forgetGraphPermitReservation(f.request(secondAuthority, 1uL))
    f.radio.closeGraphAuthority(secondAuthority)
    f.radio.closeGraphAuthority(secondAuthority)
    f.radio.forgetGraphAuthority(secondAuthority)
    f.radio.forgetGraphAuthority(secondAuthority)
    val secondReceipt = (f.radio.fenceGraphAuthorityReservation(102uL) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
    f.radio.forgetGraphAuthorityOpen(secondReceipt)
    f.radio.forgetGraphAuthorityReservation(102uL)
    assertEquals(1, f.radio.graphStoreStatus().retiredAuthorities)
    assertEquals(1, f.radio.graphStoreStatus().retiredAuthorityOpens)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.acquireGraphPermit(second))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(first))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.RELEASED, f.radio.releaseGraphPermit(first))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.FORGOTTEN, f.radio.forgetGraphPermit(first))
    f.radio.fenceGraphPermit(f.request(first.authority, 1uL))
    f.radio.forgetGraphPermitReservation(f.request(first.authority, 1uL))
    f.radio.closeGraphAuthority(first.authority)
    f.radio.forgetGraphAuthority(first.authority)
    val firstReceipt = (f.radio.fenceGraphAuthorityReservation(101uL) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
    f.radio.forgetGraphAuthorityOpen(firstReceipt)
    f.radio.forgetGraphAuthorityReservation(101uL)
    assertEquals(0, f.radio.graphStoreStatus().retiredAuthorities)
    assertEquals(0, f.radio.graphStoreStatus().retiredAuthorityOpens)
    val replacement = f.openAuthority(103uL)
    assertNotEquals(first.authority, replacement)
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.STALE, f.radio.releaseGraphPermit(first))
    assertEquals(OwnedAndroidGattRadio.GraphPermitResult.ACQUIRED, f.radio.acquireGraphPermit(f.reserve(replacement, 1uL)))
  }

  @Test fun authorityWindowRetainsOldestUntilContiguousRetirement() {
    val f = Fixture()
    val first = f.key.authority
    val later = (2uL..64uL).map { it to f.openAuthority(100uL + it) }
    later.forEach { (token, id) -> f.retireAuthority(100uL + token, id) }
    assertThrows(IllegalStateException::class.java) { f.openAuthority(1000uL) }
    assertEquals(first, f.openAuthority(101uL))
    f.retire(f.key, 1uL)
    f.retireAuthority(101uL, first)
    assertEquals(65uL, f.openAuthority(1000uL))
  }

  @Test fun reservationFenceProvesNoEffectAndIssuedReceiptReplays() {
    val f = Fixture()
    val unknown = f.request(f.key.authority, 2uL)
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.Pending, f.radio.statusGraphPermit(unknown))
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.NotCreated, f.radio.fenceGraphPermit(unknown))
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.NotCreated, f.radio.reserveGraphPermit(unknown))
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.NotCreated, f.radio.fenceGraphPermit(unknown))
    assertThrows(IllegalArgumentException::class.java) { f.radio.statusGraphPermit(unknown.copy(connection = "wrong")) }
    f.radio.forgetGraphPermitReservation(unknown)
    assertThrows(IllegalStateException::class.java) { f.radio.reserveGraphPermit(unknown) }
    val issued = f.request(f.key.authority, 1uL)
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.Issued(1uL), f.radio.reserveGraphPermit(issued))
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.Issued(1uL), f.radio.fenceGraphPermit(issued))
    assertThrows(IllegalStateException::class.java) { f.radio.forgetGraphPermitReservation(issued) }
    f.radio.forgetGraphPermit(f.key)
    f.radio.forgetGraphPermitReservation(issued)
    f.radio.forgetGraphPermitReservation(issued)
  }

  @Test fun authorityOpenFenceAndLostReplyRetainExactOutcome() {
    val f = Fixture()
    val neverSubmitted = 899uL
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.Pending, f.radio.statusGraphAuthorityReservation(neverSubmitted))
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.NotCreated, f.radio.fenceGraphAuthorityReservation(neverSubmitted))
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.NotCreated, f.radio.reserveGraphAuthority(neverSubmitted))
    f.radio.forgetGraphAuthorityReservation(neverSubmitted)
    f.radio.forgetGraphAuthorityReservation(neverSubmitted)
    val token = 900uL
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.Pending, f.radio.statusGraphAuthorityReservation(token))
    val receipt = (f.radio.reserveGraphAuthority(token) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.Issued(receipt), f.radio.reserveGraphAuthority(token))
    assertEquals(OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Pending, f.radio.statusGraphAuthorityOpen(receipt))
    assertEquals(OwnedAndroidGattRadio.GraphAuthorityOpenStatus.NotCreated, f.radio.fenceGraphAuthorityOpen(receipt))
    assertEquals(OwnedAndroidGattRadio.GraphAuthorityOpenStatus.NotCreated, f.radio.openGraphAuthority(receipt))
    assertEquals(OwnedAndroidGattRadio.GraphReservationStatus.Issued(receipt), f.radio.fenceGraphAuthorityReservation(token))
    f.radio.forgetGraphAuthorityOpen(receipt)
    f.radio.forgetGraphAuthorityOpen(receipt)
    f.radio.forgetGraphAuthorityReservation(token)
    val openedReceipt = (f.radio.reserveGraphAuthority(901uL) as OwnedAndroidGattRadio.GraphReservationStatus.Issued).permit
    val opened = f.radio.openGraphAuthority(openedReceipt) as OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Opened
    assertEquals(opened, f.radio.openGraphAuthority(openedReceipt))
    f.radio.closeGraphAuthority(opened.authority)
    assertEquals(OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Closed(opened.authority), f.radio.statusGraphAuthorityOpen(openedReceipt))
    assertEquals(OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Closed(opened.authority), f.radio.fenceGraphAuthorityOpen(openedReceipt))
    f.radio.forgetGraphAuthority(opened.authority)
    f.radio.fenceGraphAuthorityReservation(901uL)
    f.radio.forgetGraphAuthorityOpen(openedReceipt)
    f.radio.forgetGraphAuthorityReservation(901uL)
  }

}
