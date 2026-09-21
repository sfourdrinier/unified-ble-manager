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

/** Foreground-service configuration of the deferred `foreground-service` strategy. */
data class ContinuationForegroundService(val notification: ContinuationNotification)

/** Notification of the deferred `foreground-service` strategy. */
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
  val foregroundService: ContinuationForegroundService?
) {
  companion object {
    private const val OPERATION = "background.continuation"
    private val TOP_KEYS = setOf("onAppearance", "peerId", "resubscribe", "headlessTaskName", "foregroundService")
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
      return BackgroundContinuationDeclaration(strategy, peerId, resubscribe, headlessTaskName, foregroundService)
    }

    private fun selector(value: Any?): ContinuationSelector {
      val entry = value as? Map<*, *>
        ?: throw IllegalArgumentException("$OPERATION: resubscribe entry must be an object")
      rejectUnknown(entry.keys, SELECTOR_KEYS, "$OPERATION resubscribe entry")
      return ContinuationSelector(
        serviceUuid = uuid(entry["serviceUuid"], "$OPERATION resubscribe serviceUuid"),
        serviceOccurrence = occurrence(entry["serviceOccurrence"]),
        characteristicUuid = uuid(entry["characteristicUuid"], "$OPERATION resubscribe characteristicUuid"),
        characteristicOccurrence = occurrence(entry["characteristicOccurrence"])
      )
    }

    private fun uuid(value: Any?, label: String): String {
      val text = value as? String ?: throw IllegalArgumentException("$label must be a string")
      if (!UUID.matches(text)) throw IllegalArgumentException("$label must be a canonical 128-bit UUID")
      return text.lowercase()
    }

    private fun occurrence(value: Any?): Long {
      if (value == null) return 1L
      val number = (value as? Number)?.toLong()
        ?: throw IllegalArgumentException("$OPERATION resubscribe occurrence must be a positive integer")
      if (number < 1L) throw IllegalArgumentException("$OPERATION resubscribe occurrence must be a positive integer")
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
          body = raw["body"] as? String,
          icon = raw["icon"] as? String
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
    val resubscribed: Int
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
  val reason: String?
)
