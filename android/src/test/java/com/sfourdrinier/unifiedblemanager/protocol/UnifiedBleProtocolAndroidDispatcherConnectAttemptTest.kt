// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcherConnectAttemptTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.content.Context
import android.content.pm.PackageManager
import android.os.Handler
import com.sfourdrinier.unifiedblemanager.protocol.generated.RecordKind
import com.sfourdrinier.unifiedblemanager.radio.GattObservation
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.UbmGattCoreBinding
import java.util.Collections
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.anyLong
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito

/**
 * A connect command is settled only by the GATT its own `radio.connect` opened. The real public
 * dispatcher over the real [OwnedAndroidGattRadio] (mocked adapter/device/GATT, fake core JNI,
 * stubbed JSI): a prior generation's teardown for the same peer is still observed, but it never
 * claims, fails or retires the replacement connect, and the replacement then opens and settles
 * exactly once. HOST-JVM synthetic evidence, no physical radio.
 */
class UnifiedBleProtocolAndroidDispatcherConnectAttemptTest {
  private val peer = "AA:BB:CC:DD:EE:FF"
  private val direct = Executor { command -> command.run() }

  private fun connectionFor(
    peerId: String,
    connectionId: String,
    lease: String,
    generation: String
  ): ProtocolWireRecord = ProtocolWireRecord(
    RecordKind.CONNECTION_PATH,
    mapOf(
      1 to ProtocolWireValue.RecordValue(attachmentRecord()),
      2 to ProtocolWireValue.StringValue(peerId),
      3 to ProtocolWireValue.StringValue(connectionId),
      4 to ProtocolWireValue.StringValue(lease),
      5 to ProtocolWireValue.StringValue(generation)
    )
  )

  private class FakeJni : UbmGattCoreBinding.CoreJni {
    val enqueued: MutableList<String> = Collections.synchronizedList(mutableListOf())
    var afterEnqueue: (() -> Unit)? = null
    var failNextLinkRelease = false
    var failNextDisconnect = false
    override fun open(revision: String): Long = 7L
    override fun revision(): String = UbmGattCoreBinding.CONTRACT_REVISION
    override fun enqueue(handle: Long, wire: String): Int {
      enqueued.add(wire)
      afterEnqueue?.invoke()
      return enqueued.size
    }
    override fun drain(handle: Long): String {
      val wire = enqueued.lastOrNull() ?: return ""
      val fields = wire.split('|')
      val event = when {
        wire.startsWith("connect|") -> "connect"
        wire.startsWith("disconnect|") -> "disconnect"
        wire.startsWith("link.released") -> "link.released"
        else -> return ""
      }
      val peer = fields.getOrNull(1) ?: return ""
      val lease = fields.getOrNull(2) ?: return ""
      if (event == "link.released" && failNextLinkRelease) {
        failNextLinkRelease = false
        return "{\"ok\":false,\"event\":\"link.released.scoped\",\"code\":\"link.release.failed\",\"domain\":\"connection\",\"operation\":\"connection.linkReleased\",\"detail\":\"injected cleanup refusal\",\"effects\":[],\"observations\":[]}"
      }
      if (event == "disconnect" && failNextDisconnect) {
        failNextDisconnect = false
        return "{\"ok\":false,\"event\":\"connection.stale\",\"code\":\"connection.stale\",\"domain\":\"connection\",\"operation\":\"connection.disconnect\",\"detail\":\"injected compensation refusal\",\"effects\":[],\"observations\":[]}"
      }
      return if (event == "connect") {
        "{\"ok\":true,\"event\":\"connect\",\"peer\":\"$peer\",\"lease\":\"$lease\",\"op\":\"connection.connect\",\"generation\":\"test-generation\",\"effects\":[],\"observations\":[]}"
      } else {
        "{\"ok\":true,\"event\":\"disconnect\",\"peer\":\"$peer\",\"lease\":\"$lease\",\"op\":\"connection.disconnect\",\"effects\":[],\"observations\":[]}"
      }
    }
    override fun depth(handle: Long): Int = enqueued.size
    override fun close(handle: Long) {}
  }

