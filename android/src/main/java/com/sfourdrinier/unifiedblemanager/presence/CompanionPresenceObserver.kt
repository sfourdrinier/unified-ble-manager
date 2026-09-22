// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/CompanionPresenceObserver.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.companion.CompanionDeviceManager
import android.companion.DeviceNotAssociatedException
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import com.sfourdrinier.unifiedblemanager.rustcore.RadioFailureKind
import com.sfourdrinier.unifiedblemanager.rustcore.RadioPortFailure
import java.util.concurrent.ConcurrentHashMap

/** Device-presence observation for one associated peer (`ObservePresence` / `StopPresence`). */
interface PresencePort {
  fun observe(peerId: String, onResult: (Result<Unit>) -> Unit)
  fun unobserve(peerId: String, onResult: (Result<Unit>) -> Unit)
}

/**
 * Companion Device Manager device presence
 * ([CompanionDeviceManager.startObservingDevicePresence], API 31+, Android 12).
 * The peer id on Android is the device address the presence callback names,
 * so observing a known peer arms exactly that peer: no scan, no unknown
 * device. Appearance callbacks reach the process through
 * [UbmCompanionPresenceService]; this observer only arms and disarms them.
 *
 * Everything resolved here (API level, setup feature, service presence, the
 * association itself) is the platform's own answer, reported before any
 * effect when it refuses.
 */
class CompanionPresenceObserver(
  private val sdkInt: Int,
  private val hasCompanionFeature: () -> Boolean,
  private val manager: () -> CompanionDeviceManager?,
  private val startObserving: (CompanionDeviceManager, String) -> Unit,
  private val stopObserving: (CompanionDeviceManager, String) -> Unit
) : PresencePort {
  private val armed = ConcurrentHashMap.newKeySet<String>()

  override fun observe(peerId: String, onResult: (Result<Unit>) -> Unit) {
    val cdm = ready()
    try {
      startObserving(cdm, peerId)
    } catch (error: DeviceNotAssociatedException) {
      throw RadioPortFailure(
        RadioFailureKind.PLATFORM,
        "Companion Device Manager has no association for peer $peerId: arm presence only for an associated peer",
        nativeCode = "deviceNotAssociated"
      )
    }
    armed.add(peerId)
    onResult(Result.success(Unit))
  }

  override fun unobserve(peerId: String, onResult: (Result<Unit>) -> Unit) {
    if (!armed.remove(peerId)) {
      // The state asked for already holds: nothing observes this peer.
      onResult(Result.success(Unit))
      return
    }
    val cdm = ready()
    try {
      stopObserving(cdm, peerId)
    } catch (error: DeviceNotAssociatedException) {
      // The association is gone, so the OS already stopped the callbacks.
    }
    onResult(Result.success(Unit))
  }

  private fun ready(): CompanionDeviceManager {
    if (sdkInt < Build.VERSION_CODES.S) {
      throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "Companion Device Manager device presence requires Android API 31 (Android 12)",
        nativeCode = "unsupportedPresence"
      )
    }
    if (!hasCompanionFeature()) {
      throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "Companion Device Manager device presence requires companion-device setup support",
        nativeCode = "unsupportedPresence"
      )
    }
    return manager() ?: throw RadioPortFailure(
      RadioFailureKind.UNSUPPORTED,
      "Companion Device Manager is unavailable",
      nativeCode = "unsupportedPresence"
    )
  }

  companion object {
    /** Production observer from the application context (no Activity needed). */
    @JvmStatic
    fun application(context: Context): CompanionPresenceObserver {
      val application = context.applicationContext
      return CompanionPresenceObserver(
        sdkInt = Build.VERSION.SDK_INT,
        hasCompanionFeature = {
          application.packageManager.hasSystemFeature(PackageManager.FEATURE_COMPANION_DEVICE_SETUP)
        },
        manager = {
          application.getSystemService(Context.COMPANION_DEVICE_SERVICE) as? CompanionDeviceManager
        },
        startObserving = { cdm, address -> cdm.startObservingDevicePresence(address) },
        stopObserving = { cdm, address -> cdm.stopObservingDevicePresence(address) }
      )
    }
  }
}
