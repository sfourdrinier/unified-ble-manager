// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OrderedNotificationSourceTest.kt
package com.sfourdrinier.unifiedblemanager.radio

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

class OrderedNotificationSourceTest {
  @Test fun laterFailuresUseBoundedReusableCapacityAndReportUnknownOverflow() {
    val source = OrderedNotificationSource<String>(1)
    val (group, members) = source.openRegistered(setOf("x"), 2, 8) {}
    val target = mapOf(group to members.getValue("x"))
    val first = IllegalStateException("first")
    val second = IllegalStateException("second")
    val third = IllegalStateException("third")
    source.failMember("x", target, first)
    source.drain(group)
    source.failMember("x", target, second)
    source.failMember("x", target, third)
    source.failMember("x", target, IllegalStateException("omitted"))
    val retained = source.drain(group).map { it as OrderedNotificationSource.Event.SubsequentFailure }
    assertSame(second, retained[0].failure?.cause)
    assertSame(third, retained[1].failure?.cause)
    assertTrue(retained.last().overflow)
    val afterDrain = IllegalStateException("after drain")
    source.failMember("x", target, afterDrain)
    assertSame(afterDrain, (source.drain(group).single() as OrderedNotificationSource.Event.SubsequentFailure).failure?.cause)
  }

  @Test fun sealingFencesNewFailuresButKeepsUndrainedFailureDebt() {
    val source = OrderedNotificationSource<String>(1)
    val (group, members) = source.openRegistered(setOf("x"), 1, 8) {}
    val target = mapOf(group to members.getValue("x"))
    source.failMember("x", target, IllegalStateException("first"))
    val retained = IllegalStateException("retained")
    source.failMember("x", target, retained)
    source.failMember("x", target, IllegalStateException("omitted"))
    source.seal(group)
    assertFalse(source.failMember("x", target, IllegalStateException("after seal")))
    assertFalse(source.remove(group))
    assertTrue(source.drain(group).single() is OrderedNotificationSource.Event.Terminal)
    val debt = source.drain(group).map { it as OrderedNotificationSource.Event.SubsequentFailure }
    assertSame(retained, debt.first().failure?.cause)
    assertTrue(debt.last().overflow)
    assertTrue(source.remove(group))
  }
  @Test fun exactMemberFailurePreservesOrderAndFencesOnlyTargetGroups() {
    val source = OrderedNotificationSource<String>(17, sourceGeneration = 71uL)
    var wakesA = 0
    var wakesB = 0
    var wakesOther = 0
    var wakesUnrelated = 0
    val (a, aMembers) = source.openRegistered(setOf("shared", "other"), 3, 16) { assertFalse(source.lockHeldByCurrentThread()); wakesA++ }
    val (b, bMembers) = source.openRegistered(setOf("shared"), 3, 16) { assertFalse(source.lockHeldByCurrentThread()); wakesB++ }
    val (other, _) = source.openRegistered(setOf("other"), 3, 16) { wakesOther++ }
    val (unrelated, _) = source.openRegistered(setOf("shared"), 3, 16) { wakesUnrelated++ }
    val aClaim = source.retainPhysicalClaim(a, "shared", aMembers.getValue("shared"))
    source.admit("shared", byteArrayOf(1))
    source.barrier(a, 4uL)
    source.admit("other", byteArrayOf(2))
    val beforeA = wakesA
    val beforeB = wakesB
    val beforeOther = wakesOther
    val beforeUnrelated = wakesUnrelated
    val cause = IllegalStateException("CCCD delivery uncertain")
    val expected = mapOf(a to aMembers.getValue("shared"), b to bMembers.getValue("shared"))
    assertTrue(source.failMember("shared", expected, cause))
    assertEquals(beforeA + 1, wakesA)
    assertEquals(beforeB + 1, wakesB)
    assertEquals(beforeOther, wakesOther)
    assertEquals(beforeUnrelated, wakesUnrelated)
    assertTrue(source.failMember("shared", expected, cause))
    assertEquals(beforeA + 1, wakesA)
    assertEquals(beforeB + 1, wakesB)
    source.admit("shared", byteArrayOf(3))
    source.admit("other", byteArrayOf(4))
    val records = source.drain(a)
    assertEquals(4, records.size)
    assertArrayEquals(byteArrayOf(1), (records[0] as OrderedNotificationSource.Event.Value).bytes)
    assertEquals(4uL, (records[1] as OrderedNotificationSource.Event.Barrier).barrierId)
    assertArrayEquals(byteArrayOf(2), (records[2] as OrderedNotificationSource.Event.Value).bytes)
    val terminal = records[3] as OrderedNotificationSource.Event.Terminal
    assertEquals(OrderedNotificationSource.Reason.SOURCE_FAILED, terminal.reason)
    assertNull(terminal.rejectedValues)
    assertTrue(terminal.uncertainTail)
    assertEquals(71uL, terminal.sourceFailure?.sourceGeneration)
    assertEquals("shared", terminal.sourceFailure?.member)
    assertEquals(aMembers.getValue("shared"), terminal.sourceFailure?.membership)
    assertSame(cause, terminal.sourceFailure?.cause)
    val bTerminal = source.drain(b).last() as OrderedNotificationSource.Event.Terminal
    assertEquals(bMembers.getValue("shared"), bTerminal.sourceFailure?.membership)
    assertSame(cause, bTerminal.sourceFailure?.cause)
    assertTrue(source.failMember("shared", expected, IllegalStateException("later cause")))
    assertTrue(source.drain(a).single() is OrderedNotificationSource.Event.SubsequentFailure)
    assertTrue(source.drain(b).single() is OrderedNotificationSource.Event.SubsequentFailure)
    assertEquals(listOf(2, 4), source.drain(other).map { (it as OrderedNotificationSource.Event.Value).bytes[0].toInt() })
    assertEquals(listOf(1, 3), source.drain(unrelated).map { (it as OrderedNotificationSource.Event.Value).bytes[0].toInt() })
    assertFalse(source.remove(a))
    source.confirmPhysicalCleanup(a, aClaim)
    assertTrue(source.remove(a))
  }