  private inner class Harness(
    armCloseDeadline: Boolean,
    private val coreWorker: Executor = direct
  ) {
    val jni = FakeJni()
    val emitted: MutableList<ByteArray> = Collections.synchronizedList(mutableListOf())
    val diagnostics: MutableList<String> = Collections.synchronizedList(mutableListOf())
    val device: BluetoothDevice = Mockito.mock(BluetoothDevice::class.java)
    private val adapter: BluetoothAdapter = Mockito.mock(BluetoothAdapter::class.java)
    val context: Context = Mockito.mock(Context::class.java)
    val mocked = Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java).also {
      JsiStaticStubs.stubEmitRecord(it, emitted)
      JsiStaticStubs.stubEmitDiagnostic(it, diagnostics)
    }
    val dispatcher: UnifiedBleProtocolAndroidDispatcher
    val radio: OwnedAndroidGattRadio

    init {
      Mockito.`when`(context.applicationContext).thenReturn(context)
      Mockito.`when`(context.checkSelfPermission(Mockito.anyString())).thenReturn(PackageManager.PERMISSION_GRANTED)
      val manager = Mockito.mock(BluetoothManager::class.java)
      Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(manager)
      Mockito.`when`(manager.adapter).thenReturn(adapter)
      Mockito.`when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
      Mockito.`when`(adapter.getRemoteDevice(peer)).thenReturn(device)
      Mockito.`when`(device.address).thenReturn(peer)
      dispatcher = UnifiedBleProtocolAndroidDispatcher(context, 0xA11CE2L) { ctx, onRejection ->
        UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = onRejection, worker = coreWorker, clockMs = { 1000L })
      }
      val field = UnifiedBleProtocolAndroidDispatcher::class.java.getDeclaredField("radio")
      field.isAccessible = true
      radio = field.get(dispatcher) as? OwnedAndroidGattRadio ?: error("dispatcher radio is not an OwnedAndroidGattRadio")
      // The close deadline is armed (and never fires: the native callback is what ends the test)
      // or refused, which is the scheduler-refusal route that force-closes the prior GATT.
      val handler = Mockito.mock(Handler::class.java)
      Mockito.`when`(handler.postDelayed(any(Runnable::class.java), anyLong())).thenReturn(armCloseDeadline)
      val handlerField = OwnedAndroidGattRadio::class.java.getDeclaredField("mainHandler")
      handlerField.isAccessible = true
      handlerField.set(radio, handler)
    }

    fun gatt(): BluetoothGatt {
      val gatt = Mockito.mock(BluetoothGatt::class.java)
      Mockito.`when`(gatt.device).thenReturn(device)
      return gatt
    }

    /** What [BluetoothDevice.connectGatt] returns, in order, for each opened link. */
    fun connectGattReturns(vararg opened: BluetoothGatt) {
      var stub = Mockito.doReturn(opened.first())
      opened.drop(1).forEach { stub = stub.doReturn(it) }
      stub.`when`(device).connectGatt(eq(context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    }

    fun connectGattAnswers(answer: (BluetoothGattCallback) -> BluetoothGatt) {
      Mockito.doAnswer { invocation ->
        answer(invocation.getArgument(2))
      }.`when`(device).connectGatt(eq(context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    }

    fun connectGattThrows(error: Throwable) {
      Mockito.doThrow(error).`when`(device)
        .connectGatt(eq(context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    }

    fun connectGattCalls(): Int =
      Mockito.mockingDetails(device).invocations.count { it.method.name == "connectGatt" }

    fun connect(nonce: String, connection: ProtocolWireRecord = connectionRecord(peer)) {
      dispatcher.dispatch(
        commandBytes(
          "connect",
          1L,
          nonce,
          mapOf(
            10 to ProtocolWireValue.RecordValue(connection),
            20 to ProtocolWireValue.StringValue("direct")
          )
        )
      )
    }

    fun cancel(dispatchEpoch: Long, nonce: String) {
      dispatcher.cancelPendingOperation(dispatchEpoch, nonce)
    }

    fun disconnect(nonce: String, connection: ProtocolWireRecord = connectionRecord(peer)) {
      dispatcher.dispatch(
        commandBytes(
          "disconnect",
          2L,
          nonce,
          mapOf(10 to ProtocolWireValue.RecordValue(connection))
        )
      )
    }

    fun native(gatt: BluetoothGatt, status: Int, state: Int) {
      radio.nativeGattCallback().onConnectionStateChange(gatt, status, state)
    }

    fun results(): List<ParsedTerminal> =
      emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue }.let(::parseTerminals)

    fun coreLines(prefix: String): List<String> = jni.enqueued.filter { it.startsWith(prefix) }

    fun close() {
      dispatcher.close()
      mocked.close()
    }
  }

  private fun <T> Harness.use(block: (Harness) -> T): T =
    try {
      block(this)
    } finally {
      close()
    }

  /** The prior GATT the core considers idle (an earlier connect that never completed). */
  private fun Harness.priorGatt(): BluetoothGatt {
    val prior = gatt()
    radio.attachConnectedGatt(peer, prior, emptyList())
    return prior
  }

  @Test
  fun priorNativeDisconnectedDoesNotClaimTheReplacementConnectAndItsSuccessIsOwned() = Harness(true).use { h ->
    val prior = h.priorGatt()
    val replacement = h.gatt()
    h.connectGattReturns(replacement)

    h.connect("replacement")
    assertEquals("the replacement waits for the prior teardown", 0, h.connectGattCalls())

    h.native(prior, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals("the prior's loss must not settle the replacement", emptyList<ParsedTerminal>(), h.results())
    assertEquals(1, h.connectGattCalls())
    assertTrue("a prior loss must not retire the admitted core connect", h.coreLines("peer.loss").isEmpty())

    h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    val results = h.results()
    assertEquals("one terminal, was $results", 1, results.size)
    assertEquals("succeeded", results.single().outcome)
    assertEquals("connected", results.single().resultKind)
    assertEquals(1, h.coreLines("link.established").size)
  }

  @Test
  fun refusedCloseDeadlineForcedCloseOfThePriorDoesNotClaimTheReplacementConnect() = Harness(false).use { h ->
    val prior = h.priorGatt()
    val replacement = h.gatt()
    h.connectGattReturns(replacement)

    // The scheduler refuses the close deadline: the prior is force-closed inside connect(), its
    // loss is published, and the replacement opens without any throw.
    h.connect("replacement")
    assertEquals(1, h.connectGattCalls())
    assertEquals("the forced close of the prior must not settle the replacement", emptyList<ParsedTerminal>(), h.results())
    assertTrue(h.coreLines("peer.loss").isEmpty())

    h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    val results = h.results()
    assertEquals("one terminal, was $results", 1, results.size)
    assertEquals("succeeded", results.single().outcome)
    assertEquals(1, h.coreLines("link.established").size)
    Mockito.verify(prior).close()
  }

  @Test
  fun priorLossThenReplacementConnectFailureReportsTheReplacementsOwnFailureOnce() = Harness(true).use { h ->
    val prior = h.priorGatt()
    val replacement = h.gatt()
    h.connectGattReturns(replacement)
    h.connect("replacement")

    h.native(prior, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals(emptyList<ParsedTerminal>(), h.results())

    h.native(replacement, 133, BluetoothProfile.STATE_DISCONNECTED)
    val results = h.results()
    assertEquals("one terminal, was $results", 1, results.size)
    assertEquals("failed", results.single().outcome)
    assertEquals("connectionFailed", results.single().errorCode)
    assertTrue(
      "the failure is the replacement's own (status 133), was ${results.single().errorMessage}",
      results.single().errorMessage.contains("status 133")
    )
  }

  @Test
  fun replacementThatCannotOpenAfterThePriorLossFailsWithItsOwnFailureOnce() = Harness(true).use { h ->
    val prior = h.priorGatt()
    Mockito.doReturn(null).`when`(h.device)
      .connectGatt(eq(h.context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    h.connect("replacement")

    h.native(prior, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)

    val results = h.results()
    assertEquals("one terminal, was $results", 1, results.size)
    assertEquals("failed", results.single().outcome)
    assertEquals("connectionFailed", results.single().errorCode)
    assertTrue(
      "the failure is the replacement's open failure (status ${BluetoothGatt.GATT_FAILURE}), was ${results.single().errorMessage}",
      results.single().errorMessage.contains("status ${BluetoothGatt.GATT_FAILURE}")
    )
  }

  @Test
  fun staleCallbackOfThePriorAfterTheReplacementConnectedLeavesTheLinkEstablished() = Harness(true).use { h ->
    val prior = h.priorGatt()
    val replacement = h.gatt()
    h.connectGattReturns(replacement)
    h.connect("replacement")
    h.native(prior, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    val before = h.emitted.size
    val coreBefore = h.jni.enqueued.size

    // A late callback of the closed prior generation is fenced: nothing is published or posted.
    h.native(prior, 8, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals(before, h.emitted.size)
    assertEquals(coreBefore, h.jni.enqueued.size)

    // The replacement's own later loss is still its real link loss.
    h.native(replacement, 8, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals(1, h.coreLines("link.released").size)
    assertTrue(h.coreLines("peer.loss").isEmpty())
    assertTrue("the loss of the established link is reported", h.emitted.size > before)
  }

  @Test
  fun disconnectInsideConnectGattDefersCleanupUntilOpenReturnsAndRetryOwnsNewAttempt() = Harness(true).use { h ->
    val first = h.gatt()
    val replacement = h.gatt()
    var insideConnectGatt = false
    h.connectGattAnswers {
      insideConnectGatt = true
      h.disconnect(
        "inline-disconnect",
        connectionFor(peer, "conn-1", "android-link-${peer.uppercase()}-1:inline-connect", "test-generation")
      )
      assertEquals("disconnect must not close a GATT before connectGatt returns", 0, Mockito.mockingDetails(first).invocations.count { it.method.name == "close" })
      insideConnectGatt = false
      first
    }

    h.connect("inline-connect")
    assertTrue("the answer must have run", !insideConnectGatt)
    assertEquals("one native connect", 1, h.connectGattCalls())
    Mockito.verify(first, Mockito.never()).close()

    h.native(first, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    Mockito.verify(first).close()
    val cancelledConnects = h.results().filter { it.resultKind == "cancelled" }
    assertEquals("the inline disconnect cancels the connect exactly once", 1, cancelledConnects.size)
    val disconnectResults = h.results().filter { it.resultKind == "accepted" }
    assertEquals("exactly one disconnect terminal", 1, disconnectResults.size)

    h.connectGattReturns(replacement)
    h.connect("retry")
    assertEquals("retry opens after deferred physical cleanup retires", 2, h.connectGattCalls())
    h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    val connectResults = h.results().filter { it.resultKind == "connected" }
    assertEquals("retry settles its own connect once", 1, connectResults.size)
    assertEquals("succeeded", connectResults.single().outcome)
  }

  @Test
  fun cancellationReentrantDuringReservedAdmissionNeverCallsNativeConnectOrCompensatesLease() = Harness(true).use { h ->
    h.jni.afterEnqueue = { h.cancel(1L, "reserved-cancel") }

    h.connect("reserved-cancel")

    assertEquals("reservation cancellation must prevent native connect", 0, h.connectGattCalls())
    assertEquals("the reserved command is canceled once: ${h.results()}", 1, h.results().size)
    assertEquals("reserved cancellation compensates the admitted core lease once", 1, h.coreLines("disconnect|").size)
  }

  @Test
  fun scheduleFailureAfterConnectQueueAttestsAndCompensatesRealAdmissionWithoutNativeConnect() {
    var scheduleCalls = 0
    val rejectConnectDrain = Executor { command ->
      scheduleCalls += 1
      if (scheduleCalls == 2) throw IllegalStateException("connect-drain-schedule-rejected")
      command.run()
    }
    Harness(true, rejectConnectDrain).use { h ->
      val replacement = h.gatt()
      h.connectGattReturns(replacement)
      var nestedReplacementAttempted = false
      h.jni.afterEnqueue = {
        if (h.jni.enqueued.lastOrNull()?.startsWith("disconnect|") == true && !nestedReplacementAttempted) {
          nestedReplacementAttempted = true
          h.connect("replacement-before-release")
        }
      }

      h.connect("scheduled-connect")

      assertEquals("scheduled connect never starts native radio", 0, h.connectGattCalls())
      val connectTerminals = h.results().filter { it.resultKind == "connected" }
      assertEquals(2, connectTerminals.size)
      assertEquals(2, connectTerminals.count { it.outcome == "failed" })
      assertEquals(0, connectTerminals.count { it.outcome == "succeeded" })
      assertEquals(
        "the queued connect reports the scheduler authority code: $connectTerminals",
        1,
        connectTerminals.count {
          it.outcome == "failed" && it.errorCode == CoreCommandAuthority.CODE_SCHEDULE_FAILED
        }
      )
      val connectWire = h.coreLines("connect|").single()
      val disconnectWires = h.coreLines("disconnect|")
      assertEquals(1, disconnectWires.size)
      assertEquals(connectWire.split('|')[2], disconnectWires.single().split('|')[2])
      assertEquals("no replacement opens before the explicit post-release retry", 0, h.connectGattCalls())

      h.connect("replacement-after-release")
      assertEquals(1, h.connectGattCalls())
      h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
      val finalTerminals = h.results().filter { it.resultKind == "connected" }
      assertEquals(3, finalTerminals.size)
      assertEquals(2, finalTerminals.count { it.outcome == "failed" })
      assertEquals(1, finalTerminals.count { it.outcome == "succeeded" })
    }
  }

  @Test
  fun radioConnectThrowRetainsExactCoreCleanupOwnershipForRetry() = Harness(true).use { h ->
    h.connectGattThrows(IllegalStateException("connectGatt failed"))

    h.connect("radio-throws")

    assertEquals(1, h.connectGattCalls())
    assertEquals(1, h.coreLines("connect|").size)
    assertEquals(1, h.coreLines("disconnect|").size)
    assertEquals(
      h.coreLines("connect|").single().split('|')[2],
      h.coreLines("disconnect|").single().split('|')[2]
    )
    val terminals = h.results().filter { it.resultKind == "connected" }
    assertEquals(1, terminals.size)
    assertEquals("failed", terminals.single().outcome)
  }

  @Test
  fun failedCoreCompensationRetriesBeforeScopedReleaseAndReplacement() = Harness(true).use { h ->
    val replacement = h.gatt()
    h.connectGattReturns(replacement)
    h.connectGattThrows(IllegalStateException("connectGatt failed"))
    h.jni.failNextDisconnect = true

    h.connect("compensation-fails")

    val connectTerminal = h.results().single { it.resultKind == "connected" }
    assertEquals("failed", connectTerminal.outcome)
    assertEquals(1, h.coreLines("disconnect|").size)
    assertEquals(0, h.coreLines("link.released.scoped|").size)
    h.connect("replacement-before-compensation-retry")
    assertEquals("replacement is blocked by retained compensation", 1, h.connectGattCalls())

    val oldConnection = connectionFor(
      peer,
      "conn-1",
      "android-link-${peer.uppercase()}-1:compensation-fails",
      "test-generation"
    )
    h.disconnect("retry-compensation", oldConnection)
    val disconnectWires = h.coreLines("disconnect|")
    assertEquals("retry admits core disconnect before release", 2, disconnectWires.size)
    assertEquals("retry keeps exact lease", disconnectWires[0].split('|')[2], disconnectWires[1].split('|')[2])
    assertEquals(1, h.coreLines("link.released.scoped|").size)
    assertEquals(1, h.results().count { it.resultKind == "accepted" && it.outcome == "succeeded" })

    h.connectGattReturns(replacement)
    h.connect("replacement-after-compensation-retry")
    assertEquals(2, h.connectGattCalls())
    h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    assertEquals(1, h.results().count { it.resultKind == "connected" && it.outcome == "succeeded" })
  }

  @Test
  fun delayedCancellationCompensationCannotReleaseReplacementCoreLease() = Harness(true).use { h ->
    val replacement = h.gatt()
    h.connectGattReturns(replacement)
    var cancelledA = false
    var admittedB = false
    h.jni.afterEnqueue = {
      val wire = h.jni.enqueued.last()
      when {
        wire.startsWith("connect|") && !cancelledA -> {
          cancelledA = true
          h.cancel(1L, "old-owner")
        }
        wire.startsWith("disconnect|") && !admittedB -> {
          admittedB = true
          h.connect("replacement-owner")
        }
      }
    }

    h.connect("old-owner")

    val connectWires = h.coreLines("connect|")
    val disconnectWires = h.coreLines("disconnect|")
    assertEquals("replacement is refused while compensation is being attested: $connectWires", 1, connectWires.size)
    assertEquals("old admission has one delayed compensation: $disconnectWires", 1, disconnectWires.size)
    val oldLease = connectWires[0].split('|')[2]
    assertEquals("compensation uses only the canceled owner's lease", oldLease, disconnectWires.single().split('|')[2])

    h.connect("replacement-after-compensation")
    assertEquals("replacement opens only after exact compensation", 1, h.connectGattCalls())
    h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    val connected = h.results().filter { it.resultKind == "connected" }
    assertEquals("pre-cleanup rejection plus replacement terminal are both explicit", 2, connected.size)
    assertEquals(1, connected.count { it.outcome == "failed" })
    assertEquals(1, connected.count { it.outcome == "succeeded" })
  }

  @Test
  fun returnedConnectDisconnectUsesTheOriginalOperationKeyAndBlocksReplacement() = Harness(true).use { h ->
    val first = h.gatt()
    val replacement = h.gatt()
    h.connectGattReturns(first, replacement)
    h.connect("old-owner")

    val oldConnection = connectionFor(
      peer,
      "conn-1",
      "android-link-${peer.uppercase()}-1:old-owner",
      "test-generation"
    )
    h.disconnect("old-disconnect", oldConnection)
    assertEquals("returned connection cleanup waits for native loss", 0, h.results().count { it.resultKind == "accepted" })

    h.connect("replacement-before-cleanup")
    assertEquals("replacement is rejected while old core disconnect is pending", 1, h.connectGattCalls())
    assertTrue(
      "rejected replacement cannot report success: ${h.results()}",
      h.results().none { it.resultKind == "connected" && it.outcome == "succeeded" }
    )

    h.native(first, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals("returned connection cleanup settles exactly once", 1, h.results().count { it.resultKind == "accepted" })
    h.native(first, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    assertEquals(
      "late old callback cannot establish a withdrawn connection: ${h.results()}",
      0,
      h.results().count { it.resultKind == "connected" && it.outcome == "succeeded" }
    )

    h.connect("replacement-after-cleanup")
    assertEquals("replacement opens after exact old cleanup", 2, h.connectGattCalls())
    h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    val connected = h.results().filter { it.resultKind == "connected" }
    assertEquals("both replacement attempts have explicit terminals", 2, connected.size)
    assertEquals(setOf("failed", "succeeded"), connected.map { it.outcome }.toSet())
  }

  @Test
  fun failedReturnedNativeCleanupRetainsReservationForExplicitRetry() = Harness(true).use { h ->
    val first = h.gatt()
    h.connectGattReturns(first)
    h.connect("retryable-cleanup")
    val oldConnection = connectionFor(
      peer,
      "conn-1",
      "android-link-${peer.uppercase()}-1:retryable-cleanup",
      "test-generation"
    )
    Mockito.doThrow(IllegalStateException("native close refused"))
      .doNothing()
      .`when`(first)
      .disconnect()
    h.jni.failNextLinkRelease = true

    h.disconnect("first-disconnect", oldConnection)
    assertEquals("the failed cleanup is explicit", 1, h.results().count { it.errorCode == "disconnectCleanupFailed" })
    h.connect("blocked-while-cleaning")
    assertEquals("the exact failed reservation blocks replacement", 1, h.connectGattCalls())

    h.disconnect("retry-disconnect", oldConnection)
    h.native(first, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals("retry settles the original cleanup", 1, h.results().count { it.resultKind == "accepted" && it.outcome == "succeeded" })
    assertEquals("retry performs exactly one additional scoped release", 2, h.coreLines("link.released.scoped|").size)
  }

  @Test
  fun physicalCloseFailureRetainsReservationUntilOwnedRadioRetry() = Harness(true).use { h ->
    val first = h.gatt()
    val replacement = h.gatt()
    h.connectGattReturns(first, replacement)
    h.connect("physical-close-retry")
    val oldConnection = connectionFor(
      peer,
      "conn-1",
      "android-link-${peer.uppercase()}-1:physical-close-retry",
      "test-generation"
    )
    Mockito.doThrow(IllegalStateException("native disconnect refused"))
      .`when`(first)
      .disconnect()
    Mockito.doThrow(IllegalStateException("native close refused"))
      .doNothing()
      .`when`(first)
      .close()

    h.disconnect("first-physical-close", oldConnection)
    assertEquals("physical cleanup failure is explicit", 1, h.results().count { it.errorCode == "disconnectCleanupFailed" })
    assertEquals("no scoped release before physical cleanup", 0, h.coreLines("link.released.scoped|").size)
    h.connect("blocked-during-physical-close-retry")
    assertEquals("failed physical owner blocks replacement", 1, h.connectGattCalls())

    h.disconnect("retry-physical-close", oldConnection)
    assertEquals("retry closes the retained GATT", 2, Mockito.mockingDetails(first).invocations.count { it.method.name == "close" })
    assertEquals("native disconnect is attempted once", 1, Mockito.mockingDetails(first).invocations.count { it.method.name == "disconnect" })
    assertEquals("retry performs one scoped release", 1, h.coreLines("link.released.scoped|").size)
    assertEquals("retry settles the original cleanup", 1, h.results().count { it.resultKind == "accepted" && it.outcome == "succeeded" })

    h.connect("replacement-after-physical-retry")
    assertEquals(2, h.connectGattCalls())
    h.native(replacement, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)
    assertEquals("replacement succeeds after exact cleanup", 1, h.results().count { it.resultKind == "connected" && it.outcome == "succeeded" })
  }

  @Test
  fun nestedGattCommandIdentityUsesItsCanonicalConnectionForOwnLoss() = Harness(true).use { h ->
    val connection = connectionRecord(peer)
    val database = ProtocolWireRecord(
      RecordKind.DATABASE_PATH,
      mapOf(
        1 to ProtocolWireValue.RecordValue(connection),
        2 to ProtocolWireValue.StringValue("db-1"),
        3 to ProtocolWireValue.StringValue("db-generation-1")
      )
    )
    val service = ProtocolWireRecord(
      RecordKind.SERVICE_PATH,
      mapOf(
        1 to ProtocolWireValue.RecordValue(database),
        2 to ProtocolWireValue.StringValue("0000180d-0000-1000-8000-00805f9b34fb"),
        3 to ProtocolWireValue.StringValue("0")
      )
    )
    val characteristic = ProtocolWireRecord(
      RecordKind.CHARACTERISTIC_PATH,
      mapOf(
        1 to ProtocolWireValue.RecordValue(service),
        2 to ProtocolWireValue.StringValue("00002a37-0000-1000-8000-00805f9b34fb"),
        3 to ProtocolWireValue.StringValue("0")
      )
    )
    val descriptor = ProtocolWireRecord(
      RecordKind.DESCRIPTOR_PATH,
      mapOf(
        1 to ProtocolWireValue.RecordValue(characteristic),
        2 to ProtocolWireValue.StringValue("00002902-0000-1000-8000-00805f9b34fb"),
        3 to ProtocolWireValue.StringValue("0")
      )
    )
    val matcher = UnifiedBleProtocolAndroidDispatcher::class.java.getDeclaredMethod(
      "connectionIdentityMatches",
      ProtocolWireRecord::class.java,
      ProtocolWireRecord::class.java
    ).also { it.isAccessible = true }
    fun matches(command: ByteArray): Boolean = matcher.invoke(
      h.dispatcher,
      ProtocolCommandDecoder.decodeCommand(command),
      connection
    ) as Boolean

    assertTrue(
      "a read's characteristic path must retain A's connection identity",
      matches(commandBytes("read", 3L, "nested-read", mapOf(4 to ProtocolWireValue.RecordValue(characteristic))))
    )
    assertTrue(
      "a descriptor operation must retain A's connection identity",
      matches(commandBytes("readDescriptor", 3L, "nested-descriptor", mapOf(5 to ProtocolWireValue.RecordValue(descriptor))))
    )
    val otherConnection = connectionFor(peer, "conn-2", "lease-2", "conngen-2")
    val otherDatabase = ProtocolWireRecord(
      RecordKind.DATABASE_PATH,
      mapOf(1 to ProtocolWireValue.RecordValue(otherConnection), 2 to ProtocolWireValue.StringValue("db-1"), 3 to ProtocolWireValue.StringValue("db-generation-1"))
    )
    val otherService = ProtocolWireRecord(
      RecordKind.SERVICE_PATH,
      mapOf(1 to ProtocolWireValue.RecordValue(otherDatabase), 2 to ProtocolWireValue.StringValue("0000180d-0000-1000-8000-00805f9b34fb"), 3 to ProtocolWireValue.StringValue("0"))
    )
    val otherCharacteristic = ProtocolWireRecord(
      RecordKind.CHARACTERISTIC_PATH,
      mapOf(1 to ProtocolWireValue.RecordValue(otherService), 2 to ProtocolWireValue.StringValue("00002a37-0000-1000-8000-00805f9b34fb"), 3 to ProtocolWireValue.StringValue("0"))
    )
    assertTrue(
      "a stale A loss must not classify B's nested command",
      !matches(commandBytes("subscribe", 3L, "other-subscription", mapOf(4 to ProtocolWireValue.RecordValue(otherCharacteristic), 7 to ProtocolWireValue.StringValue("sub-b"))))
    )
  }

  @Test
  fun explicitDisconnectThenNativeLossProducesOneCleanupTerminal() = Harness(true).use { h ->
    val gatt = h.gatt()
    h.connectGattReturns(gatt)
    h.connect("establish-for-disconnect")
    h.native(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)

    h.disconnect("explicit-disconnect")
    assertEquals("disconnect waits for the native loss", 0, h.results().count { it.resultKind == "accepted" })

    h.native(gatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    val cleanupTerminals = h.results().filter { it.resultKind == "accepted" }
    assertEquals("native cleanup settles the explicit disconnect exactly once", 1, cleanupTerminals.size)
    assertEquals("succeeded; terminals=$cleanupTerminals all=${h.results()}", "succeeded", cleanupTerminals.single().outcome)
    Mockito.verify(gatt).close()
  }

  @Test
  fun connectionLossReentranceAdmitsReplacementBeforeOldOwnerSweepAndReplacementSettles() = Harness(true).use { h ->
    val oldGatt = h.gatt()
    val replacementGatt = h.gatt()
    h.connectGattReturns(oldGatt, replacementGatt)
    h.connect("old-owner")
    h.native(oldGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)

    var replacementDispatched = false
    h.radio.onConnectionState = { _, connected, _ ->
      if (!connected && !replacementDispatched) {
        replacementDispatched = true
        h.connect("replacement-owner")
      }
    }

    h.native(oldGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_DISCONNECTED)
    assertEquals("reentrant replacement opens after old teardown results=${h.results()} core=${h.jni.enqueued}", 2, h.connectGattCalls())
    h.native(replacementGatt, BluetoothGatt.GATT_SUCCESS, BluetoothProfile.STATE_CONNECTED)

    val connected = h.results().filter { it.resultKind == "connected" }
    assertEquals("old and replacement connects each settle once: all=${h.results()}", 2, connected.size)
    assertTrue("both connect attempts succeed: all=${h.results()}", connected.all { it.outcome == "succeeded" })
  }
}
