// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreContinuationSessionsTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.presence.ContinuationStrategy
import com.ubm.core.MobileCoreBridge
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.security.SecureRandom

/** The JS declare/status/claim surface persists and reports the standing order. */
class RustCoreContinuationSessionsTest {
  private val core = FakeCore()
  private val logs = mutableListOf<String>()
  private val host = RustCoreProcessHost(core, { unusedRadioHost() }) { logs.add(it) }
  private val sessions = RustCoreSessions(core, host, DirectExecutor, {}, { null }, SecureRandom(), { logs.add(it) })

  private fun unusedRadioHost(): MobileCoreBridge.RadioHost =
    java.lang.reflect.Proxy.newProxyInstance(
      javaClass.classLoader,
      arrayOf(MobileCoreBridge.RadioHost::class.java)
    ) { _, method, _ -> throw AssertionError("radio host must not be driven here: ${method.name}") } as MobileCoreBridge.RadioHost

  private val nativeJson =
    "{\"onAppearance\":\"native\"," +
      "\"resubscribe\":[{\"serviceUuid\":\"0000180d-0000-1000-8000-00805f9b34fb\"," +
      "\"serviceOccurrence\":1,\"characteristicUuid\":\"00002a37-0000-1000-8000-00805f9b34fb\"," +
      "\"characteristicOccurrence\":1}]}"

  private class Captured : RustCoreSessions.Reply {
    val resolved = mutableListOf<String?>()
    val rejected = mutableListOf<RustCoreRejection>()
    override fun resolve(value: String?) {
      resolved.add(value)
    }

    override fun reject(rejection: RustCoreRejection) {
      rejected.add(rejection)
    }
  }

  @Test
  fun declarePersistsAValidOrder() {
    val reply = Captured()
    sessions.declareContinuation(nativeJson, reply)
    assertEquals(listOf("{\"state\":\"declared\"}"), reply.resolved)
    assertTrue(reply.rejected.isEmpty())
    assertEquals(ContinuationStrategy.NATIVE, host.continuationStore().loadDeclaration().strategy)
  }

  @Test
  fun declareRefusesMalformedOrdersWithNoEffect() {
    val reply = Captured()
    sessions.declareContinuation("{\"onAppearance\":\"auto-magic\",\"resubscribe\":[]}", reply)
    assertTrue(reply.resolved.isEmpty())
    assertEquals(1, reply.rejected.size)
    assertEquals(ContinuationStrategy.RECORD_ONLY, host.continuationStore().loadDeclaration().strategy)
  }

  @Test
  fun statusReportsTheDeclaredStrategy() {
    sessions.declareContinuation(nativeJson, Captured())
    val reply = Captured()
    sessions.continuationStatus(reply)
    val status = reply.resolved.single() ?: error("no status")
    assertTrue(status.contains("\"strategy\":\"native\""))
    assertTrue(status.contains("\"resubscribe\":1"))
    assertTrue(status.contains("\"lastWake\":null"))
  }

  @Test
  fun reservationRefusalCannotPersistAReplacement() {
    sessions.declareContinuation(nativeJson, Captured())
    core.continuationReserveAnswer = { "{\"ok\":false,\"error\":{\"code\":\"lifecycle.invalid-state\",\"detail\":\"execution owns declaration\"}}" }
    val reply = Captured()
    sessions.declareContinuation("{}", reply)
    assertEquals("lifecycle.invalid-state", reply.rejected.single().code)
    assertEquals(ContinuationStrategy.NATIVE, host.continuationStore().loadDeclaration().strategy)
  }

  @Test
  fun statusKeepsRecoverySeparateFromOriginalWake() {
    core.continuationBacklog = "{\"ok\":true,\"value\":{\"continuationOutcome\":{\"event\":\"continuation.failed\",\"strategy\":\"native\",\"attempt\":2,\"error\":{\"code\":\"permission.denied\"}}}}"
    val reply = Captured()
    sessions.continuationStatus(reply)
    val status = RustCoreJson.parse(reply.resolved.single()!!) as? Map<*, *> ?: error("status")
    assertEquals(null, status["lastWake"])
    assertEquals(2L, (status["lastRecovery"] as? Map<*, *>)?.get("attempt"))
  }

  @Test
  fun claimWithNoWakeIsTheValidEmptyAnswer() {
    core.continuationPrepareAnswer = { items, bytes, callback ->
      assertEquals(256, items)
      assertEquals(65536, bytes)
      callback.onResult("{\"ok\":true,\"value\":{\"consumerCount\":0,\"selectors\":[],\"batches\":[],\"disposed\":false,\"disposeFailure\":null,\"afterCutoffLoss\":{\"items\":0,\"bytes\":0}}}")
    }
    val reply = Captured()
    sessions.prepareContinuationClaim(256.0, 65536.0, reply)
    val claim = reply.resolved.single() ?: error("no claim")
    assertTrue(claim.contains("\"consumerCount\":0"))
    assertFalse(claim.contains("\"claimToken\""))
    assertTrue(claim.contains("\"selectors\":[]"))
    assertTrue(claim.contains("\"batches\":[]"))
    assertTrue(claim.contains("\"disposed\":false"))
  }

  @Test
  fun claimRejectionPreservesNativeIdentityAndPlatformDetail() {
    core.continuationPrepareAnswer = { _, _, callback ->
      callback.onResult("{\"ok\":false,\"error\":{\"code\":\"lifecycle.invalid-state\",\"domain\":\"restoration\",\"operation\":\"continuation.claim\",\"detail\":\"native handoff busy\",\"platform\":{\"domain\":\"test-radio\",\"code\":\"busy\",\"safeMessage\":\"native busy\",\"metadata\":{}}}}")
    }
    val reply = Captured()
    sessions.prepareContinuationClaim(256.0, 65536.0, reply)
    assertTrue(reply.resolved.isEmpty())
    val failure = reply.rejected.single()
    assertEquals("lifecycle.invalid-state", failure.code)
    assertEquals("restoration", failure.domain)
    assertEquals("native handoff busy", failure.detail)
    val decoded = RustCoreJson.parse(failure.toJson()) as? Map<*, *> ?: error("rejection JSON missing")
    assertEquals("test-radio", (decoded["platform"] as? Map<*, *>)?.get("domain"))
  }
}
