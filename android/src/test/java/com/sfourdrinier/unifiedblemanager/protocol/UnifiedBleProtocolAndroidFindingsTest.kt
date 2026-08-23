package com.sfourdrinier.unifiedblemanager.protocol

import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

class UnifiedBleProtocolAndroidFindingsTest {
  @Test
  fun `JNI handshake storage covers every advertised version axis`() {
    val source = readAndroidSource(
      "android/src/main/jni/UnifiedBleProtocolControlJni.cpp"
    )

    assertTrue(source.contains("GetArrayLength(versionRanges) != 14"))
    assertTrue(source.contains("std::array<jlong, 14> ranges{};"))
    assertTrue(source.contains("range(12U)"))
  }

  @Test
  fun `invalidation retains the native runtime when dispatcher cleanup remains retryable`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val invalidation = source.substring(
      source.indexOf("public synchronized void invalidate"),
      source.indexOf("private void closeOwnedState")
    )

    assertTrue(
      Regex(
        """boolean dispatcherClosed = false;[\s\S]*?UnifiedBleProtocolJsiBinding\.close\(handle\);[\s\S]*?dispatcherClosed = true;[\s\S]*?if \(dispatcherClosed\) \{[\s\S]*?nativeDestroy\(handle\);"""
      ).containsMatchIn(invalidation)
    )
  }

  @Test
  fun `companion association captures its native ID before delivering result metadata`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val association = source.substring(
      source.indexOf("public synchronized void associateCompanionDevice"),
      source.indexOf("public synchronized void claimRestoration")
    )
    val activityResult = source.substring(
      source.indexOf("public synchronized void onActivityResult"),
      source.indexOf("public void onNewIntent")
    )

    assertTrue(
      Regex(
        """onAssociationCreated\(AssociationInfo associationInfo\) \{[\s\S]*?pendingAssociationId = associationInfo\.getId\(\);"""
      ).containsMatchIn(association)
    )
    assertTrue(activityResult.contains("CompanionDeviceManager.EXTRA_ASSOCIATION"))
    assertTrue(
      Regex(
        """AssociationInfo associationInfo = data\.getParcelableExtra[\s\S]*?pendingAssociationId = associationInfo\.getId\(\);"""
      ).containsMatchIn(activityResult)
    )
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
