// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/UbmCompanionPresenceService.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.companion.AssociationInfo
import android.companion.CompanionDeviceManager
import android.companion.CompanionDeviceService
import android.companion.DevicePresenceEvent
import android.content.Context
import android.os.Build
import android.util.Log
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreProcessHost
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException

/**
 * The Companion Device Manager presence endpoint (issue #212). The system
 * binds this service — creating the process when it is dead — when a device
 * the app observes through [CompanionPresenceObserver] appears or
 * disappears. API 36+ uses source-aware events and ignores the compatibility
 * callbacks Android also delivers. BLE, Bluetooth connection and self-managed
 * presence are aggregated per owned association; only the last source's
 * disappearance releases a peer's continuation. API 31–35 retain their legacy
 * address/AssociationInfo callbacks and duplicate-appearance protection.
 *
 * The service surfaces the appearance; it never scans for unknown peers. A
 * cold-start appearance installs the process radio owner
 * ([RustCoreProcessHost.ensureInstalled]) before ingesting, so the known
 * peer reaches the owner as `restored` records with no Activity and no JS
 * session; only an appearance no owner takes stays persisted for the next
 * session open. After the record-only ingest, the declared standing order
 * ([BackgroundContinuationDeclaration]) executes: `record-only` stops here,
 * `native` reconnects the declared known peer and resubscribes the declared
 * characteristics through the Rust core with no JavaScript; headless-task
 * dispatches the registered RN task and foreground-service acquires a scoped
 * process-owned connected-device lease. Platform refusals remain explicit. The callback
 * only admits the work to the process-owned single serial worker, then returns
 * to the system promptly; the worker performs the bounded connect and
 * operation waits in callback order. Destroying the service closes admission
 * without interrupting a wake already admitted to that worker.
 *
 * Manifest (added by the Expo config plugin whenever `background.android`
 * is configured):
 *
 * ```xml
 * <service
 *   android:name="com.sfourdrinier.unifiedblemanager.presence.UbmCompanionPresenceService"
 *   android:exported="true"
 *   android:permission="android.permission.BIND_COMPANION_DEVICE_SERVICE">
 *   <intent-filter>
 *     <action android:name="android.companion.CompanionDeviceService" />
 *   </intent-filter>
 * </service>
 * ```
 *
 * The export is permission-gated to the system (only a holder of
 * `BIND_COMPANION_DEVICE_SERVICE` can bind); the library never exports an
 * unguarded host service.
 */
open class UbmCompanionPresenceService : CompanionDeviceService() {
  protected open fun presenceSdkInt(): Int = Build.VERSION.SDK_INT
  protected open fun associationForPresence(id: Int): AssociationInfo? {
    val manager = applicationContext.getSystemService(Context.COMPANION_DEVICE_SERVICE) as? CompanionDeviceManager
      ?: return null
    return manager.myAssociations.singleOrNull { it.id == id }
  }

  override fun onDevicePresenceEvent(event: DevicePresenceEvent) {
    if (presenceSdkInt() < 36) return
    val ticket = coordinator.admissionTicket()
    enqueue("source-event") {
      if (event.uuid != null || event.associationId < 0) {
        Log.w(TAG, "presence UUID-only event unsupported: no scoped association")
        return@enqueue
      }
      val kind = event.event
      if (kind !in 0..5) {
        Log.w(TAG, "presence event unsupported: event=$kind")
        return@enqueue
      }
      val association = associationForPresence(event.associationId)
      val address = association?.deviceMacAddress?.toString()
      if (address == null) {
        Log.w(TAG, "presence event refused: association is absent or has no address")
        return@enqueue
      }
      if (!coordinator.acceptsTicket(address, ticket)) return@enqueue
      coordinator.presenceEvent(address, association.id, kind / 2, kind % 2 == 0)
    }
  }
  /**
   * CompanionDeviceService callbacks may arrive on the main thread. Native
   * continuation has deliberately bounded but long radio waits, so it must
   * never occupy that callback. One worker also preserves the callback order
   * that the coordinator's duplicate and disappearance semantics require.
   */
  private var closed = false

  private val coordinator: PresenceWakeCoordinator by lazy {
    // The override short-circuits before any Context use: unit tests drive
    // an unattached service, where applicationContext itself throws.
    coordinatorOverride ?: coordinatorFor(applicationContext)
  }

  @Deprecated("Use onDevicePresenceEvent(DevicePresenceEvent) on Android 36+.")
  override fun onDeviceAppeared(address: String) {
    if (presenceSdkInt() >= 36) return
    val ticket = coordinator.admissionTicket()
    enqueue("appeared") {
      if (!coordinator.acceptsTicket(address, ticket)) return@enqueue
      if (coordinator.appeared(address, null)) {
        Log.i(TAG, "presence wake delivered for $address (associationId=none)")
      }
    }
  }

