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
  private var ownerInstalled = false
  private val order = mutableListOf<String>()
  private val logs = mutableListOf<String>()
  private var ingestRefused = false
  private var ownerThrows = false
  private var ingestThrows = false

  private val coordinator = PresenceWakeCoordinator(
    associatedAddresses = { setOf(peer) },
    store = store,
    nowMs = { 12345L },
    ensureOwner = {
      order.add("ensureOwner")
      if (ownerThrows) throw IllegalStateException("no radio")
      if (!hostAlive) return@PresenceWakeCoordinator false
      ownerInstalled = true
      true
    },
    ingest = { peers ->
      order.add("ingest")
      if (ingestThrows) throw IllegalStateException("core gone")
      if (!hostAlive || ingestRefused) return@PresenceWakeCoordinator false
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
  fun aColdStartAppearanceInstallsTheOwnerBeforeIngesting() {
    coordinator.appeared(peer, null)
    assertEquals(listOf("ensureOwner", "ingest"), order)
    assertTrue(ownerInstalled)
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
  fun anIngestRefusalPersistsTheAppearance() {
    ingestRefused = true
    coordinator.appeared(peer, null)
    assertTrue(ingested.isEmpty())
    assertEquals(listOf(PresenceAppearance(peer, null, 12345L)), store.drainAppearances())
  }

  @Test
  fun aThrowingOwnerPersistsTheAppearanceAndLogs() {
    ownerThrows = true
    coordinator.appeared(peer, null)
    assertTrue(ingested.isEmpty())
    assertEquals(listOf(PresenceAppearance(peer, null, 12345L)), store.drainAppearances())
    assertTrue(logs.isNotEmpty())
  }

  @Test
  fun aThrowingIngestPersistsTheAppearanceAndLogs() {
    ingestThrows = true
    coordinator.appeared(peer, null)
    assertTrue(ingested.isEmpty())
    assertEquals(listOf(PresenceAppearance(peer, null, 12345L)), store.drainAppearances())
    assertTrue(logs.isNotEmpty())
  }

  @Test
  fun anAppearanceOfAnUnassociatedPeerIsIgnoredAndLogged() {
    coordinator.appeared(stranger, null)
    assertTrue(order.isEmpty())
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
