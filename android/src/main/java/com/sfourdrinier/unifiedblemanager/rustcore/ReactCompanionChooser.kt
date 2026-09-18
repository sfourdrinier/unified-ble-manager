// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/ReactCompanionChooser.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.companion.AssociationInfo
import android.companion.AssociationRequest
import android.companion.BluetoothDeviceFilter
import android.companion.CompanionDeviceManager
import android.content.Context
import android.content.Intent
import android.content.IntentSender
import android.content.pm.PackageManager
import android.os.Build
import android.os.ParcelUuid
import com.facebook.react.bridge.ActivityEventListener
import com.facebook.react.bridge.ReactApplicationContext
import java.util.regex.Pattern

/**
 * Companion Device Manager chooser for the Rust route (`AssociateCompanion`),
 * bound to one React context's foreground Activity. Same platform behavior as
 * the legacy protocol-control association (API 33+, one association at a
 * time, the system UI result or `onAssociationCreated` resolves it).
 */
class ReactCompanionChooser(private val reactContext: ReactApplicationContext) : CompanionPort, ActivityEventListener {
  private var pending: ((Result<CompanionAssociation>) -> Unit)? = null
  private var pendingRequestCode = 0
  private var pendingAssociationId = 0
  private var uiLaunched = false
  private var nextRequestCode = FIRST_REQUEST_CODE

  init {
    reactContext.addActivityEventListener(this)
  }

  fun detach() {
    reactContext.removeActivityEventListener(this)
    reject(
      RadioPortFailure(
        RadioFailureKind.CANCELLED,
        "the React context hosting the companion chooser was invalidated",
        nativeCode = "associationCancelled"
      )
    )
  }

  @Synchronized
  override fun associate(name: String?, serviceUuid: String?, onResult: (Result<CompanionAssociation>) -> Unit) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
      !reactContext.packageManager.hasSystemFeature(PackageManager.FEATURE_COMPANION_DEVICE_SETUP)
    ) {
      throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "Companion Device Manager association requires Android API 33 and companion-device setup support",
        nativeCode = "unsupportedAssociation"
      )
    }
    if (pending != null) {
      throw RadioPortFailure(
        RadioFailureKind.BUSY,
        "a companion association is already in progress",
        nativeCode = "associationBusy"
      )
    }
    val activity = reactContext.currentActivity
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "a foreground Activity is required to launch the chooser",
        nativeCode = "associationActivityUnavailable"
      )
    val filter = BluetoothDeviceFilter.Builder()
    if (name != null) filter.setNamePattern(Pattern.compile(Pattern.quote(name)))
    if (serviceUuid != null) filter.addServiceUuid(ParcelUuid.fromString(serviceUuid), null)
    val request = AssociationRequest.Builder().addDeviceFilter(filter.build()).setSingleDevice(true).build()
    val requestCode = nextRequestCode
    nextRequestCode = if (requestCode == Int.MAX_VALUE) FIRST_REQUEST_CODE else requestCode + 1
    pending = onResult
    pendingRequestCode = requestCode
    pendingAssociationId = 0
    uiLaunched = false
    val manager = reactContext.getSystemService(Context.COMPANION_DEVICE_SERVICE) as? CompanionDeviceManager
    if (manager == null) {
      clear()
      throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "Companion Device Manager is unavailable",
        nativeCode = "unsupportedAssociation"
      )
    }
    manager.associate(request, object : CompanionDeviceManager.Callback() {
      override fun onDeviceFound(intentSender: IntentSender) = launch(activity, intentSender, onResult, requestCode)
      override fun onAssociationPending(intentSender: IntentSender) = launch(activity, intentSender, onResult, requestCode)
      override fun onAssociationCreated(associationInfo: AssociationInfo) = created(onResult, associationInfo)
      override fun onFailure(error: CharSequence?) {
        rejectIf(
          onResult,
          RadioPortFailure(
            RadioFailureKind.PLATFORM,
            error?.toString() ?: "Companion Device Manager association failed",
            nativeCode = "associationFailed"
          )
        )
      }
    }, null)
  }

  @Synchronized
  private fun launch(activity: Activity, sender: IntentSender, owner: (Result<CompanionAssociation>) -> Unit, requestCode: Int) {
    if (pending !== owner || pendingRequestCode != requestCode || uiLaunched) return
    try {
      uiLaunched = true
      activity.startIntentSenderForResult(sender, requestCode, null, 0, 0, 0)
    } catch (error: IntentSender.SendIntentException) {
      rejectIf(
        owner,
        RadioPortFailure(
          RadioFailureKind.PLATFORM,
          "Companion Device Manager system UI could not be launched",
          cause = error,
          nativeCode = "associationUiLaunchFailed"
        )
      )
    }
  }

  @Synchronized
  private fun created(owner: (Result<CompanionAssociation>) -> Unit, info: AssociationInfo) {
    if (pending !== owner) return
    pendingAssociationId = info.id
    if (!uiLaunched) resolve(pendingAssociationId, info.deviceMacAddress?.toString(), info.displayName?.toString())
  }

  @Synchronized
  override fun onActivityResult(activity: Activity, requestCode: Int, resultCode: Int, data: Intent?) {
    if (requestCode != pendingRequestCode || pending == null || !uiLaunched) return
    if (resultCode != Activity.RESULT_OK || data == null) {
      reject(
        RadioPortFailure(
          RadioFailureKind.CANCELLED,
          "Companion Device Manager association was cancelled",
          nativeCode = "associationCancelled"
        )
      )
      return
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      val info = data.getParcelableExtra(CompanionDeviceManager.EXTRA_ASSOCIATION, AssociationInfo::class.java)
      if (info != null) {
        resolve(info.id, info.deviceMacAddress?.toString(), info.displayName?.toString())
        return
      }
      val device = data.getParcelableExtra(CompanionDeviceManager.EXTRA_DEVICE, BluetoothDevice::class.java)
      resolve(pendingAssociationId, deviceAddress(device), deviceName(device))
    }
  }

  override fun onNewIntent(intent: Intent) {}

  private fun resolve(associationId: Int, peerId: String?, displayName: String?) {
    val owner = pending ?: return
    if (associationId <= 0) {
      reject(
        RadioPortFailure(
          RadioFailureKind.UNSUPPORTED,
          "Android did not expose a Companion Device Manager association id",
          nativeCode = "unsupportedAssociationMetadata"
        )
      )
      return
    }
    clear()
    owner(Result.success(CompanionAssociation(associationId.toLong(), peerId, displayName)))
  }

  @Synchronized
  private fun rejectIf(owner: (Result<CompanionAssociation>) -> Unit, failure: RadioPortFailure) {
    if (pending === owner) reject(failure)
  }

  @Synchronized
  private fun reject(failure: RadioPortFailure) {
    val owner = pending ?: return
    clear()
    owner(Result.failure(failure))
  }

  private fun clear() {
    pending = null
    pendingRequestCode = 0
    pendingAssociationId = 0
    uiLaunched = false
  }

  private fun connectPermitted(): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.S ||
      reactContext.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) == PackageManager.PERMISSION_GRANTED

  private fun deviceAddress(device: BluetoothDevice?): String? =
    if (device == null || !connectPermitted()) null else device.address

  @Suppress("MissingPermission")
  private fun deviceName(device: BluetoothDevice?): String? =
    if (device == null || !connectPermitted()) null else device.name

  private companion object {
    const val FIRST_REQUEST_CODE = 0x5552
  }
}
