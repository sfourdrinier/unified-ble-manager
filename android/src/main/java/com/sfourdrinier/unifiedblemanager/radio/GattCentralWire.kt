// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/GattCentralWire.kt

package com.sfourdrinier.unifiedblemanager.radio

/**
 * HOST-ANDROID GATT wire builders (UBM 5.0, trackourhealth/bun-mono#1188).
 *
 * Pure Kotlin — no `android.*` imports — so this mapping stays provable on
 * HOST-JVM unit tests without an emulator. Each builder produces one
 * pipe-delimited line for `com.ubm.gatt.GattBridge.nativeEnqueueGattEvent`
 * (`kind|arg|...`); arguments must never contain `'|'` and byte values cross
 * as lowercase hex. Failures are fail-fast [IllegalArgumentException] at the
 * call site, before anything crosses JNI.
 *
 * Callback mapping (see [UbmGattCentralBridge] for the threading contract):
 * - `BluetoothLeScanner` start/stop/results → [scanStart]/[scanStop]/
 *   [scanPlatformEvent]; adapter off/reset → [adapterReset]
 * - `BluetoothGattCallback.onConnectionStateChange` connected → [connect] +
 *   [linkEstablished]; disconnected → [peerLoss] or [disconnect]+[linkReleased]
 * - `onServicesDiscovered` → [discoveryBegin] before, [discoveryComplete] /
 *   [discoveryFail] after; `onServiceChanged` → [servicesChanged]
 * - `onCharacteristicRead/Write`, `onDescriptorRead/Write` completions →
 *   [readStart]/[writeStart] at request time, [opSettle] at callback time;
 *   still-queued work revoked first via [opCancel]
 * - `onCharacteristicChanged` → [notifyDeliver] (bounded: [notifyMaxBytes])
 * - `close()`/destroy → [release] (real destroy transition; idempotent)
 */
object GattCentralWire {
  /**
   * Maximum decoded notify value bytes accepted in one line. Values cross
   * as hex (2 chars/byte), so this is half the wire ceiling: a full-size
   * value plus framing still fits under [WIRE_MAX] at enqueue.
   */
  const val NOTIFY_MAX_BYTES = 262144

  /** Maximum queued wire length accepted at enqueue (payload + framing). */
  const val WIRE_MAX = 524288 + 1024

  private fun arg(value: String, field: String): String {
    require(!value.contains('|')) { "GATT wire field $field must not contain '|'" }
    return value
  }

  private fun u64(value: Long, field: String): String {
    require(value >= 0) { "GATT wire field $field must be non-negative" }
    return value.toString()
  }

  private fun opt(value: String?, field: String): String =
    if (value.isNullOrEmpty() || value == "-") "-" else arg(value, field)

  /** Encodes bytes as lowercase hex for [notifyDeliver]. */
  fun hexOf(bytes: ByteArray): String {
    val chars = CharArray(bytes.size * 2)
    val digits = "0123456789abcdef"
    for (i in bytes.indices) {
      val b = bytes[i].toInt() and 0xFF
      chars[i * 2] = digits[b ushr 4]
      chars[i * 2 + 1] = digits[b and 0x0F]
    }
    return String(chars)
  }

  fun scanStart(
    owner: String,
    timeoutMs: Long,
    nowMs: Long,
    serviceUuids: List<String>,
    duplicatePolicy: String,
    mergePolicy: String
  ): String = listOf(
    "scan.start",
    arg(owner, "owner"),
    u64(timeoutMs, "timeoutMs"),
    u64(nowMs, "nowMs"),
    serviceUuids.joinToString(",") { arg(it, "serviceUuid") },
    arg(duplicatePolicy, "duplicatePolicy"),
    arg(mergePolicy, "mergePolicy")
  ).joinToString("|")

  fun scanPlatformStarted(op: String): String =
    "scan.platform-started|${arg(op, "op")}"

  fun scanStop(op: String, nowMs: Long): String =
    "scan.stop|${arg(op, "op")}|${u64(nowMs, "nowMs")}"

  fun scanPlatformEvent(op: String, event: String, nowMs: Long): String =
    "scan.platform-event|${arg(op, "op")}|${arg(event, "event")}|${u64(nowMs, "nowMs")}"

  fun peerResolve(domain: String, value: String): String =
    "peer.resolve|${arg(domain, "domain")}|${arg(value, "value")}"

  fun connect(peerKey: String, lease: String, timeoutMs: Long, nowMs: Long): String =
    "connect|${arg(peerKey, "peerKey")}|${arg(lease, "lease")}|" +
      "${u64(timeoutMs, "timeoutMs")}|${u64(nowMs, "nowMs")}"

  fun linkEstablished(peerKey: String): String =
    "link.established|${arg(peerKey, "peerKey")}"

  fun linkReleased(peerKey: String): String =
    "link.released|${arg(peerKey, "peerKey")}"

  fun disconnect(peerKey: String, lease: String, nowMs: Long): String =
    "disconnect|${arg(peerKey, "peerKey")}|${arg(lease, "lease")}|${u64(nowMs, "nowMs")}"

  fun peerLoss(peerKey: String, nowMs: Long): String =
    "peer.loss|${arg(peerKey, "peerKey")}|${u64(nowMs, "nowMs")}"

  fun discoveryBegin(peerKey: String): String =
    "discovery.begin|${arg(peerKey, "peerKey")}"

  fun discoveryComplete(peerKey: String): String =
    "discovery.complete|${arg(peerKey, "peerKey")}"

  fun discoveryFail(peerKey: String): String =
    "discovery.fail|${arg(peerKey, "peerKey")}"

  fun servicesChanged(peerKey: String): String =
    "services-changed|${arg(peerKey, "peerKey")}"

