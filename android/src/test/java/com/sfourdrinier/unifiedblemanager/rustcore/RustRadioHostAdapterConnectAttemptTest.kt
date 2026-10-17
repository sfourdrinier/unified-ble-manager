// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustRadioHostAdapterConnectAttemptTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothProfile
import com.sfourdrinier.unifiedblemanager.radio.GattConnectAttempt
import com.sfourdrinier.unifiedblemanager.radio.GattRadioFixture
import com.sfourdrinier.unifiedblemanager.radio.connectGattCalls
import com.sfourdrinier.unifiedblemanager.radio.connectGattReturns
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotSame
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito.doThrow
import org.mockito.Mockito.verify

/**
 * The active mobile route settles a Rust connect request only on the GATT its own
 * `radio.connect` opened. A prior generation's loss for the same peer (its native DISCONNECTED, or
 * its forced close inside `connect()`) is still processed, but it neither claims, fails nor
 * retires the replacement request, and the replacement then opens and settles exactly once.
 *
 * Evidence level: the real [RustRadioHostAdapter] over the real [OwnedRadioPort] and the real
 * `OwnedAndroidGattRadio`; only the `android.bluetooth` boundary is mocked (adapter, device, GATT,
 * main-thread scheduling) and the Rust core is [FakeCore], which records what would cross the JNI
 * boundary (`unit:` / `failure:` completions and `link:` ingress). HOST-JVM synthetic: no real Rust
 * core, no JNI, no device.
 */
class RustRadioHostAdapterConnectAttemptTest {
  private val noPhy = emptyArray<String>()

  private class ReentrantRadio(private val delegate: FakeRadio = FakeRadio()) : AndroidRadioPort by delegate {
    var connectHook: (() -> Unit)? = null
    var delayedDisconnect: ((Throwable?) -> Unit)? = null
    val calls: MutableList<String> get() = delegate.calls

    override fun connect(peerId: String, autoConnect: Boolean, phyMask: Int, attempt: GattConnectAttempt) {
      delegate.connect(peerId, autoConnect, phyMask, attempt)
      connectHook?.invoke()
    }

    override fun disconnect(peerId: String, onComplete: (Throwable?) -> Unit) {
      calls.add("disconnect:$peerId")
      delayedDisconnect = onComplete
    }
  }

  private class Stack(refuseCloseDeadline: Boolean = false) {
    val fixture = GattRadioFixture().also { it.refuseDeadline = refuseCloseDeadline }
    val core = FakeCore()
    val logs = mutableListOf<String>()
    val peer = fixture.peer
    val adapter = RustRadioHostAdapter(
      core,
      OwnedRadioPort(fixture.radio) { logs.add(it) },
      FakeBackground(),
      { null },
      { null },
      DirectExecutor,
      DirectExecutor
    ) { logs.add(it) }

    /** A GATT the OS still holds for the peer that the adapter never saw connect (a stuck connect). */
    fun priorGatt(): BluetoothGatt = fixture.gatt().also { fixture.radio.attachConnectedGatt(peer, it, emptyList()) }

    fun native(gatt: BluetoothGatt, status: Int, state: Int) =
      fixture.radio.nativeGattCallback().onConnectionStateChange(gatt, status, state)

    fun loss(gatt: BluetoothGatt, status: Int = BluetoothGatt.GATT_SUCCESS) =
      native(gatt, status, BluetoothProfile.STATE_DISCONNECTED)

    fun connected(gatt: BluetoothGatt) =
      native(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)

    fun settlements(): List<String> = core.calls.filter { it.startsWith("unit:") || it.startsWith("failure:") }
  }

  private fun <T> stack(refuseCloseDeadline: Boolean = false, block: (Stack) -> T): T = block(Stack(refuseCloseDeadline))

