package com.sfourdrinier.unifiedblemanager.background

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ForegroundServiceNotificationConfigurationTest {
  @Test
  fun `loads deterministic notification fields from managed metadata`() {
    val configuration = ForegroundServiceNotificationConfiguration.fromMetadata(
      mapOf(
        ForegroundServiceNotificationConfiguration.OWNERSHIP_METADATA to "service=1",
        ForegroundServiceNotificationConfiguration.CHANNEL_ID_METADATA to "ble-session",
        ForegroundServiceNotificationConfiguration.CHANNEL_NAME_METADATA to "BLE session",
        ForegroundServiceNotificationConfiguration.TITLE_METADATA to "Connected device active",
        ForegroundServiceNotificationConfiguration.BODY_METADATA to "Workout in progress",
        ForegroundServiceNotificationConfiguration.ICON_METADATA to "ic_ble"
      )
    )

    assertEquals("ble-session", configuration.channelId)
    assertEquals("BLE session", configuration.channelName)
    assertEquals("Connected device active", configuration.title)
    assertEquals("Workout in progress", configuration.body)
    assertEquals("ic_ble", configuration.iconName)
    assertEquals(0x55424d, ForegroundServiceNotificationConfiguration.NOTIFICATION_ID)
  }

  @Test
  fun `optional body and icon remain absent instead of receiving invented defaults`() {
    val configuration = ForegroundServiceNotificationConfiguration.fromMetadata(requiredMetadata())

    assertNull(configuration.body)
    assertNull(configuration.iconName)
  }

  @Test
  fun `missing ownership or required notification metadata is actionable`() {
    val failure = runCatching {
      ForegroundServiceNotificationConfiguration.fromMetadata(
        requiredMetadata() - ForegroundServiceNotificationConfiguration.OWNERSHIP_METADATA
      )
    }.exceptionOrNull()

    assertTrue(failure is ForegroundServiceControlException)
    assertEquals("foregroundServiceNotConfigured", (failure as ForegroundServiceControlException).code)
  }

  private fun requiredMetadata() = mapOf(
    ForegroundServiceNotificationConfiguration.OWNERSHIP_METADATA to "service=1",
    ForegroundServiceNotificationConfiguration.CHANNEL_ID_METADATA to "ble-session",
    ForegroundServiceNotificationConfiguration.CHANNEL_NAME_METADATA to "BLE session",
    ForegroundServiceNotificationConfiguration.TITLE_METADATA to "Connected device active"
  )
}
