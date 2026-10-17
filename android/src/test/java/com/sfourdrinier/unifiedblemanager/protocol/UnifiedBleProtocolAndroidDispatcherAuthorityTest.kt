// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcherAuthorityTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.content.Context
import android.content.pm.PackageManager
import com.sfourdrinier.unifiedblemanager.protocol.generated.RecordKind
import com.sfourdrinier.unifiedblemanager.radio.GattObservation
import com.sfourdrinier.unifiedblemanager.radio.UbmGattCoreBinding
import java.util.Collections
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.mockito.Mockito
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq

/**
 * R02 Android authority proofs (HOST-JVM unit tests, local Gradle runnable).
 *
 * The dispatcher executes the platform radio and consults the shared core
 * through `DeferredCoreShadow` over [UbmGattCoreBinding] (fake JNI here, real
 * wire builders + real bridge + real gate). The JSI boundary is stubbed with
 * a static mock; emitted RESULT bytes are parsed structurally and asserted
 * on their terminal outcome + error code.
 *
 * - core-rejection-binds: a core `ok:false` drain for the command's wire
 *   event must fail the command (`coreRejected`) even though the radio would
 *   succeed — never a radio success standing over a core refusal.
 * - shadow-unavailable-fails-loud: a missing/failed shadow at command time
 *   must surface an actionable `coreUnavailable` terminal — never silent
 *   radio-only execution.
 * - command-attestation: a healthy core shows the full round trip — the exact
 *   wire line enqueued, its drain observation consumed, and the radio success
 *   reported only with the core admitted.
 */
class UnifiedBleProtocolAndroidDispatcherAuthorityTest {

  private val direct = Executor { command -> command.run() }

  private class FakeJni(
    var linkedRevision: String = UbmGattCoreBinding.CONTRACT_REVISION,
    drains: List<String> = emptyList()
  ) : UbmGattCoreBinding.CoreJni {
    val enqueued = Collections.synchronizedList(mutableListOf<String>())
    private val drainQueue = drains.toMutableList()

    override fun open(revision: String): Long = 7L
    override fun revision(): String = linkedRevision
    override fun enqueue(handle: Long, wire: String): Int {
      enqueued.add(wire)
      return enqueued.size
    }
    override fun drain(handle: Long): String {
      val event = enqueued.lastOrNull()?.substringBefore('|') ?: return ""
      val marker = "\"event\":\"$event\""
      val index = drainQueue.indexOfFirst { response -> response.contains(marker) }
      if (index < 0) return ""
      val response = drainQueue.removeAt(index)
      if (event != "connect") return response
      val lease = enqueued.lastOrNull()?.split('|')?.getOrNull(2) ?: return response
      return response.replace(Regex("\\\"lease\\\":\\\"[^\\\"]+\\\""), "\\\"lease\\\":\\\"$lease\\\"")
    }
    override fun depth(handle: Long): Int = enqueued.size
    override fun close(handle: Long) {}

    fun drainsRemaining(): Int = drainQueue.size
  }