  @Test fun laterFailuresAreBoundedAndRetriesOfRetainedCausesAreIdempotent() {
    val source = OrderedNotificationSource<String>(21)
    val (group, memberships) = source.openRegistered(setOf("x"), 2, 8) { }
    val target = mapOf(group to memberships.getValue("x"))
    val first = IllegalStateException("first")
    val second = IllegalStateException("second")
    val third = IllegalStateException("third")
    assertTrue(source.failMember("x", target, first))
    assertTrue(source.failMember("x", target, first))
    assertTrue(source.failMember("x", target, second))
    assertTrue(source.failMember("x", target, second))
    assertTrue(source.failMember("x", target, third))
    assertTrue(source.failMember("x", target, third))
    assertSame(first, (source.drain(group).single() as OrderedNotificationSource.Event.Terminal).sourceFailure?.cause)
    val later = source.drain(group).map { it as OrderedNotificationSource.Event.SubsequentFailure }
    assertEquals(2, later.size)
    assertSame(second, later[0].failure?.cause)
    assertSame(third, later[1].failure?.cause)
    assertFalse(later.any { it.overflow })
    assertTrue(source.drain(group).isEmpty())
    assertTrue(source.failMember("x", target, first))
    assertTrue(source.failMember("x", target, second))
    assertTrue(source.drain(group).isEmpty())
  }

  @Test fun failureAfterSecondaryDrainRemainsOwnedUntilSeparatelyDrained() {
    val source = OrderedNotificationSource<String>(22)
    val (group, memberships) = source.openRegistered(setOf("x"), 2, 8) { }
    val target = mapOf(group to memberships.getValue("x"))
    source.failMember("x", target, IllegalStateException("first"))
    source.drain(group)
    source.failMember("x", target, IllegalStateException("second"))
    assertFalse((source.drain(group).single() as OrderedNotificationSource.Event.SubsequentFailure).overflow)
    val third = IllegalStateException("third")
    source.failMember("x", target, third)
    assertFalse(source.remove(group))
    val later = source.drain(group).single() as OrderedNotificationSource.Event.SubsequentFailure
    assertSame(third, later.failure?.cause)
    assertFalse(later.overflow)
    assertTrue(source.remove(group))
  }

