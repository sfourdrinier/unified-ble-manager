// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/UbmCompanionPresenceService.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.companion.AssociationInfo
import android.companion.CompanionDeviceManager
import android.companion.CompanionDeviceService
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
 * disappears. Both callback overloads are overridden: the API 33+
 * [AssociationInfo] variant carries the real association id, and the
 * `String`-address variant covers older dispatch paths. If one physical
 * event ever reaches both overloads, the coordinator treats the second as
 * the duplicate it is (finding 236) instead of running the wake twice.
 *
 * The service surfaces the appearance; it never scans for unknown peers. A
 * cold-start appearance installs the process radio owner
 * ([RustCoreProcessHost.ensureInstalled]) before ingesting, so the known
 * peer reaches the owner as `restored` records with no Activity and no JS
 * session; only an appearance no owner takes stays persisted for the next
 * session open. After the record-only ingest, the declared standing order
 * ([BackgroundContinuationDeclaration]) executes: `record-only` stops here,
 * `native` reconnects the declared known peer and resubscribes the declared
 * characteristics through the Rust core with no JavaScript, and the deferred
 * strategies record their `capability.unsupported` refusal. The callback
 * only admits the work to this service's single serial worker, then returns
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
  /**
   * CompanionDeviceService callbacks may arrive on the main thread. Native
   * continuation has deliberately bounded but long radio waits, so it must
   * never occupy that callback. One worker also preserves the callback order
   * that the coordinator's duplicate and disappearance semantics require.
   */
  private val callbackWorker: ExecutorService = Executors.newSingleThreadExecutor { runnable ->
    Thread(runnable, "ubm-companion-presence").also { it.isDaemon = true }
  }

  private val coordinator: PresenceWakeCoordinator by lazy {
    // The override short-circuits before any Context use: unit tests drive
    // an unattached service, where applicationContext itself throws.
    coordinatorOverride ?: coordinatorFor(applicationContext)
  }

  override fun onDeviceAppeared(address: String) {
    enqueue("appeared") {
      if (coordinator.appeared(address, null)) {
        Log.i(TAG, "presence wake delivered for $address (associationId=none)")
      }
    }
  }

  override fun onDeviceAppeared(association: AssociationInfo) {
    val address = association.deviceMacAddress?.toString()
    if (address == null) {
      Log.w(TAG, "presence appearance without a device address ignored (associationId=${association.id})")
      return
    }
    enqueue("appeared") {
      if (coordinator.appeared(address, association.id)) {
        Log.i(TAG, "presence wake delivered for $address (associationId=${association.id})")
      }
    }
  }

  override fun onDeviceDisappeared(address: String) {
    enqueue("disappeared") { coordinator.disappeared(address, null) }
  }

  override fun onDeviceDisappeared(association: AssociationInfo) {
    val address = association.deviceMacAddress?.toString()
    if (address == null) {
      Log.w(TAG, "presence disappearance without a device address ignored (associationId=${association.id})")
      return
    }
    enqueue("disappeared") { coordinator.disappeared(address, association.id) }
  }

  override fun onDestroy() {
    // shutdown(), unlike shutdownNow(), drains work that a system callback
    // already admitted. A later callback after destruction is rejected and
    // logged visibly by enqueue rather than run against a dead service.
    callbackWorker.shutdown()
    super.onDestroy()
  }

  private fun enqueue(what: String, body: () -> Unit) {
    try {
      callbackWorker.execute { run(what, body) }
    } catch (error: RejectedExecutionException) {
      Log.w(TAG, "presence $what rejected after service teardown: ${error.message ?: error.javaClass.simpleName}")
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

    /**
     * Test seam: replaces the context-built coordinator (production builds
     * it from the application context on first use).
     */
    @Volatile
    var coordinatorOverride: PresenceWakeCoordinator? = null

    private fun coordinatorFor(context: Context): PresenceWakeCoordinator {
      coordinatorOverride?.let { return it }
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
          try {
            RustCoreProcessHost.shared(application).continuationExecutor().execute(address, declaration)
          } catch (error: Throwable) {
            Log.w(TAG, "presence native continuation failed: ${error.message ?: error.javaClass.simpleName}")
            ContinuationOutcome.failed(
              ContinuationStrategy.NATIVE,
              "lifecycle.invariant-violation",
              "continuation executor threw: ${error.message ?: error.javaClass.simpleName}",
              null
            )
          }
        },
        recordWakeOutcome = { outcome ->
          continuationStore.recordWakeOutcome(outcome)
          if (outcome.event == "continuation.completed") {
            Log.i(
              TAG,
              "continuation completed strategy=${outcome.strategy.wire} peer=${outcome.peerAddress}"
            )
            scheduleBacklogProof(application)
          } else {
            Log.w(
              TAG,
              "continuation failed strategy=${outcome.strategy.wire} peer=${outcome.peerAddress} " +
                "code=${outcome.code} reason=${outcome.reason}"
            )
          }
        }
      )
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
      return manager.associations.map { it.uppercase() }.toSet()
    }
  }
}
