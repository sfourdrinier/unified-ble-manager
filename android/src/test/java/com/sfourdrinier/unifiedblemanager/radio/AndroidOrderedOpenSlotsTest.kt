package com.sfourdrinier.unifiedblemanager.radio

import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread
import org.junit.Assert.*
import org.junit.Test

class AndroidOrderedOpenSlotsTest {
  private fun identity(holder: AndroidOrderedOpenSlots, generation: ULong = 1uL, slot: Int = 0) =
    AndroidOrderedOpenSlots.Identity(2, holder.incarnation, slot, generation, 7uL, 8uL, "peer",
      AndroidOrderedOpenSlots.Spec("connection", "database", AndroidOrderedOpenSlots.Snapshot("owner", 1uL, 2uL),
        listOf(AndroidOrderedOpenSlots.Member(3uL, "peer", "service", 4uL, "characteristic", 5uL)), 6u, 7u))

  @Test fun ownerFenceRetainsQueuedAndExecutingOutcomesAcrossRetries() {
    val slots = AndroidOrderedOpenSlots()
    val queued = identity(slots)
    val executing = identity(slots, slot = 1)
    slots.reserve(queued)
    slots.reserve(executing)
    val permit = requireNotNull(slots.begin(executing))
    val retirement = UUID.randomUUID()
    val first = slots.fenceOwner(slots.incarnation, retirement)
    assertEquals(1, first.executing)
    assertEquals(1, first.terminal)
    assertEquals(first, slots.fenceOwner(slots.incarnation, retirement))
    assertEquals(first, slots.ownerStatus(slots.incarnation, retirement))
    assertNull(slots.begin(queued))
    assertThrows(IllegalStateException::class.java) { slots.reserve(identity(slots, slot = 2)) }
    assertThrows(IllegalStateException::class.java) { slots.fenceOwner(slots.incarnation, UUID.randomUUID()) }
    assertThrows(IllegalStateException::class.java) { slots.ownerStatus(UUID.randomUUID(), retirement) }
    assertTrue(slots.observe(queued).result is AndroidOrderedOpenSlots.Result.Failed)
    val opened = AndroidOrderedGroupOwner.Opened(9uL, emptyList())
    assertTrue(slots.complete(permit, AndroidOrderedOpenSlots.Result.Opened(opened)))
    assertEquals(2, slots.ownerStatus(slots.incarnation, retirement).terminal)
    slots.acknowledge(queued)
    slots.acknowledge(executing)
    val settled = slots.ownerStatus(slots.incarnation, retirement)
    assertEquals(0, settled.executing)
    assertEquals(0, settled.terminal)
    assertEquals(settled, slots.fenceOwner(slots.incarnation, retirement))
  }

  @Test fun ownerFenceIsScopedToOneIncarnation() {
    val first = AndroidOrderedOpenSlots()
    val second = AndroidOrderedOpenSlots()
    first.fenceOwner(first.incarnation, UUID.randomUUID())
    assertEquals(AndroidOrderedOpenSlots.Result.Pending, second.reserve(identity(second)).result)
    assertNotNull(second.begin(identity(second)))
  }

  @Test fun adapterWorkSurvivesSlotAcknowledgementAndRunningRejection() {
    val slots = AndroidOrderedOpenSlots()
    val id = identity(slots)
    val work = slots.reserveAdapterWork(id)
    slots.startAdapterWork(work)
    val retirement = UUID.randomUUID()
    slots.fenceOwner(slots.incarnation, retirement)
    slots.acknowledge(id)
    slots.rejectAdapterWork(work)
    assertEquals(1, slots.ownerStatus(slots.incarnation, retirement).adapterWork)
    slots.finishAdapterWork(work)
    assertEquals(0, slots.ownerStatus(slots.incarnation, retirement).adapterWork)
  }

  @Test fun fenceBeforeOpenAndStaleWorkCannotAffectReusedSlot() {
    val slots = AndroidOrderedOpenSlots()
    val first = identity(slots)
    assertTrue(slots.fence(first).result is AndroidOrderedOpenSlots.Result.Failed)
    assertEquals(AndroidOrderedOpenSlots.Retired(first.slotIdentity()), slots.acknowledge(first))
    val next = identity(slots, 2uL)
    assertEquals(AndroidOrderedOpenSlots.Result.Pending, slots.reserve(next).result)
    assertThrows(IllegalStateException::class.java) { slots.reserve(first) }
    assertThrows(IllegalStateException::class.java) { slots.fence(first) }
    assertEquals(AndroidOrderedOpenSlots.Retired(first.slotIdentity()), slots.acknowledge(first))
    assertNull(slots.begin(first))
    assertNotNull(slots.begin(next))
  }

  @Test fun executionIsNonblockingAndRetirementRequiresAResolvedOutcome() {
    val slots = AndroidOrderedOpenSlots()
    val id = identity(slots)
    slots.reserve(id)
    val permit = requireNotNull(slots.begin(id))
    val entered = CountDownLatch(1)
    val worker = thread { assertEquals(AndroidOrderedOpenSlots.Result.Pending, slots.fence(id).result); entered.countDown() }
    assertTrue(entered.await(2, TimeUnit.SECONDS)); worker.join()
    assertThrows(IllegalStateException::class.java) { slots.acknowledge(id) }
    val error = IllegalStateException("partial")
    val opened = AndroidOrderedGroupOwner.Opened(9uL, listOf(AndroidOrderedGroupOwner.Registration(1uL, 2uL, 3uL)))
    val outcome = AndroidOrderedOpenSlots.Result.Failed(error, AndroidOrderedOpenSlots.Ownership.Created(opened))
    slots.complete(permit, outcome)
    assertEquals(outcome, slots.fence(id).result)
    assertEquals(AndroidOrderedOpenSlots.Retired(id.slotIdentity()), slots.acknowledge(id))
    assertEquals(AndroidOrderedOpenSlots.Retired(id.slotIdentity()), slots.acknowledge(id))
    assertFalse(slots.complete(permit, outcome))
  }

