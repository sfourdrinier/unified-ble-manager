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
  private sealed interface CleanupObligation {
    data class Session(val id: Long) : CleanupObligation
    data class Scope(val id: String) : CleanupObligation
  }

  private class CleanupState {
    var attempts = 0
    var generation = 0L
    var scheduled = false
    var inFlight = false
  }

  private val cleanupLock = Any()
  private val retainedCleanups = mutableMapOf<CleanupObligation, CleanupState>()

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
    synchronized(cleanupLock) { retainedCleanups.keys.toList() }.forEach { scheduleRetainedCleanup(it, 0L) }
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
    val obligation = CleanupObligation.Session(sessionId)
    synchronized(cleanupLock) { retainedCleanups.getOrPut(obligation) { CleanupState() } }
    unroute(sessionId)
    log("process owner retained session $sessionId cleanup: $detail")
    scheduleRetainedCleanup(obligation, 0L)
  }

  fun retainBackgroundScopeCleanup(scope: String) {
    val obligation = CleanupObligation.Scope(scope)
    synchronized(cleanupLock) { retainedCleanups.getOrPut(obligation) { CleanupState() } }
    scheduleRetainedCleanup(obligation, 0L)
  }

  internal fun retainedCleanupSessions(): Set<Long> = synchronized(cleanupLock) {
    retainedCleanups.keys.filterIsInstance<CleanupObligation.Session>().mapTo(mutableSetOf()) { it.id }
  }

  internal fun retainedCleanupScopes(): Set<String> = synchronized(cleanupLock) {
    retainedCleanups.keys.filterIsInstance<CleanupObligation.Scope>().mapTo(mutableSetOf()) { it.id }
  }

  private fun scheduleRetainedCleanup(obligation: CleanupObligation, delayMs: Long) {
    val generation = synchronized(cleanupLock) {
      val state = retainedCleanups[obligation] ?: return
      if (state.scheduled || state.inFlight) return
      state.scheduled = true
      ++state.generation
    }
    try {
      scheduleCleanup(delayMs, Runnable { attemptRetainedCleanup(obligation, generation) })
    } catch (error: RuntimeException) {
      synchronized(cleanupLock) {
        retainedCleanups[obligation]?.takeIf { it.generation == generation }?.scheduled = false
      }
      log("process owner could not schedule $obligation cleanup: ${error.message}")
    }
  }

  private fun attemptRetainedCleanup(obligation: CleanupObligation, generation: Long) {
    val attempt = synchronized(cleanupLock) {
      val state = retainedCleanups[obligation] ?: return
      if (state.generation != generation || !state.scheduled || state.inFlight) return
      state.scheduled = false
      state.inFlight = true
      ++state.attempts
    }
    try {
      when (obligation) {
        is CleanupObligation.Session -> core.invoke(obligation.id, RustCoreSessions.DISPOSE, "{}", MobileCoreBridge.InvokeCallback { envelope ->
          finishRetainedCleanup(obligation, generation, attempt, RustCoreSessions.disposeOutcome(envelope))
        })
        is CleanupObligation.Scope -> {
          val record = core.releaseBackgroundScope(obligation.id)
          val released = try {
            (RustCoreJson.parse(record) as? Map<*, *>)?.get("state") == "released"
          } catch (_: IllegalArgumentException) {
            false
          }
          finishRetainedCleanup(obligation, generation, attempt, if (released) null else
            RustCoreRejection("platform.failure", "platform", "background.scope.release", record))
        }
      }
    } catch (error: RuntimeException) {
      val outcome = if (obligation is CleanupObligation.Session) RustCoreSessions.disposeThrownOutcome(error)
        else RustCoreRejection.platform("background.scope.release", error)
      finishRetainedCleanup(obligation, generation, attempt, outcome)
    }
  }

  private fun finishRetainedCleanup(obligation: CleanupObligation, generation: Long, attempt: Int, outcome: RustCoreRejection?) {
    val accepted = synchronized(cleanupLock) {
      val state = retainedCleanups[obligation]
      if (state == null || state.generation != generation || !state.inFlight) false else {
        state.inFlight = false
        if (outcome == null) retainedCleanups.remove(obligation)
        true
      }
    }
    if (!accepted) return
    if (outcome == null) log("process owner released $obligation on attempt $attempt") else {
      log("process owner $obligation cleanup attempt $attempt failed: ${outcome.toJson()}")
      scheduleRetainedCleanup(obligation, cleanupRetryDelay(attempt))
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
