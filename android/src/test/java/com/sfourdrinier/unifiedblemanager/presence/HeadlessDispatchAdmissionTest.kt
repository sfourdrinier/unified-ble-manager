package com.sfourdrinier.unifiedblemanager.presence

import org.junit.Assert.*
import org.junit.Test

class HeadlessDispatchAdmissionTest {
  @Test fun `timeout rejects late startup without dispatching JS`() {
    val admission = HeadlessDispatchAdmission()
    val failed = admission.await(1)
    var calls = 0
    assertFalse(admission.dispatch { calls++; ContinuationOutcome.Completed(ContinuationStrategy.HEADLESS_TASK, "peer", 0, "task-dispatched") })
    assertEquals(0, calls)
    assertEquals("operation.timed-out", (failed as ContinuationOutcome.Failed).code)
  }
  @Test fun `dispatch reports acceptance only after dispatch returns and preserves refusal`() {
    val admission = HeadlessDispatchAdmission()
    assertTrue(admission.dispatch { throw SecurityException("OS refused cold start") })
    val failed = admission.await(1) as ContinuationOutcome.Failed
    assertEquals("permission.denied", failed.code)
    assertTrue(failed.platform!!.contains("java.lang.SecurityException"))
    val accepted = HeadlessDispatchAdmission()
    accepted.dispatch { ContinuationOutcome.Completed(ContinuationStrategy.HEADLESS_TASK, "peer", 0, "task-dispatched") }
    assertEquals("task-dispatched", (accepted.await(1) as ContinuationOutcome.Completed).stage)
  }
}
