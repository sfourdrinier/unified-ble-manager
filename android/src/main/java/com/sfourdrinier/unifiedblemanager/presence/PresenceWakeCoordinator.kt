// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/PresenceWakeCoordinator.kt

package com.sfourdrinier.unifiedblemanager.presence

/** One known peer the OS handed back through device presence. */
data class PresenceRestoredPeer(val peerId: String, val name: String?, val connected: Boolean)

/**
 * Routes Companion Device Manager presence callbacks (issue #212). Only an
 * address the app associated is ours — anything else is logged, recorded as
 * a rejected wake, and never scanned for. An appearance installs the process
 * owner when the OS woke a dead process ([ensureOwner]), then ingests into
 * it; only an appearance no owner takes persists in the
 * [PresenceRestoredStore] for exactly-once drain at the next session open.
 * A disappearance clears a persisted appearance that never reached a
 * session. The link itself is established later with no scan: the app-owned
 * reconnect uses the ordinary `when-available` connect to the known address,
 * while the `native` standing order connects `direct` (finding 242: the wake
 * fires on presence, so the peer is already there) — this coordinator never
 * connects and never resubscribes; the declared standing order
 * ([continuation]) may. `record-only` is this doc comment's behaviour;
 * `native` reconnects the declared known peer and resubscribes the declared
 * characteristics through [executeContinuation] (the Rust core, no
 * JavaScript). Android task/service strategies dispatch their declared
 * mechanism and report its acceptance stage, never app-task completion.
 */
