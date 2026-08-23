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

  @Test
  fun `stop durably clears the session intent before requesting service stop`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/background/AndroidConnectedDeviceForegroundServiceDriver.java"
    )
    val stop = source.substring(
      source.indexOf("public void stop()"),
      source.indexOf("private ForegroundServiceNotificationConfiguration configuration()")
    )
    val preferenceWrite = stop.indexOf(
      ".putBoolean(BlePlxForegroundService.SESSION_INTENT_PREFERENCE, false)"
    )
    val serviceStop = stop.indexOf("context.stopService")

    assertTrue(preferenceWrite >= 0)
    assertTrue(preferenceWrite < serviceStop)
    assertTrue(stop.substring(preferenceWrite, serviceStop).contains(".commit()"))
    assertTrue(!stop.substring(preferenceWrite, serviceStop).contains(".apply()"))
  }

  @Test
  fun `ack receiver uses typed parcelable retrieval on API 33 and a guarded legacy path`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxForegroundService.java"
    )
    val acknowledgement = source.substring(source.indexOf("private void acknowledge"))

    assertTrue(acknowledgement.contains("Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU"))
    assertTrue(
      acknowledgement.contains(
        "intent.getParcelableExtra(EXTRA_ACK, ResultReceiver.class)"
      )
    )
    assertTrue(acknowledgement.contains("intent.getParcelableExtra(EXTRA_ACK);"))
  }

  @Test
  fun `caller restart policy remains the service restart policy`() {
    val source = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxForegroundService.java"
    )

    assertTrue(
      source.contains(
        "return configuration.restartWhileSessionIntentExists() ? START_STICKY : START_NOT_STICKY;"
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
