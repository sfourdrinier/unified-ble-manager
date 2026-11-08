// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/NotificationOwnershipFifoTest.kt
package com.sfourdrinier.unifiedblemanager.radio

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread

class NotificationOwnershipFifoTest {
  private fun NotificationOwnershipFifo.owner(generation: Long) =
    (arm(generation) as NotificationOwnershipFifo.Admission.Armed).claim

  @Test fun activationReentrancyAndSameGenerationFencePreserveOrder() {
    val events = mutableListOf<NotificationOwnershipFifo.Event>()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    lateinit var fifo: NotificationOwnershipFifo
    lateinit var first: NotificationOwnershipFifo.Claim
    fifo = NotificationOwnershipFifo(8, 100, 2, {}) { event ->
      events.add(event)
      if (event is NotificationOwnershipFifo.Event.Value && event.bytes[0] == 1.toByte()) {
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
      }
    }
    first = fifo.owner(11)
    val input = byteArrayOf(1)
    fifo.offer(first, 11, input)
    input[0] = 9
    fifo.offer(first, 11, byteArrayOf(2))
    val worker = thread { fifo.activate(first, 11) }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    assertTrue(fifo.retire(first, 11))
    val next = fifo.owner(11)
    assertFalse(fifo.offer(first, 11, byteArrayOf(99)))
    assertFalse(fifo.offer(next, 12, byteArrayOf(99)))
    fifo.offer(next, 11, byteArrayOf(3))
    fifo.activate(next, 11)
    release.countDown()
    worker.join(5000)
    assertFalse(worker.isAlive)
    assertEquals(4, events.size)
    assertArrayEquals(byteArrayOf(1), (events[0] as NotificationOwnershipFifo.Event.Value).bytes)
    assertArrayEquals(byteArrayOf(2), (events[1] as NotificationOwnershipFifo.Event.Value).bytes)
    assertSame(first, events[2].claim)
    assertTrue(events[2] is NotificationOwnershipFifo.Event.Terminal)
    assertSame(next, events[3].claim)
  }

  @Test fun reentrantOfferRunsAfterQueuedValues() {
    val events = mutableListOf<Int>()
    lateinit var fifo: NotificationOwnershipFifo
    lateinit var owner: NotificationOwnershipFifo.Claim
    fifo = NotificationOwnershipFifo(4, 10, 1, {}) { event ->
      if (event is NotificationOwnershipFifo.Event.Value) {
        events.add(event.bytes[0].toInt())
        if (events.size == 1) fifo.offer(owner, 1, byteArrayOf(3))
      }
    }
    owner = fifo.owner(1)
    fifo.offer(owner, 1, byteArrayOf(1))
    fifo.offer(owner, 1, byteArrayOf(2))
    fifo.activate(owner, 1)
    assertEquals(listOf(1, 2, 3), events)
  }

  @Test fun overflowReportsExactCoalescedLossBeforeTerminal() {
    val events = mutableListOf<NotificationOwnershipFifo.Event>()
    val fifo = NotificationOwnershipFifo(2, 3, 1, {}) { events.add(it) }
    val owner = fifo.owner(7)
    fifo.offer(owner, 7, byteArrayOf(1, 2))
    fifo.offer(owner, 7, byteArrayOf(3))
    assertTrue(fifo.offer(owner, 7, byteArrayOf(4, 5, 6, 7)))
    assertTrue(fifo.offer(owner, 7, byteArrayOf(8)))
    val failure = IllegalStateException("native")
    fifo.retire(owner, 7, failure)
    val loss = events[2] as NotificationOwnershipFifo.Event.Loss
    assertEquals(2L, loss.items)
    assertEquals(5L, loss.bytes)
    assertSame(failure, (events[3] as NotificationOwnershipFifo.Event.Terminal).failure)
    for ((itemCapacity, byteCapacity) in listOf(1 to 100L, 5 to 1L)) {
      val seen = mutableListOf<NotificationOwnershipFifo.Event>()
      val single = NotificationOwnershipFifo(itemCapacity, byteCapacity, 1, {}) { seen.add(it) }
      val claim = single.owner(1)
      single.offer(claim, 1, byteArrayOf(1))
      single.offer(claim, 1, byteArrayOf(2))
      single.activate(claim, 1)
      assertEquals(1L, (seen.last() as NotificationOwnershipFifo.Event.Loss).bytes)
    }
  }