  @Deprecated("Use onDevicePresenceEvent(DevicePresenceEvent) on Android 36+.")
  override fun onDeviceAppeared(association: AssociationInfo) {
    if (presenceSdkInt() >= 36) return
    val address = association.deviceMacAddress?.toString()
    if (address == null) {
      Log.w(TAG, "presence appearance without a device address ignored (associationId=${association.id})")
      return
    }
    val ticket = coordinator.admissionTicket()
    enqueue("appeared") {
      if (!coordinator.acceptsTicket(address, ticket)) return@enqueue
      if (coordinator.appeared(address, association.id)) {
        Log.i(TAG, "presence wake delivered for $address (associationId=${association.id})")
      }
    }
  }

  @Deprecated("Use onDevicePresenceEvent(DevicePresenceEvent) on Android 36+.")
  override fun onDeviceDisappeared(address: String) {
    if (presenceSdkInt() >= 36) return
    val ticket = coordinator.admissionTicket()
    enqueue("disappeared") {
      if (coordinator.acceptsTicket(address, ticket)) coordinator.disappeared(address, null)
    }
  }

  @Deprecated("Use onDevicePresenceEvent(DevicePresenceEvent) on Android 36+.")
  override fun onDeviceDisappeared(association: AssociationInfo) {
    if (presenceSdkInt() >= 36) return
    val address = association.deviceMacAddress?.toString()
    if (address == null) {
      Log.w(TAG, "presence disappearance without a device address ignored (associationId=${association.id})")
      return
    }
    val ticket = coordinator.admissionTicket()
    enqueue("disappeared") {
      if (coordinator.acceptsTicket(address, ticket)) coordinator.disappeared(address, association.id)
    }
  }

  override fun onDestroy() {
    // The process worker outlives this service: admitted work drains before
    // callbacks from a replacement service, without interrupting ownership.
    synchronized(this) { closed = true }
    super.onDestroy()
  }

  private fun enqueue(what: String, body: () -> Unit) {
    synchronized(this) {
      if (closed) {
        Log.w(TAG, "presence $what rejected after service teardown")
        return
      }
      try {
        callbackWorker.execute { run(what, body) }
      } catch (error: RejectedExecutionException) {
        Log.w(TAG, "presence $what worker rejected: ${error.message ?: error.javaClass.simpleName}")
      }
    }
  }

  private fun run(what: String, body: () -> Unit) {
    try {
      body()
    } catch (error: Throwable) {
      // A system callback must never crash the process; the failure is
      // logged where only this side can see it, never swallowed.
      Log.w(TAG, "presence $what failed: ${error.message ?: error.javaClass.simpleName}")
    }
  }

