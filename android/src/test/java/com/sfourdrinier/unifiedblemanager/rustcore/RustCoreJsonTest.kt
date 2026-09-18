// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreJsonTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test

class RustCoreJsonTest {
  @Test
  fun parsesNestedEnvelopes() {
    val parsed = RustCoreJson.parse(
      "{ \"ok\" : false, \"error\": {\"code\":\"gatt.write-failed\",\"detail\":\"a \\\"quoted\\\" \\u00e9\"}, \"commit\":\"uncertain\", \"n\": [1,-2,null,true] }"
    ) as Map<*, *>
    assertEquals(false, parsed["ok"])
    assertEquals("a \"quoted\" é", (parsed["error"] as Map<*, *>)["detail"])
    assertEquals(listOf(1L, -2L, null, true), parsed["n"])
  }

  @Test
  fun refusesMalformedAndNonIntegerInput() {
    listOf("", "{", "{\"a\":1,}", "{\"a\":1}x", "[1.5]", "{\"a\":1,\"a\":2}", "\"\u0001\"").forEach { text ->
      assertThrows(text, IllegalArgumentException::class.java) { RustCoreJson.parse(text) }
    }
  }

  @Test
  fun writesEscapedOrderedJson() {
    assertEquals(
      "{\"b\":\"x\\\"y\\n\",\"a\":[1,null,true],\"c\":{}}",
      RustCoreJson.write(linkedMapOf("b" to "x\"y\n", "a" to listOf(1, null, true), "c" to emptyMap<String, Any>()))
    )
  }
}