class PresenceWakeCoordinator(
  private val associatedAddresses: () -> Set<String>,
  private val store: PresenceRestoredStore,
  private val nowMs: () -> Long,
  /**
   * Addresses delivered since their last disappearance. The OS hands one
   * appearance per association (finding 236: two associations for one
   * strap deliver two appearances for one physical event), so a repeat
   * appearance with no disappearance between is the same event, not a new
   * one. Callbacks arrive on the system service thread, so every access
   * shares the coordinator lock.
   */
  private val delivered: MutableSet<String> = mutableSetOf(),
  /**
   * Installs the process radio owner when the OS woke a dead process.
   * Returns false when no owner is alive to take the appearance (it stays
   * persisted). A throwing owner is logged and treated as no owner, so the
   * appearance is still persisted, never dropped.
   */
  private val ensureOwner: () -> Boolean,
  /**
   * Ingests restored peers into the live owner. Returns false when the
   * owner refused them (the appearance stays persisted).
   */
  private val ingest: (List<PresenceRestoredPeer>) -> Boolean,
  private val log: (String) -> Unit,
  /**
   * The declared standing order. Defaults to `record-only`. A throwing
   * supplier is logged and treated as `record-only`, so the wake still
   * records the peer instead of dropping the appearance.
   */
  private val continuation: () -> BackgroundContinuationDeclaration =
    { BackgroundContinuationDeclaration.recordOnly() },
  /**
   * Executes the `native` standing order through the Rust core without
   * JavaScript. The default answers `capability.unsupported` (no executor
   * wired); production wires the host-owned continuation session.
   */
  private val executeContinuation: (
    address: String,
    declaration: BackgroundContinuationDeclaration
  ) -> ContinuationOutcome = { _, declaration ->
    ContinuationOutcome.failed(
      declaration.strategy,
      "capability.unsupported",
      "no native continuation executor in this context",
      null
    )
  },
  /** Records the wake outcome for Diagnostics (and unsupported states). */
  private val recordWakeOutcome: (ContinuationWakeRecord) -> Unit = {},
  private val releaseContinuation: (String) -> ContinuationOutcome.Failed? = { null }
) {
  private val modernSources = mutableMapOf<Int, Pair<String, MutableSet<Int>>>()
  private val deliveryEpochs = mutableMapOf<String, Any>()
  private var admissionRevision = 0L
  private val retiredAt = mutableMapOf<String, Long>()
  private val stoppedObservations = mutableSetOf<String>()
  @Synchronized fun admissionTicket(): Long = admissionRevision
  @Synchronized fun acceptsTicket(address: String, ticket: Long): Boolean =
    !stoppedObservations.contains(address.uppercase()) && (retiredAt[address.uppercase()] ?: 0L) <= ticket
  @Synchronized fun observationStarted(address: String) { stoppedObservations.remove(address.uppercase()) }

  /** OS observation has stopped. Fence pending bootstrap before queued cleanup. */
  fun retireObservation(address: String) {
    val peer = address.uppercase()
    synchronized(this) {
      admissionRevision += 1
      retiredAt[peer] = admissionRevision
      stoppedObservations.add(peer)
      retireDelivery(peer)
    }
  }

  fun clearRetiredAppearance(address: String) = store.removeAppearance(address.uppercase())

  @Synchronized private fun retireDelivery(peer: String) {
    modernSources.entries.removeAll { it.value.first == peer }
    delivered.remove(peer)
    deliveryEpochs.remove(peer)
  }

  /** Source identity is retained alongside process-owned wake/lease identity. */
  fun presenceEvent(address: String, associationId: Int, source: Int, present: Boolean) {
    val peer = address.uppercase()
    val ticket = admissionTicket()
    if (!acceptsTicket(peer, ticket)) return
    require(source in 0..2) { "Unsupported presence source" }
    if (associatedAddresses().none { it.equals(peer, ignoreCase = true) }) {
      log("presence source event for unassociated device ignored")
      recordWakeOutcome(unassociatedRecord(peer))
      return
    }
    val transition = synchronized(this) {
      if (!acceptsTicket(peer, ticket)) return
      val before = modernSources.values.any { it.first == peer && it.second.isNotEmpty() }
      val previous = modernSources[associationId]
      require(previous == null || previous.first == peer) { "Presence association changed its peer identity" }
      if (present) {
        val entry = previous ?: (peer to mutableSetOf<Int>()).also { modernSources[associationId] = it }
        entry.second.add(source)
      } else if (previous != null) {
        previous.second.remove(source)
        if (previous.second.isEmpty()) modernSources.remove(associationId)
      }
      val after = modernSources.values.any { it.first == peer && it.second.isNotEmpty() }
      when { !before && after -> 1; before && !after -> -1; else -> 0 }
    }
    if (transition == 1) appeared(peer, associationId)
    else if (transition == -1) disappeared(peer, associationId)
  }

  /**
   * Routes one appearance callback. Returns true only when the wake reached
   * the owner (ingest accepted); duplicates, unassociated devices and
   * appearances persisted for a later session all report false, so the
   * caller can log the outcome it actually produced.
   *
   * The coordinator lock guards state (the delivered set), never I/O: owner
   * bootstrap, ingest, and the standing-order execution all run outside it,
   * so a disappearance or teardown never queues behind radio I/O. Every wake
   * except an exact duplicate records its outcome — including rejections —
   * so `status.lastWake` distinguishes "never woken" (null) from "woken and
   * rejected".
   */
  fun appeared(address: String, associationId: Int?): Boolean {
    // MAC addresses are case-insensitive hex; normalize before comparing,
    // tracking and persisting, so a differently-cased twin of an associated
    // device never reads as unassociated and never double-delivers.
    val normalized = address.uppercase()
    val ticket = admissionTicket()
    if (!acceptsTicket(normalized, ticket)) return false
    if (associatedAddresses().map { it.uppercase() }.toSet().contains(normalized).not()) {
      log("presence appearance for unassociated device ignored")
      recordWakeOutcome(unassociatedRecord(normalized))
      return false
    }
    val epoch = synchronized(this) {
      if (!acceptsTicket(normalized, ticket)) return false
      if (delivered.add(normalized)) Any().also { deliveryEpochs[normalized] = it } else null
    }
    if (epoch == null) {
      log("presence duplicate appearance for $normalized ignored (associationId=${associationId ?: "none"}): no disappearance since the last delivery")
      return false
    }
    fun current(): Boolean = synchronized(this) { deliveryEpochs[normalized] === epoch }
    val owned = try {
      ensureOwner()
    } catch (error: RuntimeException) {
      log("presence owner bootstrap failed; appearance persisted: ${error.message ?: error.javaClass.simpleName}")
      false
    }
    var delivered = false
    if (!current()) return false
    if (owned) {
      try {
        if (ingest(listOf(PresenceRestoredPeer(normalized, null, false)))) delivered = true
      } catch (error: RuntimeException) {
        log("presence ingest failed; appearance persisted: ${error.message ?: error.javaClass.simpleName}")
      }
    } else {
      log("presence appearance persisted for the next session open: no live owner")
    }
    if (!current()) return false
    executeStandingOrder(normalized, owned, readDeclaration())
    if (!current()) {
      releaseContinuation(normalized)?.let { recordWakeOutcome(wakeRecord(normalized, it)) }
      return false
    }
    synchronized(this) {
      if (deliveryEpochs[normalized] !== epoch) return false
      if (!delivered) {
        store.saveAppearance(normalized, associationId, nowMs())
        return false
      }
    }
    return true
  }

  /** Reads the declared standing order; an unreadable one is `record-only`, never a dropped wake. */
  private fun readDeclaration(): BackgroundContinuationDeclaration {
    return try {
      continuation()
    } catch (error: RuntimeException) {
      log("presence continuation unreadable, using record-only: ${error.message ?: error.javaClass.simpleName}")
      BackgroundContinuationDeclaration.recordOnly()
    }
  }

  /** The rejection record for an appearance no association owns. */
  private fun unassociatedRecord(address: String): ContinuationWakeRecord {
    val declaration = readDeclaration()
    return ContinuationWakeRecord(
      observedAtMs = nowMs(),
      event = "continuation.failed",
      strategy = declaration.strategy,
      peerAddress = address,
      code = "association.unknown",
      reason = "presence appearance for unassociated device ignored; only an address the app associated wakes the process"
    )
  }

  /**
   * Executes the declared standing order after the record-only ingest. Only
   * what was declared runs: `native` reconnects (scoped to the declared peer
   * when one is named); task/service strategies use their matching platform
   * executor. Nothing here invents or silently substitutes a strategy.
   */
  private fun executeStandingOrder(address: String, owned: Boolean, declaration: BackgroundContinuationDeclaration) {
    if (declaration.strategy == ContinuationStrategy.RECORD_ONLY) return
    if (!owned) {
      recordWakeOutcome(
        ContinuationWakeRecord(
          observedAtMs = nowMs(),
          event = "continuation.failed",
          strategy = declaration.strategy,
          peerAddress = address,
          code = "operation.aborted",
          reason = "standing order not executed: no live owner; appearance persisted for the next session open"
        )
      )
      return
    }
    when (declaration.strategy) {
      ContinuationStrategy.NATIVE, ContinuationStrategy.HEADLESS_TASK, ContinuationStrategy.FOREGROUND_SERVICE -> {
        if (declaration.peerId != null && declaration.peerId != address) {
          log("presence native continuation skips $address: standing order scopes to ${declaration.peerId}")
          recordWakeOutcome(
            ContinuationWakeRecord(
              observedAtMs = nowMs(),
              event = "continuation.failed",
              strategy = declaration.strategy,
              peerAddress = address,
              code = "operation.aborted",
              reason = "presence native continuation skips $address: standing order scopes to ${declaration.peerId}"
            )
          )
          return
        }
        val outcome = try {
          executeContinuation(address, declaration)
        } catch (error: RuntimeException) {
          log("presence native continuation failed: ${error.message ?: error.javaClass.simpleName}")
          ContinuationOutcome.failed(
            declaration.strategy,
            "lifecycle.invariant-violation",
            "continuation executor threw: ${error.message ?: error.javaClass.simpleName}",
            null
          )
        }
        recordWakeOutcome(wakeRecord(address, outcome))
      }
      ContinuationStrategy.RECORD_ONLY -> Unit
    }
  }

  private fun wakeRecord(address: String, outcome: ContinuationOutcome): ContinuationWakeRecord {
    return when (outcome) {
      is ContinuationOutcome.Completed -> ContinuationWakeRecord(
        observedAtMs = nowMs(),
        event = outcome.event,
        strategy = outcome.strategy,
        peerAddress = address,
        code = null,
        reason = null,
        stage = outcome.stage
      )
      is ContinuationOutcome.Failed -> ContinuationWakeRecord(
        observedAtMs = nowMs(),
        event = outcome.event,
        strategy = outcome.strategy,
        peerAddress = address,
        code = outcome.code,
        reason = outcome.reason,
        platform = outcome.platform
      )
    }
  }

  fun disappeared(address: String, associationId: Int?) {
    val normalized = address.uppercase()
    retireDelivery(normalized)
    store.removeAppearance(normalized)
    releaseContinuation(normalized)?.let { recordWakeOutcome(wakeRecord(normalized, it)) }
  }
}