  @Test
  fun priorNativeDisconnectedDoesNotClaimTheReplacementConnectAndItsSuccessIsOwned() = stack { s ->
    val prior = s.priorGatt()
    val replacement = s.fixture.gatt()
    connectGattReturns(s.fixture, replacement)

    s.adapter.connect(1, s.peer, false, noPhy, "test-generation-1")
    assertEquals("the replacement waits for the prior teardown", 0, connectGattCalls(s.fixture))

    s.loss(prior)
    assertEquals("the prior's loss must not settle the replacement: ${s.core.calls}", emptyList<String>(), s.settlements())
    assertEquals(1, connectGattCalls(s.fixture))
    assertEquals("the prior's loss stays reported, never silent", 1L, s.adapter.statusCounts()["superseded-connect-outcome"])
    assertTrue(s.logs.any { it.contains("superseded") })
    assertTrue("no link ingress for a link the core never had: ${s.core.calls}", s.core.calls.none { it.startsWith("link:") })

    s.connected(replacement)
    assertEquals(listOf("unit:1"), s.settlements())
    assertEquals(listOf("link:${s.peer}:true:0"), s.core.calls.filter { it.startsWith("link:") })
    // The link is owned: operations on it are admitted, and its later loss is ingested once.
    s.loss(replacement, 8)
    assertEquals(listOf("link:${s.peer}:true:0", "link:${s.peer}:false:8"), s.core.calls.filter { it.startsWith("link:") })
    assertEquals(listOf("unit:1"), s.settlements())
  }

  @Test
  fun refusedCloseDeadlineForcedCloseOfThePriorDoesNotClaimTheReplacementConnect() = stack(refuseCloseDeadline = true) { s ->
    val prior = s.priorGatt()
    val replacement = s.fixture.gatt()
    connectGattReturns(s.fixture, replacement)

    // The scheduler refuses the close deadline: the prior is force-closed inside connect(), its
    // loss is published (status 257) and the replacement opens without any throw.
    s.adapter.connect(1, s.peer, false, noPhy, "test-generation-1")
    assertEquals(1, connectGattCalls(s.fixture))
    assertEquals("the forced close must not settle the replacement: ${s.core.calls}", emptyList<String>(), s.settlements())
    assertEquals(1L, s.adapter.statusCounts()["superseded-connect-outcome"])
    verify(prior).close()

    s.connected(replacement)
    assertEquals(listOf("unit:1"), s.settlements())
    assertEquals(listOf("link:${s.peer}:true:0"), s.core.calls.filter { it.startsWith("link:") })
  }

  @Test
  fun priorLossThenReplacementConnectFailureReportsTheReplacementsOwnFailureOnce() = stack { s ->
    val prior = s.priorGatt()
    val replacement = s.fixture.gatt()
    connectGattReturns(s.fixture, replacement)
    s.adapter.connect(1, s.peer, false, noPhy, "test-generation-1")

    s.loss(prior)
    assertEquals(emptyList<String>(), s.settlements())

    s.loss(replacement, 133)
    assertEquals(listOf("failure:1:gatt-status:133"), s.settlements())
    assertEquals(133, s.core.failures.getValue(1).gattStatus)
    // The request's ownership is gone: a new connect is admitted, not busy.
    s.core.calls.clear()
    val next = s.fixture.gatt()
    connectGattReturns(s.fixture, next)
    s.adapter.connect(2, s.peer, false, noPhy, "test-generation-2")
    assertTrue("a settled connect leaves no ownership: ${s.core.calls}", s.core.calls.none { it.startsWith("failure:2") })
  }

  @Test
  fun replacementThatCannotOpenAfterThePriorLossFailsWithItsOwnFailureOnceAndFreesTheOwnership() = stack { s ->
    val prior = s.priorGatt()
    doThrow(SecurityException("BLUETOOTH_CONNECT revoked")).`when`(s.fixture.device)
      .connectGatt(eq(s.fixture.context), eq(false), any(), eq(android.bluetooth.BluetoothDevice.TRANSPORT_LE))
    s.adapter.connect(1, s.peer, false, noPhy, "test-generation-1")
    assertEquals(emptyList<String>(), s.settlements())

    s.loss(prior)
    // The asynchronous open of the replacement failed: its own failure (257), settled once.
    assertEquals("one terminal, was ${s.core.calls}", listOf("failure:1:gatt-status:257"), s.settlements())
    assertEquals(BluetoothGatt.GATT_FAILURE, s.core.failures.getValue(1).gattStatus)
    val retry = s.fixture.gatt()
    connectGattReturns(s.fixture, retry)
    s.core.calls.clear()
    s.adapter.connect(2, s.peer, false, noPhy, "test-generation-2")
    assertTrue("no ownership left behind: ${s.core.calls}", s.core.calls.none { it.startsWith("failure:2") })
  }

