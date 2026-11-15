// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreProcessHost.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.content.Context
import android.util.Log
import com.sfourdrinier.unifiedblemanager.background.AndroidConnectedDeviceForegroundServiceDriver
import com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry
import com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationStore
import com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationDeclaration
import com.sfourdrinier.unifiedblemanager.presence.ContinuationOutcome
import com.sfourdrinier.unifiedblemanager.presence.CompanionPresenceObserver
import com.sfourdrinier.unifiedblemanager.presence.InMemoryBackgroundContinuationStore
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredStore
import com.sfourdrinier.unifiedblemanager.presence.PresenceForegroundContinuation
import com.sfourdrinier.unifiedblemanager.presence.NativeContinuationBinding
import com.sfourdrinier.unifiedblemanager.presence.SharedPreferencesBackgroundContinuationStore
import com.sfourdrinier.unifiedblemanager.presence.SharedPreferencesPresenceStore
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

data class SelectedScanFailureRecoveryBinding(
  val generation: Long,
  val binding: Long
)

/**
 * Owns one Rust mobile binding and its [RustRadioHostAdapter] over an
 * [OwnedAndroidGattRadio], and routes wakes to the module holding each
 * session. The production shared host outlives React contexts; failed
 * unpublished installs retain their port until native cleanup succeeds.
 */
