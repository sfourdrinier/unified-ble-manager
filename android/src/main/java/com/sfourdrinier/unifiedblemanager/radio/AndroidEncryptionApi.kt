package com.sfourdrinier.unifiedblemanager.radio

import java.lang.reflect.InvocationTargetException

internal class AndroidEncryptionFailure(val status: Int) : IllegalStateException("HCI Encryption Change failed status=$status")

/** Public 36.1 snapshot lookup, retaining the compile-SDK 36.0 floor. */
internal class AndroidEncryptionApi(fullSdk: Int, deviceClass: Class<*>) {
  private val snapshot = if (fullSdk < 3600001) null else {
    try { deviceClass.getMethod("getEncryptionStatus", Int::class.javaPrimitiveType) }
    catch (_: NoSuchMethodException) { null }
  }
  val snapshotAvailable: Boolean get() = snapshot != null

  fun read(device: Any): String {
    val method = snapshot ?: return "unsupported"
    val status = try { method.invoke(device, 2) }
    catch (error: InvocationTargetException) { throw error.targetException }
    // Null means unencrypted OR disconnected. A separate link query cannot disambiguate it atomically.
    if (status == null) return "unknown"
    val algorithm = status.javaClass.getMethod("getAlgorithm").invoke(status)
    val keySize = status.javaClass.getMethod("getKeySize").invoke(status)
    check(algorithm is Int && keySize is Int && keySize in 1..16) { "invalid Android encryption snapshot" }
    return when (algorithm) { 0 -> "not-encrypted"; 1, 2, 3 -> "encrypted"; else -> "unknown" }
  }

  companion object {
    fun event(status: Int, enabled: Boolean?): String {
      if (status != 0) throw AndroidEncryptionFailure(status)
      require(enabled != null) { "Encryption Change omitted encryption-enabled" }
      return if (enabled) "encrypted" else "not-encrypted"
    }
  }
}