  @Test fun memberFailureRejectsStaleTargetsAtomicallyAndWakesAllDespiteFailure() {
    val source = OrderedNotificationSource<String>(18)
    var wakes = 0
    val wakeCause = IllegalStateException("wake")
    val (a, aMembers) = source.openRegistered(setOf("x"), 4, 8) { throw wakeCause }
    val (b, bMembers) = source.openRegistered(setOf("x"), 4, 8) { wakes++ }
    val stale = bMembers.getValue("x").copy(epoch = bMembers.getValue("x").epoch + 1uL)
    val cause = IllegalArgumentException("delivery")
    assertFalse(source.failMember("x", mapOf(a to aMembers.getValue("x"), b to stale), cause))
    assertFalse(source.failMember("wrong", mapOf(a to aMembers.getValue("x")), cause))
    assertTrue(source.drain(a).isEmpty())
    assertTrue(source.drain(b).isEmpty())
    try {
      source.failMember("x", mapOf(a to aMembers.getValue("x"), b to bMembers.getValue("x")), cause)
      fail("wake failure was swallowed")
    } catch (error: OrderedNotificationSource.WakeFailure) { assertSame(wakeCause, error.cause) }
    assertEquals(1, wakes)
    val terminal = source.drain(a).single() as OrderedNotificationSource.Event.Terminal
    assertTrue(terminal.wakeFailed)
    assertSame(cause, terminal.sourceFailure?.cause)
    assertSame(cause, (source.drain(b).single() as OrderedNotificationSource.Event.Terminal).sourceFailure?.cause)
  }

  @Test fun retiredMembershipCannotFailItsReplacementAndDeferredWakeIsTracked() {
    val source = OrderedNotificationSource<String>(19)
    var wakes = 0
    val (group, memberships) = source.openRegistered(setOf("x"), 4, 8) { wakes++ }
    val old = memberships.getValue("x")
    assertTrue(source.retire(group, "x", old))
    val current = source.register(group, "x", old)
    assertFalse(source.failMember("x", mapOf(group to old), IllegalStateException()))
    assertFalse(source.failMember("x", mapOf(group to current.copy(sourceMember = old.sourceMember)), IllegalStateException()))
    val deferred = mutableListOf<() -> Unit>()
    val cause = IllegalStateException("delivery")
    assertTrue(source.failMember("x", mapOf(group to current), cause) { deferred.add(it); true })
    try { source.seal(group); fail("deferred wake was ignored") } catch (_: IllegalStateException) { }
    deferred.single().invoke()
    source.seal(group)
    val records = source.drain(group)
    assertEquals(old.epoch, (records[0] as OrderedNotificationSource.Event.Fence).epoch)
    assertSame(cause, (records[1] as OrderedNotificationSource.Event.Terminal).sourceFailure?.cause)
    assertEquals(2, wakes)
  }

