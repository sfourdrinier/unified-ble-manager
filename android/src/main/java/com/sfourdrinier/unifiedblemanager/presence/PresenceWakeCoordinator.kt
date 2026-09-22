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
 * JavaScript). Deferred strategies (`headless-task`, `foreground-service`)
 * record `capability.unsupported` ("not implemented in this release") and
 * never fall back to another strategy silently.
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
  private val recordWakeOutcome: (ContinuationWakeRecord) -> Unit = {}
) {
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
    if (associatedAddresses().map { it.uppercase() }.toSet().contains(normalized).not()) {
      log("presence appearance for unassociated device ignored")
      recordWakeOutcome(unassociatedRecord(normalized))
      return false
    }
    val firstDelivery = synchronized(this) { delivered.add(normalized) }
    if (!firstDelivery) {
      log("presence duplicate appearance for $normalized ignored (associationId=${associationId ?: "none"}): no disappearance since the last delivery")
      return false
    }
    val owned = try {
      ensureOwner()
    } catch (error: RuntimeException) {
      log("presence owner bootstrap failed; appearance persisted: ${error.message ?: error.javaClass.simpleName}")
      false
    }
    var delivered = false
    if (owned) {
      try {
        if (ingest(listOf(PresenceRestoredPeer(normalized, null, false)))) delivered = true
      } catch (error: RuntimeException) {
        log("presence ingest failed; appearance persisted: ${error.message ?: error.javaClass.simpleName}")
      }
    } else {
      log("presence appearance persisted for the next session open: no live owner")
    }
    executeStandingOrder(normalized, owned, readDeclaration())
    if (!delivered) {
      store.saveAppearance(normalized, associationId, nowMs())
      return false
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
   * when one is named) while the deferred strategies record their
   * `capability.unsupported` refusal. Nothing here invents a strategy.
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
      ContinuationStrategy.NATIVE -> {
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
            ContinuationStrategy.NATIVE,
            "lifecycle.invariant-violation",
            "continuation executor threw: ${error.message ?: error.javaClass.simpleName}",
            null
          )
        }
        recordWakeOutcome(wakeRecord(address, outcome))
      }
      ContinuationStrategy.HEADLESS_TASK, ContinuationStrategy.FOREGROUND_SERVICE -> {
        val name = declaration.strategy.wire
        log("presence $name continuation not executed: not implemented in this release")
        recordWakeOutcome(
          ContinuationWakeRecord(
            observedAtMs = nowMs(),
            event = "continuation.failed",
            strategy = declaration.strategy,
            peerAddress = address,
            code = "capability.unsupported",
            reason = "$name continuation is not implemented in this release"
          )
        )
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
        reason = null
      )
      is ContinuationOutcome.Failed -> ContinuationWakeRecord(
        observedAtMs = nowMs(),
        event = outcome.event,
        strategy = outcome.strategy,
        peerAddress = address,
        code = outcome.code,
        reason = outcome.reason
      )
    }
  }

  @Synchronized
  fun disappeared(address: String, associationId: Int?) {
    delivered.remove(address.uppercase())
    store.removeAppearance(address.uppercase())
  }
}
