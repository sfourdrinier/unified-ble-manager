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
  val detail: String?,
  val platform: Map<*, *>? = null
) : RuntimeException("$code|$domain|$operation|${detail ?: ""}") {

  fun toJson(): String {
    val failure = linkedMapOf<String, Any?>("code" to code, "domain" to domain, "operation" to operation, "detail" to detail)
    platform?.let { failure["platform"] = it }
    return RustCoreJson.write(failure)
  }

  /** Host-side refusal has no write commit and no platform retry advice.
   * Native envelopes are forwarded unchanged instead of reconstructed here. */
  fun toContinuationEnvelope(): String = RustCoreJson.write(linkedMapOf(
    "ok" to false,
    "error" to linkedMapOf("code" to code, "domain" to domain, "operation" to operation,
      "detail" to detail, "platform" to platform),
    "commit" to null,
    "retryability" to "never"
  ))

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
