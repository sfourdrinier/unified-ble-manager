// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/BackgroundContinuationDeclarationTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

/** Parses the canonical `background.continuation` JSON the binding persists (BGS4). */
class BackgroundContinuationDeclarationTest {
  private val hrService = "0000180d-0000-1000-8000-00805f9b34fb"
  private val hrMeasurement = "00002a37-0000-1000-8000-00805f9b34fb"

  @Test
  fun absentMeansRecordOnly() {
    val parsed = BackgroundContinuationDeclaration.parse(null)
    assertEquals(ContinuationStrategy.RECORD_ONLY, parsed.strategy)
    assertEquals(emptyList<ContinuationSelector>(), parsed.resubscribe)
  }

  @Test
  fun parsesANativeStandingOrder() {
    val parsed = BackgroundContinuationDeclaration.parse(
      "{\"onAppearance\":\"native\",\"peerId\":\"a0:9e:1a:e9:b9:3d\"," +
        "\"resubscribe\":[{\"serviceUuid\":\"$hrService\",\"characteristicUuid\":\"$hrMeasurement\"}]}"
    )
    assertEquals(ContinuationStrategy.NATIVE, parsed.strategy)
    assertEquals("A0:9E:1A:E9:B9:3D", parsed.peerId)
    assertEquals(
      listOf(ContinuationSelector(hrService, 1, hrMeasurement, 1)),
      parsed.resubscribe
    )
  }

  @Test
  fun uuidsAreCanonicalisedAndOccurrencesDefault() {
    val parsed = BackgroundContinuationDeclaration.parse(
      "{\"onAppearance\":\"native\"," +
        "\"resubscribe\":[{\"serviceUuid\":\"${hrService.uppercase()}\"," +
        "\"serviceOccurrence\":2,\"characteristicUuid\":\"${hrMeasurement.uppercase()}\"}]}"
    )
    assertEquals(
      listOf(ContinuationSelector(hrService, 2, hrMeasurement, 1)),
      parsed.resubscribe
    )
  }

  @Test
  fun fractionalOccurrencesAreRejectedInsteadOfTruncated() {
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse(
        "{\"onAppearance\":\"native\",\"resubscribe\":[{\"serviceUuid\":\"$hrService\",\"serviceOccurrence\":1.5," +
          "\"characteristicUuid\":\"$hrMeasurement\"}]}"
      )
    }
  }

  @Test
  fun occurrencesRejectExplicitNullAndUnsafeIntegersForBothSelectors() {
    for (field in listOf("serviceOccurrence", "characteristicOccurrence")) {
      for (invalid in listOf("null", "9007199254740992", "9223372036854775807", "0", "-1", "1.5", "true", "\"2\"")) {
        assertThrows("$field=$invalid", IllegalArgumentException::class.java) {
          BackgroundContinuationDeclaration.parse(
            """{"onAppearance":"native","resubscribe":[{"serviceUuid":"$hrService","characteristicUuid":"$hrMeasurement","$field":$invalid}]}"""
          )
        }
      }
    }
  }

  @Test
  fun occurrencesAcceptExactSafeIntegerBoundary() {
    val parsed = BackgroundContinuationDeclaration.parse(
      """{"onAppearance":"native","resubscribe":[{"serviceUuid":"$hrService","characteristicUuid":"$hrMeasurement","serviceOccurrence":9007199254740991,"characteristicOccurrence":9007199254740991}]}"""
    )
    assertEquals(9007199254740991L, parsed.resubscribe.single().serviceOccurrence)
    assertEquals(9007199254740991L, parsed.resubscribe.single().characteristicOccurrence)
  }

  @Test
  fun optionalNotificationTextRejectsInvalidPresentValues() {
    for (field in listOf("body", "icon")) {
      for (invalid in listOf("null", "true", "2", "[]", "{}", "\"\"")) {
        assertThrows("$field=$invalid", IllegalArgumentException::class.java) {
          BackgroundContinuationDeclaration.parse(
            """{"onAppearance":"foreground-service","foregroundService":{"notification":{"channelId":"ble","channelName":"BLE","title":"BLE","$field":$invalid}}}"""
          )
        }
      }
    }
    val parsed = BackgroundContinuationDeclaration.parse(
      """{"onAppearance":"foreground-service","foregroundService":{"notification":{"channelId":"ble","channelName":"BLE","title":"BLE","body":"Recording","icon":"ble_icon"}}}"""
    )
    assertEquals("Recording", parsed.foregroundService?.notification?.body)
    assertEquals("ble_icon", parsed.foregroundService?.notification?.icon)
  }

  @Test
  fun unknownStrategiesAndKeysAreRefusedNeverSubstituted() {
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse("{\"onAppearance\":\"auto-magic\",\"resubscribe\":[]}")
    }
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse("{\"onAppearance\":\"record-only\",\"retryMs\":5}")
    }
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse("{\"onAppearance\":\"native\",\"peerId\":\"not-a-mac\",\"resubscribe\":[]}")
    }
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse(
        "{\"onAppearance\":\"native\",\"resubscribe\":[{\"serviceUuid\":\"$hrService\"}]}"
      )
    }
  }

  @Test
  fun headlessTaskNamesItsTaskForegroundServiceItsNotification() {
    val headless = BackgroundContinuationDeclaration.parse(
      "{\"onAppearance\":\"headless-task\",\"headlessTaskName\":\"BleWakeTask\",\"resubscribe\":[]}"
    )
    assertEquals("BleWakeTask", headless.headlessTaskName)
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse("{\"onAppearance\":\"headless-task\",\"resubscribe\":[]}")
    }
    val fgs = BackgroundContinuationDeclaration.parse(
      "{\"onAppearance\":\"foreground-service\"," +
        "\"foregroundService\":{\"notification\":{\"channelId\":\"ble\",\"channelName\":\"BLE\",\"title\":\"BLE\"}}," +
        "\"resubscribe\":[]}"
    )
    assertEquals("BLE", fgs.foregroundService?.notification?.title)
    assertThrows(IllegalArgumentException::class.java) {
      BackgroundContinuationDeclaration.parse("{\"onAppearance\":\"foreground-service\",\"resubscribe\":[]}")
    }
  }

  @Test
  fun recordOnlyCarriesNoTask() {
    assertNull(BackgroundContinuationDeclaration.parse(null).headlessTaskName)
    assertNull(BackgroundContinuationDeclaration.parse(null).foregroundService)
  }
}
