// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreRejection.kt

package com.sfourdrinier.unifiedblemanager.rustcore

/**
 * A structured module rejection. JS reads the promise error message as the
 * JSON object `{"code","domain","operation","detail"}` (detail string or
 * null); anything else is `protocol.malformed` on the JS side.
 */
class RustCoreRejection(
  val code: String,
  val domain: String,
  val operation: String,
  val detail: String?
) : RuntimeException("$code|$domain|$operation|${detail ?: ""}") {

  fun toJson(): String = RustCoreJson.write(
    linkedMapOf("code" to code, "domain" to domain, "operation" to operation, "detail" to detail)
  )

  companion object {
    /** Parses the JNI `MobileCoreException` wire text `code|domain|operation|detail`. */
    @JvmStatic
    fun fromWire(wire: String?, fallbackOperation: String): RustCoreRejection {
      val parts = (wire ?: "").split("|", limit = 4)
      if (parts.size < 3 || parts[0].isEmpty() || parts[1].isEmpty()) {
        return RustCoreRejection("platform.failure", "platform", fallbackOperation, wire ?: "native call failed")
      }
      val detail = parts.getOrNull(3)?.takeIf { it.isNotEmpty() }
      return RustCoreRejection(parts[0], parts[1], parts[2], detail)
    }

    @JvmStatic
    fun platform(operation: String, error: Throwable): RustCoreRejection =
      RustCoreRejection("platform.failure", "platform", operation, error.message ?: error.javaClass.simpleName)

    @JvmStatic
    fun invalid(operation: String, detail: String): RustCoreRejection =
      RustCoreRejection("argument.invalid", "core", operation, detail)
  }
}
