// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidFindingsTest.kt
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
  fun `JNI handshake validates version values within uint32 before narrowing`() {
    val source = readAndroidSource(
      "android/src/main/jni/UnifiedBleProtocolControlJni.cpp"
    )

    assertTrue(
      Regex(
        "if \\(value < 0 \\|\\| value > static_cast<jlong>\\(std::numeric_limits<std::uint32_t>::max\\(\\)\\)\\)"
      ).containsMatchIn(source)
    )
    assertTrue(source.contains("checkedVersionValue(ranges[offset])"))
    assertTrue(source.contains("checkedVersionValue(ranges[offset + 1U])"))
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
        """onAssociationCreated\(AssociationInfo associationInfo\) \{[\s\S]*?handleAssociationCreated\(associationPromise, associationInfo\);"""
      ).containsMatchIn(association)
    )
    assertTrue(activityResult.contains("CompanionDeviceManager.EXTRA_ASSOCIATION"))
    assertTrue(
      Regex(
        """AssociationInfo associationInfo = data\.getParcelableExtra[\s\S]*?pendingAssociationId = associationInfo\.getId\(\);"""
      ).containsMatchIn(activityResult)
    )
  }

  @Test
  fun `companion association fails closed instead of resolving association ID zero`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val resolver = source.substring(
      source.indexOf("private synchronized void resolveAssociation"),
      source.indexOf("private synchronized void rejectAssociation")
    )

    assertTrue(resolver.contains("if (associationId <= 0)"))
    assertTrue(resolver.contains("unsupportedAssociationMetadata"))
    assertTrue(
      resolver.indexOf("if (associationId <= 0)") <
        resolver.indexOf("result.putInt(\"associationId\", associationId)")
    )
  }

  @Test
  fun `companion association is unsupported before UI on pre API 33 hosts`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val association = source.substring(
      source.indexOf("public synchronized void associateCompanionDevice"),
      source.indexOf("public synchronized void claimRestoration")
    )

    assertTrue(
      Regex(
        "if \\(Build\\.VERSION\\.SDK_INT < Build\\.VERSION_CODES\\.TIRAMISU \\|\\|[\\s\\S]*?FEATURE_COMPANION_DEVICE_SETUP\\)"
      ).containsMatchIn(association)
    )
    assertTrue(association.contains("requires Android API 33"))
    assertTrue(
      association.indexOf("unsupportedAssociation") <
        association.indexOf("reactContext.getCurrentActivity()")
    )
    assertTrue(
      association.indexOf("unsupportedAssociation") <
        association.indexOf("manager.associate")
    )
    assertTrue(
      source.indexOf("unsupportedAssociation") <
        source.indexOf("startIntentSenderForResult")
    )
  }

  @Test
  fun `AssociationInfo getters are explicitly guarded on API 33`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val associationCreated = source.substring(
      source.indexOf("private synchronized void handleAssociationCreated"),
      source.indexOf("private synchronized void resolveAssociation")
    )
    val peerProjection = source.substring(
      source.indexOf("private static String associationPeerId"),
      source.indexOf("private static String associationDisplayName")
    )
    val displayProjection = source.substring(
      source.indexOf("private static String associationDisplayName"),
      source.indexOf("private static void rejectAssociationPromise")
    )

    assertTrue(
      Regex(
        """if \(Build\.VERSION\.SDK_INT < Build\.VERSION_CODES\.TIRAMISU \|\| associationInfo == null\) \{[\s\S]*?unsupportedAssociationMetadata[\s\S]*?associationInfo\.getId\(\)"""
      ).containsMatchIn(associationCreated)
    )
    assertTrue(
      Regex(
        """if \(Build\.VERSION\.SDK_INT < Build\.VERSION_CODES\.TIRAMISU\) return null;[\s\S]*?getDeviceMacAddress\(\)"""
      ).containsMatchIn(peerProjection)
    )
    assertTrue(
      Regex(
        """if \(Build\.VERSION\.SDK_INT < Build\.VERSION_CODES\.TIRAMISU\) return null;[\s\S]*?getDisplayName\(\)"""
      ).containsMatchIn(displayProjection)
    )
  }

  @Test
  fun `API 33 association results fail closed without an AssociationInfo record`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val activityResult = source.substring(
      source.indexOf("public synchronized void onActivityResult"),
      source.indexOf("public void onNewIntent")
    )
    assertTrue(activityResult.contains("unsupportedAssociationMetadata"))
    assertTrue(activityResult.contains("did not return the API 33 Companion Device Manager association record"))
    assertTrue(!activityResult.contains("EXTRA_DEVICE"))
  }

  @Test
  fun `companion callbacks retain request ownership through synchronized state helpers`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val association = source.substring(
      source.indexOf("public synchronized void associateCompanionDevice"),
      source.indexOf("public synchronized void claimRestoration")
    )
    val callback = association.substring(
      association.indexOf("new CompanionDeviceManager.Callback()"),
      association.indexOf("}, null);")
    )

    assertTrue(callback.contains("final Promise associationPromise = promise;"))
    assertTrue(callback.contains("launchAssociationUi(activity, intentSender, associationPromise"))
    assertTrue(callback.contains("handleAssociationCreated(associationPromise, associationInfo);"))
    assertTrue(Regex("rejectAssociation\\(\\s*associationPromise,").containsMatchIn(callback))
    assertTrue(source.contains("private int pendingAssociationRequestCode = 0;"))
    assertTrue(source.contains("requestCode != pendingAssociationRequestCode"))
    assertTrue(
      Regex("private synchronized void (launchAssociationUi|handleAssociationCreated|resolveAssociation|rejectAssociation|clearPendingAssociation)")
        .findAll(source)
        .count() >= 5
    )
    assertTrue(
      Regex("pendingAssociation != associationPromise")
        .findAll(source)
        .count() >= 4
    )
  }

  @Test
  fun `API 33 association results do not use the deprecated device extra`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java"
    )
    val activityResult = source.substring(
      source.indexOf("public synchronized void onActivityResult"),
      source.indexOf("public void onNewIntent")
    )
    assertTrue(activityResult.contains("unsupportedAssociationMetadata"))
    assertTrue(!activityResult.contains("CompanionDeviceManager.EXTRA_DEVICE"))
  }

  // Regression guard for issue #140: the dispatcher stamped a literal 1 on
  // field 1 of every control-plane record while the codec required 2, so every
  // result it emitted -- including the one describing a failed connect -- was
  // quarantined by the native codec and never reached JavaScript. The stamp has
  // to follow the generated schema, not a number typed next to the field id.
  @Test
  fun `dispatcher records stamp the generated protocol version`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )

    assertTrue(source.contains("import com.sfourdrinier.unifiedblemanager.protocol.generated.NATIVE_PROTOCOL_VERSION"))
    val stamps = Regex("""\n\s*1 to ProtocolWireValue\.UnsignedIntegerValue\((.+)\),""").findAll(source).toList()
    assertTrue(stamps.isNotEmpty())
    assertTrue(stamps.all { it.groupValues[1] == "NATIVE_PROTOCOL_VERSION.toLong()" })
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
