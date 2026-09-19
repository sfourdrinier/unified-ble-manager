// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCorePlatformValues.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import java.io.ByteArrayOutputStream
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.security.SecureRandom

/**
 * Platform values the Rust route's module answers without Rust: CSPRNG bytes
 * and the app-scoped restoration identity. The restoration derivation is
 * byte-for-byte the legacy protocol-control `bootstrapRestorationIdentity`
 * (domain `ubm-restoration-v1`, length-prefixed SHA-256, base64url), so an
 * app keeps the same identity across the route change.
 */
object RustCorePlatformValues {
  const val MAX_RANDOM_BYTES = 1024
  private const val RESTORATION_DOMAIN = "ubm-restoration-v1"
  private val RESTORATION_TOKEN = Regex("[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
  private val URL_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_".toCharArray()
  private val STD_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".toCharArray()

  @JvmStatic
  fun randomBytesBase64(length: Int, random: SecureRandom): String {
    if (length < 1 || length > MAX_RANDOM_BYTES) {
      throw RustCoreRejection.invalid("random.bytes", "length must be 1..$MAX_RANDOM_BYTES, got $length")
    }
    val bytes = ByteArray(length)
    random.nextBytes(bytes)
    return base64(bytes, STD_ALPHABET, padded = true)
  }

  /**
   * `requestJson` is exactly `{restorationId, generation}`; answers the seven-field identity JSON.
   * An empty request `{}` asks for the natively configured identity: Android configures none
   * (it has no state restoration), so the answer is `null`.
   */
  @JvmStatic
  fun restorationIdentity(applicationId: String?, requestJson: String): String {
    val operation = "restoration.identity"
    fun refuse(detail: String): Nothing = throw RustCoreRejection("platform.failure", "platform", operation, detail)
    if (applicationId.isNullOrEmpty()) refuse("the Android application id is unavailable")
    val request = try {
      RustCoreJson.parse(requestJson)
    } catch (error: IllegalArgumentException) {
      refuse("request is not JSON: ${error.message}")
    }
    if (request is Map<*, *> && request.isEmpty()) return "null"
    if (request !is Map<*, *> || request.keys != setOf("restorationId", "generation")) {
      refuse("request must be exactly {restorationId, generation}")
    }
    fun token(key: String, maximumBytes: Int): String {
      val value = request[key]
      if (value !is String || !RESTORATION_TOKEN.matches(value) ||
        value.toByteArray(StandardCharsets.UTF_8).size > maximumBytes
      ) {
        refuse("invalid restoration token: $key")
      }
      return value
    }
    val restorationId = token("restorationId", 128)
    val generation = token("generation", 64)
    fun derive(label: String) = deriveRestorationValue(applicationId, restorationId, generation, label)
    return RustCoreJson.write(
      linkedMapOf(
        "applicationId" to applicationId,
        "restorationId" to restorationId,
        "generation" to generation,
        "restoreIdentifier" to "$applicationId.ubm.${derive("restore").substring(0, 22)}",
        "namespaceValue" to "ubm-ns:${derive("namespace")}",
        "clientId" to "ubm-client:${derive("client")}",
        "hostSessionScope" to "ubm-host:${derive("host")}"
      )
    )
  }

  private fun deriveRestorationValue(applicationId: String, restorationId: String, generation: String, label: String): String {
    val root = sha256(
      concatenate(
        utf8(RESTORATION_DOMAIN),
        lengthPrefixed(applicationId),
        lengthPrefixed(restorationId),
        lengthPrefixed(generation)
      )
    )
    return base64(sha256(concatenate(root, byteArrayOf(0), utf8(label))), URL_ALPHABET, padded = false)
  }

  private fun utf8(value: String) = value.toByteArray(StandardCharsets.UTF_8)

  private fun lengthPrefixed(value: String): ByteArray {
    val bytes = utf8(value)
    val output = ByteArrayOutputStream(4 + bytes.size)
    output.write((bytes.size ushr 24) and 0xff)
    output.write((bytes.size ushr 16) and 0xff)
    output.write((bytes.size ushr 8) and 0xff)
    output.write(bytes.size and 0xff)
    output.write(bytes, 0, bytes.size)
    return output.toByteArray()
  }

  private fun concatenate(vararg values: ByteArray): ByteArray {
    val output = ByteArrayOutputStream()
    values.forEach { output.write(it, 0, it.size) }
    return output.toByteArray()
  }

  private fun sha256(value: ByteArray): ByteArray = MessageDigest.getInstance("SHA-256").digest(value)

  private fun base64(value: ByteArray, alphabet: CharArray, padded: Boolean): String {
    val output = StringBuilder((value.size + 2) / 3 * 4)
    var index = 0
    while (index < value.size) {
      val first = value[index].toInt() and 0xff
      val second = if (index + 1 < value.size) value[index + 1].toInt() and 0xff else 0
      val third = if (index + 2 < value.size) value[index + 2].toInt() and 0xff else 0
      output.append(alphabet[first ushr 2])
      output.append(alphabet[((first and 0x03) shl 4) or (second ushr 4)])
      if (index + 1 < value.size) output.append(alphabet[((second and 0x0f) shl 2) or (third ushr 6)]) else if (padded) output.append('=')
      if (index + 2 < value.size) output.append(alphabet[third and 0x3f]) else if (padded) output.append('=')
      index += 3
    }
    return output.toString()
  }
}
