// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/OrderedMarkerTransactionsTest.kt
package com.sfourdrinier.unifiedblemanager.radio

import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.concurrent.thread
import org.junit.Assert.*
import org.junit.Test

class OrderedMarkerTransactionsTest {
  private val identity = OrderedMarkerTransactions.Identity(
    AndroidNativeGraphIdentity("owner", 1L, 2uL), "connection",
    OwnedAndroidNotificationPath(UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb"), 0,
      UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb"), 0),
    byteArrayOf(1, 2), true
  )

  @Test fun cancellationGateSerializesAgainstNativeAdmission() {
    val gate = OrderedMarkerCancellation()
    gate.cancel()
    var invoked = false
    assertNull(gate.admit { invoked = true })
    assertFalse(invoked)
    val live = OrderedMarkerCancellation()
    val entered = CountDownLatch(1); val finish = CountDownLatch(1); val cancelled = CountDownLatch(1)
    val worker = thread { live.admit { entered.countDown(); check(finish.await(5, TimeUnit.SECONDS)) } }
    assertTrue(entered.await(5, TimeUnit.SECONDS))
    val canceller = thread { live.cancel(); cancelled.countDown() }
    assertTrue(cancelled.await(1, TimeUnit.SECONDS))
    finish.countDown(); worker.join(5000); canceller.join(5000)
    assertEquals(0L, cancelled.count)
    assertNull(live.admit { true })
  }

  @Test fun cancellationBeforeAdmissionPreventsEffectAndRetryCannotRedispatch() {
    val owner = OrderedMarkerTransactions()
    owner.reserve(0uL, identity)
    val cancelled = owner.cancel(0uL, IllegalStateException("cancelled"))
    var calls = 0
    assertSame(cancelled, owner.admit(0uL) { calls++; OrderedMarkerTransactions.State.Started(0uL) })
    assertEquals(0, calls)
    assertSame(cancelled, owner.reserve(0uL, identity).state)
    try { owner.reserve(0uL, identity.copy(bytes = byteArrayOf(9))); fail("changed request accepted") }
    catch (_: IllegalStateException) { }
    assertFalse(owner.forget(0uL))
    owner.adapterSettledBeforeSchedule(0uL)
    assertTrue(owner.forget(0uL))
    try { owner.reserve(0uL, identity); fail("forgotten ID reused") }
    catch (_: IllegalStateException) { }
  }

  @Test fun exactCancelRetainsReceiptUntilPendingAdapterCellSettles() {
    val owner = OrderedMarkerTransactions()
    owner.reserve(0uL, identity)
    assertTrue(owner.cancelExact(0uL, OrderedMarkerCancelled()).first is OrderedMarkerTransactions.State.NotAdmitted)
    assertFalse(owner.forget(0uL))
    owner.adapterSettledBeforeSchedule(0uL)
    assertTrue(owner.forget(0uL))
  }

  @Test fun pendingWriteInitiationIsObservableWithoutRedispatch() {
    val owner = OrderedMarkerTransactions()
    owner.reserve(0uL, identity)
    val inside = CountDownLatch(1)
    val finish = CountDownLatch(1)
    val cancelled = CountDownLatch(1)
    var calls = 0
    val worker = thread {
      owner.admit(0uL) { started ->
        started(8uL)
        calls++
        inside.countDown()
        check(finish.await(5, TimeUnit.SECONDS))
        OrderedMarkerTransactions.State.Started(8uL)
      }
    }
    assertTrue(inside.await(5, TimeUnit.SECONDS))
    var cancellation: OrderedMarkerTransactions.State? = null
    val canceller = thread { cancellation = owner.cancel(0uL, IllegalStateException("cancelled")); cancelled.countDown() }
    assertTrue(cancelled.await(1, TimeUnit.SECONDS))
    assertEquals(OrderedMarkerTransactions.State.Initiating(8uL), cancellation)
    assertEquals(OrderedMarkerTransactions.State.Initiating(8uL), owner.state(0uL))
    finish.countDown()
    worker.join(5000); canceller.join(5000)
    assertEquals(OrderedMarkerTransactions.State.Started(8uL), owner.state(0uL))
    assertFalse(owner.forget(0uL))
    owner.settle(0uL, Result.success(byteArrayOf(7)))
    assertEquals(1, calls)
    assertTrue(owner.reserve(0uL, identity).state is OrderedMarkerTransactions.State.Settled)
    assertFalse(owner.forget(0uL))
    owner.acknowledgeEvidence(0uL)
    owner.driverSettled(0uL)
    assertTrue(owner.forget(0uL))
    assertTrue(owner.forget(0uL))
    assertFalse(owner.forget(1uL))
  }

  @Test fun failureAfterInitiationAndCancellationPreservesAmbiguousSubmission() {
    val owner = OrderedMarkerTransactions()
    owner.reserve(0uL, identity)
    val failure = IllegalStateException("platform invocation threw")
    val state = owner.admit(0uL) { initiating ->
      initiating(5uL)
      assertTrue(owner.cancelExact(0uL, OrderedMarkerCancelled()).first is OrderedMarkerTransactions.State.Initiating)
      throw failure
    }
    val started = state as OrderedMarkerTransactions.State.Started
    assertEquals(5uL, started.ordinal)
    assertSame(failure, started.uncertainty)
    owner.driverSettled(0uL)
    assertFalse(owner.forget(0uL))
  }
}
