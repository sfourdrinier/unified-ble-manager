// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/ProtocolResultParsing.kt

package com.sfourdrinier.unifiedblemanager.protocol

import com.sfourdrinier.unifiedblemanager.protocol.generated.NATIVE_PROTOCOL_VERSION
import com.sfourdrinier.unifiedblemanager.protocol.generated.RecordKind
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.nio.charset.StandardCharsets

/** Canonical command encoding and a structural RESULT parser (tag framing only, no schema needed) shared by the dispatcher suites. */

// -- command builders (canonical wire encoding) ----------------------------

internal fun attachmentRecord(): ProtocolWireRecord = ProtocolWireRecord(
  RecordKind.ATTACHMENT,
  mapOf(
    1 to ProtocolWireValue.StringValue("att-1"),
    2 to ProtocolWireValue.StringValue("backend-1"),
    3 to ProtocolWireValue.StringValue("gen-1"),
    4 to ProtocolWireValue.StringValue("adapter-1"),
    5 to ProtocolWireValue.StringValue("adaptergen-1")
  )
)

internal fun correlationRecord(epoch: Long, nonce: String): ProtocolWireRecord = ProtocolWireRecord(
  RecordKind.OPERATION_CORRELATION,
  mapOf(
    1 to ProtocolWireValue.RecordValue(attachmentRecord()),
    2 to ProtocolWireValue.UnsignedIntegerValue(epoch),
    3 to ProtocolWireValue.StringValue(nonce)
  )
)

internal fun connectionRecord(peerId: String): ProtocolWireRecord = ProtocolWireRecord(
  RecordKind.CONNECTION_PATH,
  mapOf(
    1 to ProtocolWireValue.RecordValue(attachmentRecord()),
    2 to ProtocolWireValue.StringValue(peerId),
    3 to ProtocolWireValue.StringValue("conn-1"),
    4 to ProtocolWireValue.StringValue("lease-1"),
    5 to ProtocolWireValue.StringValue("conngen-1")
  )
)

internal fun commandBytes(
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

// -- structural RESULT parser ----------------------------------------------

internal data class ParsedTerminal(
  val resultKind: String,
  val outcome: String,
  val cause: String?,
  val errorCode: String?,
  val errorMessage: String
)

internal fun parseTerminals(emitted: List<ByteArray>): List<ParsedTerminal> =
  emitted.map { parseResult(it) }

internal fun parseResult(bytes: ByteArray): ParsedTerminal {
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

internal data class ParsedRecord(val kindWire: Int, val fields: Map<Int, ParsedField>)

internal data class ParsedField(val tag: Int, val payload: ByteArray)

internal fun ParsedRecord.stringField(id: Int): String {
  val field = fields[id] ?: throw AssertionError("missing string field $id")
  check(field.tag == 4) { "field $id is not a string (tag ${field.tag})" }
  val buffer = ByteBuffer.wrap(field.payload).order(ByteOrder.LITTLE_ENDIAN)
  val length = buffer.int
  check(length >= 0 && length == field.payload.size - 4) { "malformed string field $id" }
  return String(field.payload, 4, length, StandardCharsets.UTF_8)
}

internal fun ParsedRecord.optionalStringField(id: Int): String? {
  val field = fields[id] ?: return null
  check(field.tag == 4) { "field $id is not a string (tag ${field.tag})" }
  return stringField(id)
}

internal fun ParsedRecord.nestedField(id: Int): ParsedRecord {
  val field = fields[id] ?: throw AssertionError("missing record field $id")
  check(field.tag == 6) { "field $id is not a record (tag ${field.tag})" }
  return parseRecord(field.payload, 0)
}

internal fun ParsedRecord.optionalNestedField(id: Int): ParsedRecord? {
  val field = fields[id] ?: return null
  check(field.tag == 6) { "field $id is not a record (tag ${field.tag})" }
  return parseRecord(field.payload, 0)
}

internal fun parseRecord(bytes: ByteArray, offset: Int): ParsedRecord {
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
