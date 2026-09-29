// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/BackgroundContinuation.kt

package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson

/** Wake strategies of the declared standing order, in declaration order. */
enum class ContinuationStrategy(val wire: String) {
  RECORD_ONLY("record-only"),
  NATIVE("native"),
  HEADLESS_TASK("headless-task"),
  FOREGROUND_SERVICE("foreground-service");

  companion object {
    fun parse(wire: Any?, operation: String): ContinuationStrategy {
      val text = wire as? String
        ?: throw IllegalArgumentException("$operation: onAppearance must be a string")
      return values().firstOrNull { it.wire == text }
        ?: throw IllegalArgumentException("$operation: unknown onAppearance $text")
    }
  }
}

/** One declared GATT resubscription of the `native` standing order. */
data class ContinuationSelector(
  val serviceUuid: String,
  val serviceOccurrence: Long,
  val characteristicUuid: String,
  val characteristicOccurrence: Long
)

data class ContinuationSetupResponse(
  val subscriptionIndex: Long, val prefix: List<Long>, val minLength: Long,
  val maxLength: Long, val statusOffset: Long, val accepted: List<Long>,
  val trailing: ContinuationSetupTrailing? = null
)

data class ContinuationSetupTrailing(val offset: Long, val accepted: List<Long>)
data class ContinuationLinkMtu(val requested: Long, val timeoutMs: Long, val onUnsupported: String)
data class ContinuationRecording(val id: String, val maxBytes: Long, val maxRecords: Long)

data class ContinuationSetupStep(
  val selector: ContinuationSelector, val value: List<Long>, val timeoutMs: Long,
  val response: ContinuationSetupResponse?
)

/** Foreground-service configuration of the Android presence strategy. */
data class ContinuationForegroundService(val notification: ContinuationNotification)

/** Notification requested by the Android foreground-service strategy. */
data class ContinuationNotification(
  val channelId: String,
  val channelName: String,
  val title: String,
  val body: String?,
  val icon: String?
)

/**
 * The declared standing order the OS wake executes (BGS4). Parsed from the
 * canonical JSON the binding persists — the same key set
 * `serializeBackgroundContinuation` writes, so a payload the JavaScript side
 * could not produce is refused here, never executed.
 */
