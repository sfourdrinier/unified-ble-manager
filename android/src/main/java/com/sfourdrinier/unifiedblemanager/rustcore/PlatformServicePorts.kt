// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/PlatformServicePorts.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry
import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceControlException

/**
 * Background leases (`AcquireBackground` / `ReleaseBackground` /
 * `UpdateBackgroundNotification`). Calls may block (foreground-service
 * promotion waits up to five seconds) and never run on the main thread.
 */
interface BackgroundPort {
  fun acquire(kind: String, reason: String): String
  fun release(leaseId: String)
  fun update(leaseId: String, title: String, body: String?)
}

/** Production background port: the existing connected-device lease registry. */
class ForegroundServiceBackgroundPort(
  private val leases: ConnectedDeviceForegroundServiceLeaseRegistry
) : BackgroundPort {
  override fun acquire(kind: String, reason: String): String {
    if (kind != "connected-device") {
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "background lease kind '$kind' is not supported on Android")
    }
    return leases.acquire(reason)
  }

  override fun release(leaseId: String) = leases.release(leaseId)

  override fun update(leaseId: String, title: String, body: String?) = leases.update(leaseId, title, body)
}

/** Companion Device Manager association chooser (`AssociateCompanion`). */
interface CompanionPort {
  fun associate(name: String?, serviceUuid: String?, onResult: (Result<CompanionAssociation>) -> Unit)
}

data class CompanionAssociation(val associationId: Long, val peerId: String?, val displayName: String?)

/** Classifies a foreground-service control failure by its stable code. */
internal fun classifyBackgroundFailure(error: Throwable): RadioFailure {
  if (error is RadioPortFailure) {
    return RadioFailure(error.kind, error.gattStatus, error.message ?: error.kind.wire, nativeCode = error.nativeCode)
  }
  if (error is ForegroundServiceControlException) {
    val kind = when (error.code) {
      "foregroundServicePermissionDenied" -> RadioFailureKind.PERMISSION_DENIED
      "foregroundServiceNotConfigured" -> RadioFailureKind.UNSUPPORTED
      else -> RadioFailureKind.PLATFORM
    }
    // The registry's code is legacy's Expo identity (finding 133).
    return RadioFailure(kind, null, error.message ?: error.code, nativeCode = error.code)
  }
  return RadioFailure(RadioFailureKind.PLATFORM, null, error.message ?: error.javaClass.name)
}
