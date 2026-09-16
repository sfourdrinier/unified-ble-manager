package com.sfourdrinier.unifiedblemanager.background

import android.content.Intent
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class RecoveryDecisionTest {
  @Test
  fun `recovers on boot completed with session intent and configured restart`() {
    assertTrue(BlePlxForegroundServiceRecoveryReceiver.shouldRecover(
      Intent.ACTION_BOOT_COMPLETED, true, true))
  }

  @Test
  fun `recovers on package replaced with session intent and configured restart`() {
    assertTrue(BlePlxForegroundServiceRecoveryReceiver.shouldRecover(
      Intent.ACTION_MY_PACKAGE_REPLACED, true, true))
  }

  @Test
  fun `fails closed on unrelated actions`() {
    for (action in listOf(Intent.ACTION_POWER_CONNECTED, "com.example.OTHER", "", null)) {
      assertFalse("action=$action", BlePlxForegroundServiceRecoveryReceiver.shouldRecover(
        action, true, true))
    }
  }

  @Test
  fun `fails closed without session intent or configured restart`() {
    for (action in listOf(Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED)) {
      assertFalse("flag missing: $action", BlePlxForegroundServiceRecoveryReceiver.shouldRecover(
        action, false, true))
      assertFalse("restart unconfigured: $action", BlePlxForegroundServiceRecoveryReceiver.shouldRecover(
        action, true, false))
      assertFalse("both missing: $action", BlePlxForegroundServiceRecoveryReceiver.shouldRecover(
        action, false, false))
    }
  }
}