  @Test fun exactIdentityAndGenerationAreCheckedAndHistoryIsBounded() {
    val slots = AndroidOrderedOpenSlots()
    var id = identity(slots)
    assertThrows(IllegalStateException::class.java) { slots.reserve(id.copy(ownerIncarnation = UUID.randomUUID())) }
    assertThrows(IllegalStateException::class.java) { slots.reserve(id.copy(slotGeneration = 2uL)) }
    assertThrows(IllegalArgumentException::class.java) { slots.reserve(id.copy(peerId = "PEER")) }
    for (generation in 1uL..512uL) {
      id = identity(slots, generation)
      assertEquals(AndroidOrderedOpenSlots.Result.Pending, slots.reserve(id).result)
      assertThrows(IllegalStateException::class.java) { slots.reserve(id.copy(spec = id.spec.copy(byteCapacity = 99u))) }
      assertTrue(slots.fence(id).result is AndroidOrderedOpenSlots.Result.Failed)
      slots.acknowledge(id)
    }
    assertThrows(IllegalStateException::class.java) { slots.reserve(id) }
  }

  @Test fun rejectedDispatchRetainsOriginalNoEffectFailure() {
    val slots = AndroidOrderedOpenSlots()
    val id = identity(slots)
    slots.reserve(id)
    val error = IllegalStateException("executor rejected")
    assertTrue(slots.rejectBeforeExecution(id, error))
    assertEquals(AndroidOrderedOpenSlots.Result.Failed(error, AndroidOrderedOpenSlots.Ownership.NotCreated), slots.fence(id).result)
    assertNull(slots.begin(id))
    slots.acknowledge(id)
  }

  @Test fun indeterminateCompletionRemainsPendingDebtUntilResolved() {
    val slots = AndroidOrderedOpenSlots()
    val id = identity(slots)
    slots.reserve(id)
    val permit = requireNotNull(slots.begin(id))
    val uncertainty = AndroidOrderedOpenSlots.Result.Indeterminate(IllegalStateException("unknown effect"))
    assertTrue(slots.complete(permit, uncertainty))
    assertEquals(AndroidOrderedOpenSlots.Observation(id.slotIdentity(), uncertainty), slots.fence(id))
    assertThrows(IllegalStateException::class.java) { slots.acknowledge(id) }
    val terminal = AndroidOrderedOpenSlots.Result.Failed(IllegalStateException("known no effect"), AndroidOrderedOpenSlots.Ownership.NotCreated)
    assertTrue(slots.complete(permit, terminal))
    assertEquals(terminal, slots.fence(id).result)
    slots.acknowledge(id)
  }

  @Test fun fingerprintIsCopiedAndAllSlotsHaveIndependentBounds() {
    val slots = AndroidOrderedOpenSlots()
    val members = identity(slots).spec.members.toMutableList()
    val original = identity(slots).copy(spec = identity(slots).spec.copy(members = members))
    slots.reserve(original)
    val saved = original.copy(spec = original.spec.copy(members = members.toList()))
    members.clear()
    assertEquals(AndroidOrderedOpenSlots.Result.Pending, slots.reserve(saved).result)
    assertThrows(IllegalStateException::class.java) { slots.reserve(original) }
    for (slot in 1 until 64) slots.reserve(identity(slots, slot = slot))
    assertThrows(IllegalArgumentException::class.java) { slots.reserve(identity(slots, slot = 64)) }
    val other = AndroidOrderedOpenSlots()
    assertThrows(IllegalStateException::class.java) { other.fence(saved) }
  }

  @Test fun runningInstallationDoesNotHoldSlotMonitor() {
    val slots = AndroidOrderedOpenSlots()
    val id = identity(slots)
    slots.reserve(id)
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    val worker = thread {
      val permit = requireNotNull(slots.begin(id))
      entered.countDown()
      check(resume.await(5, TimeUnit.SECONDS))
      slots.complete(permit, AndroidOrderedOpenSlots.Result.Opened(AndroidOrderedGroupOwner.Opened(9uL, emptyList())))
    }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    try {
      assertEquals(AndroidOrderedOpenSlots.Result.Pending, slots.fence(id).result)
      assertThrows(IllegalStateException::class.java) { slots.acknowledge(id) }
    } finally { resume.countDown(); worker.join(5000) }
    assertFalse(worker.isAlive)
    assertTrue(slots.fence(id).result is AndroidOrderedOpenSlots.Result.Opened)
  }

  @Test fun exhaustedGenerationNeverWraps() {
    val slots = AndroidOrderedOpenSlots()
    val field = slots.javaClass.getDeclaredField("slots").apply { isAccessible = true }
    val first = (field.get(slots) as Array<*>).first()!!
    first.javaClass.getDeclaredField("generation").apply { isAccessible = true; setLong(first, -1L) }
    assertThrows(IllegalStateException::class.java) { slots.reserve(identity(slots)) }
    assertThrows(IllegalStateException::class.java) { slots.fence(identity(slots)) }
  }

}