  @Test
  fun cancellationOfTheReplacementConnectSettlesOnceAndThePriorLossIsStillObserved() = stack { s ->
    val prior = s.priorGatt()
    val replacement = s.fixture.gatt()
    connectGattReturns(s.fixture, replacement)
    s.adapter.connect(1, s.peer, false, noPhy, "test-generation-1")

    s.adapter.cancel(1)
    assertEquals(listOf("failure:1:cancelled:null"), s.settlements())

    s.loss(prior)
    assertEquals("the cancelled request is not answered again: ${s.core.calls}", listOf("failure:1:cancelled:null"), s.settlements())
    assertEquals("the queued replacement was withdrawn by the cancellation", 0, connectGattCalls(s.fixture))
    assertEquals(1L, s.adapter.statusCounts()["disconnect-without-link"])
  }

  @Test
  fun staleCallbackOfThePriorAfterTheReplacementConnectedLeavesTheLinkOwned() = stack { s ->
    val prior = s.priorGatt()
    val replacement = s.fixture.gatt()
    connectGattReturns(s.fixture, replacement)
    s.adapter.connect(1, s.peer, false, noPhy, "test-generation-1")
    s.loss(prior)
    s.connected(replacement)
    s.core.calls.clear()

    // A late callback of the superseded generation is fenced by the radio and never reaches Rust.
    s.loss(prior, 8)
    assertEquals("no loss for the live link: ${s.core.calls}", emptyList<String>(), s.core.calls)
    s.adapter.connect(2, s.peer, false, noPhy, "test-generation-2")
    assertEquals("the link is still established", listOf("unit:2"), s.core.calls)
  }

  /** Token-level contract on the port, deterministic and independent of the radio's scheduling. */
  @Test
  fun connectHandsTheRadioItsOwnTokenAndOnlyThatTokenSettlesIt() {
    val core = FakeCore()
    val radio = FakeRadio()
    val peer = "AA:BB:CC:DD:EE:FF"
    val adapter = RustRadioHostAdapter(core, radio, FakeBackground(), { null }, { null }, DirectExecutor, DirectExecutor) { }
    adapter.connect(1, peer, false, noPhy, "test-generation-1")
    adapter.connect(2, "11:22:33:44:55:66", false, noPhy, "test-generation-2")
    val own = radio.attempts.getValue(peer)
    assertNotSame("each request carries its own identity", own, radio.attempts.getValue("11:22:33:44:55:66"))

    val superseded = GattConnectAttempt()
    radio.events.onConnection(peer, false, 0, superseded)
    radio.events.onConnection(peer, false, 133, null)
    radio.events.onConnection(peer, true, 0, superseded)
    assertEquals("no foreign observation settles the request: ${core.calls}", emptyList<String>(), core.calls.filter {
      it.startsWith("unit:1") || it.startsWith("failure:1")
    })
    assertTrue("a superseded success must not ingest a link: ${core.calls}", core.calls.none { it.startsWith("link:") })

    radio.events.onConnection(peer, false, 133, own)
    assertEquals(listOf("failure:1:gatt-status:133"), core.calls.filter { it.startsWith("failure:1") })
    radio.events.onConnection(peer, false, 133, own)
    assertEquals("a settled request is never answered twice", 1, core.calls.count { it.startsWith("failure:1") })
  }

  @Test
  fun adapterLossFailsEveryPendingConnectOnceAndFreesItsOwnership() {
    val core = FakeCore()
    val radio = FakeRadio()
    val peer = "AA:BB:CC:DD:EE:FF"
    val adapter = RustRadioHostAdapter(core, radio, FakeBackground(), { null }, { null }, DirectExecutor, DirectExecutor) { }
    adapter.connect(1, peer, false, noPhy, "test-generation-1")
    val own = radio.attempts.getValue(peer)

    radio.events.onAdapterState(AdapterFacts("available", "granted", "off", null))
    assertEquals(1, core.calls.count { it.startsWith("failure:1:adapter-off") })
    // The OS then reports the force-closed link for the same request: nothing is answered twice.
    radio.events.onConnection(peer, false, 0, own)
    assertEquals(1, core.calls.count { it.startsWith("failure:1") })
    adapter.connect(2, peer, false, noPhy, "test-generation-2")
    assertTrue("ownership was released: ${core.calls}", core.calls.none { it.startsWith("failure:2") })
  }