  @Test fun droppedDeferredMemberFailureWakeIsReportedWithoutLosingItsCause() {
    val source = OrderedNotificationSource<String>(20)
    var wakes = 0
    val (group, memberships) = source.openRegistered(setOf("x"), 1, 8) { wakes++ }
    val cause = IllegalStateException("delivery")
    assertTrue(source.failMember("x", mapOf(group to memberships.getValue("x")), cause) { false })
    try { source.seal(group); fail("lost wake allowed sealing") } catch (_: IllegalStateException) { }
    source.failDeferredIngress()
    source.seal(group)
    assertEquals(1, wakes)
    val terminal = source.drain(group).single() as OrderedNotificationSource.Event.Terminal
    assertSame(cause, terminal.sourceFailure?.cause)
    assertNull(terminal.rejectedLoss?.count)
    assertTrue(terminal.rejectedLoss?.scope is OrderedNotificationSource.LossScope.Unknown)
  }
  @Test fun registeredHandoffSurvivesAnExceptionAfterOwnershipTransfer() {
    val source = OrderedNotificationSource<String>(1)
    var retained: OrderedNotificationSource<String>.Group? = null
    try {
      source.openRegistered(setOf("a"), 4, 8, onRegistered = { group, _ ->
        retained = group
        error("handoff failed")
      }) { }
      fail("handoff did not fail")
    } catch (_: IllegalStateException) { }
    source.admit("a", byteArrayOf(7))
    val event = source.drain(requireNotNull(retained)).single() as OrderedNotificationSource.Event.Value
    assertArrayEquals(byteArrayOf(7), event.bytes)
  }
  @Test fun droppedDeferredWakePreventsSealUntilOverflowIsReported() {
    val source = OrderedNotificationSource<String>(1)
    var wakes = 0
    val (group, _) = source.openRegistered(setOf("a"), 4, 8) { wakes++ }
    source.admit("a", byteArrayOf(1)) { false }
    source.close(group)
    try { source.seal(group); fail("lost wake allowed sealing") } catch (_: IllegalStateException) { }
    source.failDeferredIngress()
    assertEquals(1, wakes)
    source.seal(group)
  }
  @Test fun claimRetirementCannotInterleaveValidatedPhysicalEffect() {
    val source = OrderedNotificationSource<String>(1)
    val (group, memberships) = source.openRegistered(setOf("a"), 4, 8) {}
    val membership = memberships.getValue("a")
    source.retainPhysicalClaim(group, "a", membership)
    val insideEffect = CountDownLatch(1)
    val finishEffect = CountDownLatch(1)
    val retired = CountDownLatch(1)
    val effect = thread {
      source.withPhysicalClaim(group, "a", membership) {
        insideEffect.countDown()
        check(finishEffect.await(5, TimeUnit.SECONDS))
        true
      }
    }
    assertTrue(insideEffect.await(5, TimeUnit.SECONDS))
    val retire = thread { source.retire(group, "a", membership); retired.countDown() }
    assertFalse(retired.await(100, TimeUnit.MILLISECONDS))
    finishEffect.countDown()
    effect.join(5000)
    retire.join(5000)
    assertEquals(0L, retired.count)
    var invoked = false
    assertNull(source.withPhysicalClaim(group, "a", membership) { invoked = true; true })
    assertFalse(invoked)
  }
  @Test fun sealWaitsForWakeAndPreservesAdmittedRecordsWithoutLaterControls() {
    val source = OrderedNotificationSource<String>(1)
    val entered = CountDownLatch(1)
    val resume = CountDownLatch(1)
    val (group, _) = source.openRegistered(setOf("a"), 4, 8) { entered.countDown(); check(resume.await(5, TimeUnit.SECONDS)) }
    val worker = thread { source.admit("a", byteArrayOf(1)) }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    source.close(group)
    try { source.seal(group); fail("in-flight wake was sealed") } catch (_: IllegalStateException) { }
    resume.countDown()
    worker.join(5000)
    assertFalse(worker.isAlive)
    source.seal(group)
    source.seal(group)
    source.admit("a", byteArrayOf(2))
    source.reportLoss(group, OrderedNotificationSource.LossScope.Unknown(), OrderedNotificationSource.LossUnit.SOURCE_RECORDS, null, null)
    val records = source.drain(group)
    assertEquals(2, records.size)
    assertArrayEquals(byteArrayOf(1), (records[0] as OrderedNotificationSource.Event.Value).bytes)
    assertEquals(OrderedNotificationSource.Reason.CLOSED, (records[1] as OrderedNotificationSource.Event.Terminal).reason)
    source.wakeIfPending(group)
    assertTrue(source.drain(group).isEmpty())
  }
  @Test fun markerAndWriteOrdersCallbacksAndRetainsMarkerOnWriteFailure() {
    val source = OrderedNotificationSource<String>(1)
    val (group, _) = source.openRegistered(setOf("a"), 4, 16) {}
    source.admit("a", byteArrayOf(1))
    val deferred = mutableListOf<() -> Unit>()
    val admitted = source.markerAndRun(group, 41uL, { deferred.add(it); true }) {
      source.admit("a", byteArrayOf(2)) { deferred.add(it); true }
      throw IllegalStateException("write failed before submission")
    } as OrderedNotificationSource.MarkerAdmission.Admitted
    assertEquals(1uL, admitted.ordinal)
    assertTrue(admitted.effect.isFailure)
    deferred.forEach { it() }
    val events = source.drain(group)
    assertEquals(0uL, (events[0] as OrderedNotificationSource.Event.Value).ordinal)
    val boundary = events[1] as OrderedNotificationSource.Event.Marker
    assertEquals(41uL, boundary.markerId)
    assertEquals(1uL, boundary.ordinal)
    assertEquals(2uL, (events[2] as OrderedNotificationSource.Event.Value).ordinal)
  }

  @Test fun markerRefusalDoesNotRunWriteAndPreservesTerminalEvidence() {
    val source = OrderedNotificationSource<String>(1)
    val (group, _) = source.openRegistered(setOf("a"), 1, 8) {}
    source.admit("a", byteArrayOf(1))
    var writes = 0
    val result = source.markerAndRun(group, 99uL, { it(); true }) { writes++ }
    assertTrue(result is OrderedNotificationSource.MarkerAdmission.Rejected)
    assertEquals(0, writes)
    val terminal = source.drain(group).last() as OrderedNotificationSource.Event.Terminal
    assertEquals(OrderedNotificationSource.Reason.OVERFLOW, terminal.reason)
    assertEquals(99uL, terminal.rejectedMarker)
  }

