// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/NotificationStopClassificationTest.kt
package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.radio.NotificationOwnershipFifo
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class NotificationStopClassificationTest {
  @Test fun preservesFailureKindWithoutLeakingExceptionText() {
    val failure = classifyAndroidNotificationStop(
      NotificationOwnershipFifo.Stop.DeliveryUncertain(SecurityException("private platform detail"), 1, 2)
    )
    assertEquals(RadioFailureKind.PERMISSION_DENIED, failure.kind)
    assertEquals("NOTIFICATION_DELIVERY_UNCERTAIN", failure.nativeCode)
    assertFalse(failure.detail.contains("private platform detail"))
  }

  @Test fun controlCapacityHasDistinctTypedCategory() {
    val failure = classifyAndroidNotificationStop(NotificationOwnershipFifo.Stop.ControlCapacity(0, 0))
    assertEquals(RadioFailureKind.PLATFORM, failure.kind)
    assertEquals("NOTIFICATION_CONTROL_CAPACITY", failure.nativeCode)
  }
}
