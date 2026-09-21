// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/RustCoreContinuationExecutor.kt

package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.MobileCorePort
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/**
 * Executes the `native` standing order from the wake through the Rust core
 * with no JavaScript (BGS4): a host-owned continuation session opens,
 * connects the appeared known peer `when-available`, discovers, and
 * subscribes the declared characteristics. Values arriving with no JS
 * session queue in the session's existing bounded outbox; when the app opens,
 * [claimAndDispose] drains it with the drain contract's own loss accounting.
 *
 * One continuation session per process: a second appearance while the link
 * is held is already continuing, so it reuses the session instead of
 * duplicating consumers. Multi-peer concurrency is refused truthfully (one
 * peer at a time this release); the declaration bound stays 64.
 */
class RustCoreContinuationExecutor(
  private val core: MobileCorePort,
  private val wireRevision: String,
  private val log: (String) -> Unit,
  private val connectBudgetMs: Long = CONNECT_BUDGET_MS,
  private val opTimeoutMs: Long = OP_TIMEOUT_MS
) {
  private val lock = Any()
  private var sessionId: Long? = null
  private var continuingPeer: String? = null
  private var subscribedConsumers = 0
  private var admission = 0L

  /** Executes the order; every refusal is a typed outcome, never a throw. */
  fun execute(address: String, declaration: BackgroundContinuationDeclaration): ContinuationOutcome {
    synchronized(lock) {
      try {
        val session = ensureSession()
        val held = continuingPeer
        if (held != null && held != address) {
          return ContinuationOutcome.failed(
            ContinuationStrategy.NATIVE,
            "lifecycle.invalid-state",
            "continuation session already holds $held; one peer at a time this release",
            null
          )
        }
        if (held == address && subscribedConsumers == declaration.resubscribe.size) {
          log("continuation session already holds $address; already continuing")
          return ContinuationOutcome.completed(ContinuationStrategy.NATIVE, address, subscribedConsumers)
        }
        val connected = invokeChecked(
          session,
          "connection.connect",
          linkedMapOf(
            "peerId" to address,
            "lease" to CONTINUATION_LEASE,
            "operationId" to "continuation-connect",
            "intent" to "when-available",
            "transport" to "auto",
            "preferredPhy" to emptyList<String>(),
            "budgetMs" to connectBudgetMs
          ),
          opTimeoutMs + connectBudgetMs
        )
        if (!connected.ok) {
          return ContinuationOutcome.failed(
            ContinuationStrategy.NATIVE,
            connected.code,
            connected.reason,
            connected.platform
          )
        }
        val discovered = invokeChecked(
          session,
          "gatt.discover",
          linkedMapOf(
            "peerId" to address,
            "lease" to CONTINUATION_LEASE,
            "operationId" to "continuation-discover"
          ),
          opTimeoutMs
        )
        if (!discovered.ok) {
          return ContinuationOutcome.failed(
            ContinuationStrategy.NATIVE,
            discovered.code,
            discovered.reason,
            discovered.platform
          )
        }
        var resubscribed = 0
        declaration.resubscribe.forEachIndexed { index, selector ->
          val consumer = "$CONSUMER_PREFIX$index"
          val subscribed = invokeChecked(
            session,
            "gatt.subscribe",
            linkedMapOf(
              "peerId" to address,
              "selector" to linkedMapOf(
                "serviceUuid" to selector.serviceUuid,
                "serviceOccurrence" to selector.serviceOccurrence,
                "characteristicUuid" to selector.characteristicUuid,
                "characteristicOccurrence" to selector.characteristicOccurrence
              ),
              "consumer" to consumer,
              "operationId" to "continuation-subscribe-$index"
            ),
            opTimeoutMs
          )
          if (!subscribed.ok) {
            return ContinuationOutcome.failed(
              ContinuationStrategy.NATIVE,
              subscribed.code,
              subscribed.reason,
              subscribed.platform
            )
          }
          resubscribed += 1
        }
        continuingPeer = address
        subscribedConsumers = resubscribed
        log("continuation completed for $address: connected when-available, resubscribed $resubscribed")
        return ContinuationOutcome.completed(ContinuationStrategy.NATIVE, address, resubscribed)
      } catch (error: ContinuationFailure) {
        return ContinuationOutcome.failed(
          ContinuationStrategy.NATIVE,
          "platform.failure",
          error.message ?: "continuation invoke refused",
          null
        )
      }
    }
  }

  /**
   * Reads the queued backlog size without consuming it (`counters.describe`):
   * proof values are arriving while no JS session exists. Null when no
   * continuation session is alive.
   */
  fun describeBacklog(): BacklogCounts? {
    val session = synchronized(lock) { sessionId } ?: return null
    val result = try {
      invoke(session, "counters.describe", emptyMap(), opTimeoutMs)
    } catch (error: ContinuationFailure) {
      log("continuation backlog unreadable: ${error.message}")
      return null
    }
    val checked = checkEnvelope("counters.describe", result)
    if (!checked.ok) return null
    return backlogCounts(checked.value)
  }

  /**
   * Drains the continuation backlog (verbatim drain batches for the JS
   * codec) and disposes the session. The app's own session connects next;
   * the link gap is covered by the core's loss accounting, never silence.
   */
  fun claimAndDispose(maxItems: Int, maxBytes: Int, maxBatches: Int = 32): ContinuationClaim {
    val session = synchronized(lock) { sessionId }
      ?: return ContinuationClaim(emptyList(), false)
    val batches = ArrayList<String>(4)
    var more = true
    var rounds = 0
    while (more && rounds < maxBatches) {
      rounds += 1
      val batch = try {
        core.drain(session, maxItems, maxBytes)
      } catch (error: RuntimeException) {
        log("continuation drain failed: ${error.message ?: error.javaClass.simpleName}")
        break
      }
      batches.add(batch)
      more = try {
        val root = RustCoreJson.parse(batch) as? Map<*, *>
        root?.get("more") as? Boolean ?: false
      } catch (error: IllegalArgumentException) {
        log("continuation drain batch unparseable: ${error.message}")
        false
      }
    }
    val disposed = try {
      invoke(session, "session.dispose", emptyMap(), opTimeoutMs)
      true
    } catch (error: ContinuationFailure) {
      log("continuation dispose failed: ${error.message}")
      false
    }
    synchronized(lock) {
      sessionId = null
      continuingPeer = null
      subscribedConsumers = 0
    }
    return ContinuationClaim(batches, disposed)
  }

  private fun ensureSession(): Long {
    sessionId?.let { return it }
    val record = try {
      core.openSession(CONTINUATION_OWNER, wireRevision, CONTINUATION_SCOPE)
    } catch (error: RuntimeException) {
      throw ContinuationFailure("continuation session open refused: ${error.message ?: error.javaClass.simpleName}")
    }
    val parsed = try {
      RustCoreJson.parse(record) as? Map<*, *>
    } catch (error: IllegalArgumentException) {
      throw ContinuationFailure("continuation session admission unparseable: ${error.message}")
    }
    val id = (parsed?.get("sessionId") as? Number)?.toLong()
      ?: throw ContinuationFailure("continuation session admission without a session id")
    admission = 0L
    sessionId = id
    return id
  }

  private data class Checked(val ok: Boolean, val code: String, val reason: String, val platform: String?, val value: Map<*, *>)

  private fun invokeChecked(session: Long, op: String, args: Map<String, Any?>, timeoutMs: Long): Checked {
    admission += 1
    val withAdmission = LinkedHashMap<String, Any?>(args)
    withAdmission["admission"] = admission
    return checkEnvelope(op, invoke(session, op, withAdmission, timeoutMs))
  }

  private fun invoke(session: Long, op: String, args: Map<String, Any?>, timeoutMs: Long): Map<*, *> {
    val envelope = AtomicReference<String?>(null)
    val latch = CountDownLatch(1)
    try {
      core.invoke(session, op, RustCoreJson.write(args), MobileCoreBridge.InvokeCallback { json ->
        envelope.set(json)
        latch.countDown()
      })
    } catch (error: RuntimeException) {
      throw ContinuationFailure("$op invoke refused: ${error.message ?: error.javaClass.simpleName}")
    }
    if (!latch.await(timeoutMs, TimeUnit.MILLISECONDS)) {
      throw ContinuationFailure("$op timed out after ${timeoutMs}ms")
    }
    val text = envelope.get() ?: throw ContinuationFailure("$op answered without an envelope")
    return try {
      RustCoreJson.parse(text) as? Map<*, *>
        ?: throw ContinuationFailure("$op envelope is not an object")
    } catch (error: IllegalArgumentException) {
      throw ContinuationFailure("$op envelope unparseable: ${error.message}")
    }
  }

  private fun checkEnvelope(op: String, root: Map<*, *>): Checked {
    if (root["ok"] == true) {
      return Checked(true, "", "", null, root["value"] as? Map<*, *> ?: emptyMap<Any?, Any?>())
    }
    val error = root["error"] as? Map<*, *>
    val code = error?.get("code") as? String ?: "$op.failed"
    val detail = error?.get("detail") as? String ?: error?.get("operation") as? String ?: op
    val platform = (error?.get("platform") as? Map<*, *>)?.let { RustCoreJson.write(it) }
    return Checked(false, code, detail, platform, emptyMap<Any?, Any?>())
  }

  private fun backlogCounts(value: Map<*, *>): BacklogCounts {
    val counters = value["counters"] as? Map<*, *>
    val bytes = (counters?.get("retainedByteBuffers") as? Number)?.toLong()
    val process = value["process"] as? Map<*, *>
    val native = process?.get("native") as? Map<*, *>
    val drops = (native?.get("ingressDrops") as? Map<*, *>)
      ?.mapNotNull { (key, count) ->
        val name = key as? String ?: return@mapNotNull null
        val total = (count as? Number)?.toLong() ?: return@mapNotNull null
        name to total
      }?.toMap() ?: emptyMap()
    return BacklogCounts(queuedBytes = bytes, ingressDrops = drops)
  }

  companion object {
    const val CONTINUATION_OWNER = "unified-ble-manager/continuation"
    const val CONTINUATION_SCOPE = "ubm-continuation"
    const val CONTINUATION_LEASE = "continuation-lease"
    const val CONSUMER_PREFIX = "ubm-continuation-"
    const val CONNECT_BUDGET_MS = 15_000L
    const val OP_TIMEOUT_MS = 10_000L
  }
}

/** A refused invoke that becomes a typed outcome (never a silent nothing). */
private class ContinuationFailure(message: String) : RuntimeException(message)

/** Queued-backlog proof without consuming it. */
data class BacklogCounts(val queuedBytes: Long?, val ingressDrops: Map<String, Long>)

/**
 * Verbatim drain batches for the JS codec plus whether a session was
 * disposed. Empty batches with `disposed: false` is the valid no-wake
 * answer (no continuation session alive) — never an error.
 */
data class ContinuationClaim(val batches: List<String>, val disposed: Boolean)
