// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/DeferredCoreShadowTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import android.content.Context
import android.content.pm.PackageManager
import java.util.ArrayDeque
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.mockito.Mockito

/**
 * R02 remainder (a): the dispatcher must not touch JNI synchronously at
 * construction, and a shadow that fails to open must recover instead of
 * staying permanently disabled. [DeferredCoreShadow] opens off the
 * constructing thread, publishes the ready shadow, retries on demand, and
 * reports each distinct failure cause exactly once.
 */
class DeferredCoreShadowTest {

  private val direct = Executor { command -> command.run() }

  private class FakeJni(
    var linkedRevision: String = UbmGattCoreBinding.CONTRACT_REVISION,
    var drains: ArrayDeque<String> = ArrayDeque()
  ) : UbmGattCoreBinding.CoreJni {
    var opened = 0
    var closed = 0

    override fun open(revision: String): Long {
      opened++
      return opened.toLong()
    }
    override fun revision(): String = linkedRevision
    override fun enqueue(handle: Long, wire: String): Int = 1
    override fun drain(handle: Long): String =
      if (drains.isEmpty()) "" else drains.removeFirst()
    override fun depth(handle: Long): Int = 0
    override fun close(handle: Long) {
      closed++
    }
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
  fun constructionDefersTheOpenOffTheCallingThread() {
    val tasks = mutableListOf<Runnable>()
    val manual = Executor { command -> tasks.add(command) }
    var factoryCalls = 0
    val gate = DeferredCoreShadow(
      factory = {
        factoryCalls++
        binding(FakeJni())
      },
      diagnose = { _, _ -> },
      opener = manual
    )
    // The constructor returned without touching the factory: the open is
    // queued on the provided executor instead of running on this thread.
    assertEquals("factory untouched during construction", 0, factoryCalls)
    assertEquals("one open task queued", 1, tasks.size)
    tasks.forEach { it.run() }
    assertEquals("open ran on the executor", 1, factoryCalls)
    assertNotNull("shadow ready after the queued open", gate.current())
    gate.release()
  }

  @Test
  fun openRunsOnABackgroundThread() {
    val opener = Executors.newSingleThreadExecutor()
    try {
      val opened = CountDownLatch(1)
      var openThread: String? = null
      val gate = DeferredCoreShadow(
        factory = {
          openThread = Thread.currentThread().name
          opened.countDown()
          binding(FakeJni())
        },
        diagnose = { _, _ -> },
        opener = opener
      )
      assertTrue("background open completes", opened.await(10, TimeUnit.SECONDS))
      assertTrue(
        "open ran off the constructing thread (was $openThread)",
        openThread != null && openThread != Thread.currentThread().name
      )
      assertNotNull(gate.current())
      gate.release()
    } finally {
      opener.shutdownNow()
    }
  }

  @Test
  fun currentReturnsTheReadyShadowWithoutReopening() {
    var factoryCalls = 0
    val gate = DeferredCoreShadow(
      factory = {
        factoryCalls++
        binding(FakeJni())
      },
      diagnose = { _, _ -> fail("no diagnosis expected for a healthy shadow") },
      opener = null
    )
    val first = gate.current()
    assertNotNull(first)
    assertSame(first, gate.current())
    assertSame(first, gate.current())
    assertEquals("inline open at construction, no reopen", 1, factoryCalls)
    gate.release()
  }

  @Test
  fun missingShadowDiagnosesOnceAndRecoversOnDemand() {
    var factoryCalls = 0
    val diagnoses = mutableListOf<String>()
    val gate = DeferredCoreShadow(
      factory = {
        factoryCalls++
        if (factoryCalls < 3) null else binding(FakeJni())
      },
      diagnose = { code, detail -> diagnoses.add("$code|$detail") },
      opener = null
    )
    // Construction attempt (attempt 1) already failed: the first demand
    // retry (attempt 2) still finds nothing.
    assertNull("no shadow while the factory is empty", gate.current())
    assertEquals("factory: init + first demand", 2, factoryCalls)
    assertEquals("one diagnosis for the repeated cause", 1, diagnoses.size)
    assertTrue(diagnoses.single(), diagnoses.single().startsWith("coreShadowUnavailable|"))
    // The next demand recovers and resets the diagnosis latch.
    assertNotNull("demand retry recovers the shadow", gate.current())
    assertEquals(3, factoryCalls)
    gate.release()
  }

  @Test
  fun throwingFactoryDiagnosesAndRecoversOnDemand() {
    var factoryCalls = 0
    val diagnoses = mutableListOf<String>()
    val gate = DeferredCoreShadow(
      factory = {
        factoryCalls++
        if (factoryCalls == 1) throw IllegalStateException("no .so yet")
        binding(FakeJni())
      },
      diagnose = { code, detail -> diagnoses.add("$code|$detail") },
      opener = null
    )
    assertEquals("construction attempt threw into a diagnosis", 1, diagnoses.size)
    assertTrue(diagnoses.single(), diagnoses.single().contains("no .so yet"))
    assertNotNull("demand retry recovers", gate.current())
    assertEquals(2, factoryCalls)
    assertEquals("recovery emits no new diagnosis", 1, diagnoses.size)
    gate.release()
  }

  @Test
  fun failedBindingIsReplacedOnDemand() {
    val jni = FakeJni(linkedRevision = "WRONG-REVISION")
    var factoryCalls = 0
    val diagnoses = mutableListOf<String>()
    val gate = DeferredCoreShadow(
      factory = {
        factoryCalls++
        binding(if (factoryCalls == 1) jni else FakeJni())
      },
      diagnose = { code, detail -> diagnoses.add("$code|$detail") },
      opener = null
    )
    // The construction attempt kept a revision-mismatched binding: the
    // first demand replaces it with a healthy one.
    assertNotNull("failed shadow replaced on demand", gate.current())
    assertEquals(2, factoryCalls)
    assertEquals(1, diagnoses.size)
    assertTrue(diagnoses.single(), diagnoses.single().contains("protocol.incompatible"))
    assertSame("healthy shadow is stable", gate.current(), gate.current())
    assertEquals("no further attempts once healthy", 2, factoryCalls)
    gate.release()
  }

  @Test
  fun consecutiveIdenticalCausesReportOnceAndResetOnRecovery() {
    var mode = "null"
    val diagnoses = mutableListOf<String>()
    val gate = DeferredCoreShadow(
      factory = {
        when (mode) {
          "null" -> null
          "throw" -> throw IllegalStateException("boom")
          else -> binding(FakeJni())
        }
      },
      diagnose = { code, detail -> diagnoses.add("$code|$detail") },
      opener = null
    )
    gate.current()
    gate.current()
    assertEquals("repeated null cause reports once", 1, diagnoses.size)
    mode = "throw"
    gate.current()
    assertEquals("changed cause reports again", 2, diagnoses.size)
    mode = "healthy"
    assertNotNull(gate.current())
    assertSame("healthy shadow is kept, factory not re-polled", gate.current(), gate.current())
    // Release drops the healthy shadow: the next outage reports fresh
    // instead of staying suppressed behind the old latch.
    gate.release()
    mode = "null"
    gate.current()
    assertEquals("fresh outage after recovery reports again", 3, diagnoses.size)
    gate.release()
  }

  @Test
  fun releaseTakesTheShadowWithoutReopening() {
    var factoryCalls = 0
    val gate = DeferredCoreShadow(
      factory = {
        factoryCalls++
        binding(FakeJni())
      },
      diagnose = { _, _ -> fail("no diagnosis expected") },
      opener = null
    )
    assertNotNull(gate.current())
    assertEquals(1, factoryCalls)
    gate.release()
    assertEquals("release opens nothing", 1, factoryCalls)
    // Documented demand semantics: a later use reopens lazily instead of
    // wedging on a released shadow.
    assertNotNull(gate.current())
    assertEquals(2, factoryCalls)
    gate.release()
  }
}
