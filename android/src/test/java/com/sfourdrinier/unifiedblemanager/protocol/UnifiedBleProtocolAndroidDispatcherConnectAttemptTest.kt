// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcherConnectAttemptTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
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

  private class FakeJni : UbmGattCoreBinding.CoreJni {
    val enqueued: MutableList<String> = Collections.synchronizedList(mutableListOf())
    override fun open(revision: String): Long = 7L
    override fun revision(): String = UbmGattCoreBinding.CONTRACT_REVISION
    override fun enqueue(handle: Long, wire: String): Int {
      enqueued.add(wire)
      return enqueued.size
    }
    override fun drain(handle: Long): String = ""
    override fun depth(handle: Long): Int = enqueued.size
    override fun close(handle: Long) {}
  }

  private inner class Harness(armCloseDeadline: Boolean) {
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
        UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = onRejection, worker = direct, clockMs = { 1000L })
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

    fun connectGattCalls(): Int =
      Mockito.mockingDetails(device).invocations.count { it.method.name == "connectGatt" }

    fun connect(nonce: String) {
      dispatcher.dispatch(
        commandBytes(
          "connect",
          1L,
          nonce,
          mapOf(
            10 to ProtocolWireValue.RecordValue(connectionRecord(peer)),
            20 to ProtocolWireValue.StringValue("direct")
          )
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
}