  @Test fun throwingConsumerStopsWithExactUncertainTail() {
    val cause = IllegalStateException("consumer")
    val fifo = NotificationOwnershipFifo(2, 3, 1, {}) { throw cause }
    val owner = fifo.owner(1)
    fifo.offer(owner, 1, byteArrayOf(1, 2))
    fifo.offer(owner, 1, byteArrayOf(3))
    fifo.offer(owner, 1, byteArrayOf(4, 5))
    fifo.activate(owner, 1)
    val stop = fifo.stopReason() as NotificationOwnershipFifo.Stop.DeliveryUncertain
    assertSame(cause, stop.cause)
    assertEquals(3L, stop.uncertainItems)
    assertEquals(5L, stop.uncertainBytes)
    assertFalse(fifo.offer(owner, 1, byteArrayOf(9)))
    assertTrue(fifo.arm(2) is NotificationOwnershipFifo.Admission.Stopped)
  }

  @Test fun exhaustedControlCapacityFailsStoppedWithUncertainTail() {
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val signals = mutableListOf<NotificationOwnershipFifo.Stop>()
    val fifo = NotificationOwnershipFifo(4, 10, 1, { signals.add(it) }) { event ->
      if (event is NotificationOwnershipFifo.Event.Value) {
        entered.countDown()
        check(release.await(5, TimeUnit.SECONDS))
      }
    }
    val owner = fifo.owner(1)
    fifo.offer(owner, 1, byteArrayOf(1))
    fifo.offer(owner, 1, byteArrayOf(2, 3))
    val worker = thread { fifo.activate(owner, 1) }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    fifo.retire(owner, 1)
    val rejected = fifo.arm(1) as NotificationOwnershipFifo.Admission.Stopped
    assertTrue(rejected.reason is NotificationOwnershipFifo.Stop.ControlCapacity)
    assertEquals(listOf(rejected.reason), signals)
    assertEquals(2L, rejected.reason.uncertainItems)
    assertEquals(3L, rejected.reason.uncertainBytes)
    release.countDown()
    worker.join(5000)
    assertFalse(worker.isAlive)
  }
  @Test fun downstreamFailureSignalsOnceWithoutAnotherOffer() {
    val started = CountDownLatch(1)
    val release = CountDownLatch(1)
    val signalled = CountDownLatch(1)
    val signals = mutableListOf<NotificationOwnershipFifo.Stop>()
    val cause = IllegalStateException("delivery failed")
    val fifo = NotificationOwnershipFifo(4, 10, 1, { reason ->
      signals.add(reason)
      signalled.countDown()
    }) {
      started.countDown()
      check(release.await(5, TimeUnit.SECONDS))
      throw cause
    }
    val owner = fifo.owner(4)
    fifo.offer(owner, 4, byteArrayOf(1))
    val worker = thread { fifo.activate(owner, 4) }
    assertTrue(started.await(5, TimeUnit.SECONDS))
    release.countDown()
    assertTrue(signalled.await(5, TimeUnit.SECONDS))
    worker.join(5000)
    assertFalse(worker.isAlive)
    assertEquals(1, signals.size)
    assertSame(cause, (signals.single() as NotificationOwnershipFifo.Stop.DeliveryUncertain).cause)
    assertTrue(fifo.arm(5) is NotificationOwnershipFifo.Admission.Stopped)
    assertEquals(1, signals.size)
  }

  @Test fun stopHandlerExceptionPropagatesAndKeepsStoppedState() {
    val failure = IllegalStateException("stop handler failed")
    val fifo = NotificationOwnershipFifo(1, 1, 1, { throw failure }) { throw IllegalArgumentException("delivery") }
    val owner = fifo.owner(1)
    fifo.offer(owner, 1, byteArrayOf(1))
    try {
      fifo.activate(owner, 1)
      fail("expected stop handler exception")
    } catch (caught: IllegalStateException) {
      assertSame(failure, caught)
    }
    assertTrue(fifo.stopReason() is NotificationOwnershipFifo.Stop.DeliveryUncertain)
  }

  @Test fun reentrantCapacityStopHandlerExceptionReachesDrainer() {
    val failure = IllegalStateException("stop handler")
    var notifications = 0
    lateinit var fifo: NotificationOwnershipFifo
    lateinit var owner: NotificationOwnershipFifo.Claim
    fifo = NotificationOwnershipFifo(2, 4, 1, {
      notifications++
      throw failure
    }) { event ->
      if (event is NotificationOwnershipFifo.Event.Value) {
        assertTrue(fifo.retire(owner, 1))
        fifo.arm(1)
      }
    }
    owner = fifo.owner(1)
    fifo.offer(owner, 1, byteArrayOf(1))
    try {
      fifo.activate(owner, 1)
      fail("expected stop handler exception")
    } catch (caught: IllegalStateException) {
      assertSame(failure, caught)
    }
    assertTrue(fifo.stopReason() is NotificationOwnershipFifo.Stop.ControlCapacity)
    assertEquals(1, notifications)
    assertTrue(fifo.arm(1) is NotificationOwnershipFifo.Admission.Stopped)
    assertEquals(1, notifications)
  }

}