class RustCoreProcessHost(
  core: MobileCorePort,
  private val radioHost: () -> MobileCoreBridge.RadioHost,
  private val scheduleCleanup: (Long, Runnable) -> Unit = { delayMs, task ->
    cleanupExecutor.schedule(task, delayMs, TimeUnit.MILLISECONDS)
  },
  private val log: (String) -> Unit
) {
  @Volatile var core: MobileCorePort = core
    private set
  private var coreFactory: (() -> MobileCorePort)? = null
  private var failedInstall = false
  private var installCleanupScheduled = false
  private var installCleanupAttempts = 0
  private var failedInstallNativeCleanupPending = false
  private var installRadioCleanup: (() -> Boolean)? = null

  /** Retain the driver before adapter construction can register a receiver. */
  private fun ownInstallingRadio(radio: OwnedAndroidGattRadio) {
    check(installRadioCleanup == null) { "install already owns a radio" }
    installRadioCleanup = {
      val result = radio.destroy()
      result.failures.forEach { log("failed-install radio cleanup ${it.operation}: ${it.throwable.javaClass.name}: ${it.throwable.message}") }
      result.failures.isEmpty()
    }
  }

  internal fun createOwnedAdapter(
    core: MobileCorePort, radio: OwnedAndroidGattRadio, background: BackgroundPort,
    companion: () -> CompanionPort?, presence: () -> com.sfourdrinier.unifiedblemanager.presence.PresencePort?,
    radioExecutor: java.util.concurrent.Executor, serviceExecutor: java.util.concurrent.Executor,
    adapterLog: (String) -> Unit
  ): RustRadioHostAdapter {
    ownInstallingRadio(radio)
    return RustRadioHostAdapter(core, OwnedRadioPort(radio, adapterLog), background, companion, presence,
      radioExecutor, serviceExecutor, adapterLog)
  }

  @Synchronized internal fun setCoreFactory(factory: () -> MobileCorePort) { coreFactory = factory }

  @Synchronized private fun retryFailedInstallCleanup() {
    installCleanupScheduled = false
    if (!failedInstall) return
    val nativeCleared = try { !failedInstallNativeCleanupPending || core.cleanupFailedInstall() } catch (error: Throwable) {
      log("failed-install cleanup failed: ${error.javaClass.name}: ${error.message}")
      false
    }
    if (nativeCleared) failedInstallNativeCleanupPending = false
    val cleared = nativeCleared && try { installRadioCleanup?.invoke() ?: true } catch (error: Throwable) {
      log("failed-install radio cleanup failed: ${error.javaClass.name}: ${error.message}")
      false
    }
    if (cleared) {
      installRadioCleanup = null
      core = checkNotNull(coreFactory).invoke()
      failedInstall = false
      installCleanupAttempts = 0
      notifyRecoveryHint()
    } else {
      log("failed-install cleanup remains pending; retaining its binding and radio")
      scheduleInstallCleanup()
    }
  }

  private fun scheduleInstallCleanup() {
    if (installCleanupScheduled) return
    installCleanupScheduled = true
    installCleanupAttempts++
    try {
      scheduleCleanup(cleanupRetryDelay(installCleanupAttempts), Runnable { retryFailedInstallCleanup() })
    } catch (error: RuntimeException) {
      installCleanupScheduled = false
      log("failed-install cleanup scheduling failed: ${error.javaClass.name}: ${error.message}")
    }
  }
  private var presenceForeground: PresenceForegroundContinuation? = null
  private val routes = ConcurrentHashMap<Long, (Long) -> Unit>()
  private val unroutedWakes = AtomicLong()
  private val recoveryObservers = ConcurrentHashMap.newKeySet<Runnable>()

  fun addRecoveryObserver(observer: Runnable) { recoveryObservers.add(observer) }
  fun removeRecoveryObserver(observer: Runnable) { recoveryObservers.remove(observer) }
  internal fun notifyRecoveryHint() {
    recoveryObservers.forEach { observer ->
      try { observer.run() } catch (error: RuntimeException) {
        log("recovery hint delivery failed: ${error.javaClass.name}")
      }
    }
  }
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
  private var continuationExecutor: NativeContinuationBinding? = null
  private var recordingDirectory: (() -> java.io.File)? = null
  private var recordingConfigured = false
  // A recovery attach is not a renderer capability.  The embedding native
  // application must nominate a recording id while it owns process setup;
  // TurboModule calls can neither enumerate nor add this set.
  private val trustedScanRecoveryRecordingIds = ConcurrentHashMap.newKeySet<String>()
  @Volatile private var selectedScanRecoveryRecordingId: String? = null
  private val scanRecoverySelectionGeneration = AtomicLong(0L)
  private val scanRecoverySelectionObservers = ConcurrentHashMap.newKeySet<(Long) -> Unit>()
  /** Serializes native alias revocation with publication of a replacement selection. */
  private val scanRecoverySelectionLock = Any()

  /** Native application-private path only; never sourced from a JS declaration. */
  fun attachRecordingDirectory(directory: () -> java.io.File) { recordingDirectory = directory }

  @Synchronized
  fun configureRecordingStorage() {
    if (recordingConfigured) return
    try {
      val directory = recordingDirectory?.invoke() ?: throw RustCoreRejection("capability.unsupported", "restoration", "continuation.recording.configure", "No trusted private recording directory was configured")
      if (!directory.isDirectory && !directory.mkdirs()) throw java.io.IOException("Directory creation failed")
      val result = RustCoreJson.parse(core.continuationConfigureRecordingDirectory(directory.canonicalPath)) as? Map<*, *>
      if (result?.get("ok") != true) {
        val error = result?.get("error") as? Map<*, *>
        val code = error?.get("code") as? String
        val domain = error?.get("domain") as? String
        val operation = error?.get("operation") as? String
        if (code == null || domain == null || operation == null) throw RustCoreRejection("protocol.malformed", "protocol", "continuation.recording.configure", "Malformed storage configuration response")
        throw RustCoreRejection(code, domain, operation, error["detail"] as? String, error["platform"] as? Map<*, *>)
      }
      recordingConfigured = true
    } catch (error: RustCoreRejection) { throw error }
      catch (error: MobileCoreBridge.MobileCoreException) { throw RustCoreRejection.fromWire(error.message, "continuation.recording.configure") }
      catch (error: Exception) {
        val reason = "Private recording storage could not be configured"
        throw RustCoreRejection("platform.failure", "platform", "continuation.recording.configure", reason,
          linkedMapOf("domain" to "android", "code" to error.javaClass.name, "message" to reason, "metadata" to emptyMap<String, Any?>()))
      }
  }

  fun recordingControl(operation: String, id: String, token: String, maxItems: Int, maxBytes: Int): String {
    configureRecordingStorage()
    return core.continuationRecordingControl(operation, id, token, maxItems, maxBytes)
  }

  /**
   * Native bootstrap policy for a durable scan-recovery recording.  Recording
   * ids are names only: paths, sidecars and recovery credentials remain in
   * Rust's protected registry.  This is deliberately internal so React
   * Native/Expo JavaScript cannot turn an arbitrary string into authority.
   */
  internal fun authorizeScanFailureRecoveryRecording(recordingId: String) {
    require(recordingId.isNotEmpty() && recordingId.length <= 512 &&
      recordingId.none { it == '/' || it == '\\' || it == '\u0000' }) {
      "scan recovery recording id is not a native recording name"
    }
    trustedScanRecoveryRecordingIds.add(recordingId)
  }

  /**
   * Native embedding bootstrap only.  This selects one already-authorized
   * app-private recording for the lifecycle of this process host.  Renderers
   * can subsequently ask to open the selected recovery but can never supply,
   * enumerate, or learn this recording id.
   */
  internal fun selectScanFailureRecoveryRecording(recordingId: String) {
    authorizeScanFailureRecoveryRecording(recordingId)
    synchronized(scanRecoverySelectionLock) {
      // `scan_recovery` is keyed by binding, not by recording name. Revoke it
      // before A can be replaced by B or a new generation becomes observable;
      // direct Turbo calls then fail in the same JNI slot lock as JSI calls.
      val previousRecordingId = this.selectedScanRecoveryRecordingId
      if (previousRecordingId != null) {
        scanFailureRecoveryPort(previousRecordingId).revokeScanFailureRecovery()
      }
      selectedScanRecoveryRecordingId = recordingId
      val generation = scanRecoverySelectionGeneration.incrementAndGet()
      scanRecoverySelectionObservers.forEach { observer -> observer(generation) }
    }
  }

  /** Native embedding observers use this to replace a stale JSI snapshot. */
  fun addScanFailureRecoverySelectionObserver(observer: (Long) -> Unit) {
    scanRecoverySelectionObservers.add(observer)
  }

  fun removeScanFailureRecoverySelectionObserver(observer: (Long) -> Unit) {
    scanRecoverySelectionObservers.remove(observer)
  }

  /** Opens only a pre-authorized app-private recording through the exact JNI binding. */
  fun scanFailureRecoveryPort(recordingId: String): JniBoundContinuationPort {
    if (!trustedScanRecoveryRecordingIds.contains(recordingId)) {
      throw RustCoreRejection("ownership.denied", "restoration", "scan.failure-recovery.attach", "recording was not authorized by the native host")
    }
    configureRecordingStorage()
    ensureInstalled()
    val bound = core as? JniBoundMobileCorePort
      ?: throw RustCoreRejection("capability.unsupported", "restoration", "scan.failure-recovery.attach", "loaded mobile core has no binding-scoped recovery bridge")
    return bound.scanFailureRecoveryPort()
  }

  /** Opens the native-bootstrap-selected recovery and returns only opaque JNI handles. */
  fun openSelectedScanFailureRecovery(): String {
    val recordingId = selectedScanRecoveryRecordingId
      ?: throw RustCoreRejection("capability.unsupported", "restoration", "scan.failure-recovery.open", "No trusted scan recovery recording was selected by the native host")
    return scanFailureRecoveryPort(recordingId).scanFailureRecoveryAttach(recordingId)
  }

  /**
   * Returns the selected native-only recording identity to the RustCore
   * module's private JSI bridge. It is deliberately internal: no TurboModule
   * method or JavaScript caller can supply, inspect, or enumerate it.
   */
  fun selectedScanFailureRecoveryRecordingId(): String =
    selectedScanRecoveryRecordingId
      ?: throw RustCoreRejection("capability.unsupported", "restoration", "scan.failure-recovery.read", "No trusted scan recovery recording was selected by the native host")

  /** Captures one exact selection and JNI binding for a JSI HostObject. */
  fun selectedScanFailureRecoveryBinding(): SelectedScanFailureRecoveryBinding {
    val generation = scanRecoverySelectionGeneration.get()
    check(generation > 0L) { "scan recovery recording has not been selected" }
    val port = scanFailureRecoveryPort(selectedScanFailureRecoveryRecordingId())
    return SelectedScanFailureRecoveryBinding(generation, port.bindingIdentity())
  }

  /**
   * Copies a page only while the exact selection and JNI binding captured by
   * the JSI HostObject remain current. Both checks run before and after the
   * JNI call so replacement cannot make a captured HostObject read new state.
   */
  fun readSelectedScanFailureRecoveryPage(
    headId: String,
    offset: String,
    expectedSelectionGeneration: Long,
    expectedBinding: Long
  ): MobileCoreBridge.ScanFailureRecoveryPage {
    fun currentPort(): JniBoundContinuationPort {
      check(scanRecoverySelectionGeneration.get() == expectedSelectionGeneration) {
        "scan recovery selection was replaced"
      }
      val port = scanFailureRecoveryPort(selectedScanFailureRecoveryRecordingId())
      check(port.bindingIdentity() == expectedBinding) { "scan recovery binding was replaced" }
      return port
    }
    val result = currentPort().scanFailureRecoveryReadPage(headId, offset)
    currentPort()
    return result
  }

  /**
   * Reads a single raw-source token only while the JSI snapshot's selection
   * and exact JNI binding remain current.  This deliberately mirrors the page
   * fence: a captured HostObject must never start reading a replacement
   * recording after application selection changes.
   */
  fun readSelectedScanFailureRecoveryRawToken(
    ownerId: String,
    tokenIndex: Int,
    offset: String,
    maxBytes: String,
    expectedSelectionGeneration: Long,
    expectedBinding: Long
  ): MobileCoreBridge.ScanFailureRecoveryRawToken {
    fun currentPort(): JniBoundContinuationPort {
      check(scanRecoverySelectionGeneration.get() == expectedSelectionGeneration) {
        "scan recovery selection was replaced"
      }
      val port = scanFailureRecoveryPort(selectedScanFailureRecoveryRecordingId())
      check(port.bindingIdentity() == expectedBinding) { "scan recovery binding was replaced" }
      return port
    }
    val result = currentPort().rawToken(ownerId, tokenIndex, offset, maxBytes)
    currentPort()
    return result
  }

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
    if (failedInstall) {
      scheduleInstallCleanup()
      throw RustCoreRejection("lifecycle.invalid-state", "core", "host.install", "Previous host installation cleanup is still pending")
    }
    if (core.hostInstalled()) return
    var installStarted = false
    try {
      val radio = radioHost()
      installStarted = true
      core.installHost(radio, wake, OWNER, ADAPTER_LABEL)
      installedAdapter = radio as? RustRadioHostAdapter
      installRadioCleanup = null
    } catch (failure: Throwable) {
      if (coreFactory != null) {
        failedInstall = true
        failedInstallNativeCleanupPending = installStarted
        scheduleInstallCleanup()
      }
      throw failure
    }
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
  fun continuationExecutor(): NativeContinuationBinding {
    ensureInstalled()
    continuationExecutor?.let { return it }
    val built = NativeContinuationBinding(
      core = core,
      log = log
    )
    continuationExecutor = built
    return built
  }

  /** Only acquisition of a recording-enabled order needs writable storage;
   * status, handoff and disarming must remain available after storage fails. */
  fun executeNativeContinuation(peerId: String, declaration: BackgroundContinuationDeclaration): ContinuationOutcome {
    if (declaration.recording != null) configureRecordingStorage()
    return continuationExecutor().execute(peerId, declaration)
  }

  /** App-owned direct execution, not an OS appearance. The host retains storage
   * and process authority; the receiver decodes the canonical result before ACK. */
  fun executeNativeContinuationRaw(peerId: String, declaration: BackgroundContinuationDeclaration, callback: MobileCoreBridge.InvokeCallback) {
    try {
      if (continuationStore().loadDeclaration() != declaration) throw RustCoreRejection(
        "lifecycle.invalid-state", "restoration", "continuation.execute", "Explicit matching persisted native declaration required")
      if (declaration.recording != null) configureRecordingStorage()
      continuationExecutor().executeRaw(peerId, declaration, callback) { continuationStore().loadDeclaration() }
    } catch (error: RuntimeException) {
      val failure = when (error) {
        is RustCoreRejection -> error
        is MobileCoreBridge.MobileCoreException -> RustCoreRejection.fromWire(error.message, "continuation.execute")
        else -> RustCoreRejection.platform("continuation.execute", error)
      }
      callback.onResult(failure.toContinuationEnvelope())
    }
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
          finishRetainedCleanup(obligation, generation, attempt, RustCoreSessions.disposeOutcome(envelope), RustCoreSessions.disposeReleased(envelope))
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

  private fun finishRetainedCleanup(obligation: CleanupObligation, generation: Long, attempt: Int, outcome: RustCoreRejection?, confirmedSessionRelease: Boolean = false) {
    val accepted = synchronized(cleanupLock) {
      val state = retainedCleanups[obligation]
      if (state == null || state.generation != generation || !state.inFlight) false else {
        state.inFlight = false
        if (outcome == null) retainedCleanups.remove(obligation)
        true
      }
    }
    if (!accepted) return
    if (outcome == null && confirmedSessionRelease) notifyRecoveryHint()
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

  fun foregroundContinuation(): PresenceForegroundContinuation = checkNotNull(presenceForeground) {
    "This process host has no connected-device foreground-service authority"
  }

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

    /** Each factory invocation owns one port, including its unpublished cleanup. */
    internal fun bound(
      radio: (MobileCorePort) -> MobileCoreBridge.RadioHost,
      schedule: (Long, Runnable) -> Unit = { delay, task -> cleanupExecutor.schedule(task, delay, TimeUnit.MILLISECONDS) },
      log: (String) -> Unit,
      portFactory: () -> JniBoundMobileCorePort = { JniBoundMobileCorePort() }
    ): RustCoreProcessHost {
      lateinit var host: RustCoreProcessHost
      host = RustCoreProcessHost(portFactory(), { radio(host.core) }, schedule, log)
      host.setCoreFactory(portFactory)
      return host
    }

    /** The production process host, created on first use from the application context. */
    @JvmStatic
    @Synchronized
    fun shared(context: Context): RustCoreProcessHost {
      shared?.let { return it }
      val application = context.applicationContext
      val backgroundLeases = ConnectedDeviceForegroundServiceLeaseRegistry(
        AndroidConnectedDeviceForegroundServiceDriver(application), LeaseIds()
      )
      val foreground = PresenceForegroundContinuation(backgroundLeases)
      lateinit var host: RustCoreProcessHost
      host = bound(
        { core ->
          val driver = OwnedAndroidGattRadio(application)
          host.createOwnedAdapter(
            core = core,
            radio = driver,
            background = ForegroundServiceBackgroundPort(backgroundLeases),
            companion = { host.companionChooser() },
            presence = { CompanionPresenceObserver.application(application, foreground::releaseOrThrow) },
            radioExecutor = Executors.newSingleThreadExecutor { runnable -> Thread(runnable, "ubm-rust-radio") },
            serviceExecutor = Executors.newSingleThreadExecutor { runnable -> Thread(runnable, "ubm-rust-services") },
            adapterLog = ::log
          )
        },
        log = ::log
      )
      host.presenceForeground = foreground
      host.attachPresenceStore(SharedPreferencesPresenceStore(application))
      host.attachContinuationStore(SharedPreferencesBackgroundContinuationStore(application))
      host.attachRecordingDirectory { java.io.File(application.noBackupFilesDir, "ubm-continuation") }
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
