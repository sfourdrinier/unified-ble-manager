// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/DeferredCoreShadowCauseTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.content.Context
import android.content.pm.PackageManager
import java.util.concurrent.Executor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.mockito.Mockito

/**
 * R02 authority aid: [DeferredCoreShadow.lastCause] names the already-reported
 * outage cause for fail-loud command terminals. Demand-retry and diagnosis
 * semantics are unchanged (see [DeferredCoreShadowTest]); this pins only the
 * read-only cause accessor.
 */
class DeferredCoreShadowCauseTest {

  private val direct = Executor { command -> command.run() }

  private class FakeJni(
    var linkedRevision: String = UbmGattCoreBinding.CONTRACT_REVISION
  ) : UbmGattCoreBinding.CoreJni {
    override fun open(revision: String): Long = 7L
    override fun revision(): String = linkedRevision
    override fun enqueue(handle: Long, wire: String): Int = 1
    override fun drain(handle: Long): String = ""
    override fun depth(handle: Long): Int = 0
    override fun close(handle: Long) {}
  }

  private fun permittedContext(): Context {
    val context = Mockito.mock(Context::class.java)
    Mockito.`when`(context.applicationContext).thenReturn(context)
    Mockito.`when`(context.checkSelfPermission(Mockito.anyString())).thenReturn(
      PackageManager.PERMISSION_GRANTED
    )
    return context
  }

  private fun binding(jni: FakeJni): UbmGattCoreBinding =
    UbmGattCoreBinding(
      permittedContext(),
      jni = jni,
      worker = direct,
      clockMs = { 1000L }
    )

  @Test
  fun outageCauseIsVisibleWithoutOpening() {
    var factoryCalls = 0
    val gate = DeferredCoreShadow(
      factory = {
        factoryCalls++
        null
      },
      diagnose = { _, _ -> },
      opener = null
    )
    val cause = gate.lastCause()
    assertNotNull("outage cause must be visible for fail-loud terminals", cause)
    assertTrue(cause!!, cause.contains("factory returned no binding"))
    assertEquals("reading the cause opens nothing", 1, factoryCalls)
    gate.release()
  }

  @Test
  fun failedBindingCauseNamesTheOpenFailure() {
    val gate = DeferredCoreShadow(
      factory = { binding(FakeJni(linkedRevision = "WRONG-REVISION")) },
      diagnose = { _, _ -> },
      opener = null
    )
    val cause = gate.lastCause()
    assertNotNull(cause)
    assertTrue(cause!!, cause.contains("protocol.incompatible"))
    gate.release()
  }

  @Test
  fun causeClearsOnRecovery() {
    var healthy = false
    val gate = DeferredCoreShadow(
      factory = { if (healthy) binding(FakeJni()) else null },
      diagnose = { _, _ -> },
      opener = null
    )
    assertNotNull(gate.lastCause())
    healthy = true
    assertNotNull("demand retry recovers", gate.current())
    assertNull("recovered shadow reports no cause", gate.lastCause())
    gate.release()
  }

  @Test
  fun causeClearsOnRelease() {
    val gate = DeferredCoreShadow(
      factory = { null },
      diagnose = { _, _ -> },
      opener = null
    )
    assertNotNull(gate.lastCause())
    gate.release()
    assertNull("released gate reports no cause", gate.lastCause())
    gate.release()
  }
}
