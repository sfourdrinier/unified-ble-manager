// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidSubscriptionOwnershipTest.kt
package com.sfourdrinier.unifiedblemanager.radio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class OwnedAndroidSubscriptionOwnershipTest {
  @Test fun callbackDuringActivationStaysBehindStagedValues() {
    val seen = mutableListOf<Int>()
    lateinit var owner: OwnedAndroidSubscriptionOwnership<String>
    owner = OwnedAndroidSubscriptionOwnership(onEvent = { _, _, _, _, _, event ->
      if (event is NotificationOwnershipFifo.Event.Value) {
        seen.add(event.bytes[0].toInt())
        if (event.bytes[0].toInt() == 1) assertTrue(owner.offer("A", 1, "x", byteArrayOf(3)))
      }
    })
    assertTrue(owner.arm("a", 1, "x"))
    assertTrue(owner.offer("A", 1, "x", byteArrayOf(1)))
    assertTrue(owner.offer("A", 1, "x", byteArrayOf(2)))
    assertTrue(owner.activate("A", 1, "x"))
    assertEquals(listOf(1, 2, 3), seen)
  }

  @Test fun overflowIsCountedAndStaleGenerationCannotActivateOrReplaceOwner() {
    val events = mutableListOf<NotificationOwnershipFifo.Event>()
    val owner = OwnedAndroidSubscriptionOwnership<String>(onEvent = { _, _, _, _, _, event -> events.add(event) })
    assertTrue(owner.arm("a", 1, "x"))
    assertFalse(owner.arm("a", 2, "x"))
    assertFalse(owner.offer("a", 2, "x", byteArrayOf(9)))
    assertFalse(owner.activate("a", 2, "x"))
    repeat(ANDROID_ENABLEMENT_STAGING_ITEM_LIMIT + 2) { assertTrue(owner.offer("a", 1, "x", byteArrayOf(1))) }
    assertTrue(owner.activate("a", 1, "x"))
    val loss = events.last() as NotificationOwnershipFifo.Event.Loss
    assertEquals(2L, loss.items)
    assertEquals(2L, loss.bytes)
    owner.abandon("a", 1, "x")
    assertTrue(owner.arm("a", 2, "x"))
    assertFalse(owner.offer("a", 1, "x", byteArrayOf(3)))
  }
  @Test fun oldClaimCannotCancelOrRetireSameGenerationReplacement() {
    val seen = mutableListOf<Int>()
    val owner = OwnedAndroidSubscriptionOwnership<String>(onEvent = { _, _, _, _, _, event ->
      if (event is NotificationOwnershipFifo.Event.Value) seen.add(event.bytes[0].toInt())
    })
    assertTrue(owner.arm("a", 5, "x", 41))
    val old = owner.ownerToken("a", 5, "x") ?: error("missing old claim")
    owner.invalidateForDatabaseChange("a", 5)
    assertTrue(owner.arm("a", 5, "x", 42))
    val replacement = owner.ownerToken("a", 5, "x") ?: error("missing replacement")
    assertTrue(owner.activate("a", 5, "x", replacement))
    owner.quiesceForCompensation("a", 5, "x", old)
    owner.abandon("a", 5, "x", old)
    assertFalse(owner.activate("a", 5, "x", old))
    assertTrue(owner.ownsEpoch("a", 5, "x", 42))
    assertFalse(owner.ownsEpoch("a", 5, "x", 41))
    assertTrue(owner.offer("a", 5, "x", byteArrayOf(7)))
    assertEquals(listOf(7), seen)
  }

}