data class BackgroundContinuationDeclaration(
  val strategy: ContinuationStrategy,
  /** Uppercase MAC subject; null scopes the order to whichever armed peer appears. */
  val peerId: String?,
  val resubscribe: List<ContinuationSelector>,
  val headlessTaskName: String?,
  val foregroundService: ContinuationForegroundService?,
  val setup: List<ContinuationSetupStep>? = null,
  val link: ContinuationLinkMtu? = null,
  val recording: ContinuationRecording? = null
) {
  companion object {
    private const val OPERATION = "background.continuation"
    private val TOP_KEYS = setOf("onAppearance", "peerId", "resubscribe", "setup", "link", "recording", "headlessTaskName", "foregroundService")
    private val SELECTOR_KEYS = setOf("serviceUuid", "serviceOccurrence", "characteristicUuid", "characteristicOccurrence")
    private val MAC = Regex("^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$")
    private val UUID = Regex("^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")

    fun recordOnly(): BackgroundContinuationDeclaration = BackgroundContinuationDeclaration(
      strategy = ContinuationStrategy.RECORD_ONLY,
      peerId = null,
      resubscribe = emptyList(),
      headlessTaskName = null,
      foregroundService = null
    )

    /** Parses persisted JSON; null (never declared) means `record-only`. */
    fun parse(json: String?): BackgroundContinuationDeclaration {
      if (json == null) return recordOnly()
      val root = try {
        RustCoreJson.parse(json) as? Map<*, *> ?: throw IllegalArgumentException("$OPERATION: not an object")
      } catch (error: RustCoreJson.MalformedJson) {
        throw IllegalArgumentException("$OPERATION: malformed: ${error.message}")
      }
      rejectUnknown(root.keys, TOP_KEYS, OPERATION)
      val strategy = if (!root.containsKey("onAppearance")) ContinuationStrategy.RECORD_ONLY
      else ContinuationStrategy.parse(root["onAppearance"], OPERATION)
      val peerId = if (!root.containsKey("peerId")) null else {
        val text = root["peerId"] as? String
          ?: throw IllegalArgumentException("$OPERATION: peerId must be a string")
        if (!MAC.matches(text)) throw IllegalArgumentException("$OPERATION: peerId must be a MAC address")
        text.uppercase()
      }
      val resubscribe = if (!root.containsKey("resubscribe")) emptyList()
      else {
        val entries = root["resubscribe"] as? List<*>
          ?: throw IllegalArgumentException("$OPERATION: resubscribe must be an array")
        if (entries.size > 64) throw IllegalArgumentException("$OPERATION: resubscribe too many")
        entries.map { selector(it) }
      }
      val headlessTaskName = if (!root.containsKey("headlessTaskName")) null else {
        val text = root["headlessTaskName"] as? String
          ?: throw IllegalArgumentException("$OPERATION: headlessTaskName must be a string")
        if (text.isEmpty()) throw IllegalArgumentException("$OPERATION: headlessTaskName must be non-empty")
        text
      }
      if (strategy == ContinuationStrategy.HEADLESS_TASK && headlessTaskName == null) {
        throw IllegalArgumentException("$OPERATION: headlessTaskName required for headless-task")
      }
      if (strategy != ContinuationStrategy.HEADLESS_TASK && headlessTaskName != null) {
        throw IllegalArgumentException("$OPERATION: headlessTaskName applies only to headless-task")
      }
      val foregroundService = if (!root.containsKey("foregroundService")) null
      else foregroundService(root["foregroundService"])
      if (strategy == ContinuationStrategy.FOREGROUND_SERVICE && foregroundService == null) {
        throw IllegalArgumentException("$OPERATION: foregroundService required for foreground-service")
      }
      if (strategy != ContinuationStrategy.FOREGROUND_SERVICE && foregroundService != null) {
        throw IllegalArgumentException("$OPERATION: foregroundService applies only to foreground-service")
      }
      val setup = if (!root.containsKey("setup")) null else {
        if (strategy != ContinuationStrategy.NATIVE) throw IllegalArgumentException("$OPERATION: setup requires native strategy")
        setup(root["setup"], resubscribe.size)
      }
      val link = if (!root.containsKey("link")) null else {
        if (strategy != ContinuationStrategy.NATIVE) throw IllegalArgumentException("$OPERATION: link requires native strategy")
        val link = record(root["link"], setOf("mtu"), "link")
        val mtu = record(link["mtu"], setOf("requested", "timeoutMs", "onUnsupported"), "link mtu")
        val policy = mtu["onUnsupported"] as? String
        if (policy != "continue" && policy != "fail") throw IllegalArgumentException("$OPERATION: link mtu onUnsupported")
        ContinuationLinkMtu(boundedInteger(mtu["requested"], 23, 517, "link mtu requested"),
          boundedInteger(mtu["timeoutMs"], 1, 20000, "link mtu timeoutMs"), policy)
      }
      val recording = if (!root.containsKey("recording")) null else {
        if (strategy != ContinuationStrategy.NATIVE) throw IllegalArgumentException("$OPERATION: recording requires native strategy")
        val journal = record(root["recording"], setOf("id", "maxBytes", "maxRecords"), "recording")
        val id = journal["id"] as? String
        if (id == null || !Regex("^[A-Za-z0-9_-]{1,64}$").matches(id)) throw IllegalArgumentException("$OPERATION: recording id")
        ContinuationRecording(id, boundedInteger(journal["maxBytes"], 1048576, 1073741824, "recording maxBytes"),
          boundedInteger(journal["maxRecords"], 1, 1000000, "recording maxRecords"))
      }
      return BackgroundContinuationDeclaration(strategy, peerId, resubscribe, headlessTaskName, foregroundService, setup, link, recording)
    }

    private fun setup(value: Any?, subscriptions: Int): List<ContinuationSetupStep> {
      val steps = value as? List<*> ?: throw IllegalArgumentException("$OPERATION: setup must be an array")
      if (steps.size > 16) throw IllegalArgumentException("$OPERATION: setup too many steps")
      var totalTimeout = 0L
      return steps.map { raw ->
        val entry = record(raw, setOf("selector", "value", "timeoutMs", "response"), "setup step")
        val timeout = boundedInteger(entry["timeoutMs"], 1, 20000, "setup timeoutMs")
        totalTimeout += timeout
        if (totalTimeout > 60000) throw IllegalArgumentException("$OPERATION: setup total timeout exceeds 60000")
        val response = if (!entry.containsKey("response")) null else {
          val reply = record(entry["response"], setOf("subscriptionIndex", "prefix", "minLength", "maxLength", "status", "trailing"), "setup response")
          val prefix = bytes(reply["prefix"], 512, "setup response prefix")
          val minimum = boundedInteger(reply["minLength"], prefix.size.toLong(), 512, "setup response minLength")
          val maximum = boundedInteger(reply["maxLength"], minimum, 512, "setup response maxLength")
          val status = record(reply["status"], setOf("offset", "accepted"), "setup response status")
          val accepted = bytes(status["accepted"], 256, "setup response accepted")
          if (accepted.distinct().size != accepted.size) throw IllegalArgumentException("$OPERATION: setup response duplicate accepted status")
          val trailing = if (!reply.containsKey("trailing")) null else {
            val tail = record(reply["trailing"], setOf("offset", "accepted"), "setup response trailing")
            val offset = boundedInteger(tail["offset"], minimum, minimum, "setup response trailing offset")
            if (maximum != offset + 1) throw IllegalArgumentException("$OPERATION: setup response trailing maxLength")
            val acceptedTail = bytes(tail["accepted"], 256, "setup response trailing accepted")
            if (acceptedTail.distinct().size != acceptedTail.size) throw IllegalArgumentException("$OPERATION: setup response duplicate trailing status")
            ContinuationSetupTrailing(offset, acceptedTail)
          }
          ContinuationSetupResponse(
            boundedInteger(reply["subscriptionIndex"], 0, subscriptions.toLong() - 1, "setup response subscriptionIndex"),
            prefix, minimum, maximum,
            boundedInteger(status["offset"], prefix.size.toLong(), minimum - 1, "setup response status offset"), accepted, trailing
          )
        }
        ContinuationSetupStep(selector(entry["selector"]), bytes(entry["value"], 512, "setup value"), timeout, response)
      }
    }

    private fun record(value: Any?, keys: Set<String>, label: String): Map<*, *> {
      val entry = value as? Map<*, *> ?: throw IllegalArgumentException("$OPERATION: $label must be an object")
      rejectUnknown(entry.keys, keys, "$OPERATION $label")
      return entry
    }

    private fun boundedInteger(value: Any?, minimum: Long, maximum: Long, label: String): Long {
      val integer = value as? Long ?: throw IllegalArgumentException("$OPERATION: $label must be an integer")
      if (integer < minimum || integer > maximum) throw IllegalArgumentException("$OPERATION: $label out of range")
      return integer
    }

    private fun bytes(value: Any?, maximum: Int, label: String): List<Long> {
      val array = value as? List<*> ?: throw IllegalArgumentException("$OPERATION: $label must be an array")
      if (array.isEmpty() || array.size > maximum) throw IllegalArgumentException("$OPERATION: $label invalid length")
      return array.map { boundedInteger(it, 0, 255, label) }
    }

    private fun selector(value: Any?): ContinuationSelector {
      val entry = value as? Map<*, *>
        ?: throw IllegalArgumentException("$OPERATION: resubscribe entry must be an object")
      rejectUnknown(entry.keys, SELECTOR_KEYS, "$OPERATION resubscribe entry")
      return ContinuationSelector(
        serviceUuid = uuid(entry["serviceUuid"], "$OPERATION resubscribe serviceUuid"),
        serviceOccurrence = occurrence(entry, "serviceOccurrence"),
        characteristicUuid = uuid(entry["characteristicUuid"], "$OPERATION resubscribe characteristicUuid"),
        characteristicOccurrence = occurrence(entry, "characteristicOccurrence")
      )
    }

    private fun uuid(value: Any?, label: String): String {
      val text = value as? String ?: throw IllegalArgumentException("$label must be a string")
      if (!UUID.matches(text)) throw IllegalArgumentException("$label must be a canonical 128-bit UUID")
      return text.lowercase()
    }

    private fun occurrence(entry: Map<*, *>, key: String): Long {
      if (!entry.containsKey(key)) return 1L
      // RustCoreJson yields Long integers. Never truncate another numeric type,
      // and keep the public Number.isSafeInteger boundary on persisted input.
      val number = entry[key] as? Long
        ?: throw IllegalArgumentException("$OPERATION resubscribe occurrence must be a positive integer")
      if (number < 1L || number > 9007199254740991L) {
        throw IllegalArgumentException("$OPERATION resubscribe occurrence must be a positive safe integer")
      }
      return number
    }

    private fun foregroundService(value: Any?): ContinuationForegroundService {
      val service = value as? Map<*, *>
        ?: throw IllegalArgumentException("$OPERATION: foregroundService must be an object")
      rejectUnknown(service.keys, setOf("notification"), "$OPERATION foregroundService")
      val raw = service["notification"] as? Map<*, *>
        ?: throw IllegalArgumentException("$OPERATION: foregroundService notification required")
      rejectUnknown(raw.keys, setOf("channelId", "channelName", "title", "body", "icon"), "$OPERATION notification")
      fun text(key: String): String {
        val text = raw[key] as? String
          ?: throw IllegalArgumentException("$OPERATION: notification $key must be a string")
        if (text.isEmpty()) throw IllegalArgumentException("$OPERATION: notification $key must be non-empty")
        return text
      }
      return ContinuationForegroundService(
        ContinuationNotification(
          channelId = text("channelId"),
          channelName = text("channelName"),
          title = text("title"),
          body = if (raw.containsKey("body")) text("body") else null,
          icon = if (raw.containsKey("icon")) text("icon") else null
        )
      )
    }

    private fun rejectUnknown(keys: Set<Any?>, expected: Set<String>, label: String) {
      val unknown = keys.filterIsInstance<String>().filter { it !in expected }.sorted()
      if (unknown.isNotEmpty()) throw IllegalArgumentException("$label unknown keys: ${unknown.joinToString(",")}")
    }
  }
}