  fun pathRegister(
    peerKey: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String?,
    characteristicOccurrence: Long?,
    descriptorUuid: String?,
    descriptorOccurrence: Long?,
    properties: Int,
    lease: String
  ): String {
    require(properties in 0..255) { "GATT wire field properties must fit u8" }
    return listOf(
      "path.register",
      arg(peerKey, "peerKey"),
      arg(serviceUuid, "serviceUuid"),
      u64(serviceOccurrence, "serviceOccurrence"),
      opt(characteristicUuid, "characteristicUuid"),
      characteristicOccurrence?.toString() ?: "-",
      opt(descriptorUuid, "descriptorUuid"),
      descriptorOccurrence?.toString() ?: "-",
      properties.toString(),
      arg(lease, "lease")
    ).joinToString("|")
  }

  fun readStart(pathIndex: Long, timeoutMs: Long, nowMs: Long): String =
    "read.start|${u64(pathIndex, "pathIndex")}|" +
      "${u64(timeoutMs, "timeoutMs")}|${u64(nowMs, "nowMs")}"

  fun writeStart(
    pathIndex: Long,
    mode: String,
    valueLength: Long,
    maximum: Long?,
    modeSupported: Boolean,
    timeoutMs: Long,
    nowMs: Long
  ): String = listOf(
    "write.start",
    u64(pathIndex, "pathIndex"),
    arg(mode, "mode"),
    u64(valueLength, "valueLength"),
    maximum?.toString() ?: "-",
    modeSupported.toString(),
    u64(timeoutMs, "timeoutMs"),
    u64(nowMs, "nowMs")
  ).joinToString("|")

  fun opDispatch(op: String): String = "op.dispatch|${arg(op, "op")}"

  fun opSettle(
    op: String,
    contenderKind: String,
    valid: Boolean,
    ordinal: Long,
    nowMs: Long
  ): String = listOf(
    "op.settle",
    arg(op, "op"),
    arg(contenderKind, "contenderKind"),
    valid.toString(),
    u64(ordinal, "ordinal"),
    u64(nowMs, "nowMs")
  ).joinToString("|")

  fun opCancel(op: String, nowMs: Long): String =
    "op.cancel|${arg(op, "op")}|${u64(nowMs, "nowMs")}"

  fun subscribe(
    pathIndex: Long,
    overflowPolicy: String,
    itemCapacity: Long,
    byteCapacity: Long,
    consumer: String,
    timeoutMs: Long,
    nowMs: Long
  ): String = listOf(
    "subscribe",
    u64(pathIndex, "pathIndex"),
    arg(overflowPolicy, "overflowPolicy"),
    u64(itemCapacity, "itemCapacity"),
    u64(byteCapacity, "byteCapacity"),
    arg(consumer, "consumer"),
    u64(timeoutMs, "timeoutMs"),
    u64(nowMs, "nowMs")
  ).joinToString("|")

  fun subscribeEnableSettled(pathIndex: Long, success: Boolean, nowMs: Long): String =
    "subscribe.enable-settled|${u64(pathIndex, "pathIndex")}|$success|${u64(nowMs, "nowMs")}"

  fun unsubscribe(pathIndex: Long, consumer: String, nowMs: Long): String =
    "unsubscribe|${u64(pathIndex, "pathIndex")}|${arg(consumer, "consumer")}|${u64(nowMs, "nowMs")}"

  fun subscribeDisableSettled(pathIndex: Long, nowMs: Long): String =
    "subscribe.disable-settled|${u64(pathIndex, "pathIndex")}|${u64(nowMs, "nowMs")}"

  fun notifyDeliver(pathIndex: Long, value: ByteArray): String {
    require(value.size <= NOTIFY_MAX_BYTES) {
      "GATT notify value exceeds $NOTIFY_MAX_BYTES bytes"
    }
    val line = "notify.deliver|${u64(pathIndex.toLong(), "pathIndex")}|${hexOf(value)}"
    require(line.length <= WIRE_MAX) {
      "GATT wire line exceeds $WIRE_MAX chars"
    }
    return line
  }

  fun expireSweep(nowMs: Long): String = "expire-sweep|${u64(nowMs, "nowMs")}"

  fun adapterReset(nowMs: Long): String = "adapter.reset|${u64(nowMs, "nowMs")}"

  /** Lifecycle release on destroy: drives the real destroy transition. */
  fun release(): String = "release"

  /**
   * Parses one drain result (newline-joined JSON observation objects, one per
   * queued line) into structured observations. Never throws on content: an
   * unparsable line becomes a single observation with `ok=false` and
   * `code=platform.failure`.
   */
  fun parseObservations(drained: String): List<GattObservation> {
    if (drained.isEmpty()) return emptyList()
    return drained.split('\n').map { parseOne(it) }
  }

  private fun parseOne(line: String): GattObservation {
    fun field(name: String): String? {
      val key = "\"$name\":\""
      val start = line.indexOf(key)
      if (start < 0) return null
      val from = start + key.length
      // Values escape '"' and '\'; unescape minimally for identity fields.
      val out = StringBuilder()
      var i = from
      while (i < line.length) {
        val c = line[i]
        if (c == '\\' && i + 1 < line.length) {
          out.append(line[i + 1])
          i += 2
          continue
        }
        if (c == '"') break
        out.append(c)
        i++
      }
      return out.toString()
    }
    val ok = line.contains("\"ok\":true")
    return GattObservation(
      ok = ok,
      event = field("event") ?: "",
      code = field("code"),
      domain = field("domain"),
      operation = field("operation"),
      detail = field("detail"),
      raw = line
    )
  }
}

/** One parsed drain observation line. */
data class GattObservation(
  val ok: Boolean,
  val event: String,
  val code: String?,
  val domain: String?,
  val operation: String?,
  val detail: String?,
  val raw: String
)
