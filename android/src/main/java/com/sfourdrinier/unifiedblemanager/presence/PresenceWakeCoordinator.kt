// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/PresenceWakeCoordinator.kt

package com.sfourdrinier.unifiedblemanager.presence

/** One known peer the OS handed back through device presence. */
data class PresenceRestoredPeer(val peerId: String, val name: String?, val connected: Boolean)

/**
 * Routes Companion Device Manager presence callbacks (issue #212). Only an
 * address the app associated is ours — anything else is logged and ignored,
 * never recorded and never scanned for. An appearance installs the process
 * owner when the OS woke a dead process ([ensureOwner]), then ingests into
 * it; only an appearance no owner takes persists in the
 * [PresenceRestoredStore] for exactly-once drain at the next session open.
 * A disappearance clears a persisted appearance that never reached a
 * session. The link itself is established later through the ordinary
 * `when-available` connect: direct to the known address, no scan — this
 * coordinator never connects and never resubscribes.
 */
class PresenceWakeCoordinator(
  private val associatedAddresses: () -> Set<String>,
  private val store: PresenceRestoredStore,
  private val nowMs: () -> Long,
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
  private val log: (String) -> Unit
) {
  fun appeared(address: String, associationId: Int?) {
    if (!associatedAddresses().contains(address)) {
      log("presence appearance for unassociated device ignored")
      return
    }
    val owned = try {
      ensureOwner()
    } catch (error: RuntimeException) {
      log("presence owner bootstrap failed; appearance persisted: ${error.message ?: error.javaClass.simpleName}")
      false
    }
    if (owned) {
      try {
        if (ingest(listOf(PresenceRestoredPeer(address, null, false)))) return
      } catch (error: RuntimeException) {
        log("presence ingest failed; appearance persisted: ${error.message ?: error.javaClass.simpleName}")
      }
    } else {
      log("presence appearance persisted for the next session open: no live owner")
    }
    store.saveAppearance(address, associationId, nowMs())
  }

  fun disappeared(address: String, associationId: Int?) {
    store.removeAppearance(address)
  }
}