  private inner class Harness(
    val jni: FakeJni,
    val emitted: MutableList<ByteArray> = Collections.synchronizedList(mutableListOf()),
    val diagnostics: MutableList<String> = Collections.synchronizedList(mutableListOf()),
    val rejections: MutableList<GattObservation> = Collections.synchronizedList(mutableListOf())
  ) {
    val adapter: BluetoothAdapter = Mockito.mock(BluetoothAdapter::class.java)
    val device: BluetoothDevice = Mockito.mock(BluetoothDevice::class.java)
    val gatt: BluetoothGatt = Mockito.mock(BluetoothGatt::class.java)
    var callback: BluetoothGattCallback? = null
    val context: Context = permittedContext(adapter)

    init {
      Mockito.`when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
      Mockito.`when`(adapter.getRemoteDevice("AA:BB:CC:DD:EE:FF")).thenReturn(device)
      Mockito.`when`(device.address).thenReturn("AA:BB:CC:DD:EE:FF")
      Mockito.`when`(gatt.device).thenReturn(device)
      Mockito.doAnswer { invocation ->
        callback = invocation.getArgument(2)
        gatt
      }.`when`(device).connectGatt(eq(context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    }

    fun dispatcher(coreShadowFactory: ((Context, (GattObservation) -> Unit) -> UbmGattCoreBinding?)?): UnifiedBleProtocolAndroidDispatcher =
      UnifiedBleProtocolAndroidDispatcher(context, NATIVE_HANDLE, coreShadowFactory, null)

    fun healthyFactory(): (Context, (GattObservation) -> Unit) -> UbmGattCoreBinding? =
      { ctx, onRejection ->
        UbmGattCoreBinding(
          ctx,
          jni = jni,
          onCoreRejection = { observation ->
            rejections.add(observation)
            onRejection(observation)
          },
          worker = direct,
          clockMs = { 1000L }
        )
      }

    fun establish(dispatcher: UnifiedBleProtocolAndroidDispatcher) {
      dispatcher.dispatch(
        commandBytes(
          "connect", 10L, "establish",
          mapOf(
            10 to ProtocolWireValue.RecordValue(connectionRecord("AA:BB:CC:DD:EE:FF")),
            20 to ProtocolWireValue.StringValue("direct")
          )
        )
      )
      (callback ?: error("connectGatt callback was not captured")).onConnectionStateChange(
        gatt,
        BluetoothGatt.GATT_SUCCESS,
        BluetoothProfile.STATE_CONNECTED
      )
    }
  }

  // -- the three proofs ------------------------------------------------------

  @Test
  fun coreRejectionBindsRadioSuccessIsNeverReported() {
    val harness = Harness(
      FakeJni(
        drains = listOf(
          "{\"ok\":true,\"event\":\"connect\",\"peer\":\"public-address:AA:BB:CC:DD:EE:FF\",\"lease\":\"android-link-AA:BB:CC:DD:EE:FF\",\"generation\":\"test-generation\",\"op\":\"connection.connect\",\"effects\":[],\"observations\":[]}",
          "{\"ok\":false,\"event\":\"disconnect\",\"code\":\"link.busy\",\"domain\":\"connection\",\"operation\":\"gatt-drain\",\"detail\":\"teardown-refused\",\"effects\":[],\"observations\":[]}"
        )
      )
    )
    mockJsi(harness).use {
      val dispatcher = harness.dispatcher(harness.healthyFactory())
      try {
        harness.establish(dispatcher)
        dispatcher.dispatch(disconnectCommand(epoch = 11L, nonce = "rejection-binds"))
      } finally {
        dispatcher.close()
      }
    }
    assertTrue(
      "disconnect line must be enqueued: ${harness.jni.enqueued}",
      harness.jni.enqueued.any {
        it.startsWith("disconnect|public-address:AA:BB:CC:DD:EE:FF|android-link-AA:BB:CC:DD:EE:FF-")
      }
    )
    assertEquals(1, harness.rejections.size)
    assertEquals("link.busy", harness.rejections.single().code)
    val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
    assertEquals("connect plus rejected disconnect, was $terminals", 2, terminals.size)
    val rejected = terminals.filter { it.errorCode != null }
    assertEquals("exactly one failed disconnect, terminals=$terminals", 1, rejected.size)
    assertTrue(rejected.single().errorMessage.contains("link.busy"))
    assertTrue(terminals.none { it.resultKind == "accepted" && it.outcome == "succeeded" })
  }

  @Test
  fun unknownDisconnectIsRejectedBeforeCoreOrRadioTouch() {
    val harness = Harness(FakeJni())
    mockJsi(harness).use {
      val dispatcher = harness.dispatcher(harness.healthyFactory())
      try {
        dispatcher.dispatch(disconnectCommand(epoch = 14L, nonce = "unknown-disconnect"))
      } finally {
        dispatcher.close()
      }
    }
    assertTrue(
      "unknown disconnect must not enqueue a disconnect: ${harness.jni.enqueued}",
      harness.jni.enqueued.none { line -> line.startsWith("disconnect|") }
    )
    val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
    assertEquals(1, terminals.size)
    assertEquals(CoreCommandAuthority.CODE_REJECTED, terminals.single().errorCode)
  }

  @Test
  fun shadowUnavailableFailsLoudInsteadOfRadioOnlySuccess() {
    val harness = Harness(FakeJni())
    mockJsi(harness).use {
      // No binding can ever open: every demand retry finds nothing.
      val dispatcher = harness.dispatcher { _, _ -> null }
      try {
        dispatcher.dispatch(stopScanCommand(epoch = 12L, nonce = "shadow-unavailable"))
      } finally {
        dispatcher.close()
      }
    }
    // Note: static mocks are thread-local, so the gate's background opener
    // attempt (when it wins the race) diagnoses through the real native and
    // its duplicate is then suppressed by the gate's once-per-cause latch.
    // The deterministic contract is the terminal below, which quotes the
    // seam's recorded cause; diagnosis latch semantics themselves are pinned
    // single-threaded in DeferredCoreShadowCauseTest / DeferredCoreShadowTest.
    // Fail loud: one actionable terminal quoting the cause — never the
    // radio-only "accepted" success the mirror path would have emitted.
    val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
    assertEquals("one terminal, was $terminals", 1, terminals.size)
    assertEquals("failed", terminals.single().outcome)
    assertEquals(CoreCommandAuthority.CODE_UNAVAILABLE, terminals.single().errorCode)
    assertTrue(
      "terminal must quote the outage cause, was $terminals",
      terminals.single().errorMessage.contains("factory returned no binding")
    )
  }

  @Test
  fun healthyCoreAttestsTheCommandEndToEnd() {
    val harness = Harness(
      FakeJni(
        drains = listOf(
          "{\"ok\":true,\"event\":\"connect\",\"peer\":\"public-address:AA:BB:CC:DD:EE:FF\",\"lease\":\"android-link-AA:BB:CC:DD:EE:FF\",\"generation\":\"test-generation\",\"op\":\"connection.connect\",\"effects\":[],\"observations\":[]}",
          "{\"ok\":true,\"event\":\"disconnect\",\"effects\":[],\"observations\":[]}"
        )
      )
    )
    mockJsi(harness).use {
      val dispatcher = harness.dispatcher(harness.healthyFactory())
      try {
        harness.establish(dispatcher)
        dispatcher.dispatch(disconnectCommand(epoch = 13L, nonce = "attested"))
      } finally {
        dispatcher.close()
      }
    }
    assertTrue(
      "disconnect line must be enqueued: ${harness.jni.enqueued}",
      harness.jni.enqueued.any {
        it.startsWith("disconnect|public-address:AA:BB:CC:DD:EE:FF|android-link-AA:BB:CC:DD:EE:FF-")
      }
    )
    assertEquals("connect/disconnect drains consumed", 0, harness.jni.drainsRemaining())
    val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
    assertEquals("connect and disconnect terminals, was $terminals", 2, terminals.size)
    assertTrue("connect success missing: $terminals", terminals.any { it.resultKind == "connected" && it.outcome == "succeeded" })
    assertTrue("disconnect success missing: $terminals", terminals.any { it.resultKind == "accepted" && it.outcome == "succeeded" })
  }

  // -- harness ---------------------------------------------------------------

  private fun mockJsi(harness: Harness): org.mockito.MockedStatic<UnifiedBleProtocolJsiBinding> {
    // Static stubbing lives in Java (see JsiStaticStubs): Kotlin cannot
    // disambiguate MockedStatic.when overloads for SAM lambdas.
    val mocked = Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java)
    JsiStaticStubs.stubEmitRecord(mocked, harness.emitted)
    JsiStaticStubs.stubEmitDiagnostic(mocked, harness.diagnostics)
    return mocked
  }

  private fun permittedContext(adapter: BluetoothAdapter): Context {
    val context = Mockito.mock(Context::class.java)
    Mockito.`when`(context.applicationContext).thenReturn(context)
    Mockito.`when`(context.checkSelfPermission(Mockito.anyString())).thenReturn(
      PackageManager.PERMISSION_GRANTED
    )
    val bluetoothManager = Mockito.mock(BluetoothManager::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(bluetoothManager)
    Mockito.`when`(bluetoothManager.adapter).thenReturn(adapter)
    return context
  }

  // -- command builders (canonical wire encoding) ----------------------------

  private fun scanOptionsRecord(): ProtocolWireRecord = ProtocolWireRecord(
    RecordKind.SCAN_OPTIONS,
    mapOf(
      1 to ProtocolWireValue.StringListValue(emptyList()),
      2 to ProtocolWireValue.BooleanValue(true),
      3 to ProtocolWireValue.SignedIntegerValue(1L),
      4 to ProtocolWireValue.SignedIntegerValue(0L),
      5 to ProtocolWireValue.BooleanValue(true)
    )
  )

  private fun disconnectCommand(epoch: Long, nonce: String): ByteArray = commandBytes(
    "disconnect",
    epoch,
    nonce,
    mapOf(10 to ProtocolWireValue.RecordValue(connectionRecord("AA:BB:CC:DD:EE:FF")))
  )

  private fun stopScanCommand(epoch: Long, nonce: String): ByteArray =
    commandBytes("scanStop", epoch, nonce, emptyMap())

  @Suppress("unused")
  private fun scanStartCommand(epoch: Long, nonce: String): ByteArray = commandBytes(
    "scanStart",
    epoch,
    nonce,
    mapOf(12 to ProtocolWireValue.RecordValue(scanOptionsRecord()))
  )

  companion object {
    private const val NATIVE_HANDLE = 0xA11CE1L
  }
}
