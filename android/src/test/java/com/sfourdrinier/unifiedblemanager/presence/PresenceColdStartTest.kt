// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/PresenceColdStartTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.DirectExecutor
import com.sfourdrinier.unifiedblemanager.rustcore.FakeBackground
import com.sfourdrinier.unifiedblemanager.rustcore.FakeCore
import com.sfourdrinier.unifiedblemanager.rustcore.FakeRadio
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreProcessHost
import com.sfourdrinier.unifiedblemanager.rustcore.RustRadioHostAdapter
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Issue #212: an OS-initiated cold start — `onDeviceAppeared` with no
 * Activity and no JS session — must hand the known peer to an installed
 * owner, not only persist the appearance. The app's reconnect then travels
 * the ordinary `when-available` connect to the known address, with no scan.
 */
class PresenceColdStartTest {
  private val peer = "AA:BB:CC:DD:EE:FF"

  private val core = FakeCore()
  private val radio = FakeRadio()
  private val logs = mutableListOf<String>()
  private lateinit var adapter: RustRadioHostAdapter
  private val host = RustCoreProcessHost(
    core,
    radioHost = { adapter },
    log = { logs.add(it) }
  )

  private val store = InMemoryPresenceStore()

  init {
    adapter = RustRadioHostAdapter(
      core = core,
      radio = radio,
      background = FakeBackground(),
      companion = { null },
      presence = { null },
      radioExecutor = DirectExecutor,
      serviceExecutor = DirectExecutor,
      log = { logs.add(it) }
    )
    host.attachPresenceStore(store)
  }

  /** The service's wiring: install the owner on wake, then ingest into it. */
  private fun coordinator() = PresenceWakeCoordinator(
    associatedAddresses = { setOf(peer) },
    store = store,
    nowMs = { 12345L },
    ensureOwner = {
      host.ensureInstalled()
      true
    },
    ingest = { peers -> host.ingestPresenceRestored(peers) },
    log = { logs.add(it) }
  )

  @Test
  fun coldStartAppearanceInstallsTheOwnerAndDeliversTheRestoredPeer() {
    coordinator().appeared(peer, null)

    assertEquals("cold start must install the process owner", 1, core.installCount)
    assertTrue(
      "cold start must deliver the restored peer, not only persist it: ${core.calls}",
      core.calls.any { it == "restored:$peer:false" }
    )
    assertTrue(store.drainAppearances().isEmpty())
  }

  @Test
  fun reconnectAfterColdStartIsAWhenAvailableConnectWithNoScan() {
    coordinator().appeared(peer, null)
    host.drainPresenceAppearances()

    adapter.connect(1L, peer, true, emptyArray())

    assertTrue(
      "reconnect must request the known peer when-available: ${radio.calls}",
      radio.calls.contains("connect:$peer:true")
    )
    assertTrue("reconnect must never scan: ${radio.calls}", radio.calls.none { it.startsWith("startScan") })
  }
}
