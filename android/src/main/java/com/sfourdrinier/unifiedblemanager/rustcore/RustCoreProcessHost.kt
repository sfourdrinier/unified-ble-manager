// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreProcessHost.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.content.Context
import android.util.Log
import com.sfourdrinier.unifiedblemanager.background.AndroidConnectedDeviceForegroundServiceDriver
import com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry
import com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationStore
import com.sfourdrinier.unifiedblemanager.presence.CompanionPresenceObserver
import com.sfourdrinier.unifiedblemanager.presence.InMemoryBackgroundContinuationStore
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredStore
import com.sfourdrinier.unifiedblemanager.presence.RustCoreContinuationExecutor
import com.sfourdrinier.unifiedblemanager.presence.SharedPreferencesBackgroundContinuationStore
import com.sfourdrinier.unifiedblemanager.presence.SharedPreferencesPresenceStore
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
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
  private val scheduleCleanup: (Long, Runnable) -> Unit = { delayMs, task ->
    cleanupExecutor.schedule(task, delayMs, TimeUnit.MILLISECONDS)
  },
  private val log: (String) -> Unit
) {
  private val routes = ConcurrentHashMap<Long, (Long) -> Unit>()
  private val unroutedWakes = AtomicLong()
  private val retainedCleanupAttempts = ConcurrentHashMap<Long, AtomicInteger>()
  private val scheduledCleanups = ConcurrentHashMap.newKeySet<Long>()

  @Volatile
  private var companionChooser: CompanionPort? = null

  @Volatile
  private var installedAdapter: RustRadioHostAdapter? = null

  @Volatile
  private var presenceStore: PresenceRestoredStore? = null

  /** The persisted standing order the OS wake executes with no JavaScript. */
  @Volatile
  private var continuationStore: BackgroundContinuationStore? = null

  @Volatile
  private var continuationExecutor: RustCoreContinuationExecutor? = null

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
    retainedCleanupAttempts.keys.forEach { scheduleRetainedCleanup(it, 0L) }
    if (core.hostInstalled()) return
    val radio = radioHost()
    installedAdapter = radio as? RustRadioHostAdapter
    core.installHost(radio, wake, OWNER, ADAPTER_LABEL)
  }

  /** Presence appearance persistence (written by the presence service, drained at session open). */
  fun attachPresenceStore(store: PresenceRestoredStore) {
    presenceStore = store
  }

  /** Standing-order persistence (written by JS declare, read by the wake). */
  fun attachContinuationStore(store: BackgroundContinuationStore) {
    continuationStore = store
  }

  fun continuationStore(): BackgroundContinuationStore =
    continuationStore ?: InMemoryBackgroundContinuationStore().also { continuationStore = it }

  /**
   * The host-owned continuation executor (one continuation session per
   * process). Built on first use against the installed core.
   */
  @Synchronized
  fun continuationExecutor(): RustCoreContinuationExecutor {
    continuationExecutor?.let { return it }
    val built = RustCoreContinuationExecutor(
      core = core,
      wireRevision = core.wireRevision(),
      log = log
    )
    continuationExecutor = built
    return built
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
  private var reportedMalformedAppearances = 0L

  @Synchronized
  fun drainPresenceAppearances(): Int {
    val store = presenceStore ?: return 0
    val pending = store.drainAppearances()
    val malformed = store.malformedRecordCount()
    if (malformed > reportedMalformedAppearances) {
      log("presence drain reported ${malformed - reportedMalformedAppearances} malformed appearance records")
      reportedMalformedAppearances = malformed
    }
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

  /**
   * Takes cleanup ownership from a React module that is going away. The
   * process host outlives React contexts, so a refused native dispose remains
   * visible and retryable instead of being forgotten with the module's
   * executor. Retries remain bounded in frequency, never in count: the owner
   * keeps the session until Rust confirms that the lease is gone.
   */
  fun retainSessionCleanup(sessionId: Long, detail: String) {
    retainedCleanupAttempts.putIfAbsent(sessionId, AtomicInteger(0))
    unroute(sessionId)
    log("process owner retained session $sessionId cleanup: $detail")
    scheduleRetainedCleanup(sessionId, 0L)
  }

  internal fun retainedCleanupSessions(): Set<Long> = retainedCleanupAttempts.keys.toSet()

  private fun scheduleRetainedCleanup(sessionId: Long, delayMs: Long) {
    if (!retainedCleanupAttempts.containsKey(sessionId) || !scheduledCleanups.add(sessionId)) return
    try {
      scheduleCleanup(delayMs, Runnable { attemptRetainedCleanup(sessionId) })
    } catch (error: RuntimeException) {
      scheduledCleanups.remove(sessionId)
      log("process owner could not schedule session $sessionId cleanup: ${error.message}")
    }
  }

  private fun attemptRetainedCleanup(sessionId: Long) {
    scheduledCleanups.remove(sessionId)
    val attempts = retainedCleanupAttempts[sessionId] ?: return
    val attempt = attempts.incrementAndGet()
    try {
      core.invoke(sessionId, RustCoreSessions.DISPOSE, "{}", MobileCoreBridge.InvokeCallback { envelope ->
        val outcome = RustCoreSessions.disposeOutcome(envelope)
        if (outcome == null) {
          retainedCleanupAttempts.remove(sessionId)
          scheduledCleanups.remove(sessionId)
          log("process owner released retained session $sessionId on attempt $attempt")
        } else {
          log("process owner session $sessionId cleanup attempt $attempt failed: ${outcome.toJson()}")
          scheduleRetainedCleanup(sessionId, cleanupRetryDelay(attempt))
        }
      })
    } catch (error: RuntimeException) {
      val outcome = RustCoreSessions.disposeThrownOutcome(error)
      if (outcome == null) {
        retainedCleanupAttempts.remove(sessionId)
        scheduledCleanups.remove(sessionId)
        log("process owner found retained session $sessionId already released on attempt $attempt")
      } else {
        log("process owner session $sessionId cleanup attempt $attempt threw: ${outcome.toJson()}")
        scheduleRetainedCleanup(sessionId, cleanupRetryDelay(attempt))
      }
    }
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
    private const val MAX_CLEANUP_RETRY_DELAY_MS = 5_000L
    private val cleanupExecutor = Executors.newSingleThreadScheduledExecutor { runnable ->
      Thread(runnable, "ubm-rust-process-cleanup").also { it.isDaemon = true }
    }

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
        log = ::log
      )
      host.attachPresenceStore(SharedPreferencesPresenceStore(application))
      host.attachContinuationStore(SharedPreferencesBackgroundContinuationStore(application))
      shared = host
      return host
    }

    private fun cleanupRetryDelay(attempt: Int): Long {
      val shift = (attempt - 1).coerceIn(0, 6)
      return (100L shl shift).coerceAtMost(MAX_CLEANUP_RETRY_DELAY_MS)
    }
  }

  private class LeaseIds : java.util.function.Supplier<String> {
    private val next = AtomicLong(1)
    override fun get(): String = "background-${next.getAndIncrement()}"
  }
}