  @Test
  fun aConnectTheDriverRefusesLeavesNoOwnership() {
    val core = FakeCore()
    val radio = FakeRadio()
    radio.connectFailure = RadioPortFailure(RadioFailureKind.PLATFORM, "refused")
    val peer = "AA:BB:CC:DD:EE:FF"
    val adapter = RustRadioHostAdapter(core, radio, FakeBackground(), { null }, { null }, DirectExecutor, DirectExecutor) { }
    adapter.connect(1, peer, false, noPhy, "test-generation-1")
    assertEquals(1, core.calls.count { it.startsWith("failure:1:platform") })
    radio.connectFailure = null
    adapter.connect(2, peer, false, noPhy, "test-generation-2")
    assertTrue("a refused connect is not busy-owned: ${core.calls}", core.calls.none { it.startsWith("failure:2") })
  }

  @Test
  fun explicitDisconnectWithdrawsPendingConnectBeforeAllowingRetry() {
    val core = FakeCore()
    val radio = FakeRadio()
    val peer = "AA:BB:CC:DD:EE:FF"
    val adapter = RustRadioHostAdapter(core, radio, FakeBackground(), { null }, { null }, DirectExecutor, DirectExecutor) { }

    adapter.connect(1, peer, false, noPhy, "test-generation-1")
    adapter.disconnect(2, peer)

    assertEquals("failure:1:cancelled:null", core.calls.single { it.startsWith("failure:1") })
    assertEquals("unit:2", core.calls.single { it.startsWith("unit:2") })
    assertEquals(listOf("connect:$peer:false", "disconnect:$peer"), radio.calls)

    adapter.connect(3, peer, false, noPhy, "test-generation-2")
    assertTrue("cleanup releases the reservation for retry: ${core.calls}", core.calls.none { it.startsWith("failure:3") })
  }

  @Test
  fun cancellationInsideConnectDispatchDefersCleanupUntilDispatchReturns() {
    val core = FakeCore()
    val radio = ReentrantRadio()
    val peer = "AA:BB:CC:DD:EE:FF"
    lateinit var adapter: RustRadioHostAdapter
    adapter = RustRadioHostAdapter(core, radio, FakeBackground(), { null }, { null }, DirectExecutor, DirectExecutor) { }
    radio.connectHook = {
      adapter.cancel(1)
    }

    adapter.connect(1, peer, false, noPhy, "test-generation-1")

    assertEquals(listOf("failure:1:cancelled:null"), core.calls.filter { it.startsWith("failure:1") })
    assertEquals(listOf("connect:$peer:false", "disconnect:$peer"), radio.calls)
  }

  @Test
  fun repeatedDisconnectsJoinDelayedCleanupAndPreserveFailure() {
    val core = FakeCore()
    val radio = ReentrantRadio()
    val peer = "AA:BB:CC:DD:EE:FF"
    val adapter = RustRadioHostAdapter(core, radio, FakeBackground(), { null }, { null }, DirectExecutor, DirectExecutor) { }

    adapter.connect(1, peer, false, noPhy, "test-generation-1")
    adapter.disconnect(2, peer)
    adapter.disconnect(3, peer)
    assertEquals(listOf("failure:1:cancelled:null"), core.calls.filter { it.startsWith("failure:1") })
    assertTrue("cleanup is single-shot: ${radio.calls}", radio.calls.count { it == "disconnect:$peer" } == 1)
    assertTrue("waiters remain pending until native cleanup returns", core.calls.none { it == "unit:2" || it == "unit:3" })

    radio.delayedDisconnect?.invoke(RadioPortFailure(RadioFailureKind.PLATFORM, "cleanup failed"))

    assertEquals(listOf("failure:2:platform:null", "failure:3:platform:null"), core.calls.filter {
      it.startsWith("failure:2") || it.startsWith("failure:3")
    })
  }
}
