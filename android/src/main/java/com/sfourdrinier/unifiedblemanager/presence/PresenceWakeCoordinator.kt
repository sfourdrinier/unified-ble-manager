// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/PresenceWakeCoordinator.kt

package com.sfourdrinier.unifiedblemanager.presence

/** One known peer the OS handed back through device presence. */
data class PresenceRestoredPeer(val peerId: String, val name: String?, val connected: Boolean)

/**
 * Routes Companion Device Manager presence callbacks (issue #212). Only an
 * address the app associated is ours — anything else is logged and ignored,
 * never recorded and never scanned for. An appearance is ingested into the
 * live owner when one exists; with no live session it persists in the
 * [PresenceRestoredStore] for exactly-once drain at the next session open.
 * A disappearance clears a persisted appearance that never reached a
 * session. The link itself is established later through the ordinary
 * `when-available` connect: direct to the known address, no scan.
 */
class PresenceWakeCoordinator(
  private val associatedAddresses: () -> Set<String>,
  private val store: PresenceRestoredStore,
  private val nowMs: () -> Long,
  /**
   * Ingests restored peers into the live owner. Returns false when no owner
   * is alive to take them (the appearance stays persisted).
   */
  private val ingest: (List<PresenceRestoredPeer>) -> Boolean,
  private val log: (String) -> Unit
) {
  fun appeared(address: String, associationId: Int?) {
    if (!associatedAddresses().contains(address)) {
      log("presence appearance for unassociated device ignored")
      return
    }
    if (!ingest(listOf(PresenceRestoredPeer(address, null, false)))) {
      store.saveAppearance(address, associationId, nowMs())
    }
  }

  fun disappeared(address: String, associationId: Int?) {
    store.removeAppearance(address)
  }
}
