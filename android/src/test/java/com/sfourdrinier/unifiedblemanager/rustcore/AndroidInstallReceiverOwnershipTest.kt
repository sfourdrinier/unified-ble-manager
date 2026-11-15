package com.sfourdrinier.unifiedblemanager.rustcore

import android.content.BroadcastReceiver
import android.content.Context
import android.content.IntentFilter
import android.content.Intent
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothGattService
import android.bluetooth.BluetoothProfile
import com.sfourdrinier.unifiedblemanager.radio.GattRadioFixture
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.GattConnectAttempt
import com.ubm.core.MobileCoreBridge
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.*
import java.util.ArrayDeque
import java.util.concurrent.Executor
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

class AndroidInstallReceiverOwnershipTest {
  @Test fun cancelledConnectRetainsOwnerWorkUntilExactCleanupReturns() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val fake = FakeRadio()
    var cleanup: ((Throwable?) -> Unit)? = null
    val port = object : AndroidRadioPort by fake, OrderedGroupPort by owner {
      override fun cancelConnect(peerId: String, attempt: GattConnectAttempt, onComplete: (Throwable?) -> Unit) {
        cleanup = onComplete
      }
    }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    adapter.cancel(1)
    val retirement = java.util.UUID.randomUUID()
    val slots = owner.orderedSlots()
    adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals("cleanup still owns the cancelled connect", 1, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    checkNotNull(cleanup)(null)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    checkNotNull(cleanup)(null)
    assertEquals("duplicate old cleanup must not release work twice", 0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
  }

  @Test fun withdrawnConnectRetainsOwnerWorkAcrossDisconnectCompletion() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val fake = FakeRadio()
    var cleanup: ((Throwable?) -> Unit)? = null
    val port = object : AndroidRadioPort by fake, OrderedGroupPort by owner {
      override fun cancelConnect(peerId: String, attempt: GattConnectAttempt, onComplete: (Throwable?) -> Unit) {
        cleanup = onComplete
      }
    }
    val retirement = java.util.UUID.randomUUID()
    val slots = owner.orderedSlots()
    var duringCompletion = -1
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
      override fun completeUnit(requestId: Long): Int {
        if (requestId == 3L) duringCompletion = slots.ownerStatus(slots.incarnation, retirement).effectWork
        return MobileCoreBridge.STATUS_DELIVERED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    adapter.disconnect(3, "peer")
    adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    checkNotNull(cleanup)(null)
    assertEquals("foreign completion is part of the cleanup", 1, duringCompletion)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
  }

  @Test fun synchronousConnectCleanupRetainsOwnerUntilNativeDispatchReturns() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    val fake = FakeRadio()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val port = object : AndroidRadioPort by fake, OrderedGroupPort by owner {
      override fun cancelConnect(peerId: String, attempt: GattConnectAttempt, onComplete: (Throwable?) -> Unit) {
        onComplete(null)
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
      }
    }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    val retirement = java.util.UUID.randomUUID()
    val slots = owner.orderedSlots()
    val worker = Thread { adapter.cancel(1) }
    worker.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
      assertEquals("native cleanup dispatch has not returned", 1, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    } finally {
      release.countDown()
      worker.join(5000)
    }
    assertFalse(worker.isAlive)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
  }

