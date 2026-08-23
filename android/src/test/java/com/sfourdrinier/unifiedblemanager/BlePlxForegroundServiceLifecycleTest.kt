package com.sfourdrinier.unifiedblemanager

import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

class BlePlxForegroundServiceLifecycleTest {
  @Test
  fun `persists caller session intent before foreground promotion and start acknowledgement`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxForegroundService.java"
    )
    val preferenceWrite = source.indexOf(
      ".putBoolean(SESSION_INTENT_PREFERENCE, configuration.restartWhileSessionIntentExists())"
    )
    val foregroundPromotion = source.indexOf("startForeground(")
    val startedAcknowledgement = source.indexOf("acknowledge(intent, ACK_STARTED")

    assertTrue(preferenceWrite >= 0)
    assertTrue(preferenceWrite < foregroundPromotion)
    assertTrue(preferenceWrite < startedAcknowledgement)
    assertTrue(
      source.substring(preferenceWrite, foregroundPromotion).contains(".commit()")
    )
  }

  @Test
  fun `failed foreground start clears the persisted session intent`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxForegroundService.java"
    )
    val failure = source.substring(source.indexOf("} catch (RuntimeException error)"))

    assertTrue(
      failure.contains(
        ".putBoolean(SESSION_INTENT_PREFERENCE, false)"
      )
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
