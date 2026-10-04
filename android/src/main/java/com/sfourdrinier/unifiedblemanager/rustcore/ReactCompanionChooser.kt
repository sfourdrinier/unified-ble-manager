// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/ReactCompanionChooser.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import android.Manifest
import android.app.Activity
import android.bluetooth.BluetoothDevice
import android.bluetooth.le.ScanFilter
import android.companion.AssociationInfo
import android.companion.AssociationRequest
import android.companion.BluetoothLeDeviceFilter
import android.companion.CompanionDeviceManager
import android.content.Context
import android.content.Intent
import android.content.IntentSender
import android.content.pm.PackageManager
import android.os.Build
import android.os.ParcelUuid
import com.facebook.react.bridge.ActivityEventListener
import com.facebook.react.bridge.ReactApplicationContext
import com.sfourdrinier.unifiedblemanager.companion.CompanionAssociations
import java.util.regex.Pattern
import org.json.JSONArray

/**
 * Companion Device Manager chooser for the Rust route (`AssociateCompanion`),
 * bound to one React context's foreground Activity. Same platform behavior as
 * the legacy protocol-control association (API 33+, one association at a
 * time, the system UI result or `onAssociationCreated` resolves it).
 *
 * Finding 236: a named request first checks this app's existing associations
 * for the same display name and reports the existing record as
 * already-associated instead of launching the system UI into a duplicate.
 * Only an unscoped request (no name identifies the device) always reaches
 * the chooser.
 */