/**
 * The wake outcome in ONE vocabulary on both hosts: `continuation.completed`
 * or `continuation.failed` with the platform's own reason underneath — the
 * same event names and words on Android and iOS.
 */
sealed interface ContinuationOutcome {
  val strategy: ContinuationStrategy
  val event: String

  data class Completed(
    override val strategy: ContinuationStrategy,
    val peerAddress: String,
    val resubscribed: Int,
    /** Platform dispatch acceptance, never completion of the app's task. */
    val stage: String? = null
  ) : ContinuationOutcome {
    override val event = "continuation.completed"
  }

  data class Failed(
    override val strategy: ContinuationStrategy,
    /** Public error code (`capability.unsupported`, `connection.failed`, …). */
    val code: String,
    /** Human reason; the platform's own detail rides in [platform]. */
    val reason: String,
    /** The platform's own error identity, or null when the failure is ours. */
    val platform: String?
  ) : ContinuationOutcome {
    override val event = "continuation.failed"
  }

  companion object {
    fun completed(strategy: ContinuationStrategy, peerAddress: String, resubscribed: Int): ContinuationOutcome =
      Completed(strategy, peerAddress, resubscribed)

    fun failed(strategy: ContinuationStrategy, code: String, reason: String, platform: String?): ContinuationOutcome =
      Failed(strategy, code, reason, platform)
  }
}

/** The last-wake record Diagnostics renders (and unsupported states quote). */
data class ContinuationWakeRecord(
  val observedAtMs: Long,
  val event: String,
  val strategy: ContinuationStrategy,
  val peerAddress: String?,
  val code: String?,
  val reason: String?,
  val stage: String? = null,
  val platform: String? = null
) {
  fun wire(): Map<String, Any?> = linkedMapOf<String, Any?>(
    "observedAtMs" to observedAtMs, "event" to event, "strategy" to strategy.wire,
    "peerAddress" to peerAddress, "code" to code, "reason" to reason
  ).also { value ->
    stage?.let { value["stage"] = it }
    platform?.let { value["platform"] = com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson.parse(it) }
  }
}
