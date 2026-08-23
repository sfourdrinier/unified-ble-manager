package com.sfourdrinier.unifiedblemanager.protocol

import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

class UnifiedBleProtocolControlModuleLifecycleTest {
  @Test
  fun `synchronous companion association failure clears the pending owner before rejecting`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val association = source.substring(
      source.indexOf("public synchronized void associateCompanionDevice"),
      source.indexOf("public synchronized void claimRestoration")
    )

    assertTrue(
      Regex(
        "pendingAssociation = promise;[\\s\\S]*?catch \\(RuntimeException error\\) \\{[\\s\\S]*?clearPendingAssociation\\(promise\\);[\\s\\S]*?rejectAssociationPromise"
      ).containsMatchIn(association)
    )
  }

  @Test
  fun `invalidation keeps trying native cleanup after background cleanup fails`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val invalidation = source.substring(
      source.indexOf("public synchronized void invalidate"),
      source.indexOf("private void closeOwnedState")
    )

    assertTrue(invalidation.contains("backgroundLeases.close();"))
    assertTrue(invalidation.contains("UnifiedBleProtocolJsiBinding.close(handle);"))
    assertTrue(invalidation.contains("nativeDestroy(handle);"))
    assertTrue(invalidation.contains("appendCleanupFailure"))
  }

  @Test
  fun `control surface remains part of the versioned handshake`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )

    assertTrue(source.contains("requireVersionRange(request.getMap(\"controlSurface\")"))
    assertTrue(source.contains("result.putInt(\"controlSurface\", CONTROL_SURFACE_VERSION)"))
  }

  private fun readAndroidSource(relativePath: String): String {
    val candidates = listOf(
      File(relativePath),
      File("../$relativePath"),
      File("../../$relativePath"),
      File("../../../$relativePath")
    )
    return candidates.firstOrNull { it.isFile }?.readText()
      ?: throw AssertionError("Unable to locate Android source guard target: $relativePath")
  }
}
