// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreProcessHost.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.content.Context
import android.util.Log
import com.sfourdrinier.unifiedblemanager.background.AndroidConnectedDeviceForegroundServiceDriver
import com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry
import com.sfourdrinier.unifiedblemanager.presence.CompanionPresenceObserver
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredStore
import com.sfourdrinier.unifiedblemanager.presence.SharedPreferencesPresenceStore
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicLong

/**
 * The process owner of the Rust mobile host: installs it exactly once
 * (`nativeInstallHost`) with the one [RustRadioHostAdapter] over the one
 * [OwnedAndroidGattRadio], and routes Rust wakes to whichever module
 * instance holds the session. The host outlives React contexts (reload,
 * several managers); sessions are the per-manager leases.
 */
class RustCoreProcessHost(
  private val core: MobileCorePort,
  private val radioHost: () -> MobileCoreBridge.RadioHost,
  private val log: (String) -> Unit
) {
  private val routes = ConcurrentHashMap<Long, (Long) -> Unit>()
  private val unroutedWakes = AtomicLong()

  @Volatile
  private var companionChooser: CompanionPort? = null

  @Volatile
  private var installedAdapter: RustRadioHostAdapter? = null

  @Volatile
  private var presenceStore: PresenceRestoredStore? = null

  /** Rust's single wake sink for the process. */
  val wake = MobileCoreBridge.WakeListener { sessionId ->
    val route = routes[sessionId]
    if (route == null) {
      unroutedWakes.incrementAndGet()
      log("wake for session $sessionId has no live module route")
    } else {
      route(sessionId)
    }
  }

  fun unroutedWakeCount(): Long = unroutedWakes.get()

  @Synchronized
  fun ensureInstalled() {
    if (core.hostInstalled()) return
    val radio = radioHost()
    installedAdapter = radio as? RustRadioHostAdapter
    core.installHost(radio, wake, OWNER, ADAPTER_LABEL)
  }

  /** Presence appearance persistence (written by the presence service, drained at session open). */
  fun attachPresenceStore(store: PresenceRestoredStore) {
    presenceStore = store
  }

  /**
   * Ingests presence-restored peers into the live owner. Returns false when
   * no owner is alive to take them (the caller keeps them persisted).
   */
  fun ingestPresenceRestored(peers: List<PresenceRestoredPeer>): Boolean =
    installedAdapter?.ingestPresenceRestored(peers) ?: false

  /**
   * Drains persisted presence appearances into the live owner, exactly once
   * each; appearances the owner refused are persisted again. Returns the
   * count ingested.
   */
  @Synchronized
  fun drainPresenceAppearances(): Int {
    val store = presenceStore ?: return 0
    val pending = store.drainAppearances()
    if (pending.isEmpty()) return 0
    val peers = pending.map { PresenceRestoredPeer(it.address, null, false) }
    return if (ingestPresenceRestored(peers)) {
      pending.size
    } else {
      pending.forEach { store.saveAppearance(it.address, it.associationId, it.observedAtMs) }
      0
    }
  }

  fun route(sessionId: Long, onWake: (Long) -> Unit) {
    routes[sessionId] = onWake
  }

  fun unroute(sessionId: Long) {
    routes.remove(sessionId)
  }

  /** The chooser of the most recently attached React context (the one with a foreground Activity). */
  fun attachCompanionChooser(chooser: CompanionPort) {
    companionChooser = chooser
  }

  fun detachCompanionChooser(chooser: CompanionPort) {
    if (companionChooser === chooser) companionChooser = null
  }

  fun companionChooser(): CompanionPort? = companionChooser

  companion object {
    const val OWNER = "unified-ble-manager/react-native-android"
    const val ADAPTER_LABEL = "android-default"
    private const val TAG = "UnifiedBleRustCore"

    @Volatile
    private var shared: RustCoreProcessHost? = null

    private fun log(message: String) {
      try {
        Log.w(TAG, message)
      } catch (_: RuntimeException) {
        System.err.println("$TAG: $message")
      }
    }

    /** The production process host, created on first use from the application context. */
    @JvmStatic
    @Synchronized
    fun shared(context: Context): RustCoreProcessHost {
      shared?.let { return it }
      val application = context.applicationContext
      lateinit var host: RustCoreProcessHost
      host = RustCoreProcessHost(
        JniMobileCorePort,
        {
          RustRadioHostAdapter(
            core = JniMobileCorePort,
            radio = OwnedRadioPort(OwnedAndroidGattRadio(application), ::log),
            background = ForegroundServiceBackgroundPort(
              ConnectedDeviceForegroundServiceLeaseRegistry(
                AndroidConnectedDeviceForegroundServiceDriver(application),
                LeaseIds()
              )
            ),
            companion = { host.companionChooser() },
            presence = { CompanionPresenceObserver.application(application) },
            radioExecutor = Executors.newSingleThreadExecutor { runnable -> Thread(runnable, "ubm-rust-radio") },
            serviceExecutor = Executors.newSingleThreadExecutor { runnable -> Thread(runnable, "ubm-rust-services") },
            log = ::log
          )
        },
        ::log
      )
      host.attachPresenceStore(SharedPreferencesPresenceStore(application))
      shared = host
      return host
    }
  }

  private class LeaseIds : java.util.function.Supplier<String> {
    private val next = AtomicLong(1)
    override fun get(): String = "background-${next.getAndIncrement()}"
  }
}
