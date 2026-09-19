// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/UbmCompanionPresenceService.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.companion.CompanionDeviceManager
import android.companion.CompanionDeviceService
import android.content.Context
import android.os.Build
import android.util.Log
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreProcessHost

/**
 * The Companion Device Manager presence endpoint (issue #212). The system
 * binds this service — creating the process when it is dead — when a device
 * the app observes through [CompanionPresenceObserver] appears or
 * disappears. Only the `String`-address overloads are overridden: the
 * framework forwards the API 33+ `AssociationInfo` callbacks to them
 * (AOSP `CompanionDeviceService`), so one override covers API 31+.
 *
 * The service surfaces the appearance; it never scans and never connects
 * itself. A cold-start appearance installs the process radio owner
 * ([RustCoreProcessHost.ensureInstalled]) before ingesting, so the known
 * peer reaches the owner as `restored` records with no Activity and no JS
 * session; only an appearance no owner takes stays persisted for the next
 * session open. The app reconnects directly through the ordinary
 * `when-available` connect — the package starts no headless JS runtime and
 * performs no connect or resubscribe here.
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
  private val coordinator: PresenceWakeCoordinator by lazy { coordinatorFor(applicationContext) }

  override fun onDeviceAppeared(address: String) {
    run("appeared") { coordinator.appeared(address, null) }
  }

  override fun onDeviceDisappeared(address: String) {
    run("disappeared") { coordinator.disappeared(address, null) }
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
        log = { message -> Log.w(TAG, message) }
      )
    }

    private fun associatedAddresses(context: Context): Set<String> {
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return emptySet()
      val manager = context.getSystemService(Context.COMPANION_DEVICE_SERVICE) as? CompanionDeviceManager
        ?: return emptySet()
      return manager.associations.toSet()
    }
  }
}