  companion object {
    private const val TAG = "UbmPresenceService"
    @Volatile private var callbackThread: Thread? = null
    private val callbackWorker: ExecutorService = Executors.newSingleThreadExecutor { runnable ->
      Thread(runnable, "ubm-companion-presence").also { it.isDaemon = true; callbackThread = it }
    }

    /** Reopen wake admission only after the OS accepts observation on the shared worker. */
    internal fun observe(context: Context, address: String, start: () -> Unit) = onWorker {
      val owner = coordinatorFor(context)
      start()
      owner.observationStarted(address)
    }

    /** Confirmed OS stop fences pending wakes, then settles already-admitted work. */
    internal fun retireObservation(context: Context, address: String, cleanup: () -> Unit) {
      val owner = coordinatorFor(context)
      owner.retireObservation(address)
      onWorker {
        owner.clearRetiredAppearance(address)
        cleanup()
      }
    }

    private fun onWorker(action: () -> Unit) {
      if (Thread.currentThread() === callbackThread) action()
      else try {
        callbackWorker.submit(action).get()
      } catch (error: java.util.concurrent.ExecutionException) {
        throw (error.cause ?: error)
      } catch (error: InterruptedException) {
        Thread.currentThread().interrupt()
        throw error
      }
    }

    /**
     * Test seam: replaces the context-built coordinator (production builds
     * it from the application context on first use).
     */
    @Volatile
    var coordinatorOverride: PresenceWakeCoordinator? = null

    private var processCoordinator: PresenceWakeCoordinator? = null

    @Synchronized
    private fun coordinatorFor(context: Context): PresenceWakeCoordinator {
      coordinatorOverride?.let { return it }
      processCoordinator?.let { return it }
      val application = context.applicationContext
      val store = SharedPreferencesPresenceStore(application)
      val continuationStore = SharedPreferencesBackgroundContinuationStore(application)
      return PresenceWakeCoordinator(
        associatedAddresses = { associatedAddresses(application) },
        store = store,
        nowMs = { System.currentTimeMillis() },
        ensureOwner = {
          try {
            RustCoreProcessHost.shared(application).ensureInstalled()
            true
          } catch (error: Throwable) {
            Log.w(TAG, "presence owner bootstrap failed: ${error.message ?: error.javaClass.simpleName}")
            false
          }
        },
        ingest = { peers ->
          try {
            RustCoreProcessHost.shared(application).ingestPresenceRestored(peers)
          } catch (error: Throwable) {
            Log.w(TAG, "presence ingest failed: ${error.message ?: error.javaClass.simpleName}")
            false
          }
        },
        log = { message -> Log.w(TAG, message) },
        continuation = { continuationStore.loadDeclaration() },
        executeContinuation = { address, declaration ->
          executePresenceContinuation(declaration.strategy) {
            when (declaration.strategy) {
              ContinuationStrategy.NATIVE -> RustCoreProcessHost.shared(application).executeNativeContinuation(address, declaration)
              ContinuationStrategy.FOREGROUND_SERVICE, ContinuationStrategy.HEADLESS_TASK -> RustCoreProcessHost.shared(application).continuationExecutor().executePlatform(declaration) {
                if (declaration.strategy == ContinuationStrategy.FOREGROUND_SERVICE) RustCoreProcessHost.shared(application).foregroundContinuation().execute(address, declaration)
                else UbmHeadlessContinuationService.dispatch(application, address, declaration)
              }
              ContinuationStrategy.RECORD_ONLY -> error("Record-only must not dispatch a continuation")
            }
          }
        },
        recordWakeOutcome = { outcome ->
          continuationStore.recordWakeOutcome(outcome)
          if (outcome.event == "continuation.completed") {
            Log.i(
              TAG,
              "continuation completed strategy=${outcome.strategy.wire} peer=${outcome.peerAddress}"
            )
            if (outcome.strategy == ContinuationStrategy.NATIVE) scheduleBacklogProof(application)
          } else {
            Log.w(
              TAG,
              "continuation failed strategy=${outcome.strategy.wire} peer=${outcome.peerAddress} " +
                "code=${outcome.code} reason=${outcome.reason}"
            )
          }
        },
        releaseContinuation = { address -> RustCoreProcessHost.shared(application).foregroundContinuation().release(address) }
      ).also { processCoordinator = it }
    }

    private val proofScheduler =
      java.util.concurrent.Executors.newSingleThreadScheduledExecutor { runnable ->
        Thread(runnable, "ubm-continuation-proof").also { it.isDaemon = true }
      }

    /**
     * Best-effort proof values are arriving with no JS session: reads the
     * queued backlog size without consuming it. Delayed past the first
     * notifications so the demo runbook can quote the lines.
     */
    private fun scheduleBacklogProof(context: Context) {
      for (delaySeconds in listOf(15L, 45L)) {
        proofScheduler.schedule(
          {
            try {
              val backlog = RustCoreProcessHost.shared(context).continuationExecutor().describeBacklog()
              if (backlog == null) {
                Log.i(TAG, "continuation backlog: no session")
              } else {
                Log.i(
                  TAG,
                  "continuation backlog: queuedBytes=${backlog.queuedBytes} " +
                    "ingressDrops=${backlog.ingressDrops}"
                )
              }
            } catch (error: Throwable) {
              Log.w(TAG, "continuation backlog unreadable: ${error.message ?: error.javaClass.simpleName}")
            }
          },
          delaySeconds,
          java.util.concurrent.TimeUnit.SECONDS
        )
      }
    }

    private fun associatedAddresses(context: Context): Set<String> {
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return emptySet()
      val manager = context.getSystemService(Context.COMPANION_DEVICE_SERVICE) as? CompanionDeviceManager
        ?: return emptySet()
      // MAC addresses are case-insensitive hex; the platform reports them
      // in its stored case while callbacks arrive in theirs. Normalize to
      // uppercase (the same normalization the associate path applies), so a
      // differently-cased twin never reads as "unassociated".
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
        return manager.myAssociations.mapNotNull { it.deviceMacAddress?.toString()?.uppercase() }.toSet()
      }
      return legacyAssociationAddresses(manager)
    }

    /** API 31–32 only: use the legacy address list before myAssociations exists. */
    @Suppress("DEPRECATION")
    private fun legacyAssociationAddresses(manager: CompanionDeviceManager): Set<String> =
      manager.associations.map { it.uppercase() }.toSet()
  }
}