  @Test fun barrierOrdersCallbacksAndRetryDoesNotRetireMembership() {
    val source = OrderedNotificationSource<String>(1)
    val (group, memberships) = source.openRegistered(setOf("a"), 4, 16) {}
    val claim = source.retainPhysicalClaim(group, "a", memberships.getValue("a"))
    source.admit("a", byteArrayOf(1))
    assertTrue(source.barrier(group, ULong.MAX_VALUE))
    assertTrue(source.barrier(group, ULong.MAX_VALUE))
    source.admit("a", byteArrayOf(2))
    val records = source.drain(group)
    assertArrayEquals(byteArrayOf(1), (records[0] as OrderedNotificationSource.Event.Value).bytes)
    assertEquals(ULong.MAX_VALUE, (records[1] as OrderedNotificationSource.Event.Barrier).barrierId)
    assertEquals(memberships.getValue("a").epoch, (records[2] as OrderedNotificationSource.Event.Value).epoch)
    assertTrue(source.barrier(group, ULong.MAX_VALUE))
    assertTrue(source.drain(group).isEmpty())
    source.close(group)
    source.drain(group)
    assertFalse(source.remove(group))
    source.confirmPhysicalCleanup(group, claim)
    assertTrue(source.remove(group))
  }

  @Test fun fullQueueRejectsBarrierWithTerminalEvidence() {
    val source = OrderedNotificationSource<String>(1)
    val (group, _) = source.openRegistered(setOf("a"), 1, 8) {}
    source.admit("a", byteArrayOf(1))
    assertFalse(source.barrier(group, 9uL))
    val records = source.drain(group)
    val terminal = records.last() as OrderedNotificationSource.Event.Terminal
    assertEquals(OrderedNotificationSource.Reason.OVERFLOW, terminal.reason)
    assertEquals(9uL, terminal.rejectedBarrier)
  }
  @Test fun distinctBarrierWaitsForPriorBatchAcknowledgment() {
    val source = OrderedNotificationSource<String>(1)
    val (group, _) = source.openRegistered(setOf("a"), 4, 8) {}
    assertTrue(source.barrier(group, 10uL))
    source.drain(group)
    try { source.barrier(group, 11uL); fail("unacknowledged barrier replaced") }
    catch (_: IllegalStateException) { }
    source.acknowledgeBarrier(group, 10uL)
    assertTrue(source.barrier(group, 10uL))
    assertTrue(source.drain(group).isEmpty())
    assertTrue(source.barrier(group, 11uL))
  }
  @Test fun setupInterleavingFenceAndReRegistration() {
    val source = OrderedNotificationSource<String>(7)
    var wakes = 0
    val group = source.open(setOf("a", "b"), 8, 100) { wakes++ }
    val firstMembership = source.register(group, "a")
    assertEquals(1uL, firstMembership.epoch)
    val bytes = byteArrayOf(1)
    source.admit("a", bytes); bytes[0] = 9
    source.register(group, "b"); source.admit("b", byteArrayOf(2))
    assertTrue(source.retire(group, "a", firstMembership))
    val secondMembership = source.register(group, "a", firstMembership)
    assertTrue(source.retire(group, "a", firstMembership))
    assertEquals(secondMembership, source.register(group, "a", firstMembership))
    assertEquals(2uL, secondMembership.epoch)
    assertNotEquals(firstMembership.sourceMember, secondMembership.sourceMember)
    source.admit("a", byteArrayOf(3))
    assertEquals(1, wakes)
    val records = source.drain(group)
    val first = records[0] as OrderedNotificationSource.Event.Value
    val second = records[1] as OrderedNotificationSource.Event.Value
    val fence = records[2] as OrderedNotificationSource.Event.Fence
    val third = records[3] as OrderedNotificationSource.Event.Value
    assertArrayEquals(byteArrayOf(1), first.bytes)
    assertEquals(listOf(0uL, 1uL, 2uL), listOf(first.ordinal, second.ordinal, third.ordinal))
    assertEquals(1uL, fence.epoch); assertEquals(2uL, third.epoch)
    assertEquals(firstMembership.sourceMember, fence.sourceMember)
    assertEquals(secondMembership.sourceMember, third.sourceMember)
    source.close(group); assertEquals(2, wakes)
    assertEquals(OrderedNotificationSource.Reason.CLOSED, (source.drain(group).single() as OrderedNotificationSource.Event.Terminal).reason)
  }
  @Test fun delayedTransitionsCannotChangeReplacementRegistration() {
    val source = OrderedNotificationSource<String>(7)
    val (group, initial) = source.openRegistered(setOf("a"), 8, 16) {}
    val first = initial.getValue("a")
    assertTrue(source.retire(group, "a", first))
    val second = source.register(group, "a", first)
    assertTrue(source.retire(group, "a", first))
    assertEquals(second, source.register(group, "a", first))
    assertTrue(source.retire(group, "a", second))
    val third = source.register(group, "a", second)
    try { source.retire(group, "a", first); fail("stale retire accepted") }
    catch (_: IllegalStateException) { }
    try { source.register(group, "a", first); fail("stale register accepted") }
    catch (_: IllegalStateException) { }
    source.admit("a", byteArrayOf(7))
    val value = source.drain(group).last() as OrderedNotificationSource.Event.Value
    assertEquals(third.sourceMember, value.sourceMember)
    assertEquals(third.epoch, value.epoch)
  }
  @Test fun overflowAndInvalidationRetainTerminal() {
    val source = OrderedNotificationSource<String>(8)
    val small = source.open(setOf("a"), 1, 2) {}
    val other = source.open(setOf("a"), 3, 4) {}
    source.register(small, "a"); source.register(other, "a")
    source.admit("a", byteArrayOf(1)); source.admit("a", byteArrayOf(2))
    val terminal = source.drain(small).last() as OrderedNotificationSource.Event.Terminal
    assertEquals(OrderedNotificationSource.Reason.OVERFLOW, terminal.reason)
    assertEquals(1uL, terminal.rejectedValues); assertTrue(terminal.uncertainTail)
    assertEquals("a", terminal.rejectedValue?.member)
    assertEquals(1uL, terminal.rejectedValue?.bytes)
    source.invalidate(OrderedNotificationSource.Reason.CONNECTION_LOST); source.admit("a", byteArrayOf(3))
    assertEquals(3, source.drain(other).size); assertTrue(source.remove(small))
  }
  @Test fun unsignedOrdinalMaximumTerminatesWithoutWrapping() {
    val source = OrderedNotificationSource<String>(9, initialOrdinal = ULong.MAX_VALUE)
    val group = source.open(setOf("a"), 4, 4) {}
    source.register(group, "a")
    source.admit("a", byteArrayOf(1)); source.admit("a", byteArrayOf(2))
    val records = source.drain(group)
    assertEquals("18446744073709551615", (records[0] as OrderedNotificationSource.Event.Value).ordinal.toString())
    assertEquals(OrderedNotificationSource.Reason.ORDINAL_EXHAUSTED, (records[1] as OrderedNotificationSource.Event.Terminal).reason)
  }
  @Test fun lossWithUnknownCountAndLostFenceAreExplicit() {
    val source = OrderedNotificationSource<String>(10)
    val group = source.open(setOf("a"), 1, 4) {}
    source.register(group, "a")
    source.reportLoss(group, OrderedNotificationSource.LossScope.Unknown(), OrderedNotificationSource.LossUnit.SOURCE_RECORDS, null, null)
    source.reportLoss(group, OrderedNotificationSource.LossScope.Member("a"), OrderedNotificationSource.LossUnit.VALUES, ULong.MAX_VALUE, ULong.MAX_VALUE)
    val records = source.drain(group)
    assertNull((records[0] as OrderedNotificationSource.Event.Loss).count)
    val terminal = records[1] as OrderedNotificationSource.Event.Terminal
    assertEquals(OrderedNotificationSource.Reason.OVERFLOW, terminal.reason)
    assertEquals(ULong.MAX_VALUE, terminal.rejectedLoss?.count)
    assertEquals(ULong.MAX_VALUE, terminal.rejectedLoss?.bytes)
    assertTrue(terminal.rejectedLoss?.scope is OrderedNotificationSource.LossScope.Member)
    assertEquals(OrderedNotificationSource.LossUnit.VALUES, terminal.rejectedLoss?.unit)
    val fenceGroup = source.open(setOf("a"), 1, 4) {}
    val fenceMembership = source.register(fenceGroup, "a")
    source.admit("a", byteArrayOf(1))
    assertFalse(source.retire(fenceGroup, "a", fenceMembership))
    val fenceTerminal = source.drain(fenceGroup).last() as OrderedNotificationSource.Event.Terminal
    assertEquals(OrderedNotificationSource.Reason.OVERFLOW, fenceTerminal.reason)
    assertEquals("a", fenceTerminal.rejectedFence?.member)
  }
  @Test fun wakeFailureTerminatesOnlyItsGroupAndRetainsValues() {
    val source = OrderedNotificationSource<String>(11)
    val group = source.open(setOf("a"), 4, 4) { error("broken") }
    source.register(group, "a")
    try { source.admit("a", byteArrayOf(1)); fail("missing failure") }
    catch (error: OrderedNotificationSource.WakeFailure) {
      assertEquals(source.sourceGeneration, error.sourceGeneration)
      assertEquals(group.id, error.groupId)
    }
    source.admit("a", byteArrayOf(2))
    val records = source.drain(group)
    assertArrayEquals(byteArrayOf(1), (records[0] as OrderedNotificationSource.Event.Value).bytes)
    val terminal = records[1] as OrderedNotificationSource.Event.Terminal
    assertEquals(OrderedNotificationSource.Reason.WAKE_FAILED, terminal.reason)
    assertTrue(terminal.wakeFailed)
  }
  @Test fun registeredOpenInstallsEveryMemberBeforeFirstAdmission() {
    val source = OrderedNotificationSource<String>(12)
    val (group, memberships) = source.openRegistered(linkedSetOf("a", "b"), 4, 4) {}
    source.admit("b", byteArrayOf(1))
    source.admit("a", byteArrayOf(2))
    val records = source.drain(group)
    assertEquals(memberships.getValue("b").sourceMember, (records[0] as OrderedNotificationSource.Event.Value).sourceMember)
    assertEquals(memberships.getValue("a").sourceMember, (records[1] as OrderedNotificationSource.Event.Value).sourceMember)
  }
  @Test fun cleanupClaimsSurviveCloseAndRequireExactSourceAndEpoch() {
    val source = OrderedNotificationSource<String>(13)
    val (group, memberships) = source.openRegistered(setOf("a"), 4, 4) {}
    val original = memberships.getValue("a")
    val first = source.retainPhysicalClaim(group, "a", original)
    assertEquals(first, source.retainPhysicalClaim(group, "a", original))
    assertTrue(source.retire(group, "a", original))
    val replacement = source.register(group, "a", original)
    try { source.retainPhysicalClaim(group, "a", original); fail("stale claim retained the replacement") }
    catch (_: IllegalStateException) { }
    val second = source.retainPhysicalClaim(group, "a", replacement)
    source.close(group); source.drain(group)
    assertFalse(source.remove(group))
    try { source.confirmPhysicalCleanup(group, first.copy(sourceGeneration = first.sourceGeneration + 1uL)); fail("wrong generation") }
    catch (_: IllegalStateException) { }
    try { source.confirmPhysicalCleanup(group, first.copy(membership = first.membership.copy(epoch = second.membership.epoch))); fail("wrong epoch") }
    catch (_: IllegalStateException) { }
    source.confirmPhysicalCleanup(group, first)
    assertFalse(source.remove(group))
    source.confirmPhysicalCleanup(group, second)
    assertTrue(source.remove(group))
  }
  @Test fun confirmedClaimCannotBeReacquiredAndOldConfirmationCannotClearNextEpoch() {
    val source = OrderedNotificationSource<String>(14)
    val (group, memberships) = source.openRegistered(setOf("a"), 4, 4) {}
    val original = memberships.getValue("a")
    val first = source.retainPhysicalClaim(group, "a", original)
    source.confirmPhysicalCleanup(group, first)
    try { source.retainPhysicalClaim(group, "a", original); fail("confirmed claim reacquired") }
    catch (_: IllegalStateException) { }
    assertTrue(source.retire(group, "a", original))
    val replacement = source.register(group, "a", original)
    val second = source.retainPhysicalClaim(group, "a", replacement)
    source.confirmPhysicalCleanup(group, first)
    source.close(group)
    source.drain(group)
    assertFalse(source.remove(group))
    source.confirmPhysicalCleanup(group, second)
    assertTrue(source.remove(group))
  }
}