class ReactCompanionChooser @JvmOverloads constructor(
  private val reactContext: ReactApplicationContext,
  private val sdkInt: Int = Build.VERSION.SDK_INT,
  private val hasCompanionFeature: () -> Boolean = {
    reactContext.packageManager.hasSystemFeature(PackageManager.FEATURE_COMPANION_DEVICE_SETUP)
  }
) : CompanionPort, ActivityEventListener {
  private var pending: ((Result<CompanionAssociation>) -> Unit)? = null
  private var pendingRequestCode = 0
  private var pendingAssociationId = 0
  private var uiLaunched = false
  private var nextRequestCode = FIRST_REQUEST_CODE
  private var pendingActivity: Activity? = null
  private val cancelledBeforeAdmission = mutableSetOf<(Result<CompanionAssociation>) -> Unit>()

  fun available(): Boolean = sdkInt >= Build.VERSION_CODES.TIRAMISU && hasCompanionFeature()

  init {
    reactContext.addActivityEventListener(this)
  }

  @Synchronized
  fun detach() {
    // Context invalidation must retire the exact UI owner, not merely settle
    // its promise while leaving the system activity alive. Reuse cancellation
    // so a refused activity release keeps its owner rather than faking success.
    pending?.let { cancelAssociation(it) }
    reactContext.removeActivityEventListener(this)
  }

  @Synchronized
  override fun associate(name: String?, serviceUuid: String?, onResult: (Result<CompanionAssociation>) -> Unit) {
    associateRequest(name, serviceUuid, null, onResult)
  }

  @Synchronized
  override fun associateWithFilters(filtersJson: String, onResult: (Result<CompanionAssociation>) -> Unit) {
    associateRequest(null, null, filtersJson, onResult)
  }

  @Synchronized
  override fun cancelAssociation(onResult: (Result<CompanionAssociation>) -> Unit): Boolean {
    if (pending !== onResult) {
      if (cancelledBeforeAdmission.size >= 64) return false
      cancelledBeforeAdmission.add(onResult)
      return true
    }
    reactContext.runOnUiQueueThread {
      synchronized(this) {
        if (pending !== onResult) return@synchronized
        try {
          if (uiLaunched) pendingActivity?.finishActivity(pendingRequestCode)
          reject(RadioPortFailure(RadioFailureKind.CANCELLED, "Companion chooser cancelled", nativeCode = "associationCancelled"))
        } catch (error: RuntimeException) {
          // Keep pending ownership; a refused UI release is not a completed
          // cancellation and a subsequent attempt may retry it.
          android.util.Log.w("UBM", "Companion chooser cancellation refused", error)
        }
      }
    }
    return true
  }

  private fun associateRequest(name: String?, serviceUuid: String?, filtersJson: String?, onResult: (Result<CompanionAssociation>) -> Unit) {
    if (cancelledBeforeAdmission.remove(onResult)) {
      onResult(Result.failure(RadioPortFailure(RadioFailureKind.CANCELLED, "Chooser cancelled before admission")))
      return
    }
    if (sdkInt < Build.VERSION_CODES.TIRAMISU || !hasCompanionFeature()) {
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
    val manager = reactContext.getSystemService(Context.COMPANION_DEVICE_SERVICE) as? CompanionDeviceManager
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "Companion Device Manager is unavailable",
        nativeCode = "unsupportedAssociation"
      )
    val existing = findExistingAssociation(manager, name)
    if (existing != null) {
      onResult(
        Result.success(
          CompanionAssociation(
            existing.id.toLong(),
            existing.macAddress,
            existing.displayName,
            alreadyAssociated = true
          )
        )
      )
      return
    }
    val activity = reactContext.currentActivity
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "a foreground Activity is required to launch the chooser",
        nativeCode = "associationActivityUnavailable"
      )
    val request = if (filtersJson == null) buildCompanionAssociationRequest(name, serviceUuid)
      else buildFilteredCompanionAssociationRequest(filtersJson)
    val requestCode = nextRequestCode
    nextRequestCode = if (requestCode == Int.MAX_VALUE) FIRST_REQUEST_CODE else requestCode + 1
    pending = onResult
    pendingRequestCode = requestCode
    pendingAssociationId = 0
    uiLaunched = false
    pendingActivity = activity
    try {
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
    } catch (error: RuntimeException) {
      // A synchronous submission refusal accepted no UI obligation. Retire
      // only this owner; a synchronous callback may already have retired it.
      if (pending === onResult && !uiLaunched) clear()
      throw error
    }
  }

  @Synchronized
  override fun listAssociations(): List<CompanionAssociationRecord> {
    val manager = companionManager()
    return CompanionAssociations.summarize(manager.myAssociations).map {
      CompanionAssociationRecord(it.id.toLong(), it.macAddress, it.displayName)
    }
  }

  @Synchronized
  override fun disassociate(associationId: Long) {
    if (associationId <= 0 || associationId > Int.MAX_VALUE) {
      throw RadioPortFailure(
        RadioFailureKind.PEER_UNKNOWN,
        "no companion association carries id $associationId",
        nativeCode = "associationUnknown"
      )
    }
    val manager = companionManager()
    val known = CompanionAssociations.summarize(manager.myAssociations).any { it.id.toLong() == associationId }
    if (!known) {
      throw RadioPortFailure(
        RadioFailureKind.PEER_UNKNOWN,
        "no companion association carries id $associationId",
        nativeCode = "associationUnknown"
      )
    }
    manager.disassociate(associationId.toInt())
  }

  private fun companionManager(): CompanionDeviceManager {
    if (sdkInt < Build.VERSION_CODES.TIRAMISU || !hasCompanionFeature()) {
      throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "Companion Device Manager associations require Android API 33 and companion-device setup support",
        nativeCode = "unsupportedAssociation"
      )
    }
    return reactContext.getSystemService(Context.COMPANION_DEVICE_SERVICE) as? CompanionDeviceManager
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "Companion Device Manager is unavailable",
        nativeCode = "unsupportedAssociation"
      )
  }

  /**
   * This app's existing association for the requested device, if the name
   * identifies one. A lookup the OS refuses (or an unscoped request) is
   * not an association: the caller proceeds to the system chooser, whose
   * result reports what the platform did.
   */
  private fun findExistingAssociation(
    manager: CompanionDeviceManager,
    name: String?
  ): CompanionAssociations.Summary? {
    if (name == null) return null
    val associations = try {
      manager.myAssociations
    } catch (error: RuntimeException) {
      return null
    }
    return CompanionAssociations.findByDisplayName(CompanionAssociations.summarize(associations), name)
  }

  @Synchronized
  private fun launch(activity: Activity, sender: IntentSender, owner: (Result<CompanionAssociation>) -> Unit, requestCode: Int) {
    if (pending !== owner || pendingRequestCode != requestCode || uiLaunched) return
    reactContext.runOnUiQueueThread {
      synchronized(this) {
        if (pending !== owner || pendingRequestCode != requestCode || uiLaunched) return@synchronized
        try {
          uiLaunched = true
          activity.startIntentSenderForResult(sender, requestCode, null, 0, 0, 0)
        } catch (error: IntentSender.SendIntentException) {
          rejectIf(owner, RadioPortFailure(RadioFailureKind.PLATFORM,
            "Companion Device Manager system UI could not be launched", cause = error, nativeCode = "associationUiLaunchFailed"))
        }
      }
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
    pendingActivity = null
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

/** Public chooser filters are OR alternatives; each descriptor retains its
 * service/name/manufacturer conjunction. No regex or manufacturer-prefix widening. */
internal fun buildFilteredCompanionAssociationRequest(filtersJson: String): AssociationRequest {
  val entries = JSONArray(filtersJson)
  require(entries.length() in 1..16) { "invalid system chooser filter count" }
  val request = AssociationRequest.Builder().setSingleDevice(false)
  for (index in 0 until entries.length()) {
    val entry = entries.getJSONObject(index)
    val filter = BluetoothLeDeviceFilter.Builder()
    if (entry.has("namePrefix")) {
      val name = entry.getString("namePrefix")
      require(name.isNotEmpty())
      filter.setNamePattern(companionNamePrefixPattern(name))
    }
    val scan = ScanFilter.Builder()
    if (entry.has("serviceUuid")) scan.setServiceUuid(ParcelUuid.fromString(entry.getString("serviceUuid")))
    if (entry.has("companyIdentifier")) {
      val company = entry.getInt("companyIdentifier")
      require(company in 0..65535)
      val prefix = entry.optJSONArray("manufacturerPrefix") ?: JSONArray()
      require(prefix.length() <= 512)
      val bytes = ByteArray(prefix.length()) { byteIndex ->
        val byte = prefix.getInt(byteIndex); require(byte in 0..255); byte.toByte()
      }
      scan.setManufacturerData(company, bytes, ByteArray(bytes.size) { 0xff.toByte() })
    }
    filter.setScanFilter(scan.build())
    request.addDeviceFilter(filter.build())
  }
  return request.build()
}

internal fun companionNamePrefixPattern(prefix: String): Pattern = Pattern.compile("^" + Pattern.quote(prefix) + ".*", Pattern.DOTALL)

/**
 * Builds the Companion Device Manager association request for the chooser.
 * Test seam (finding 222): pure request construction, pinned by
 * `ReactCompanionChooserTest`.
 *
 * Finding 222: association targets BLE peripherals, so the filter is
 * `BluetoothLeDeviceFilter` (API 26+; association itself requires API 33+, so
 * the existing TIRAMISU gate already covers the filter floor — minSdk 24
 * never reaches here on older runtimes). The classic `BluetoothDeviceFilter`
 * only ever matched Bluetooth Classic peers, which is why the H10 never
 * appeared. Name scoping keeps the exact-name pattern; service-UUID scoping
 * rides the LE scan filter. `setSingleDevice` is only set when a name scopes
 * the request: unscoped + single-device offers an arbitrary device, which is
 * how the wrong association happened. This library is BLE-only, so there is
 * deliberately no classic/dual transport option.
 */
internal fun buildCompanionAssociationRequest(name: String?, serviceUuid: String?): AssociationRequest {
  val filter = BluetoothLeDeviceFilter.Builder()
  if (name != null) filter.setNamePattern(Pattern.compile(Pattern.quote(name)))
  if (serviceUuid != null) {
    filter.setScanFilter(ScanFilter.Builder().setServiceUuid(ParcelUuid.fromString(serviceUuid)).build())
  }
  return AssociationRequest.Builder()
    .addDeviceFilter(filter.build())
    .setSingleDevice(name != null)
    .build()
}
