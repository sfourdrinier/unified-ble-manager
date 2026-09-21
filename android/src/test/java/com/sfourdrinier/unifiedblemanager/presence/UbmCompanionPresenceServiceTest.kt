// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/UbmCompanionPresenceServiceTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.companion.AssociationInfo
import android.net.MacAddress
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Test
import org.mockito.Mockito.mock
import org.mockito.Mockito.`when`

/**
 * Finding 236: the OS may dispatch an appearance as `AssociationInfo`
 * (API 33+, with the real association id) or as a bare address. Both
 * overloads must reach the coordinator with the device address — otherwise
 * the wake never runs — and a second delivery of the same event must be
 * the coordinator's logged duplicate, not a second wake.
 */
class UbmCompanionPresenceServiceTest {
  private val peer = "A0:9E:1A:E9:B9:3D"
  private val store = InMemoryPresenceStore()
  private val ingested = mutableListOf<PresenceRestoredPeer>()
  private lateinit var coordinator: PresenceWakeCoordinator

  @Before
  fun setUp() {
    coordinator = PresenceWakeCoordinator(
      associatedAddresses = { setOf(peer) },
      store = store,
      nowMs = { 12345L },
      ensureOwner = { true },
      ingest = { peers -> ingested.addAll(peers); true },
      log = {}
    )
    UbmCompanionPresenceService.coordinatorOverride = coordinator
  }

  @After
  fun tearDown() {
    UbmCompanionPresenceService.coordinatorOverride = null
  }

  private fun association(id: Int, mac: String?): AssociationInfo {
    val info = mock(AssociationInfo::class.java)
    `when`(info.id).thenReturn(id)
    if (mac == null) {
      `when`(info.deviceMacAddress).thenReturn(null)
    } else {
      val address = mock(MacAddress::class.java)
      `when`(address.toString()).thenReturn(mac)
      `when`(info.deviceMacAddress).thenReturn(address)
    }
    return info
  }

  @Test
  fun associationInfoAppearanceReachesTheCoordinatorWithItsId() {
    val service = UbmCompanionPresenceService()

    service.onDeviceAppeared(association(4, peer))
    service.onDeviceAppeared(association(5, peer))

    // One physical event, two association callbacks: a single ingest.
    assertEquals(listOf(PresenceRestoredPeer(peer, null, false)), ingested)
  }

  @Test
  fun bareAddressAppearanceStillReachesTheCoordinator() {
    val service = UbmCompanionPresenceService()

    service.onDeviceAppeared(peer)

    assertEquals(listOf(PresenceRestoredPeer(peer, null, false)), ingested)
  }

  @Test
  fun differentlyCasedCallbacksForOneDeviceDeliverOneWake() {
    // The platform stores one case and delivers another; both callbacks
    // describe one physical event, so the second must be the logged
    // duplicate, not a second wake.
    val service = UbmCompanionPresenceService()

    service.onDeviceAppeared(association(4, peer.lowercase()))
    service.onDeviceAppeared(association(5, peer))

    assertEquals(listOf(PresenceRestoredPeer(peer, null, false)), ingested)
  }

  @Test
  fun associationInfoDisappearanceClearsTheDelivery() {
    val service = UbmCompanionPresenceService()
    service.onDeviceAppeared(association(4, peer))

    service.onDeviceDisappeared(association(4, peer))
    service.onDeviceAppeared(association(4, peer))

    assertEquals(
      listOf(
        PresenceRestoredPeer(peer, null, false),
        PresenceRestoredPeer(peer, null, false)
      ),
      ingested
    )
  }

  @Test
  fun appearanceWithoutAnAddressIsIgnored() {
    val service = UbmCompanionPresenceService()

    service.onDeviceAppeared(association(4, null))

    assertEquals(emptyList<PresenceRestoredPeer>(), ingested)
  }
}
