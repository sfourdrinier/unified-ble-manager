package com.sfourdrinier.unifiedblemanager.background

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Looper
import android.os.Parcelable
import com.sfourdrinier.unifiedblemanager.BlePlxForegroundService
import org.junit.Assert.*
import org.junit.Test
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq
import org.mockito.Mockito.*

class AndroidForegroundServiceStartOwnershipTest {
  @Test fun `driver retains an accepted start when its promotion wait is interrupted`() {
    val context = mock(Context::class.java)
    val intent = mock(Intent::class.java)
    val component = mock(ComponentName::class.java)
    val configuration = ForegroundServiceNotificationConfiguration.fromValues("ble", "BLE", "Recording", null, null, false)
    `when`(intent.putExtra(eq(BlePlxForegroundService.EXTRA_ACK), any(Parcelable::class.java))).thenReturn(intent)
    `when`(context.startService(intent)).thenReturn(component)
    `when`(context.startForegroundService(intent)).thenReturn(component)
    mockStatic(Looper::class.java).use { loopers ->
      loopers.`when`<Looper> { Looper.getMainLooper() }.thenReturn(mock(Looper::class.java))
      mockStatic(BlePlxForegroundService::class.java).use { service ->
        service.`when`<Intent> { BlePlxForegroundService.startIntent(context, configuration) }.thenReturn(intent)
        try {
          Thread.currentThread().interrupt()
          val failure = runCatching { AndroidConnectedDeviceForegroundServiceDriver(context).start("presence", configuration) }.exceptionOrNull()
          assertTrue(failure is ForegroundServiceControlException)
          failure as ForegroundServiceControlException
          assertEquals("foregroundServiceStartInterrupted", failure.code)
          assertTrue(failure.cleanupRequired)
          assertTrue(Thread.currentThread().isInterrupted)
        } finally { Thread.interrupted() }
      }
    }
  }
}
