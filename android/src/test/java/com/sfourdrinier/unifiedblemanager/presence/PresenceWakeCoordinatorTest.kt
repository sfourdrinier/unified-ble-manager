// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/PresenceWakeCoordinatorTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** Presence wake routing (issue #212) without the system or the radio. */
class PresenceWakeCoordinatorTest {
  private val peer = "AA:BB:CC:DD:EE:FF"
  private val stranger = "11:22:33:44:55:66"
  private val store = InMemoryPresenceStore()
  private val ingested = mutableListOf<PresenceRestoredPeer>()
  private var hostAlive = true
  private val logs = mutableListOf<String>()

  private val coordinator = PresenceWakeCoordinator(
    associatedAddresses = { setOf(peer) },
    store = store,
    nowMs = { 12345L },
    ingest = { peers ->
      if (!hostAlive) return@PresenceWakeCoordinator false
      ingested.addAll(peers)
      true
    },
    log = { logs.add(it) }
  )

  @Test
  fun anAppearanceOfAnAssociatedPeerIsRecordedAndIngested() {
    coordinator.appeared(peer, null)
    assertEquals(listOf(PresenceRestoredPeer(peer, null, false)), ingested)
    assertTrue(store.drainAppearances().isEmpty())
  }

  @Test
  fun anAppearanceWithNoLiveHostPersistsForTheNextSessionOpen() {
    hostAlive = false
    coordinator.appeared(peer, null)
    assertTrue(ingested.isEmpty())
    val pending = store.drainAppearances()
    assertEquals(listOf(PresenceAppearance(peer, null, 12345L)), pending)
  }

  @Test
  fun anAppearanceOfAnUnassociatedPeerIsIgnoredAndLogged() {
    coordinator.appeared(stranger, null)
    assertTrue(ingested.isEmpty())
    assertTrue(store.drainAppearances().isEmpty())
    assertTrue(logs.isNotEmpty())
  }

  @Test
  fun aDisappearanceClearsAPersistedAppearance() {
    hostAlive = false
    coordinator.appeared(peer, null)
    coordinator.disappeared(peer, null)
    assertTrue(store.drainAppearances().isEmpty())
  }
}
