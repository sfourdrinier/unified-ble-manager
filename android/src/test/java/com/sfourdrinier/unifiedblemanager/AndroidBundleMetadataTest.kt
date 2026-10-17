// android/src/test/java/com/sfourdrinier/unifiedblemanager/AndroidBundleMetadataTest.kt
package com.sfourdrinier.unifiedblemanager

import java.io.Serializable
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

class AndroidBundleMetadataTest {
  @Test
  fun `metadata helpers preserve string and boolean values through the Android boundary`() {
    val metadata = boundary(
      "text" to "value",
      "enabled" to true,
      "disabled" to false,
      "stringFalse" to "false",
      "stringBoolean" to "true",
    )

    assertEquals("value", AndroidBundleMetadata.optionalString(metadata, "text"))
    assertTrue(AndroidBundleMetadata.booleanOrDefault(metadata, "enabled", false))
    assertTrue(AndroidBundleMetadata.booleanOrDefault(metadata, "stringBoolean", false))
    assertFalse(AndroidBundleMetadata.booleanOrDefault(metadata, "disabled", true))
    assertFalse(AndroidBundleMetadata.booleanOrDefault(metadata, "stringFalse", true))
  }

  @Test
  fun `metadata helpers preserve absent defaults and reject wrong string types`() {
    val metadata = boundary("wrong" to 7)

    assertEquals(null, AndroidBundleMetadata.optionalString(metadata, "missing"))
    assertFalse(AndroidBundleMetadata.booleanOrDefault(metadata, "missing", false))
    assertTrue(AndroidBundleMetadata.booleanOrDefault(metadata, "missing", true))
    assertFalse(AndroidBundleMetadata.booleanOrDefault(metadata, "wrong", false))
    assertTrue(AndroidBundleMetadata.booleanOrDefault(metadata, "wrong", true))
    assertThrows(IllegalStateException::class.java) {
      AndroidBundleMetadata.optionalString(metadata, "wrong")
    }
  }

  @Test
  fun `explicit null metadata keeps the absent value policy`() {
    val metadata = boundary("null" to null)
    assertEquals(null, AndroidBundleMetadata.optionalString(metadata, "null"))
    assertTrue(AndroidBundleMetadata.booleanOrDefault(metadata, "null", true))
    assertFalse(AndroidBundleMetadata.booleanOrDefault(metadata, "null", false))
  }

  private fun boundary(vararg entries: Pair<String, Serializable?>): AndroidBundleMetadata.MetadataReader {
    val values = entries.toMap()
    return object : AndroidBundleMetadata.MetadataReader {
      override fun containsKey(key: String): Boolean = values.containsKey(key)

      override fun getSerializable(key: String): Serializable? = values[key]
    }
  }
}