  @Test fun staleCleanupDoesNotReleaseReplacementOwnersWork() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val callbacks = mutableListOf<(Throwable?) -> Unit>()
    fun newOwner(): Pair<OwnedOrderedGroupPort, RustRadioHostAdapter> {
      val owner = OwnedOrderedGroupPort(radio)
      val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
        override fun cancelConnect(peerId: String, attempt: GattConnectAttempt, onComplete: (Throwable?) -> Unit) {
          callbacks.add(onComplete)
        }
      }
      val core = object : MobileCorePort by FakeCore() {
        override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
      }
      val direct = Executor { it.run() }
      return owner to RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    }
    val (oldOwner, oldAdapter) = newOwner()
    val (replacementOwner, replacementAdapter) = newOwner()
    oldAdapter.connect(1, "peer", false, emptyArray(), "old")
    oldAdapter.cancel(1)
    replacementAdapter.connect(1, "peer", false, emptyArray(), "new")
    replacementAdapter.cancel(1)
    val oldRetirement = java.util.UUID.randomUUID()
    val newRetirement = java.util.UUID.randomUUID()
    oldAdapter.ownerFence(2, oldOwner.orderedAdmissionOwner(3), oldRetirement.toString())
    replacementAdapter.ownerFence(2, replacementOwner.orderedAdmissionOwner(3), newRetirement.toString())
    callbacks[0](null)
    callbacks[0](null)
    assertEquals(0, oldOwner.orderedSlots().ownerStatus(oldOwner.orderedSlots().incarnation, oldRetirement).effectWork)
    assertEquals(1, replacementOwner.orderedSlots().ownerStatus(replacementOwner.orderedSlots().incarnation, newRetirement).effectWork)
    callbacks[1](null)
    assertEquals(0, replacementOwner.orderedSlots().ownerStatus(replacementOwner.orderedSlots().incarnation, newRetirement).effectWork)
  }

  @Test fun thrownCancellationDispatchDoesNotAssumeItsCallbackCannotArrive() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(radio)
    var cleanup: ((Throwable?) -> Unit)? = null
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {
      override fun cancelConnect(peerId: String, attempt: GattConnectAttempt, onComplete: (Throwable?) -> Unit) {
        cleanup = onComplete
        throw IllegalStateException("ambiguous cancellation dispatch")
      }
    }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    adapter.cancel(1)
    val retirement = java.util.UUID.randomUUID()
    val slots = owner.orderedSlots()
    adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, slots.ownerStatus(slots.incarnation, retirement).effectWork)
    checkNotNull(cleanup)(null)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).effectWork)
  }

  @Test fun retiringOnePortPreservesSharedReceiversForAnotherPort() {
    val context = mock(Context::class.java)
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val first = OwnedRadioPort(radio) {}
    val second = OwnedRadioPort(radio) {}
    first.setEvents(mock(RadioPortEvents::class.java))
    second.setEvents(mock(RadioPortEvents::class.java))
    val liveCallback = radio.onAdapterState

    assertTrue(first.retireReceivers().failures.isEmpty())
    verify(context, never()).unregisterReceiver(any(BroadcastReceiver::class.java))
    assertTrue(first.retireReceivers().failures.isEmpty())
    verify(context, never()).unregisterReceiver(any(BroadcastReceiver::class.java))
    try {
      first.setEvents(mock(RadioPortEvents::class.java))
      fail("retired port must not replace a live owner's callbacks")
    } catch (_: IllegalStateException) { }
    assertSame("retired port must not replace a live owner's callbacks", liveCallback, radio.onAdapterState)
    assertTrue(second.retireReceivers().failures.isEmpty())
    verify(context, times(2)).unregisterReceiver(any(BroadcastReceiver::class.java))
  }
  @Test fun retiringLatestPortKeepsSecurityEventsWithTheRemainingPort() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val firstEvents = mock(RadioPortEvents::class.java)
    val retiredEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(radio) {}
    val last = OwnedRadioPort(radio) {}
    first.setEvents(firstEvents)
    last.setEvents(retiredEvents)
    last.retireReceivers()
    radio.onSecurityState?.invoke("peer", com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidSecurityState("bonded", true))
    val firstSecurity = mockingDetails(firstEvents).invocations.count { it.method.name == "onSecurity" && it.arguments[0] == "peer" }
    val retiredSecurity = mockingDetails(retiredEvents).invocations.count { it.method.name == "onSecurity" && it.arguments[0] == "peer" }
    assertEquals("remaining owner receives the observation", 1, firstSecurity)
    assertEquals("retired owner receives no observation", 0, retiredSecurity)
  }

  @Test fun sharedReceiverObservationsReachEachLiveOwnerOnceAndRejectLateCallbacks() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val firstEvents = mock(RadioPortEvents::class.java)
    val secondEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(radio) {}
    val second = OwnedRadioPort(radio) {}
    first.setEvents(firstEvents)
    second.setEvents(secondEvents)
    val lateSecurity = checkNotNull(radio.onSecurityState)
    val state = com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidSecurityState("bonded", true)
    lateSecurity("peer", state)
    fun observations(events: RadioPortEvents) = mockingDetails(events).invocations.count { it.method.name == "onSecurity" && it.arguments[0] == "peer" }
    assertEquals(1, observations(firstEvents))
    assertEquals(1, observations(secondEvents))
    first.retireReceivers()
    lateSecurity("peer", state)
    assertEquals(1, observations(firstEvents))
    assertEquals(2, observations(secondEvents))
    second.retireReceivers()
    lateSecurity("peer", state)
    assertEquals(2, observations(secondEvents))
  }

  @Test fun receiverRetirementRetainsTheAdmittedOwnerCallbackUntilItReturns() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val events = mock(RadioPortEvents::class.java) { call ->
      if (call.method.name == "onSecurity") { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
      null
    }
    val port = OwnedRadioPort(radio) {}
    port.setEvents(events)
    val callback = checkNotNull(radio.onSecurityState)
    val worker = Thread { callback("peer", com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidSecurityState("bonded", true)) }
    worker.start()
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    try {
      assertEquals(1, port.retireReceivers().callbacks)
      assertEquals(1, mockingDetails(events).invocations.count { it.method.name == "onSecurity" })
    } finally { release.countDown(); worker.join(5000) }
    assertFalse(worker.isAlive)
    assertEquals(0, port.retireReceivers().callbacks)
  }

  @Test fun receiverFailureAndAdapterHooksReachEveryLiveOwnerAndReplacementDoesNotDuplicate() {
    val context = mock(Context::class.java)
    `when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(mock(BluetoothManager::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val oldEvents = mock(RadioPortEvents::class.java)
    val firstEvents = mock(RadioPortEvents::class.java)
    val secondEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(radio) {}
    val second = OwnedRadioPort(radio) {}
    first.setEvents(oldEvents)
    first.setEvents(firstEvents)
    second.setEvents(secondEvents)
    radio.onAdapterState?.invoke("off")
    radio.onSecurityFailure?.invoke(null, IllegalStateException("missing device"))
    fun count(events: RadioPortEvents, method: String) = mockingDetails(events).invocations.count { it.method.name == method }
    assertEquals(0, count(oldEvents, "onAdapterState"))
    assertEquals(0, count(oldEvents, "onSecurityFailure"))
    listOf(firstEvents, secondEvents).forEach {
      assertEquals(1, count(it, "onAdapterState"))
      assertEquals(1, count(it, "onSecurityFailure"))
    }
    first.retireReceivers()
    second.retireReceivers()
  }

  @Test fun receiverObserverFailureDoesNotStarveTheOtherOwner() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val error = IllegalStateException("observer failed")
    val failing = mock(RadioPortEvents::class.java) { call -> if (call.method.name == "onSecurity") throw error else null }
    val alsoFailing = mock(RadioPortEvents::class.java) { call -> if (call.method.name == "onSecurity") throw error else null }
    val surviving = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(radio) {}
    val middle = OwnedRadioPort(radio) {}
    val second = OwnedRadioPort(radio) {}
    first.setEvents(failing)
    middle.setEvents(alsoFailing)
    second.setEvents(surviving)
    try {
      checkNotNull(radio.onSecurityState)("peer", com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidSecurityState("bonded", true))
      fail("observer failure must remain visible")
    } catch (observed: IllegalStateException) { assertSame(error, observed) }
    assertEquals(1, mockingDetails(surviving).invocations.count { it.method.name == "onSecurity" })
    assertEquals(0, first.retireReceivers().callbacks)
    assertEquals(0, middle.retireReceivers().callbacks)
    assertEquals(0, second.retireReceivers().callbacks)
  }

  @Test fun bondReceiverDoesNotDeliverStaleBroadcastAfterLastOwnerCleanup() {
    val context = mock(Context::class.java)
    val receivers = mutableListOf<BroadcastReceiver>()
    doAnswer { call -> receivers.add(call.getArgument(0)); null }
      .`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true }, securitySdkInt = 35)
    val firstEvents = mock(RadioPortEvents::class.java)
    val secondEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(radio) {}
    val second = OwnedRadioPort(radio) {}
    first.setEvents(firstEvents)
    second.setEvents(secondEvents)
    val device = mock(BluetoothDevice::class.java)
    `when`(device.address).thenReturn("AA:BB:CC:DD:EE:FF")
    val intent = mock(Intent::class.java)
    `when`(intent.action).thenReturn(BluetoothDevice.ACTION_BOND_STATE_CHANGED)
    `when`(intent.getParcelableExtra<BluetoothDevice>(BluetoothDevice.EXTRA_DEVICE)).thenReturn(device)
    `when`(intent.getIntExtra(BluetoothDevice.EXTRA_BOND_STATE, BluetoothDevice.ERROR)).thenReturn(BluetoothDevice.BOND_BONDED)
    val receiver = receivers.last()
    receiver.onReceive(context, intent)
    first.retireReceivers()
    receiver.onReceive(context, intent)
    second.retireReceivers()
    receiver.onReceive(context, intent)
    assertEquals(1, mockingDetails(firstEvents).invocations.count { it.method.name == "onSecurity" })
    assertEquals(2, mockingDetails(secondEvents).invocations.count { it.method.name == "onSecurity" })
    verify(context, times(2)).unregisterReceiver(any(BroadcastReceiver::class.java))
  }

  @Test fun foreignGattAttemptIsNotDeliveredToAnUnrelatedPort() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val firstEvents = mock(RadioPortEvents::class.java)
    val secondEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(radio) {}
    val second = OwnedRadioPort(radio) {}
    first.setEvents(firstEvents)
    second.setEvents(secondEvents)
    radio.dispatchOwnedConnection("foreign", false, 0, GattConnectAttempt())
    assertEquals(0, mockingDetails(firstEvents).invocations.count { it.method.name == "onConnection" })
    assertEquals(0, mockingDetails(secondEvents).invocations.count { it.method.name == "onConnection" })
    first.retireReceivers()
    second.retireReceivers()
  }

  @Test fun lateGattCallbackCannotReachARetiredPort() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val events = mock(RadioPortEvents::class.java)
    val port = OwnedRadioPort(radio) {}
    port.setEvents(events)
    val late = radio::dispatchOwnedConnection
    port.retireReceivers()
    late("peer", false, 0, GattConnectAttempt())
    assertEquals(0, mockingDetails(events).invocations.count { it.method.name == "onConnection" })
  }

  @Test fun collidingPeerReconnectRoutesEachGattGenerationToItsAdmittingPort() {
    val f = GattRadioFixture()
    val firstGatt = f.gatt()
    val nextGatt = f.gatt()
    doReturn(firstGatt).doReturn(nextGatt).`when`(f.device)
      .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val firstEvents = mock(RadioPortEvents::class.java)
    val secondEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    first.setEvents(firstEvents)
    second.setEvents(secondEvents)
    val firstAttempt = GattConnectAttempt()
    val secondAttempt = GattConnectAttempt()
    first.connect(f.peer, false, 0, firstAttempt)
    f.radio.nativeGattCallback().onConnectionStateChange(firstGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    second.connect(f.peer, false, 0, secondAttempt)
    first.retireReceivers()
    f.nativeDisconnected(firstGatt)
    f.radio.nativeGattCallback().onConnectionStateChange(nextGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    f.radio.nativeGattCallback().onConnectionStateChange(firstGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    val firstCalls = mockingDetails(firstEvents).invocations.filter { it.method.name == "onConnection" }
    val secondCalls = mockingDetails(secondEvents).invocations.filter { it.method.name == "onConnection" }
    assertEquals("the old generation remains with its admitting port through disconnect", 2, firstCalls.size)
    assertTrue(firstCalls.all { it.arguments[3] === firstAttempt })
    assertEquals("the replacement cannot receive the old generation or a stale callback", 1, secondCalls.size)
    assertSame(secondAttempt, secondCalls.single().arguments[3])
  }

  @Test fun nativeGattCallbackRemainsOwnedUntilObserverReturns() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    doReturn(gatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    lateinit var port: OwnedRadioPort
    var callbacksDuringDelivery = -1
    val events = mock(RadioPortEvents::class.java) { call ->
      if (call.method.name == "onConnection") callbacksDuringDelivery = port.retireReceivers().callbacks
      null
    }
    port = OwnedRadioPort(f.radio) {}
    port.setEvents(events)
    port.connect(f.peer, false, 0, GattConnectAttempt())
    f.radio.nativeGattCallback().onConnectionStateChange(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    assertTrue("retirement must join the callback until delivery returns", callbacksDuringDelivery > 0)
  }

  @Test fun savedGattHookCannotReviveAClosedGenerationOnAnotherOwner() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    doReturn(gatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val firstEvents = mock(RadioPortEvents::class.java)
    val secondEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    first.setEvents(firstEvents)
    val attempt = GattConnectAttempt()
    first.connect(f.peer, false, 0, attempt)
    val savedHook = f.radio::dispatchOwnedConnection
    f.nativeDisconnected(gatt)
    second.setEvents(secondEvents)
    val firstBefore = mockingDetails(firstEvents).invocations.count { it.method.name == "onConnection" }
    savedHook(f.peer, false, 0, attempt)
    assertEquals(firstBefore, mockingDetails(firstEvents).invocations.count { it.method.name == "onConnection" })
    assertEquals(0, mockingDetails(secondEvents).invocations.count { it.method.name == "onConnection" })
  }

  @Test fun servicesChangedFollowsTheNativeGattGenerationNotTheLatestPort() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    doReturn(gatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val firstEvents = mock(RadioPortEvents::class.java)
    val secondEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    first.setEvents(firstEvents)
    first.connect(f.peer, false, 0, GattConnectAttempt())
    second.setEvents(secondEvents)
    f.radio.nativeGattCallback().onServiceChanged(gatt)
    assertEquals(1, mockingDetails(firstEvents).invocations.count { it.method.name == "onServicesChanged" })
    assertEquals(0, mockingDetails(secondEvents).invocations.count { it.method.name == "onServicesChanged" })
  }

  @Test fun orderedLinkFenceFollowsTheAttemptOwnerAfterAnotherPortInstalls() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    doReturn(gatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val firstEvents = mock(RadioPortEvents::class.java)
    val secondEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    first.setEvents(firstEvents)
    val attempt = GattConnectAttempt()
    first.connect(f.peer, false, 0, attempt)
    second.setEvents(secondEvents)
    f.nativeDisconnected(gatt)
    verify(firstEvents).onOrderedLinkFence(f.peer, attempt)
    assertEquals(0, mockingDetails(secondEvents).invocations.count { it.method.name == "onOrderedLinkFence" })
  }

  @Test fun retirementReportsAnAdmittedRouteUntilItCanNoLongerDeliver() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    doReturn(gatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val port = OwnedRadioPort(f.radio) {}
    port.setEvents(mock(RadioPortEvents::class.java))
    port.connect(f.peer, false, 0, GattConnectAttempt())
    assertTrue(port.retireReceivers().callbacks > 0)
    f.nativeDisconnected(gatt)
    assertEquals(0, port.retireReceivers().callbacks)
  }

  @Test fun cancellingAQueuedPortCannotWithdrawAnotherPortsReplacement() {
    val f = GattRadioFixture()
    val firstGatt = f.gatt()
    val replacementGatt = f.gatt()
    val cores = List(3) { FakeCore() }
    val hosts = cores.map { core ->
      RustRadioHostAdapter(core, OwnedRadioPort(f.radio) {}, FakeBackground(), { null }, { null },
        DirectExecutor, DirectExecutor) {}
    }
    var opens = 0
    doAnswer { invocation ->
      opens++
      if (opens == 1) {
        hosts[1].connect(2, f.peer, false, emptyArray(), "b")
        hosts[2].connect(3, f.peer, false, emptyArray(), "c")
        hosts[1].cancel(2)
        invocation.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(firstGatt, 0, BluetoothProfile.STATE_CONNECTED)
        firstGatt
      } else replacementGatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    hosts[0].connect(1, f.peer, false, emptyArray(), "a")
    f.nativeDisconnected(firstGatt)
    assertEquals(2, opens)
    f.radio.nativeGattCallback().onConnectionStateChange(replacementGatt, 0, BluetoothProfile.STATE_CONNECTED)
    assertEquals(1, cores[2].calls.count { it == "unit:3" })
  }

  @Test fun queuedConnectOpenFailureBelongsToTheQueuedPortAndCannotBeReplayed() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    doReturn(gatt).doThrow(IllegalStateException("open failed")).`when`(f.device)
      .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val firstEvents = mock(RadioPortEvents::class.java)
    val nextEvents = mock(RadioPortEvents::class.java)
    val first = OwnedRadioPort(f.radio) {}
    val next = OwnedRadioPort(f.radio) {}
    first.setEvents(firstEvents)
    next.setEvents(nextEvents)
    first.connect(f.peer, false, 0, GattConnectAttempt())
    val attempt = GattConnectAttempt()
    next.connect(f.peer, false, 0, attempt)
    val saved = f.radio::dispatchOwnedConnection
    f.nativeDisconnected(gatt)
    val calls = mockingDetails(nextEvents).invocations.filter { it.method.name == "onConnection" }
    assertEquals(1, calls.size)
    assertSame(attempt, calls.single().arguments[3])
    saved(f.peer, false, BluetoothGatt.GATT_FAILURE, attempt)
    assertEquals(1, mockingDetails(nextEvents).invocations.count { it.method.name == "onConnection" })
  }

  @Test fun serviceCallbackRemainsCountedUntilObserverReturnsAndClosedGattIsIgnored() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    doReturn(gatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    lateinit var port: OwnedRadioPort
    var during = 0
    val events = mock(RadioPortEvents::class.java) { call ->
      if (call.method.name == "onServicesChanged") during = port.retireReceivers().callbacks
      null
    }
    port = OwnedRadioPort(f.radio) {}
    port.setEvents(events)
    port.connect(f.peer, false, 0, GattConnectAttempt())
    f.radio.nativeGattCallback().onServiceChanged(gatt)
    assertTrue(during > 0)
    f.nativeDisconnected(gatt)
    f.radio.nativeGattCallback().onServiceChanged(gatt)
    assertEquals(1, mockingDetails(events).invocations.count { it.method.name == "onServicesChanged" })
  }

  @Test fun legacyConnectionHookCannotReplaceTheAdmittingPortRoute() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    doReturn(gatt).`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val events = mock(RadioPortEvents::class.java)
    val port = OwnedRadioPort(f.radio) {}
    port.setEvents(events)
    var legacyCalls = 0
    f.radio.onConnectionOutcome = { _, _, _, _ -> legacyCalls++ }
    port.connect(f.peer, false, 0, GattConnectAttempt())
    f.radio.nativeGattCallback().onConnectionStateChange(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    assertEquals(1, legacyCalls)
    assertEquals(1, mockingDetails(events).invocations.count { it.method.name == "onConnection" })
  }

  @Test fun rejectedAndSupersededConnectsWithdrawTheirSavedRoutes() {
    val f = GattRadioFixture()
    val firstGatt = f.gatt()
    val replacementGatt = f.gatt()
    doThrow(IllegalStateException("refused")).doReturn(firstGatt).doReturn(replacementGatt).`when`(f.device)
      .connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    val events = mock(RadioPortEvents::class.java)
    val port = OwnedRadioPort(f.radio) {}
    port.setEvents(events)
    val rejected = GattConnectAttempt()
    try { port.connect(f.peer, false, 0, rejected); fail("open should fail") } catch (_: IllegalStateException) { }
    f.radio.dispatchOwnedConnection(f.peer, false, 0, rejected)
    assertEquals(0, mockingDetails(events).invocations.count { it.method.name == "onConnection" })
    port.connect(f.peer, false, 0, GattConnectAttempt())
    val superseded = GattConnectAttempt()
    port.connect(f.peer, false, 0, superseded)
    val replacement = GattConnectAttempt()
    port.connect(f.peer, false, 0, replacement)
    f.radio.dispatchOwnedConnection(f.peer, false, 0, superseded)
    val displacedCalls = mockingDetails(events).invocations.filter { it.method.name == "onConnection" }
    assertEquals(1, displacedCalls.size)
    assertSame(superseded, displacedCalls.single().arguments[3])
    assertEquals(false, displacedCalls.single().arguments[1])
    assertEquals(BluetoothGatt.GATT_FAILURE, displacedCalls.single().arguments[2])
    f.nativeDisconnected(firstGatt)
    f.radio.nativeGattCallback().onConnectionStateChange(replacementGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    val calls = mockingDetails(events).invocations.filter { it.method.name == "onConnection" }
    assertEquals(3, calls.size)
    assertSame(replacement, calls.last().arguments[3])
  }

  @Test fun synchronousNativeConnectionStaysWithAdmittingPortUntilObserverReturns() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val otherEvents = mock(RadioPortEvents::class.java)
    val other = OwnedRadioPort(f.radio) {}
    other.setEvents(otherEvents)
    lateinit var port: OwnedRadioPort
    var callbacks = 0
    var pendingCallbacks = 0
    val events = mock(RadioPortEvents::class.java) { call ->
      if (call.method.name == "onConnection") callbacks = port.retireReceivers().callbacks
      null
    }
    port = OwnedRadioPort(f.radio) {}
    port.setEvents(events)
    val attempt = GattConnectAttempt()
    doAnswer { call ->
      call.getArgument<BluetoothGattCallback>(2).onConnectionStateChange(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
      pendingCallbacks = port.retireReceivers().callbacks
      gatt
    }.`when`(f.device).connectGatt(eq(f.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    port.connect(f.peer, false, 0, attempt)
    val calls = mockingDetails(events).invocations.filter { it.method.name == "onConnection" }
    assertEquals(1, calls.size)
    assertSame(attempt, calls.single().arguments[3])
    assertTrue("buffered callback remains owned before publication", pendingCallbacks > 0)
    assertTrue(callbacks > 0)
    assertEquals(0, mockingDetails(otherEvents).invocations.count { it.method.name == "onConnection" })
  }

  @Test fun notificationUsesThePortThatAdmittedTheNativeSubscription() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val serviceId = java.util.UUID.randomUUID()
    val characteristicId = java.util.UUID.randomUUID()
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val descriptor = mock(BluetoothGattDescriptor::class.java)
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(characteristicId).`when`(characteristic).uuid
    doReturn(service).`when`(characteristic).service
    doReturn(0x10).`when`(characteristic).properties
    doReturn(descriptor).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(characteristic).`when`(descriptor).characteristic
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    doReturn(true).`when`(gatt).writeDescriptor(descriptor)
    f.radio.attachConnectedGatt(f.peer, gatt, listOf(service))
    lateinit var first: OwnedRadioPort
    var callbacksDuringDelivery = -1
    val firstEvents = mock(RadioPortEvents::class.java) { call ->
      if (call.method.name == "onNotification") callbacksDuringDelivery = first.retireReceivers().callbacks
      null
    }
    val secondEvents = mock(RadioPortEvents::class.java)
    first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    first.setEvents(firstEvents)
    val instance = CharacteristicInstance(f.peer, serviceId.toString(), 0, characteristicId.toString(), 0)
    first.setNotification(instance, true, "notification", 7) { assertTrue(it.isSuccess) }
    second.setEvents(secondEvents)
    f.radio.nativeGattCallback().onDescriptorWrite(gatt, descriptor, BluetoothGatt.GATT_SUCCESS)
    f.radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(1))
    assertEquals(1, mockingDetails(firstEvents).invocations.count { it.method.name == "onNotification" })
    assertEquals(0, mockingDetails(secondEvents).invocations.count { it.method.name == "onNotification" })
    assertTrue("notification delivery remains owned until the observer returns", callbacksDuringDelivery > 0)
  }

  @Test fun anotherPortCannotDisableAnOwnedSubscriptionWithTheSameEpoch() {
    val f = GattRadioFixture()
    val gatt = f.gatt()
    val serviceId = java.util.UUID.randomUUID()
    val characteristicId = java.util.UUID.randomUUID()
    val service = mock(BluetoothGattService::class.java)
    val characteristic = mock(BluetoothGattCharacteristic::class.java)
    val descriptor = mock(BluetoothGattDescriptor::class.java)
    doReturn(serviceId).`when`(service).uuid
    doReturn(listOf(characteristic)).`when`(service).characteristics
    doReturn(characteristicId).`when`(characteristic).uuid
    doReturn(service).`when`(characteristic).service
    doReturn(0x10).`when`(characteristic).properties
    doReturn(descriptor).`when`(characteristic).getDescriptor(OwnedAndroidGattRadio.CCCD_UUID)
    doReturn(characteristic).`when`(descriptor).characteristic
    doReturn(true).`when`(gatt).setCharacteristicNotification(characteristic, true)
    doReturn(true).`when`(gatt).writeDescriptor(descriptor)
    f.radio.attachConnectedGatt(f.peer, gatt, listOf(service))
    val first = OwnedRadioPort(f.radio) {}
    val second = OwnedRadioPort(f.radio) {}
    val firstEvents = mock(RadioPortEvents::class.java)
    first.setEvents(firstEvents)
    second.setEvents(mock(RadioPortEvents::class.java))
    val instance = CharacteristicInstance(f.peer, serviceId.toString(), 0, characteristicId.toString(), 0)
    first.setNotification(instance, true, "notification", 7) { assertTrue(it.isSuccess) }
    f.radio.nativeGattCallback().onDescriptorWrite(gatt, descriptor, BluetoothGatt.GATT_SUCCESS)
    var result: Result<Unit>? = null
    second.setNotification(instance, false, null, 7) { result = it }
    assertTrue("another port's matching epoch must not release the claim", result?.isFailure == true)
    f.radio.nativeGattCallback().onCharacteristicChanged(gatt, characteristic, byteArrayOf(1))
    assertEquals(1, mockingDetails(firstEvents).invocations.count { it.method.name == "onNotification" })
  }

  @Test fun sharedReceiverCleanupFailureRemainsWithTheLastOwnerForRetry() {
    val context = mock(Context::class.java)
    var denied = true
    doAnswer { if (denied) throw SecurityException("receiver cleanup denied") else null }
      .`when`(context).unregisterReceiver(any(BroadcastReceiver::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val first = OwnedRadioPort(radio) {}
    val last = OwnedRadioPort(radio) {}
    first.setEvents(mock(RadioPortEvents::class.java))
    last.setEvents(mock(RadioPortEvents::class.java))
    assertTrue(first.retireReceivers().failures.isEmpty())
    assertEquals(2, last.retireReceivers().failures.size)
    assertTrue(first.retireReceivers().failures.isEmpty())
    verify(context, times(2)).unregisterReceiver(any(BroadcastReceiver::class.java))
    denied = false
    assertTrue(last.retireReceivers().failures.isEmpty())
    verify(context, times(4)).unregisterReceiver(any(BroadcastReceiver::class.java))
  }

  @Test fun connectionCallbackRemainsOwnedAfterRequestCompletionUntilIngressReturns() {
    listOf(false, true).forEach { fenceBeforeCallback ->
      val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
      val owner = OwnedOrderedGroupPort(driver)
      val fakeRadio = FakeRadio()
      val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
      val retirement = java.util.UUID.randomUUID()
      var duringIngress = -1
      lateinit var adapter: RustRadioHostAdapter
      val core = object : MobileCorePort by FakeCore() {
        override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
        override fun ingestConnectionForGeneration(peerId: String, expectedGeneration: String, connected: Boolean, status: Int?): Int {
          if (!fenceBeforeCallback) adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
          duringIngress = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
          return MobileCoreBridge.STATUS_ACCEPTED
        }
      }
      val direct = Executor { it.run() }
      adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
      adapter.connect(1, "peer", false, emptyArray(), "generation")
      if (fenceBeforeCallback) adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
      fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
      assertTrue("callback remains admitted through its foreign ingress", duringIngress > 0)
      assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    }
  }

  // These receiver-only fixtures never acquire a background lease. The real
  // registry supplies an authoritative idle snapshot and rejects surprises.
  private fun idleBackground(): ForegroundServiceBackgroundPort {
    val driver = object : com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceDriver {
      override fun start(reason: String) { error("unexpected background start") }
      override fun stop() { error("unexpected background stop") }
      override fun update(title: String, body: String?) { error("unexpected background update") }
    }
    return ForegroundServiceBackgroundPort(
      com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry(driver) { error("unexpected lease") }
    )
  }
  @Test fun queuedPresenceObservationCannotBeginAfterOwnerFence() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    var observed = false
    val presence = object : com.sfourdrinier.unifiedblemanager.presence.PresencePort {
      override fun observe(peerId: String, onResult: (Result<Unit>) -> Unit) { observed = true }
      override fun unobserve(peerId: String, onResult: (Result<Unit>) -> Unit) = Unit
    }
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val queued = QueuedExecutor()
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { presence }, Executor { it.run() }, queued) {}
    adapter.observePresence(1, "peer")
    adapter.ownerFence(2, owner.orderedAdmissionOwner(3), java.util.UUID.randomUUID().toString())
    queued.runAll()
    assertFalse(observed)
    assertTrue(fake.failures.containsKey(1))
  }

  @Test fun presenceObservationStaysOwnedUntilItsCompletionReturns() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    var callback: ((Result<Unit>) -> Unit)? = null
    val presence = object : com.sfourdrinier.unifiedblemanager.presence.PresencePort {
      override fun observe(peerId: String, onResult: (Result<Unit>) -> Unit) { callback = onResult }
      override fun unobserve(peerId: String, onResult: (Result<Unit>) -> Unit) { onResult(Result.success(Unit)) }
    }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { presence }, direct, direct) {}
    adapter.observePresence(1, "peer")
    val retirement = java.util.UUID.randomUUID()
    adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    checkNotNull(callback)(Result.failure(IllegalStateException("synthetic failure")))
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
  }

  @Test fun queuedServiceUpdateCannotBeginAfterOwnerFence() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val background = FakeBackground()
    val queued = QueuedExecutor()
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val adapter = RustRadioHostAdapter(core, port, background, { null }, { null }, Executor { it.run() }, queued) {}
    adapter.updateBackgroundNotification(1, "lease", "title", null)
    adapter.ownerFence(2, owner.orderedAdmissionOwner(3), java.util.UUID.randomUUID().toString())
    queued.runAll()
    assertTrue(background.calls.isEmpty())
    assertTrue(fake.failures.containsKey(1))
  }

  @Test fun companionCallbackRemainsOwnedThroughCompletion() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    var callback: ((Result<CompanionAssociation>) -> Unit)? = null
    val chooser = object : CompanionPort {
      override fun associate(name: String?, serviceUuid: String?, onResult: (Result<CompanionAssociation>) -> Unit) { callback = onResult }
      override fun listAssociations() = emptyList<CompanionAssociationRecord>()
      override fun disassociate(associationId: Long) = Unit
    }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { chooser }, { null }, direct, direct) {}
    adapter.associateCompanion(1, null, null, null)
    val retirement = java.util.UUID.randomUUID()
    adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    checkNotNull(callback)(Result.failure(IllegalStateException("cancelled")))
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
  }
  @Test fun receiverCleanupWaitsForAlreadyAdmittedOwnerEffects() {
    val context = mock(Context::class.java)
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val port = OwnedRadioPort(radio) {}
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val slots = port.orderedSlots()
    val work = checkNotNull(slots.reserveEffectWork())
    assertTrue(slots.beginEffectWork(work))
    val owner = adapter.orderedAdmissionOwner(3)
    val retirement = java.util.UUID.randomUUID().toString()
    adapter.ownerFence(1, owner, retirement)
    verify(context, never()).unregisterReceiver(any(BroadcastReceiver::class.java))
    slots.finishEffectWork(work)
    adapter.ownerStatus(2, owner, retirement)
    verify(context, times(2)).unregisterReceiver(any(BroadcastReceiver::class.java))
  }
  @Test fun receiverRetirementTracksCallbackUntilItsBodyReturns() {
    val context = mock(Context::class.java)
    val receivers = mutableListOf<BroadcastReceiver>()
    doAnswer { call -> receivers.add(call.getArgument(0)); null }
      .`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    radio.registerAdapterStateReceiver()
    val entered = CountDownLatch(1)
    val leave = CountDownLatch(1)
    val intent = mock(Intent::class.java)
    `when`(intent.action).thenAnswer {
      entered.countDown()
      check(leave.await(5, TimeUnit.SECONDS))
      "ignored"
    }
    val callback = Thread { receivers.single().onReceive(context, intent) }
    callback.start()
    try {
      assertTrue(entered.await(5, TimeUnit.SECONDS))
      val pending = radio.retireReceivers()
      assertTrue(pending.failures.isEmpty())
      assertEquals(1, pending.callbacks)
      receivers.single().onReceive(context, intent)
    } finally {
      leave.countDown()
      callback.join(5000)
    }
    assertFalse(callback.isAlive)
    assertEquals(0, radio.retireReceivers().callbacks)
    try { radio.registerAdapterStateReceiver(); fail("retired owner must reject receiver registration") }
    catch (_: IllegalStateException) { }
    assertEquals(1, receivers.size)
  }
  @Test fun ownerRetirementRetriesReceiverFailuresOnlyForTheExactFence() {
    val context = mock(Context::class.java)
    val registered = mutableListOf<BroadcastReceiver>()
    doAnswer { call -> registered.add(call.getArgument(0)); null }
      .`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    var denied = true
    val released = mutableListOf<BroadcastReceiver>()
    doAnswer { call ->
      if (denied) throw SecurityException("receiver cleanup denied")
      released.add(call.getArgument(0)); null
    }.`when`(context).unregisterReceiver(any(BroadcastReceiver::class.java))
    val results = mutableListOf<MobileCoreBridge.OwnerRetirementResult>()
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult): Int {
        results.add(result); return MobileCoreBridge.STATUS_DELIVERED
      }
    }
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, OwnedRadioPort(radio) {}, idleBackground(), { null }, { null }, direct, direct) {}
    val retirement = java.util.UUID.randomUUID().toString()
    adapter.ownerFence(1, java.util.UUID.randomUUID().toString(), retirement)
    assertEquals("rejected", results.last().state)
    verify(context, never()).unregisterReceiver(any(BroadcastReceiver::class.java))
    val owner = adapter.orderedAdmissionOwner(3)
    adapter.ownerFence(2, owner, retirement)
    assertEquals("fenced-pending", results.last().state)
    assertTrue(results.last().failures.any { it.failureKind == "permission-denied" })
    assertTrue(released.isEmpty())
    denied = false
    adapter.ownerStatus(3, owner, retirement)
    assertEquals(registered.toSet(), released.toSet())
    assertEquals(2, released.size)
    assertEquals("receiver cleanup alone cannot prove owner-wide quiescence", "fenced-pending", results.last().state)
    assertEquals(setOf("attachments", "reservations", "native-work", "callbacks", "cleanup"), results.last().outstandingClasses.toSet())
    assertTrue(results.last().failures.isEmpty())
  }
  @Test fun pendingLinkAndBondOperationsRemainOwnedUntilTheirCallbacks() {
    val operations: List<Pair<String, (RustRadioHostAdapter) -> Unit>> = listOf(
      "readMtu:peer" to { it.readMtu(2, "peer") },
      "readRssi:peer" to { it.readRssi(2, "peer") },
      "priority:peer:high" to { it.requestConnectionPriority(2, "peer", "high") },
      "subrate:peer:default" to { it.requestSubrate(2, "peer", "default") },
      "readPhy:peer" to { it.readPhy(2, "peer") },
      "requestPhy:peer:1m:1m" to { it.requestPhy(2, "peer", "1m", "1m") },
      "createBond:peer:auto" to { it.createBond(2, "peer", "auto") }
    )
    operations.forEach { (call, start) ->
      val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
      val owner = OwnedOrderedGroupPort(driver)
      val fakeRadio = FakeRadio().apply { subrateSupported = true }
      val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
      val core = object : MobileCorePort by FakeCore() {
        override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
      }
      val direct = Executor { it.run() }
      val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
      adapter.connect(1, "peer", false, emptyArray(), "generation")
      fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
      start(adapter)
      assertTrue("$call must reach the driver", call in fakeRadio.pending)
      val retirement = java.util.UUID.randomUUID()
      adapter.ownerFence(3, owner.orderedAdmissionOwner(3), retirement.toString())
      assertEquals(call, 1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
      fakeRadio.answer(call, Result.failure(IllegalStateException("synthetic failure")))
      assertEquals(call, 0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    }
  }
  @Test fun pendingNotificationEnableRemainsOwnedThroughCompletionRelay() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val retirement = java.util.UUID.randomUUID()
    var duringCompletion = -1
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
      override fun completeNotifyEnabled(requestId: Long, delivery: String): Int {
        duringCompletion = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
        return MobileCoreBridge.STATUS_DELIVERED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val path = CharacteristicInstance("peer", "service", 0, "characteristic", 0)
    fakeRadio.characteristics[path] = CharacteristicFacts(0x10, true)
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
    adapter.enableNotifications(2, "peer", "service", 0, "characteristic", 0, 1, "notification", null)
    adapter.ownerFence(3, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    fakeRadio.answer("notify:characteristic:true:notification", Result.success(Unit))
    assertEquals(1, duringCompletion)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
  }
  @Test fun pendingMtuRequestRemainsOwnedThroughCompletionRelay() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val retirement = java.util.UUID.randomUUID()
    var duringCompletion = -1
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
      override fun completeMtu(requestId: Long, mtu: Int): Int {
        duringCompletion = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
        return MobileCoreBridge.STATUS_DELIVERED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
    adapter.requestMtu(2, "peer", 128)
    adapter.ownerFence(3, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    fakeRadio.answer("requestMtu:peer:128", Result.success(128))
    assertEquals(1, duringCompletion)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
  }

  @Test fun queuedMtuRequestCannotStartAfterOwnerFence() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val queued = QueuedExecutor()
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, queued, Executor { it.run() }) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    queued.runAll()
    fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
    adapter.requestMtu(2, "peer", 128)
    adapter.ownerFence(3, owner.orderedAdmissionOwner(3), java.util.UUID.randomUUID().toString())
    queued.runAll()
    assertFalse(fakeRadio.calls.contains("requestMtu:peer:128"))
    assertTrue(fake.failures.containsKey(2))
  }
  @Test fun pendingReadRemainsOwnedThroughItsCompletionRelay() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val retirement = java.util.UUID.randomUUID()
    var duringCompletion = -1
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
      override fun completeRead(requestId: Long, value: ByteArray, provenance: String): Int {
        duringCompletion = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
        return MobileCoreBridge.STATUS_DELIVERED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val path = CharacteristicInstance("peer", "service", 0, "characteristic", 0)
    fakeRadio.characteristics[path] = CharacteristicFacts(0x12, true)
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
    adapter.read(2, "peer", "service", 0, "characteristic", 0)
    adapter.ownerFence(3, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    fakeRadio.answer("read:characteristic", Result.success(byteArrayOf(1)))
    assertEquals(1, duringCompletion)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
  }

  @Test fun queuedReadCannotBeginAfterOwnerFence() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val queued = QueuedExecutor()
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, queued, Executor { it.run() }) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    queued.runAll()
    fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
    adapter.read(2, "peer", "service", 0, "characteristic", 0)
    val retirement = java.util.UUID.randomUUID()
    adapter.ownerFence(3, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    queued.runAll()
    assertFalse(fakeRadio.calls.any { it.startsWith("read:") })
    assertTrue(fake.failures.containsKey(2))
  }

  @Test fun scanCallbackIsOwnedDuringRelayAndLateCallbackIsFenced() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val retirement = java.util.UUID.randomUUID()
    var relays = 0
    var duringRelay = -1
    lateinit var adapter: RustRadioHostAdapter
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
      override fun ingestAdvertisement(advertisement: AdvertisementFacts): Int {
        relays++
        adapter.ownerFence(7, owner.orderedAdmissionOwner(3), retirement.toString())
        duringRelay = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
        return MobileCoreBridge.STATUS_ACCEPTED
      }
    }
    val direct = Executor { it.run() }
    adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val advertisement = AdvertisementFacts("peer", null, null, null, null, emptyList(), emptyList(), emptyList(), null, null)
    fakeRadio.events.onAdvertisement(advertisement)
    assertEquals(1, duringRelay)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    fakeRadio.events.onAdvertisement(advertisement)
    assertEquals(1, relays)
  }
  @Test fun queuedServiceAcquisitionCannotBeginAfterOwnerFence() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val port = object : AndroidRadioPort by FakeRadio(), OrderedGroupPort by owner {}
    val background = FakeBackground()
    val queued = QueuedExecutor()
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val adapter = RustRadioHostAdapter(core, port, background, { null }, { null }, Executor { it.run() }, queued) {}
    val retirement = java.util.UUID.randomUUID()
    adapter.acquireBackground(1, "connected-device", "test")
    adapter.ownerFence(2, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    queued.runAll()
    assertTrue(background.calls.isEmpty())
    assertTrue(fake.failures.containsKey(1))
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
  }
  @Test fun queuedDiscoveryCannotBeginAfterOwnerFence() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val queued = QueuedExecutor()
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, queued, Executor { it.run() }) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    queued.runAll()
    fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
    adapter.discover(2, "peer")
    val retirement = java.util.UUID.randomUUID()
    adapter.ownerFence(3, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    queued.runAll()
    assertFalse(fakeRadio.calls.any { it.startsWith("discover:") })
    assertTrue(fake.failures.containsKey(2))
  }
  @Test fun activeDiscoveryAndItsReentrantCompletionRemainOwned() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val retirement = java.util.UUID.randomUUID()
    var duringCompletion = -1
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
      override fun completeDiscovered(requestId: Long, levels: IntArray, uuids: Array<String>, occurrences: LongArray, properties: IntArray): Int {
        duringCompletion = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
        return MobileCoreBridge.STATUS_DELIVERED
      }
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    adapter.connect(1, "peer", false, emptyArray(), "generation")
    fakeRadio.events.onConnection("peer", true, 0, fakeRadio.attempts.getValue("peer"))
    adapter.discover(2, "peer")
    adapter.ownerFence(3, owner.orderedAdmissionOwner(3), retirement.toString())
    assertEquals(1, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    fakeRadio.answer("discover:peer", Result.success(emptyList<GattServiceNode>()))
    assertEquals(1, duringCompletion)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
  }
  @Test fun serviceChangedRelayRemainsOwnedDuringReentrantRetirementAndRejectsLateCallbacks() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val retirement = java.util.UUID.randomUUID()
    var relays = 0
    var activeDuringFence = -1
    lateinit var adapter: RustRadioHostAdapter
    val core = object : MobileCorePort by FakeCore() {
      override fun ingestServicesChanged(peerId: String): Int {
        relays++
        adapter.ownerFence(7, owner.orderedAdmissionOwner(3), retirement.toString())
        activeDuringFence = owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork
        return MobileCoreBridge.STATUS_ACCEPTED
      }
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    fakeRadio.events.onServicesChanged("peer")
    assertEquals(1, activeDuringFence)
    assertEquals(0, owner.orderedSlots().ownerStatus(owner.orderedSlots().incarnation, retirement).effectWork)
    fakeRadio.events.onServicesChanged("peer")
    assertEquals(1, relays)
  }
  @Test fun retiredOwnerRejectsAdapterSecurityAndDroppedRelays() {
    val driver = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = OwnedOrderedGroupPort(driver)
    val fakeRadio = FakeRadio()
    val port = object : AndroidRadioPort by fakeRadio, OrderedGroupPort by owner {}
    val fake = FakeCore()
    val core = object : MobileCorePort by fake {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    adapter.ownerFence(1, owner.orderedAdmissionOwner(3), java.util.UUID.randomUUID().toString())
    fakeRadio.events.onAdapterState(AdapterFacts("available", "granted", "off", null))
    fakeRadio.events.onSecurity("peer", fakeRadio.security)
    fakeRadio.events.onSecurityFailure("peer", IllegalStateException("failed"))
    fakeRadio.events.onDropped("control", "overflow", 1)
    assertFalse(fake.calls.any { it.startsWith("adapter-state:") || it.startsWith("security-") || it.startsWith("dropped:") })
  }
  @Test fun productionOwnerAdvertisesTheDistinctV4CoordinatorOwner() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val port = OwnedRadioPort(radio) {}
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(FakeCore(), port, FakeBackground(), { null }, { null }, direct, direct) {}
    val admission = port.orderedAdmissionOwner(3)
    assertEquals(java.util.UUID.fromString(admission).toString(), admission)
    assertEquals(admission, adapter.orderedAdmissionOwner(3))
    val v4 = port.orderedAdmissionOwner(4)
    assertEquals(v4, adapter.orderedAdmissionOwner(4))
    assertNotEquals(admission, v4)
    assertEquals(4, java.util.UUID.fromString(v4).version())
    assertEquals("", port.orderedAdmissionOwner(5))
  }

  @Test fun acceptedOwnerFenceRejectsNewGraphAuthoritiesWithoutRetiringExistingReceipts() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val port = OwnedRadioPort(radio) {}
    val existing = port.orderedGraphAuthority("reserve", 1uL)
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val direct = Executor { it.run() }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val retirement = java.util.UUID.randomUUID().toString()
    adapter.ownerFence(1, java.util.UUID.randomUUID().toString(), retirement)
    assertEquals(existing, port.orderedGraphAuthority("reserve", 1uL))
    adapter.ownerFence(2, port.orderedAdmissionOwner(3), retirement)
    assertEquals(existing, port.orderedGraphAuthority("reserve-status", 1uL))
    assertThrows(IllegalStateException::class.java) { port.orderedGraphAuthority("reserve", 2uL) }
    assertEquals(existing, port.orderedGraphAuthority("reserve", 1uL))
  }

  @Test fun exactOwnerFenceRejectsNewLegacyOpenAdmissionsButRetainsExistingIdentity() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val port = OwnedRadioPort(radio) {}
    val existing = port.orderedAdmitOpen(1uL, 11uL, "peer")
    val direct = Executor { it.run() }
    val core = object : MobileCorePort by FakeCore() {
      override fun completeOwnerRetirement(requestId: Long, result: MobileCoreBridge.OwnerRetirementResult) = MobileCoreBridge.STATUS_DELIVERED
    }
    val adapter = RustRadioHostAdapter(core, port, FakeBackground(), { null }, { null }, direct, direct) {}
    val retirement = java.util.UUID.randomUUID().toString()
    adapter.ownerFence(1, java.util.UUID.randomUUID().toString(), retirement)
    val second = port.orderedAdmitOpen(2uL, 22uL, "peer")
    adapter.ownerFence(2, port.orderedAdmissionOwner(3), retirement)
    assertSame(existing, port.orderedAdmitOpen(1uL, 11uL, "peer"))
    assertSame(second, port.orderedAdmitOpen(2uL, 22uL, "peer"))
    assertThrows(IllegalStateException::class.java) { port.orderedAdmitOpen(3uL, 33uL, "peer") }
  }

  @Test fun ownerFenceDoesNotWaitForTheLegacyGroupMonitor() {
    val radio = OwnedAndroidGattRadio(mock(Context::class.java), post = { it(); true }, scheduleDelayed = { _, _ -> true })
    val owner = com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner(radio)
    val field = owner.javaClass.getDeclaredField("lock").apply { isAccessible = true }
    val groupLock = checkNotNull(field.get(owner))
    val fenced = CountDownLatch(1)
    val admission = Thread { try { owner.admitOpen(1uL, 11uL, "peer") } catch (_: IllegalStateException) { } }
    val fence = Thread {
      owner.orderedOpenSlots.fenceOwner(owner.orderedOpenSlots.incarnation, java.util.UUID.randomUUID())
      fenced.countDown()
    }
    try {
      synchronized(groupLock) {
        admission.start()
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
        while (admission.state != Thread.State.BLOCKED && System.nanoTime() < deadline) Thread.yield()
        assertEquals(Thread.State.BLOCKED, admission.state)
        fence.start()
        assertTrue("fencing must not wait behind the group monitor", fenced.await(1, TimeUnit.SECONDS))
      }
    } finally {
      admission.join(5000)
      if (fence.isAlive) fence.join(5000)
    }
    assertFalse(admission.isAlive)
    assertFalse(fence.isAlive)
  }

  @Test fun productionFactoryAndJniRetainFailedCleanupThenInstallReplacement() {
    org.junit.Assume.assumeTrue("requires the offline host JNI test library", java.lang.Boolean.getBoolean("ubm.test.productionJni"))
    val context = mock(Context::class.java)
    val registered = mutableListOf<BroadcastReceiver>()
    val released = mutableListOf<BroadcastReceiver>()
    doAnswer { call -> registered.add(call.getArgument(0)); null }
      .`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    var denyCleanup = true
    var cleanupAttempts = 0
    doAnswer { call ->
      cleanupAttempts++
      if (denyCleanup) throw SecurityException("synthetic receiver cleanup failure")
      released.add(call.getArgument(0)); null
    }.`when`(context).unregisterReceiver(any(BroadcastReceiver::class.java))
    var attempts = 0
    var failedRadio: MobileCoreBridge.RadioHost? = null
    var failedBinding = 0L
    val api = object : BoundInstallApi {
      override fun routeVersion() = MobileCoreBridge.nativeBindingRouteVersion()
      override fun install(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, label: String): Long {
        attempts++
        if (attempts == 1) failedRadio = radio
        try {
          return MobileCoreBridge.nativeInstallHostForBinding(1, radio, wake, "android", if (attempts == 1) "" else owner, label)
        } finally {
          if (attempts == 1) failedBinding = MobileCoreBridge.nativeFindRetainedBinding(radio)
        }
      }
      override fun find(radio: MobileCoreBridge.RadioHost) = MobileCoreBridge.nativeFindRetainedBinding(radio)
      override fun shutdown(binding: Long) = MobileCoreBridge.nativeShutdownHostForBinding(binding)
    }
    val tasks = ArrayDeque<Runnable>()
    lateinit var host: RustCoreProcessHost
    host = RustCoreProcessHost.bound({ core ->
      host.createOwnedAdapter(core, OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true }),
        idleBackground(), { null }, { null }, Executor { it.run() }, Executor { it.run() }) {}
    }, { _, task -> tasks.add(task) }, {}, { JniBoundMobileCorePort().also { it.installApi = api } })
    val first = host.core
    try { host.ensureInstalled(); fail("expected post-route install failure") } catch (_: MobileCoreBridge.MobileCoreException) { }
    assertTrue("JNI retained accepted route", failedBinding > 0)
    assertEquals(failedBinding, MobileCoreBridge.nativeFindRetainedBinding(checkNotNull(failedRadio)))
    tasks.removeFirst().run()
    assertSame(first, host.core)
    assertEquals(failedBinding, MobileCoreBridge.nativeFindRetainedBinding(checkNotNull(failedRadio)))
    assertTrue(released.isEmpty())
    assertTrue("native retirement attempted exact receiver cleanup", cleanupAttempts > 0)
    denyCleanup = false
    tasks.removeFirst().run()
    assertEquals(0L, MobileCoreBridge.nativeFindRetainedBinding(checkNotNull(failedRadio)))
    assertEquals(MobileCoreBridge.STATUS_NO_HOST, MobileCoreBridge.nativeCompleteUnitForBinding(failedBinding, 1))
    assertNotSame(first, host.core)
    assertEquals(registered.take(2).toSet(), released.toSet())
    host.ensureInstalled()
    assertEquals(2, attempts)
    assertTrue(host.core.hostInstalled())
  }
  @Test fun boundProductionFactoryCleansBeforeRouteAcceptance() = boundFactoryFailure(false)
  @Test fun boundProductionFactoryWaitsForAcceptedRouteBeforeReceiverCleanup() = boundFactoryFailure(true)

  private fun boundFactoryFailure(acceptRoute: Boolean) {
    val context = mock(Context::class.java)
    val registered = mutableListOf<BroadcastReceiver>()
    val released = mutableListOf<BroadcastReceiver>()
    doAnswer { call -> registered.add(call.getArgument(0)); null }
      .`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    doAnswer { call -> released.add(call.getArgument(0)); null }
      .`when`(context).unregisterReceiver(any(BroadcastReceiver::class.java))
    var retained = false
    var releaseNative = false
    var installs = 0
    val api = object : BoundInstallApi {
      override fun routeVersion() = 1
      override fun install(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, label: String): Long {
        installs++
        if (acceptRoute) { retained = true; check(radio.acceptBindingRoute(1, 42)) }
        throw IllegalStateException("injected install failure")
      }
      override fun find(radio: MobileCoreBridge.RadioHost) = if (retained) 42L else 0L
      override fun shutdown(binding: Long): String {
        assertEquals(42L, binding)
        if (releaseNative) { retained = false; return "{\"state\":\"released\"}" }
        return "{\"state\":\"fenced-pending\"}"
      }
    }
    val tasks = ArrayDeque<Runnable>()
    lateinit var host: RustCoreProcessHost
    host = RustCoreProcessHost.bound({ core ->
      host.createOwnedAdapter(core, OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true }),
        FakeBackground(), { null }, { null }, Executor { it.run() }, Executor { it.run() }) {}
    }, { _, task -> tasks.add(task) }, {}, { JniBoundMobileCorePort().also { it.installApi = api } })
    val first = host.core
    try { host.ensureInstalled(); fail("expected install failure") } catch (_: IllegalStateException) { }
    assertEquals(1, installs)
    assertEquals(2, registered.size)
    if (acceptRoute) {
      tasks.removeFirst().run()
      assertSame(first, host.core)
      assertTrue(released.isEmpty())
      try { host.ensureInstalled(); fail("pending owner must block replacement") }
      catch (_: RustCoreRejection) { }
      assertEquals(1, installs)
      assertEquals(2, registered.size)
      releaseNative = true
    }
    tasks.removeFirst().run()
    assertNotSame(first, host.core)
    assertEquals(registered.toSet(), released.toSet())
    assertEquals(2, released.size)
  }

  @Test fun uncertainFirstReceiverRegistrationRetainsTheExactReceiverForRetry() {
    val context = mock(Context::class.java)
    var receiver: BroadcastReceiver? = null
    doAnswer { call -> receiver = call.getArgument(0); throw IllegalStateException("uncertain admission") }
      .`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    doThrow(SecurityException("cleanup denied")).doNothing()
      .`when`(context).unregisterReceiver(any(BroadcastReceiver::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    try { radio.registerAdapterStateReceiver(); fail("expected failure") } catch (_: IllegalStateException) { }
    assertNotNull(radio.unregisterAdapterStateReceiver())
    assertNull(radio.unregisterAdapterStateReceiver())
    verify(context, times(2)).unregisterReceiver(checkNotNull(receiver))
  }

  @Test fun receiverReplacementRetriesCleanupAndRejectsStaleCallbacks() {
    val context = mock(Context::class.java)
    val receivers = mutableListOf<BroadcastReceiver>()
    doAnswer { call -> receivers.add(call.getArgument(0)); null }
      .`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    doThrow(SecurityException("cleanup denied")).doNothing()
      .`when`(context).unregisterReceiver(any(BroadcastReceiver::class.java))
    val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
    var observed = 0
    radio.onAdapterState = { observed++ }
    radio.registerAdapterStateReceiver()
    assertNotNull(radio.unregisterAdapterStateReceiver())
    radio.registerAdapterStateReceiver()
    assertEquals(2, receivers.size)
    val intent = mock(Intent::class.java)
    `when`(intent.action).thenReturn(BluetoothAdapter.ACTION_STATE_CHANGED)
    `when`(intent.getIntExtra(BluetoothAdapter.EXTRA_STATE, BluetoothAdapter.ERROR)).thenReturn(BluetoothAdapter.STATE_ON)
    receivers[0].onReceive(context, intent)
    assertEquals(0, observed)
    receivers[1].onReceive(context, intent)
    assertEquals(1, observed)
    assertNull(radio.unregisterAdapterStateReceiver())
  }
  @Test fun constructorFailureRetainsReceiverUntilCleanupSucceeds() = exercise(true)
  @Test fun installFailureRetainsReceiversUntilNativeAndLocalCleanupSucceed() = exercise(false)

  private fun exercise(failConstruction: Boolean) {
    val context = mock(Context::class.java)
    val registrations = mutableListOf<BroadcastReceiver>()
    val releases = mutableListOf<BroadcastReceiver>()
    var failRelease = true
    doAnswer { call ->
      registrations.add(call.getArgument(0))
      if (failConstruction && registrations.size == 2) throw IllegalStateException("registration failed")
      null
    }.`when`(context).registerReceiver(any(BroadcastReceiver::class.java), any(IntentFilter::class.java))
    doAnswer { call ->
      val receiver = call.getArgument<BroadcastReceiver>(0)
      if (failRelease) { failRelease = false; throw SecurityException("cleanup denied") }
      releases.add(receiver)
      null
    }.`when`(context).unregisterReceiver(any(BroadcastReceiver::class.java))
    var nativeClear = failConstruction
    val core = object : MobileCorePort by FakeCore() {
      override fun installHost(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, adapterLabel: String) {
        throw IllegalStateException("install failed")
      }
      override fun cleanupFailedInstall() = nativeClear
    }
    val tasks = ArrayDeque<Runnable>()
    lateinit var host: RustCoreProcessHost
    host = RustCoreProcessHost(core, {
      val radio = OwnedAndroidGattRadio(context, post = { it(); true }, scheduleDelayed = { _, _ -> true })
      host.createOwnedAdapter(core, radio, FakeBackground(), { null }, { null }, Executor { it.run() }, Executor { it.run() }) {}
    }, { _, task -> tasks.add(task) }, {})
    host.setCoreFactory { FakeCore() }
    try { host.ensureInstalled(); fail("expected failure") } catch (_: IllegalStateException) { }
    assertEquals(2, registrations.size)
    assertEquals(1, tasks.size)
    if (!failConstruction) {
      tasks.removeFirst().run()
      assertTrue(releases.isEmpty())
      nativeClear = true
    }
    tasks.removeFirst().run()
    assertEquals(1, releases.size)
    assertSame(core, host.core)
    try { host.ensureInstalled(); fail("pending receiver must block replacement") }
    catch (_: RustCoreRejection) { }
    assertEquals(2, registrations.size)
    tasks.removeFirst().run()
    assertEquals(2, releases.size)
    assertEquals(registrations.toSet(), releases.toSet())
    assertNotSame(core, host.core)
  }
}
