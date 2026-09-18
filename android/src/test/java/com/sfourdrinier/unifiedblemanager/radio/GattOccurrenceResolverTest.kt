// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/GattOccurrenceResolverTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Test
import java.util.UUID

class GattOccurrenceResolverTest {
  private val serviceA = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
  private val serviceB = UUID.fromString("0000180f-0000-1000-8000-00805f9b34fb")
  private val characteristicA = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
  private val characteristicB = UUID.fromString("00002a38-0000-1000-8000-00805f9b34fb")
  private val descriptorA = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
  private val descriptorB = UUID.fromString("00002901-0000-1000-8000-00805f9b34fb")

  private data class Node(val uuid: UUID, val label: String)

  @Test
  fun duplicateOccurrencesAreScopedToEachUuidAtEveryGattContainmentLevel() {
    for ((first, second) in listOf(serviceA to serviceB, characteristicA to characteristicB, descriptorA to descriptorB)) {
      val ordinals = mutableMapOf<UUID, Int>()
      assertEquals(
        listOf(0, 0, 1),
        listOf(first, second, first).map { uuid -> nextUuidOccurrence(ordinals, uuid) }
      )
      assertEquals(first, resolveUuidOccurrence(listOf(first, second, first), first, 1) { it })
    }
    assertNull(resolveUuidOccurrence(listOf(serviceA, serviceB, serviceA), serviceA, 2) { it })
  }

  @Test
  fun resolvesTheExactSiblingInstanceForEachOrdinal() {
    val firstA = Node(serviceA, "first")
    val onlyB = Node(serviceB, "only")
    val secondA = Node(serviceA, "second")
    val siblings = listOf(firstA, onlyB, secondA)

    assertSame(firstA, resolveUuidOccurrence(siblings, serviceA, 0) { it.uuid })
    assertSame(secondA, resolveUuidOccurrence(siblings, serviceA, 1) { it.uuid })
    assertSame(onlyB, resolveUuidOccurrence(siblings, serviceB, 0) { it.uuid })
    assertNull(resolveUuidOccurrence(siblings, serviceB, 1) { it.uuid })
  }

  @Test
  fun negativeOrUnknownOccurrencesResolveToNothing() {
    val siblings = listOf(serviceA, serviceA)

    assertNull(resolveUuidOccurrence(siblings, serviceA, -1) { it })
    assertNull(resolveUuidOccurrence(siblings, characteristicA, 0) { it })
    assertNull(resolveUuidOccurrence(emptyList<UUID>(), serviceA, 0) { it })
  }

  @Test
  fun ordinalCountsAreIndependentPerMap() {
    val first = mutableMapOf<UUID, Int>()
    val second = mutableMapOf<UUID, Int>()

    assertEquals(0, nextUuidOccurrence(first, serviceA))
    assertEquals(1, nextUuidOccurrence(first, serviceA))
    assertEquals(0, nextUuidOccurrence(second, serviceA))
    assertEquals(mapOf(serviceA to 2), first)
    assertEquals(mapOf(serviceA to 1), second)
  }
}
