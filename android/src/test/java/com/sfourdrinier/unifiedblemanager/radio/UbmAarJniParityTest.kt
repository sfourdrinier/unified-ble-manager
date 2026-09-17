// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/UbmAarJniParityTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Guards the AAR-local `com.ubm` JNI declarations against drift from their
 * single owners in `bindings/jni/java` (signatures owned by the Rust cdylib
 * `Java_com_ubm_*` exports). The Android build files are frozen, so the
 * shipped AAR carries its own copies — any signature change must update both
 * trees, and this test fails the build otherwise. It also pins the binding's
 * contract revision against `contracts/src/version.ts`.
 *
 * Paths resolve from the module directory (the Gradle test working dir).
 */
class UbmAarJniParityTest {

  private fun read(relative: String): String {
    val file = File(relative)
    assertTrue("expected tree file $relative", file.isFile)
    return file.readText()
  }

  private fun nativeSignatures(source: String): Set<String> {
    val code = source
      .replace(Regex("/\\*[\\s\\S]*?\\*/"), "")
      .split('\n')
      .map { it.substringBefore("//") }
      .joinToString("\n")
    return code
      .split(';')
      .map { it.replace(Regex("\\s+"), " ").trim() }
      .filter { it.contains(" native ") }
      .map { it.substringAfter(" native ").trim() + ";" }
      .toSet()
  }

  @Test
  fun echoBridgeSignaturesMatchTheBindingsOwner() {
    val aar = nativeSignatures(read("src/main/java/com/ubm/echo/EchoBridge.java"))
    val owner = nativeSignatures(read("../bindings/jni/java/com/ubm/echo/EchoBridge.java"))
    assertEquals(owner, aar)
  }

  @Test
  fun gattBridgeSignaturesMatchTheBindingsOwner() {
    val aar = nativeSignatures(read("src/main/java/com/ubm/gatt/GattBridge.java"))
    val owner = nativeSignatures(read("../bindings/jni/java/com/ubm/gatt/GattBridge.java"))
    assertEquals(owner, aar)
  }

  @Test
  fun echoExceptionCarriesTheTypedIdentityOnBothSides() {
    for (relative in listOf(
      "src/main/java/com/ubm/echo/EchoException.java",
      "../bindings/jni/java/com/ubm/echo/EchoException.java"
    )) {
      val source = read(relative)
      for (token in listOf("code()", "domain()", "operation()", "detail()", "extends RuntimeException")) {
        assertTrue("$relative must declare $token", source.contains(token))
      }
    }
  }

  @Test
  fun bindingRevisionMatchesTheFrozenContractsRevision() {
    val version = read("../contracts/src/version.ts")
    val match = Regex("CONTRACT_REVISION[^=]*=[^'\"]*['\"]([^'\"]+)['\"]").find(version)
    assertNotNull(match, "contracts revision")
    assertEquals(match!!.groupValues[1], UbmGattCoreBinding.CONTRACT_REVISION)
  }

  private fun assertNotNull(value: Any?, label: String) {
    assertTrue("expected $label", value != null)
  }
}
