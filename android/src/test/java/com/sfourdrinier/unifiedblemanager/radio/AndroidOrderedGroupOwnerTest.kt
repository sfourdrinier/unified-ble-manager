// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/AndroidOrderedGroupOwnerTest.kt
package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattService
import android.content.Context
import java.util.UUID
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.doReturn
import org.mockito.Mockito.mock

class AndroidOrderedGroupOwnerTest {
  @Test fun slotTableIsStableForOwnerAndDistinctForReplacement() {
    val radio = mock(OwnedAndroidGattRadio::class.java)
    val owner = AndroidOrderedGroupOwner(radio)
    assertSame(owner.orderedOpenSlots, owner.orderedOpenSlots)
    assertNotEquals(owner.orderedOpenSlots.incarnation, AndroidOrderedGroupOwner(radio).orderedOpenSlots.incarnation)
  }
  @Test fun unresolvedOpenFailureCannotBeAcknowledged() {
    // A driver that does not establish either installation or no effect leaves a receipt in doubt.
    val owner = AndroidOrderedGroupOwner(mock(OwnedAndroidGattRadio::class.java))
    val peer = "AA:BB:CC:DD:EE:FF"
    val admission = owner.admitOpen(18uL, 2uL, peer)
    val path = OwnedAndroidNotificationPath(UUID.randomUUID(), 0, UUID.randomUUID(), 0)
    try { owner.openAdmitted(admission, peer, mapOf(1uL to path), 1, 1) { _, _ -> }; fail("open unexpectedly succeeded") }
    catch (_: NullPointerException) { }
    val recovery = owner.recoverOpenExact(18uL, 2uL, peer) as AndroidOrderedGroupOwner.OpenRecovery.Failed
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.Indeterminate, recovery.ownership)
    try { owner.acknowledgeOpen(18uL, 2uL, peer); fail("unresolved open acknowledged") } catch (_: IllegalStateException) { }
    assertEquals(recovery, owner.recoverOpenExact(18uL, 2uL, peer))
  }
  @Test fun unknownAcknowledgementCannotClaimSuccessAndOldRetryCannotRetireNewAdmission() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = AndroidOrderedGroupOwner(radio)
    val peer = "AA:BB:CC:DD:EE:FF"
    try { owner.acknowledgeOpen(17uL, 1uL, peer); fail("unknown acknowledgement accepted") } catch (_: IllegalStateException) { }
    val old = owner.admitOpen(17uL, 1uL, peer)
    owner.recoverOpenExact(17uL, 1uL, peer)
    owner.acknowledgeOpen(17uL, 1uL, peer)
    try { owner.admitOpen(17uL, 1uL, peer); fail("acknowledged identity reused") } catch (_: IllegalStateException) { }
    owner.admitOpen(17uL, 2uL, peer)
    owner.acknowledgeOpen(17uL, 1uL, peer)
    try { owner.openAdmitted(old, peer, emptyMap(), 1, 1) { _, _ -> }; fail("old deferred open revived") } catch (_: OrderedOpenNotCreated) { }
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.NotCreated, owner.recoverOpenExact(17uL, 2uL, peer))
  }
  @Test fun fencedAdmissionsRetainExactReceiptsUntilAcknowledgedAndCannotRevive() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = AndroidOrderedGroupOwner(radio)
    val peer = "AA:BB:CC:DD:EE:FF"
    val admission = owner.admitOpen(17uL, 99uL, peer)
    assertSame(admission, owner.admitOpen(17uL, 99uL, peer.lowercase()))
    try { owner.admitOpen(17uL, 100uL, peer); fail("different request admitted") } catch (_: IllegalStateException) { }
    val path = OwnedAndroidNotificationPath(UUID.randomUUID(), 0, UUID.randomUUID(), 0)
    try { owner.open(17uL, peer, mapOf(1uL to path), 1, 1) { _, _ -> }; fail("legacy open bypassed admission") }
    catch (error: IllegalStateException) { assertEquals("ordered group admission is pending", error.message) }
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.NotCreated, owner.recoverOpen(17uL))
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.NotCreated, owner.recoverOpenExact(17uL, 99uL, peer))
    try { owner.recoverOpenExact(17uL, 100uL, peer); fail("wrong request recovered") } catch (_: IllegalStateException) { }
    try { owner.acknowledgeOpen(17uL, 100uL, peer); fail("wrong request acknowledged") } catch (_: IllegalStateException) { }
    try { owner.acknowledgeOpen(17uL, 99uL, "other"); fail("wrong peer acknowledged") } catch (_: IllegalStateException) { }
    owner.acknowledgeOpen(17uL, 99uL, peer)
    owner.acknowledgeOpen(17uL, 99uL, peer)
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.NotCreated, owner.recoverAdmission(admission))
    val otherHost = AndroidOrderedGroupOwner(radio)
    try { otherHost.recoverAdmission(admission); fail("foreign host admission accepted") } catch (_: IllegalStateException) { }
    try { otherHost.acknowledgeOpen(17uL, 99uL, peer); fail("foreign host acknowledged receipt") } catch (_: IllegalStateException) { }
    try { owner.openAdmitted(admission, peer, emptyMap(), 1, 1) { _, _ -> }; fail("fenced open started") } catch (_: OrderedOpenNotCreated) { }
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.Indeterminate, owner.recoverOpen(17uL))

    val pending = (100uL until 164uL).map { owner.admitOpen(it, it, peer) }
    try { owner.admitOpen(164uL, 164uL, peer); fail("capacity exceeded") } catch (_: IllegalStateException) { }
    try { owner.acknowledgeOpen(100uL, 100uL, peer); fail("pending open acknowledged") } catch (_: IllegalStateException) { }
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.NotCreated, owner.recoverOpen(100uL))
    owner.acknowledgeOpen(100uL, 100uL, peer)
    owner.admitOpen(164uL, 164uL, peer)
    try { owner.openAdmitted(pending.first(), peer, emptyMap(), 1, 1) { _, _ -> }; fail("retired admission revived") } catch (_: OrderedOpenNotCreated) { }
  }
  @Test fun missingReceiptDoesNotProveAQueuedOpenCannotCreate() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = AndroidOrderedGroupOwner(radio)
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.Indeterminate, owner.recoverOpen(17uL))
  }

  @Test fun retireAndCloseWakeWithoutHoldingGroupOwnerMonitor() {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    doReturn(peer).`when`(device).address; doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid; doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid; doReturn(service).`when`(characteristic).service
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val owner = AndroidOrderedGroupOwner(radio)
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    fun wake(generation: ULong, group: ULong) {
      val entered = java.util.concurrent.CountDownLatch(1)
      val worker = kotlin.concurrent.thread { owner.memberId(generation, group, characteristic); entered.countDown() }
      assertTrue("wake blocked by owner monitor", entered.await(1, java.util.concurrent.TimeUnit.SECONDS))
      worker.join(1000)
    }
    val retiring = owner.open(1uL, peer, mapOf(1uL to path), 4, 8, wake = ::wake)
    assertTrue(owner.retire(retiring.generation, 1uL, retiring.members.single()))
    val closing = owner.open(2uL, peer, mapOf(1uL to path), 4, 8, wake = ::wake)
    owner.close(closing.generation, 2uL)
  }

  @Test fun retainsBatchesUntilExactAckAndClaimsUntilCleanup() {
    val peer = "AA:BB:CC:DD:EE:FF"
    val serviceId = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val charId = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val device = mock(BluetoothDevice::class.java)
    val gatt = mock(BluetoothGatt::class.java)
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    doReturn(peer).`when`(device).address
    doReturn(device).`when`(gatt).device
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(charId).`when`(characteristic).uuid
    doReturn(service).`when`(characteristic).service
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.attachConnectedGatt(peer, gatt, listOf(service))
    val owner = AndroidOrderedGroupOwner(radio)
    val wakes = mutableListOf<Pair<ULong, ULong>>()
    val initialAdmission = owner.admitOpen(ULong.MAX_VALUE, 999uL, peer)
    val opened = owner.openAdmitted(initialAdmission, peer, mapOf(7uL to OwnedAndroidNotificationPath(serviceId, 0, charId, 0)), 8, 32) { generation, group -> wakes.add(generation to group) }
    try { owner.forgetRemovalReceipt(opened.generation + 1uL, ULong.MAX_VALUE); fail("wrong live generation accepted") }
    catch (_: IllegalStateException) { }
    try { owner.forgetRemovalReceipt(opened.generation, ULong.MAX_VALUE); fail("live group forgotten") }
    catch (_: IllegalStateException) { }
    val claim = owner.retainClaim(opened.generation, ULong.MAX_VALUE, opened.members.single())
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(3))
    assertEquals(opened.generation to ULong.MAX_VALUE, wakes.single())
    val first = owner.drain(opened.generation, ULong.MAX_VALUE)
    assertEquals(first, owner.drain(opened.generation, ULong.MAX_VALUE))
    assertArrayEquals(byteArrayOf(3), (first.events.single() as OrderedNotificationSource.Event.Value).bytes)
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(4))
    assertEquals(2, wakes.size)
    assertEquals(first, owner.drain(opened.generation, ULong.MAX_VALUE))
    try { owner.acknowledge(opened.generation, ULong.MAX_VALUE, 42uL); fail("wrong batch acknowledged") }
    catch (_: IllegalStateException) { }
    owner.acknowledge(opened.generation, ULong.MAX_VALUE, requireNotNull(first.id))
    assertEquals(3, wakes.size)
    val second = owner.drain(opened.generation, ULong.MAX_VALUE)
    assertArrayEquals(byteArrayOf(4), (second.events.single() as OrderedNotificationSource.Event.Value).bytes)
    owner.acknowledge(opened.generation, ULong.MAX_VALUE, requireNotNull(first.id))
    owner.acknowledge(opened.generation, ULong.MAX_VALUE, requireNotNull(second.id))
    owner.close(opened.generation, ULong.MAX_VALUE)
    val terminal = owner.drain(opened.generation, ULong.MAX_VALUE)
    owner.acknowledge(opened.generation, ULong.MAX_VALUE, requireNotNull(terminal.id))
    assertFalse(owner.remove(opened.generation, ULong.MAX_VALUE))
    owner.confirmCleanup(opened.generation, ULong.MAX_VALUE, claim)
    owner.confirmCleanup(opened.generation, ULong.MAX_VALUE, claim)
    assertTrue(owner.remove(opened.generation, ULong.MAX_VALUE))
    assertTrue(owner.remove(opened.generation, ULong.MAX_VALUE))
    assertEquals(AndroidOrderedGroupOwner.OpenRecovery.Created(opened), owner.recoverOpenExact(ULong.MAX_VALUE, 999uL, peer))
    owner.acknowledgeOpen(ULong.MAX_VALUE, 999uL, peer)
    try { owner.remove(opened.generation + 1uL, ULong.MAX_VALUE); fail("wrong removed generation accepted") }
    catch (_: IllegalStateException) { }
    try { owner.forgetRemovalReceipt(opened.generation + 1uL, ULong.MAX_VALUE); fail("wrong receipt generation accepted") }
    catch (_: IllegalStateException) { }
    owner.forgetRemovalReceipt(opened.generation, ULong.MAX_VALUE)
    owner.forgetRemovalReceipt(opened.generation, ULong.MAX_VALUE)

    val failures = mutableListOf<Pair<ULong, ULong>>()
    val cleanupFailures = mutableListOf<OwnedRadioTeardownFailure>()
    radio.onCleanupFailure = { cleanupFailures.add(it) }
    val path = OwnedAndroidNotificationPath(serviceId, 0, charId, 0)
    val failed = owner.open(400uL, peer, mapOf(1uL to path), 8, 32,
      onWakeFailure = { generation, group, _ -> failures.add(generation to group) }) { _, _ -> error("rejected wake") }
    val other = owner.open(500uL, peer, mapOf(1uL to path), 8, 32) { _, _ -> }
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(5))
    assertEquals(listOf(failed.generation to 400uL), failures)
    assertEquals(400uL, (cleanupFailures.single().throwable as OrderedNotificationSource.WakeFailure).groupId)
    val failedRecords = owner.drain(failed.generation, 400uL).events
    assertEquals(OrderedNotificationSource.Reason.WAKE_FAILED, (failedRecords.last() as OrderedNotificationSource.Event.Terminal).reason)
    assertArrayEquals(byteArrayOf(5), (owner.drain(other.generation, 500uL).events.single() as OrderedNotificationSource.Event.Value).bytes)

    owner.onInstalledForTest = { error("handoff failed") }
    try { owner.open(600uL, peer, mapOf(1uL to path), 8, 32) { _, _ -> }; fail("handoff did not fail") }
    catch (_: IllegalStateException) { }
    owner.onInstalledForTest = null
    val recovered = (owner.recoverOpen(600uL) as AndroidOrderedGroupOwner.OpenRecovery.Created).opened
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(6))
    assertArrayEquals(byteArrayOf(6), (owner.drain(recovered.generation, 600uL).events.single() as OrderedNotificationSource.Event.Value).bytes)

    val admission = owner.admitOpen(601uL, 1601uL, peer)
    radio.onOrderedOpenHandoffForTest = { error("mapping failed after registration") }
    try { owner.openAdmitted(admission, peer, mapOf(1uL to path), 8, 32) { _, _ -> }; fail("handoff did not fail") }
    catch (_: IllegalStateException) { }
    radio.onOrderedOpenHandoffForTest = null
    val handoffResult = owner.recoverOpenExact(601uL, 1601uL, peer)
    assertTrue("partial failure must retain the original error", handoffResult is AndroidOrderedGroupOwner.OpenRecovery.Failed)
    assertEquals(handoffResult, owner.recoverOpenExact(601uL, 1601uL, peer))
    val failedOpen = handoffResult as AndroidOrderedGroupOwner.OpenRecovery.Failed
    assertEquals("mapping failed after registration", failedOpen.error.message)
    val handoff = (failedOpen.ownership as AndroidOrderedGroupOwner.OpenRecovery.Created).opened
    owner.acknowledgeOpen(601uL, 1601uL, peer)
    owner.acknowledgeOpen(601uL, 1601uL, peer)
    radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(7))
    assertArrayEquals(byteArrayOf(7), (owner.drain(handoff.generation, 601uL).events.single() as OrderedNotificationSource.Event.Value).bytes)

    val started = owner.admitOpen(602uL, 1602uL, peer)
    val installed = java.util.concurrent.CountDownLatch(1)
    val resume = java.util.concurrent.CountDownLatch(1)
    owner.onInstalledForTest = { installed.countDown(); check(resume.await(5, java.util.concurrent.TimeUnit.SECONDS)) }
    val opening = kotlin.concurrent.thread { owner.openAdmitted(started, peer, mapOf(1uL to path), 8, 32) { _, _ -> } }
    assertTrue(installed.await(5, java.util.concurrent.TimeUnit.SECONDS))
    val recoveryStarted = java.util.concurrent.CountDownLatch(1)
    val recoveredResult = java.util.concurrent.atomic.AtomicReference<AndroidOrderedGroupOwner.OpenRecovery>()
    val recovering = kotlin.concurrent.thread { recoveryStarted.countDown(); recoveredResult.set(owner.recoverOpenExact(602uL, 1602uL, peer)) }
    assertTrue(recoveryStarted.await(5, java.util.concurrent.TimeUnit.SECONDS))
    resume.countDown()
    opening.join(5000); recovering.join(5000)
    owner.onInstalledForTest = null
    assertFalse(opening.isAlive); assertFalse(recovering.isAlive)
    assertTrue(recoveredResult.get() is AndroidOrderedGroupOwner.OpenRecovery.Created)

    val bounded = AndroidOrderedGroupOwner(radio)
    for (id in 0uL until 64uL) bounded.open(id, peer, mapOf(1uL to path), 1, 1) { _, _ -> }
    try { bounded.open(64uL, peer, mapOf(1uL to path), 1, 1) { _, _ -> }; fail("receipt capacity was not reserved") }
    catch (_: IllegalStateException) { }
  }
}
