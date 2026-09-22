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

  private fun optU64(value: Long?, field: String): String =
    if (value == null) "-" else u64(value, field)

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
    serviceUuids.joinToString(",") {
      require(!it.contains(',')) { "GATT wire field serviceUuid must not contain ','" }
      arg(it, "serviceUuid")
    },
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
      optU64(characteristicOccurrence, "characteristicOccurrence"),
      opt(descriptorUuid, "descriptorUuid"),
      optU64(descriptorOccurrence, "descriptorOccurrence"),
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
    optU64(maximum, "maximum"),
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
    // Blank lines (e.g. a trailing newline) are never observations; dropping
    // them beats a phantom ok=false record.
    return drained.split('\n').filter { it.isNotBlank() }.map { parseOne(it) }
  }

  private fun parseOne(line: String): GattObservation {
    fun field(name: String): String? = fieldIn(line, name)
    // Anchored: a `"ok":true` substring inside a string field must not flip
    // the verdict. Rust emits {"ok":true,...} / {"ok":false,...} exactly.
    val ok = line.startsWith("{\"ok\":true") && (line.length == 10 || line[10] == ',' || line[10] == '}')
    val event = field("event") ?: ""
    // F01: kernel effects plus typed observations ride every drained line.
    // A present-but-malformed section fails the line closed (a truncated
    // section must never read as "no effects"); absent sections parse as
    // empty (pre-F01 lines only).
    val effects = parseEffectSection(line, "effects")
      ?: return GattObservation(false, event, "platform.failure", "gatt", "gatt-drain", "effects-malformed", line)
    val observations = parseEffectSection(line, "observations")
      ?: return GattObservation(false, event, "platform.failure", "gatt", "gatt-drain", "observations-malformed", line)
    return GattObservation(
      ok = ok,
      event = event,
      code = field("code"),
      domain = field("domain"),
      operation = field("operation"),
      detail = field("detail"),
      raw = line,
      effects = effects,
      observations = observations
    )
  }

  private fun fieldIn(text: String, name: String): String? {
    val key = "\"$name\":\""
    val start = text.indexOf(key)
    if (start < 0) return null
    val from = start + key.length
    // Full JSON string unescape (Rust's json_escape_into emits \" \\ \n
    // \r \t plus \uXXXX): identity fields round-trip exactly.
    val out = StringBuilder()
    var i = from
    while (i < text.length) {
      val c = text[i]
      if (c == '\\' && i + 1 < text.length) {
        when (val e = text[i + 1]) {
          'n' -> out.append('\n')
          'r' -> out.append('\r')
          't' -> out.append('\t')
          'b' -> out.append('\b')
          'f' -> out.append(12.toChar())
          '/' -> out.append('/')
          'u' -> {
            val hex = text.substring(i + 2, minOf(i + 6, text.length))
            val code = hex.toIntOrNull(16)
            if (hex.length == 4 && code != null) {
              out.append(code.toChar())
              i += 6
              continue
            }
            out.append(e)
          }
          else -> out.append(e)
        }
        i += 2
        continue
      }
      if (c == '"') break
      out.append(c)
      i++
    }
    return out.toString()
  }

  /**
   * Parses one `"key":[{"kind","op","detail"},...]` section. Returns null
   * when the section is present but malformed (the line then fails closed);
   * an absent section returns empty (pre-F01 lines). Never throws.
   */
  private fun parseEffectSection(line: String, key: String): List<GattEffect>? {
    val anchor = "\"$key\":["
    val from = line.indexOf(anchor)
    if (from < 0) return emptyList()
    var i = from + anchor.length
    val entries = mutableListOf<GattEffect>()
    fun skipGaps() {
      while (i < line.length && (line[i] == ' ' || line[i] == '\t')) i++
    }
    skipGaps()
    if (i < line.length && line[i] == ']') return entries
    while (true) {
      if (i >= line.length || line[i] != '{') return null
      // String-aware object extent: braces inside quoted values (details
      // may carry JSON) must not end the scan.
      var j = i
      var inString = false
      var escaped = false
      var depth = 0
      var end = -1
      while (j < line.length) {
        val c = line[j]
        if (escaped) {
          escaped = false
        } else if (c == '\\' && inString) {
          escaped = true
        } else if (c == '"') {
          inString = !inString
        } else if (!inString && c == '{') {
          depth++
        } else if (!inString && c == '}') {
          depth--
          if (depth == 0) {
            end = j
            break
          }
        }
        j++
      }
      if (end < 0) return null
      val entry = line.substring(i, end + 1)
      val kind = fieldIn(entry, "kind") ?: return null
      val op = fieldIn(entry, "op") ?: return null
      val detail = fieldIn(entry, "detail") ?: return null
      entries.add(GattEffect(kind, op, detail))
      i = end + 1
      skipGaps()
      if (i >= line.length) return null
      if (line[i] == ']') return entries
      if (line[i] != ',') return null
      i++
      skipGaps()
    }
  }
}

/**
 * One kernel effect or typed observation surfaced on a drained line (F01).
 *
 * Both sections share the `{"kind","op","detail"}` shape. `effects` are
 * MUST-EXECUTE for the host owner, in order (see the per-kind contract):
 * - `radio.dispatch` — perform the admitted radio op, then feed the outcome
 *   back through the matching wire verb (`op.settle`, `link.established`,
 *   ...); the op id binds the feedback.
 * - `timer.schedule` — arm the kernel deadline in `detail`, then post
 *   `expire-sweep` when it fires; `timer.cancel` disarms it.
 * - `state.publish` — surface the lifecycle fact in `detail` to the owner.
 * - `cleanup.release` — settle the recorded destroy and report it;
 *   `observation.deliver` — deliver the payload to the consumer.
 * `observations` are typed central facts (`central.scan-start`, ...) to
 * publish. Neither section is ever dropped: quiet lines emit empty arrays.
 */
data class GattEffect(
  val kind: String,
  val op: String,
  val detail: String
)

/** One parsed drain observation line. */
data class GattObservation(
  val ok: Boolean,
  val event: String,
  val code: String?,
  val domain: String?,
  val operation: String?,
  val detail: String?,
  val raw: String,
  val effects: List<GattEffect> = emptyList(),
  val observations: List<GattEffect> = emptyList()
)
