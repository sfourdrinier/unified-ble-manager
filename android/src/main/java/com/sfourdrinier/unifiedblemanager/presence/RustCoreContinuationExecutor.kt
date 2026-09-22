// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/RustCoreContinuationExecutor.kt

package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.MobileCorePort
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

/**
 * Executes the `native` standing order from the wake through the Rust core
 * with no JavaScript (BGS4): a host-owned continuation session opens,
 * connects the appeared known peer `direct`, discovers, and subscribes the
 * declared characteristics. Values arriving with no JS session queue in the
 * session's existing bounded outbox; when the app opens, the native surface
 * prepares then acknowledges a claim, while [claimAndDispose] is JVM-test
 * convenience that exercises that same protocol.
 *
 * `direct`, not `when-available`: Companion Device Manager only fires on an
 * absent-to-present transition, so the peer is already advertising when this
 * runs and the opportunistic background wait buys nothing (finding 242: the
 * wake link must establish at once, like the foreground's). Every radio op
 * carries the budget its client latch honors, so an outcome always reports
 * the core's answer instead of the latch contradicting it.
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
  private val opTimeoutMs: Long = OP_TIMEOUT_MS,
  private val discoverBudgetMs: Long = DISCOVER_BUDGET_MS
) {
  private val lock = Any()
  private var sessionId: Long? = null
  private var continuingPeer: String? = null
  /** Immutable declaration identity for the session's active continuation. */
  private var continuingSelectors: List<ContinuationSelector> = emptyList()
  /** Consumers whose routes still describe the currently live continuation. */
  private var activeConsumers: List<String> = emptyList()
  /** Selector identity for every consumer that can still occur in this session's outbox. */
  private var claimedSelectors: List<ContinuationSelector> = emptyList()
  private var subscribedConsumers = 0
  private var activeSession: Long? = null
  private var activeSessionActivity: SessionActivity? = null
  private val admission = AtomicLong(0)
  /** A sealed native handoff remains authoritative until TypeScript validates and acknowledges it. */
  private var preparedClaim: PreparedClaim? = null

  private data class PreparedClaim(
    val token: String,
    val claim: ContinuationClaim,
    /** A complete drain may release after acknowledgement; a prefix may only advance to its tail. */
    val cleanupEligible: Boolean,
    var acknowledged: Boolean = false,
    /** A released session keeps one replay-safe acknowledgement receipt. */
    var acknowledgement: ContinuationAcknowledgement? = null
  )

  /** One execution's state snapshot: the lock guards this handoff, not the radio I/O. */
  private data class Execution(val session: Long, val reconcileHeldSession: Boolean)

  private enum class SessionActivity { EXECUTING, CLAIMING }

  /** Executes the order; every refusal is a typed outcome, never a throw. */
  fun execute(address: String, declaration: BackgroundContinuationDeclaration): ContinuationOutcome {
    // State under the lock, radio I/O outside it: a second appearance, a
    // disappearance, or teardown never queues behind the connect budget and
    // the resubscribe timeouts.
    val execution = synchronized(lock) {
      val session = try {
        ensureSession()
      } catch (error: ContinuationFailure) {
        return ContinuationOutcome.failed(
          ContinuationStrategy.NATIVE,
          "platform.failure",
          error.message ?: "continuation invoke refused",
          null
        )
      }
      if (activeSession == session) {
        return ContinuationOutcome.failed(
          ContinuationStrategy.NATIVE,
          "lifecycle.invalid-state",
          "continuation session is ${activeSessionActivity!!.name.lowercase()}; the wake is still in progress",
          null
        )
      }
      val held = continuingPeer
      if (held != null && held != address) {
        return ContinuationOutcome.failed(
          ContinuationStrategy.NATIVE,
          "lifecycle.invalid-state",
          "continuation session already holds $held; one peer at a time this release",
          null
        )
      }
      if (held == address) {
        if (continuingSelectors != declaration.resubscribe) {
          return ContinuationOutcome.failed(
            ContinuationStrategy.NATIVE,
            "lifecycle.invalid-state",
            "continuation session holds a different declaration; claim the pinned backlog before replacing it",
            null
          )
        }
        if (activeConsumers.size != declaration.resubscribe.size) {
          return ContinuationOutcome.failed(
            ContinuationStrategy.NATIVE,
            "lifecycle.invalid-state",
            "continuation session has a partial declaration; claim the pinned backlog before retrying it",
            null
          )
        }
        activeSession = session
        activeSessionActivity = SessionActivity.EXECUTING
        return@synchronized Execution(session, true)
      }
      activeSession = session
      activeSessionActivity = SessionActivity.EXECUTING
      Execution(session, false)
    }
    try {
      if (execution.reconcileHeldSession) {
        when (heldSessionHealthy(execution.session, address)) {
          true -> {
            log("continuation session already holds $address with an authoritative live link and subscriptions")
            return ContinuationOutcome.completed(ContinuationStrategy.NATIVE, address, declaration.resubscribe.size)
          }
          false -> {
            log("continuation session for $address is no longer live; reconnecting the pinned declaration")
            // Retain every historical selector for backlog decoding, but the
            // next authoritative reconcile must assess only the replacement
            // routes. An ended route cannot become live again by name.
            synchronized(lock) { activeConsumers = emptyList() }
          }
        }
      }
      val connected = invokeChecked(
        execution.session,
        "connection.connect",
        linkedMapOf(
          "peerId" to address,
          "lease" to CONTINUATION_LEASE,
          "operationId" to "continuation-connect",
          "intent" to "direct",
          "transport" to "auto",
          "preferredPhy" to emptyList<String>(),
          "budgetMs" to connectBudgetMs
        ),
        latchFor(connectBudgetMs)
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
        execution.session,
        "gatt.discover",
        linkedMapOf(
          "peerId" to address,
          "lease" to CONTINUATION_LEASE,
          "operationId" to "continuation-discover",
          "budgetMs" to discoverBudgetMs
        ),
        latchFor(discoverBudgetMs)
      )
      if (!discovered.ok) {
        return ContinuationOutcome.failed(
          ContinuationStrategy.NATIVE,
          discovered.code,
          discovered.reason,
          discovered.platform
        )
      }
      val consumerBase = synchronized(lock) { claimedSelectors.size }
      var resubscribed = 0
      declaration.resubscribe.forEachIndexed { index, selector ->
        val consumer = "$CONSUMER_PREFIX${consumerBase + index}"
        val subscribed = invokeChecked(
          execution.session,
          "gatt.subscribe",
          linkedMapOf(
            "peerId" to address,
            "selector" to linkedMapOf(
              "serviceUuid" to selector.serviceUuid,
              // The public GATT path is 1-based; the Rust mobile wire indexes
              // duplicate UUID occurrences from zero (its discovery facts do
              // the same). Convert exactly at this boundary.
              "serviceOccurrence" to selector.serviceOccurrence - 1L,
              "characteristicUuid" to selector.characteristicUuid,
              "characteristicOccurrence" to selector.characteristicOccurrence - 1L
            ),
            "consumer" to consumer,
            "operationId" to "continuation-subscribe-$index",
            "budgetMs" to opTimeoutMs
          ),
          latchFor(opTimeoutMs)
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
        synchronized(lock) {
          // Pin each successful consumer immediately. If a later selector
          // fails, this session can already queue values for earlier ones and
          // its eventual claim must authorize those consumer names.
          if (sessionId == execution.session) {
            continuingPeer = address
            continuingSelectors = declaration.resubscribe.toList()
            activeConsumers = activeConsumers + consumer
            claimedSelectors = claimedSelectors + selector
            subscribedConsumers = claimedSelectors.size
          }
        }
      }
      log("continuation completed for $address: connected direct, resubscribed $resubscribed")
      return ContinuationOutcome.completed(ContinuationStrategy.NATIVE, address, resubscribed)
    } catch (error: ContinuationFailure) {
      return ContinuationOutcome.failed(
        ContinuationStrategy.NATIVE,
        "platform.failure",
        error.message ?: "continuation invoke refused",
        null
      )
    } finally {
      clearActivity(execution.session, SessionActivity.EXECUTING)
    }
  }

  /**
   * A persisted replacement cannot claim success while an older session can
   * still drain records under a different selector identity.
   */
  fun declarationReplacementFailure(declaration: BackgroundContinuationDeclaration): String? = synchronized(lock) {
    if (sessionId != null && continuingSelectors != declaration.resubscribe) {
      "continuation session holds a different declaration; claim the pinned backlog before replacing it"
    } else {
      null
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
   * Establishes the Rust-owned intake cutoff, drains the sealed continuation
   * backlog (verbatim batches for the JS codec), then disposes the session.
   * The cutoff and each data admission share the Rust outbox mutex: a record
   * accepted before it is returned once; an attempted later admission is
   * reported as [ContinuationClaim.afterCutoffLoss], never silently folded
   * into the foreground handoff.
   *
   * The dispose envelope decides ownership: `released` clears the session,
   * while `release-failed` (or an unreadable dispose) keeps the id with the
   * failure in [ContinuationClaim.disposeFailure], so the next claim retries
   * the dispose instead of abandoning a live session with no owner. An
   * incomplete drain (a drain throw, an unparseable batch, or a full batch
   * cap with `more` still queued) likewise keeps the session: disposing now
   * would discard the unread tail silently.
   */
  fun prepareClaim(maxItems: Int, maxBytes: Int, maxBatches: Int = 32): ContinuationClaim {
    synchronized(lock) {
      preparedClaim?.let { prepared ->
        return if (prepared.acknowledged) prepared.claim.copy(batches = emptyList()) else prepared.claim
      }
    }
    val (session, consumerCount, selectors) = synchronized(lock) {
      val current = sessionId ?: return ContinuationClaim(0, emptyList(), false)
      val currentConsumerCount = subscribedConsumers
      val currentSelectors = claimedSelectors.toList()
      if (activeSession == current) {
        return ContinuationClaim(
          currentConsumerCount,
          emptyList(),
          false,
          "continuation session is ${activeSessionActivity!!.name.lowercase()}; it is kept for a follow-up claim"
        )
      }
      activeSession = current
      activeSessionActivity = SessionActivity.CLAIMING
      Triple(current, currentConsumerCount, currentSelectors)
    }
    try {
      val cutoff = try {
        quiesce(session)
      } catch (error: ContinuationFailure) {
        val failure = "continuation quiesce failed: ${error.message}"
        log(failure)
        return ContinuationClaim(consumerCount, emptyList(), false, failure, selectors = selectors)
      }
      val batches = ArrayList<String>(4)
      var more = true
      var rounds = 0
      var drainFailure: String? = null
      while (more && rounds < maxBatches) {
        rounds += 1
        val batch = try {
          core.drain(session, maxItems, maxBytes)
        } catch (error: RuntimeException) {
          drainFailure = "continuation drain failed: ${error.message ?: error.javaClass.simpleName}"
          log(drainFailure)
          break
        }
        more = try {
          val completion = parseDrainCompletion(batch)
          // Return only a prefix the TypeScript drain codec can consume.
          // A malformed following batch remains native-side failure state;
          // including it here would make the valid prefix unusable.
          batches.add(batch)
          completion
        } catch (error: ContinuationFailure) {
          drainFailure = "continuation drain batch malformed: ${error.message}"
          log(drainFailure)
          false
        }
      }
      if (drainFailure == null && more) {
        drainFailure = "continuation claim stopped after $rounds batches with more queued; the session is kept for a follow-up claim"
        log(drainFailure)
      }
      if (drainFailure != null) {
        // `drain` has already consumed every valid prefix batch. Keep that
        // prefix behind a token so JS can decode and acknowledge it before a
        // later claim advances to the unread tail. Returning it tokenless
        // would make a valid prefix neither deliverable nor replayable.
        synchronized(lock) {
          val token = "continuation-${admission.incrementAndGet()}"
          val prepared = ContinuationClaim(
            consumerCount,
            batches,
            false,
            drainFailure,
            afterCutoffLoss = cutoff,
            selectors = selectors,
            claimToken = token
          )
          preparedClaim = PreparedClaim(token, prepared, cleanupEligible = false)
          return prepared
        }
      }
      synchronized(lock) {
        val token = "continuation-${admission.incrementAndGet()}"
        val prepared = ContinuationClaim(
          consumerCount,
          batches,
          false,
          null,
          cutoff,
          selectors,
          token
        )
        preparedClaim = PreparedClaim(token, prepared, cleanupEligible = true)
        return prepared
      }
    } finally {
      clearActivity(session, SessionActivity.CLAIMING)
    }
  }

  /** A successful TypeScript decode is the only authority that permits continuation cleanup. */
  fun acknowledgeClaim(token: String): ContinuationAcknowledgement {
    val (session, prepared) = synchronized(lock) {
      val current = preparedClaim ?: throw ContinuationFailure("no prepared continuation claim")
      if (current.token != token) throw ContinuationFailure("continuation claim token does not match the prepared handoff")
      current.acknowledgement?.let { return it }
      val currentSession = sessionId ?: throw ContinuationFailure("prepared continuation session is gone")
      if (activeSession == currentSession) throw ContinuationFailure("continuation session is ${activeSessionActivity!!.name.lowercase()}")
      activeSession = currentSession
      activeSessionActivity = SessionActivity.CLAIMING
      current.acknowledged = true
      Pair(currentSession, current)
    }
    try {
      if (!prepared.cleanupEligible) {
        // JS has validated the prefix, so it is safe to advance past it. Do
        // not dispose: the sealed session still owns the unread tail. The
        // explicit failure remains part of the acknowledgement so callers
        // know to issue the next claim.
        synchronized(lock) {
          if (preparedClaim === prepared) preparedClaim = null
        }
        return ContinuationAcknowledgement(false, prepared.claim.afterCutoffLoss, prepared.claim.disposeFailure)
      }
      val disposal = try {
        continuationDispose(session)
      } catch (error: ContinuationFailure) {
        ContinuationDisposal("continuation dispose failed: ${error.message}", prepared.claim.afterCutoffLoss)
      }
      if (disposal.failure != null) {
        log(disposal.failure)
        return ContinuationAcknowledgement(false, disposal.afterCutoffLoss, disposal.failure)
      }
      val acknowledgement = ContinuationAcknowledgement(true, disposal.afterCutoffLoss, null)
      synchronized(lock) {
        if (sessionId == session) {
          sessionId = null
          continuingPeer = null
          continuingSelectors = emptyList()
          activeConsumers = emptyList()
          claimedSelectors = emptyList()
          subscribedConsumers = 0
          // Keep the one acknowledged receipt after releasing the native
          // session. If the JS promise is lost or its result is malformed,
          // the next prepare/ack can obtain this terminal answer without
          // replaying batches that JS has already decoded.
          prepared.acknowledgement = acknowledgement
        }
      }
      return acknowledgement
    } finally {
      clearActivity(session, SessionActivity.CLAIMING)
    }
  }

  /** JVM-only convenience for lifecycle tests; the exported native surface uses prepare then acknowledge. */
  fun claimAndDispose(maxItems: Int, maxBytes: Int, maxBatches: Int = 32): ContinuationClaim {
    val prepared = prepareClaim(maxItems, maxBytes, maxBatches)
    if (prepared.claimToken.isEmpty()) return prepared
    val acknowledgement = acknowledgeClaim(prepared.claimToken)
    return prepared.copy(
      batches = prepared.batches,
      disposed = acknowledgement.disposed,
      disposeFailure = acknowledgement.disposeFailure,
      afterCutoffLoss = acknowledgement.afterCutoffLoss
    )
  }

  /** `session.quiesce` is idempotent and establishes the native intake cutoff. */
  private fun quiesce(session: Long): CutoffLoss {
    val checked = checkEnvelope("session.quiesce", invoke(session, "session.quiesce", emptyMap(), opTimeoutMs))
    if (!checked.ok) throw ContinuationFailure("${checked.code} ${checked.reason}")
    if (checked.value["state"] != "sealed") throw ContinuationFailure("session.quiesce did not report sealed")
    return cutoffLoss("session.quiesce", checked.value)
  }

  /** Cleanup after the cutoff includes every attempted post-cutoff admission. */
  private fun continuationDispose(session: Long): ContinuationDisposal {
    val checked = checkEnvelope(
      "session.continuation-dispose",
      invoke(session, "session.continuation-dispose", emptyMap(), opTimeoutMs)
    )
    if (!checked.ok) {
      if (checked.code == "lifecycle.destroyed") return ContinuationDisposal(null, CutoffLoss(0, 0))
      return ContinuationDisposal(
        "session.continuation-dispose refused: ${checked.code} ${checked.reason}",
        CutoffLoss(0, 0)
      )
    }
    val loss = cutoffLoss("session.continuation-dispose", checked.value)
    return if (checked.value["state"] == "released") {
      ContinuationDisposal(null, loss)
    } else {
      ContinuationDisposal(
        "session.continuation-dispose reported ${checked.value["state"] ?: "release-failed"}; " +
          "the session is kept for a retry: ${RustCoreJson.write(checked.value)}",
        loss
      )
    }
  }

  /** Strict enough to decide whether destructive cleanup may run. */
  private fun parseDrainCompletion(batch: String): Boolean {
    val root = try {
      RustCoreJson.parse(batch) as? Map<*, *>
        ?: throw ContinuationFailure("root is not an object")
    } catch (error: IllegalArgumentException) {
      throw ContinuationFailure("JSON is unparseable: ${error.message}")
    }
    if (root.keys.any { it !in setOf("more", "records", "controlLost") }) {
      throw ContinuationFailure("root contains unknown fields")
    }
    val more = root["more"]
    if (more !is Boolean) throw ContinuationFailure("more is missing or not boolean")
    if (root["records"] !is List<*>) throw ContinuationFailure("records is missing or not an array")
    val controlLost = root["controlLost"]
    if (controlLost !is Number || controlLost.toLong() < 0) {
      throw ContinuationFailure("controlLost is missing, negative, or not numeric")
    }
    return more
  }

  private fun cutoffLoss(operation: String, value: Map<*, *>): CutoffLoss {
    val items = (value["afterCutoffItems"] as? Number)?.toLong()
    val bytes = (value["afterCutoffBytes"] as? Number)?.toLong()
    if (items == null || bytes == null || items < 0 || bytes < 0) {
      throw ContinuationFailure("$operation did not report non-negative after-cutoff loss counts")
    }
    return CutoffLoss(items, bytes)
  }

  private data class ContinuationDisposal(val failure: String?, val afterCutoffLoss: CutoffLoss)

  private fun clearActivity(session: Long, activity: SessionActivity) {
    synchronized(lock) {
      if (activeSession == session && activeSessionActivity == activity) {
        activeSession = null
        activeSessionActivity = null
      }
    }
  }

  /**
   * `session.reconcile` is the core's authoritative owner snapshot. A cached
   * successful wake is reusable only while this exact peer has a current
   * link and every active continuation consumer still has a live route.
   */
  private fun heldSessionHealthy(session: Long, address: String): Boolean {
    // `session.reconcile` is a state read and intentionally has no operation
    // id/admission on the Rust wire.
    val checked = checkEnvelope("session.reconcile", invoke(session, "session.reconcile", emptyMap(), opTimeoutMs))
    if (!checked.ok) return false
    val links = checked.value["links"] as? List<*> ?: return false
    val connected = links.any { entry ->
      val link = entry as? Map<*, *> ?: return@any false
      link["peerId"] == address && link["state"] == "connected" && link["databaseState"] == "current"
    }
    if (!connected) return false
    val subscriptions = checked.value["subscriptions"] as? List<*> ?: return false
    return activeConsumers.all { consumer ->
      subscriptions.any { entry ->
        val subscription = entry as? Map<*, *> ?: return@any false
        subscription["consumer"] == consumer && subscription["state"] == "live"
      }
    }
  }

  /**
   * Reads the `session.dispose` envelope. Null when the lease is gone
   * (`released`, or `lifecycle.destroyed` for a session the core already
   * forgot); otherwise why the session is kept for a retry.
   */
  private fun disposeFailure(root: Map<*, *>): String? {
    val checked = checkEnvelope("session.dispose", root)
    if (checked.ok) {
      if (checked.value["state"] == "released") return null
      return "session.dispose reported ${checked.value["state"] ?: "release-failed"}; " +
        "the session is kept for a retry: ${RustCoreJson.write(checked.value)}"
    }
    if (checked.code == "lifecycle.destroyed") return null
    return "session.dispose refused: ${checked.code} ${checked.reason}"
  }

  private fun ensureSession(): Long {
    sessionId?.let { return it }
    // A terminal receipt belongs to the just-finished session. Opening a new
    // continuation begins a distinct handoff and may replace that one bounded
    // receipt; incomplete prepared claims always still have a live session.
    if (preparedClaim?.acknowledgement != null) preparedClaim = null
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
    admission.set(0)
    sessionId = id
    return id
  }

  private data class Checked(val ok: Boolean, val code: String, val reason: String, val platform: String?, val value: Map<*, *>)

  /**
   * The client latch for an op budgeted at [budgetMs]: the budget itself
   * plus one generic hop margin, so the core always answers first and the
   * outcome reports what the operation did, never the latch firing early.
   */
  private fun latchFor(budgetMs: Long) = budgetMs + opTimeoutMs

  private fun invokeChecked(session: Long, op: String, args: Map<String, Any?>, timeoutMs: Long): Checked {
    val withAdmission = LinkedHashMap<String, Any?>(args)
    withAdmission["admission"] = admission.incrementAndGet()
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
    /**
     * Finding 242: the discovery budget the wake gives the core. 20 s is
     * the bound the foreground proves sufficient for this peer's database
     * (the shared driver discovers with `OPERATION_TIMEOUT_MS = 20_000`);
     * the old 10 s client latch contradicted the core's own 120 s window,
     * so it could only ever fabricate the outcome.
     */
    const val DISCOVER_BUDGET_MS = 20_000L
  }
}

/** A refused invoke that becomes a typed outcome (never a silent nothing). */
private class ContinuationFailure(message: String) : RuntimeException(message)

/** Queued-backlog proof without consuming it. */
data class BacklogCounts(val queuedBytes: Long?, val ingressDrops: Map<String, Long>)

/**
 * Verbatim drain batches for the JS codec plus the consumer count captured
 * from that exact session and whether the session was disposed. Empty batches
 * with `disposed: false`, [consumerCount] zero, and no [disposeFailure] is
 * the valid no-wake answer (no continuation session alive) — never an
 * error. A non-null [disposeFailure] is why the session is still alive: the
 * drain did not complete or the dispose reported failures, so the next
 * claim retries instead of abandoning the session with no owner.
 */
data class ContinuationClaim(
  /** Number of consumers subscribed by this exact session, captured atomically before claim. */
  val consumerCount: Int,
  val batches: List<String>,
  val disposed: Boolean,
  val disposeFailure: String? = null,
  /** Native intake attempts observed after the sealed handoff cutoff. */
  val afterCutoffLoss: CutoffLoss = CutoffLoss(0, 0),
  /** Immutable selector identity for every numeric consumer this session can still drain. */
  val selectors: List<ContinuationSelector> = emptyList(),
  /** Opaque native claim token, acknowledged only after TypeScript validates these batches. */
  val claimToken: String = ""
)

/** Native cleanup result after TypeScript acknowledged a prepared continuation claim. */
data class ContinuationAcknowledgement(
  val disposed: Boolean,
  val afterCutoffLoss: CutoffLoss,
  val disposeFailure: String?
)

/** A post-cutoff intake was observed by the native process and loss-accounted. */
data class CutoffLoss(val items: Long, val bytes: Long)
