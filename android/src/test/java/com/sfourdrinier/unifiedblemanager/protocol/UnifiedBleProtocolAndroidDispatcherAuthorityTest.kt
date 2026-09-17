// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcherAuthorityTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.BluetoothManager
import android.content.Context
import android.content.pm.PackageManager
import com.sfourdrinier.unifiedblemanager.protocol.generated.NATIVE_PROTOCOL_VERSION
import com.sfourdrinier.unifiedblemanager.protocol.generated.RecordKind
import com.sfourdrinier.unifiedblemanager.radio.GattObservation
import com.sfourdrinier.unifiedblemanager.radio.UbmGattCoreBinding
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.charset.StandardCharsets
import java.util.Collections
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.mockito.Mockito

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
    private val drainQueue = ConcurrentLinkedQueue(drains)

    override fun open(revision: String): Long = 7L
    override fun revision(): String = linkedRevision
    override fun enqueue(handle: Long, wire: String): Int {
      enqueued.add(wire)
      return enqueued.size
    }
    override fun drain(handle: Long): String = drainQueue.poll() ?: ""
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
    val context: Context = permittedContext()

    fun dispatcher(coreShadowFactory: ((Context, (GattObservation) -> Unit) -> UbmGattCoreBinding?)?): UnifiedBleProtocolAndroidDispatcher =
      UnifiedBleProtocolAndroidDispatcher(context, NATIVE_HANDLE, coreShadowFactory)

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
  }

  // -- the three proofs ------------------------------------------------------

  @Test
  fun coreRejectionBindsRadioSuccessIsNeverReported() {
    val harness = Harness(
      FakeJni(
        drains = listOf(
          "{\"ok\":false,\"event\":\"disconnect\",\"code\":\"link.busy\",\"domain\":\"connection\"," +
            "\"operation\":\"gatt-drain\",\"detail\":\"teardown-refused\",\"effects\":[],\"observations\":[]}"
        )
      )
    )
    mockJsi(harness).use {
      val dispatcher = harness.dispatcher(harness.healthyFactory())
      try {
        dispatcher.dispatch(disconnectCommand(epoch = 11L, nonce = "rejection-binds"))
      } finally {
        dispatcher.close()
      }
    }
    // The core was consulted: the exact disconnect line reached the core queue.
    assertTrue(
      "core line must be enqueued, was ${harness.jni.enqueued}",
      harness.jni.enqueued.any { line ->
        line.startsWith("disconnect|public-address:AA:BB:CC:DD:EE:FF|android-link-AA:BB:CC:DD:EE:FF|")
      }
    )
    // The rejection arrived through the binding sink (diagnostic path intact).
    assertEquals(1, harness.rejections.size)
    assertEquals("link.busy", harness.rejections.single().code)
    // Authority: exactly one terminal, a failure naming the core refusal —
    // the radio-only "accepted" success must not appear.
    val terminals = parseTerminals(harness.emitted)
    assertEquals("one terminal, was $terminals", 1, terminals.size)
    assertEquals("failed", terminals.single().outcome)
    assertEquals(CoreCommandAuthority.CODE_REJECTED, terminals.single().errorCode)
    assertTrue(
      "rejection detail must quote the core identity, was $terminals",
      terminals.single().errorMessage.contains("link.busy")
    )
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
    val terminals = parseTerminals(harness.emitted)
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
          "{\"ok\":true,\"event\":\"disconnect\",\"effects\":[],\"observations\":[]}"
        )
      )
    )
    mockJsi(harness).use {
      val dispatcher = harness.dispatcher(harness.healthyFactory())
      try {
        dispatcher.dispatch(disconnectCommand(epoch = 13L, nonce = "attested"))
      } finally {
        dispatcher.close()
      }
    }
    // Attestation: the exact wire line reached the core queue ...
    assertTrue(
      "core line must be enqueued, was ${harness.jni.enqueued}",
      harness.jni.enqueued.any { line ->
        line.startsWith("disconnect|public-address:AA:BB:CC:DD:EE:FF|android-link-AA:BB:CC:DD:EE:FF|")
      }
    )
    // ... its drain observation was consumed (no stranded verdict) ...
    assertEquals("drain observation must be consumed", 0, harness.jni.drainsRemaining())
    assertTrue("no core rejection expected, was ${harness.rejections}", harness.rejections.isEmpty())
    // ... and the radio success is reported with the core admitted.
    val terminals = parseTerminals(harness.emitted)
    assertEquals("one terminal, was $terminals", 1, terminals.size)
    assertEquals("succeeded", terminals.single().outcome)
    assertEquals("accepted", terminals.single().resultKind)
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

  private fun permittedContext(): Context {
    val context = Mockito.mock(Context::class.java)
    Mockito.`when`(context.applicationContext).thenReturn(context)
    Mockito.`when`(context.checkSelfPermission(Mockito.anyString())).thenReturn(
      PackageManager.PERMISSION_GRANTED
    )
    val bluetoothManager = Mockito.mock(BluetoothManager::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(bluetoothManager)
    // No adapter on HOST-JVM: radio paths needing hardware throw fail-closed.
    Mockito.`when`(bluetoothManager.adapter).thenReturn(null)
    return context
  }

  // -- command builders (canonical wire encoding) ----------------------------

  private fun attachmentRecord(): ProtocolWireRecord = ProtocolWireRecord(
    RecordKind.ATTACHMENT,
    mapOf(
      1 to ProtocolWireValue.StringValue("att-1"),
      2 to ProtocolWireValue.StringValue("backend-1"),
      3 to ProtocolWireValue.StringValue("gen-1"),
      4 to ProtocolWireValue.StringValue("adapter-1"),
      5 to ProtocolWireValue.StringValue("adaptergen-1")
    )
  )

  private fun correlationRecord(epoch: Long, nonce: String): ProtocolWireRecord = ProtocolWireRecord(
    RecordKind.OPERATION_CORRELATION,
    mapOf(
      1 to ProtocolWireValue.RecordValue(attachmentRecord()),
      2 to ProtocolWireValue.UnsignedIntegerValue(epoch),
      3 to ProtocolWireValue.StringValue(nonce)
    )
  )

  private fun connectionRecord(peerId: String): ProtocolWireRecord = ProtocolWireRecord(
    RecordKind.CONNECTION_PATH,
    mapOf(
      1 to ProtocolWireValue.RecordValue(attachmentRecord()),
      2 to ProtocolWireValue.StringValue(peerId),
      3 to ProtocolWireValue.StringValue("conn-1"),
      4 to ProtocolWireValue.StringValue("lease-1"),
      5 to ProtocolWireValue.StringValue("conngen-1")
    )
  )

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

  private fun commandBytes(
    kind: String,
    epoch: Long,
    nonce: String,
    extra: Map<Int, ProtocolWireValue>
  ): ByteArray {
    val fields = mutableMapOf<Int, ProtocolWireValue>(
      1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
      2 to ProtocolWireValue.RecordValue(correlationRecord(epoch, nonce)),
      3 to ProtocolWireValue.StringValue(kind)
    )
    fields.putAll(extra)
    return ProtocolWireEncoder.encode(ProtocolWireRecord(RecordKind.COMMAND, fields))
  }

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

  // -- structural RESULT parser (tag framing only, no schema needed) ---------

  private data class ParsedTerminal(
    val resultKind: String,
    val outcome: String,
    val cause: String?,
    val errorCode: String?,
    val errorMessage: String
  )

  private fun parseTerminals(emitted: List<ByteArray>): List<ParsedTerminal> =
    emitted.map { parseResult(it) }

  private fun parseResult(bytes: ByteArray): ParsedTerminal {
    val record = parseRecord(bytes, 0)
    check(record.kindWire == RecordKind.RESULT.wireValue) { "expected RESULT, was ${record.kindWire}" }
    val resultKind = record.stringField(2)
    val terminal = record.nestedField(3)
    val outcome = terminal.stringField(2)
    val cause = terminal.optionalStringField(3)
    val error = record.optionalNestedField(10)
    return ParsedTerminal(
      resultKind = resultKind,
      outcome = outcome,
      cause = cause,
      errorCode = error?.stringField(1),
      errorMessage = error?.optionalStringField(7) ?: ""
    )
  }

  private data class ParsedRecord(val kindWire: Int, val fields: Map<Int, ParsedField>)

  private data class ParsedField(val tag: Int, val payload: ByteArray)

  private fun ParsedRecord.stringField(id: Int): String {
    val field = fields[id] ?: throw AssertionError("missing string field $id")
    check(field.tag == 4) { "field $id is not a string (tag ${field.tag})" }
    val buffer = ByteBuffer.wrap(field.payload).order(ByteOrder.LITTLE_ENDIAN)
    val length = buffer.int
    check(length >= 0 && length == field.payload.size - 4) { "malformed string field $id" }
    return String(field.payload, 4, length, StandardCharsets.UTF_8)
  }

  private fun ParsedRecord.optionalStringField(id: Int): String? {
    val field = fields[id] ?: return null
    check(field.tag == 4) { "field $id is not a string (tag ${field.tag})" }
    return stringField(id)
  }

  private fun ParsedRecord.nestedField(id: Int): ParsedRecord {
    val field = fields[id] ?: throw AssertionError("missing record field $id")
    check(field.tag == 6) { "field $id is not a record (tag ${field.tag})" }
    return parseRecord(field.payload, 0)
  }

  private fun ParsedRecord.optionalNestedField(id: Int): ParsedRecord? {
    val field = fields[id] ?: return null
    check(field.tag == 6) { "field $id is not a record (tag ${field.tag})" }
    return parseRecord(field.payload, 0)
  }

  private fun parseRecord(bytes: ByteArray, offset: Int): ParsedRecord {
    val buffer = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
    buffer.position(offset)
    check(buffer.int == 0x314E4255) { "bad record magic" }
    buffer.int // protocol version
    val kindWire = buffer.short.toInt() and 0xffff
    val fieldCount = buffer.short.toInt() and 0xffff
    val fields = mutableMapOf<Int, ParsedField>()
    repeat(fieldCount) {
      val fieldId = buffer.short.toInt() and 0xffff
      val tag = buffer.get().toInt() and 0xff
      val length = buffer.int
      check(length >= 0 && length <= buffer.remaining()) { "bad field length for $fieldId" }
      val payload = ByteArray(length)
      buffer.get(payload)
      fields[fieldId] = ParsedField(tag, payload)
    }
    check(!buffer.hasRemaining()) { "trailing bytes in record kind $kindWire" }
    return ParsedRecord(kindWire, fields)
  }

  companion object {
    private const val NATIVE_HANDLE = 0xA11CE1L
  }
}
