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
  @Test fun foregroundServiceWrapperPreservesItsStableIdentityOverSecurityCause() {
    val failure = classifyBackgroundFailure(com.sfourdrinier.unifiedblemanager.background.ForegroundServiceControlException(
      "foregroundServicePermissionDenied", "Check Bluetooth permission and foreground-service entitlements", SecurityException("binder denied")
    ))
    assertEquals(RadioFailureKind.PERMISSION_DENIED, failure.kind)
    assertEquals("foregroundServicePermissionDenied", failure.nativeCode)
    assertEquals("Check Bluetooth permission and foreground-service entitlements", failure.detail)
  }

  @Test fun executorCleanupAndStatusDoNotRequireRecordingStorage() {
    host.continuationStore().saveDeclaration(nativeJson.dropLast(1) + ",\"recording\":{\"id\":\"capture\",\"maxBytes\":1048576,\"maxRecords\":10}}")
    host.attachRecordingDirectory { throw IllegalStateException("storage unavailable") }
    assertEquals(null, host.continuationExecutor().describeBacklog())
    val reply = Captured()
    sessions.declareContinuation("{\"onAppearance\":\"record-only\",\"resubscribe\":[]}", reply)
    assertTrue(reply.rejected.isEmpty())
    assertEquals(ContinuationStrategy.RECORD_ONLY, host.continuationStore().loadDeclaration().strategy)
  }

  @Test fun nativeRecordingExecutionStillRequiresStorageBeforeRadioAcquisition() {
    val declaration = com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationDeclaration.parse(
      nativeJson.dropLast(1) + ",\"recording\":{\"id\":\"capture\",\"maxBytes\":1048576,\"maxRecords\":10}}"
    )
    host.attachRecordingDirectory { throw IllegalStateException("storage unavailable") }
    try {
      host.executeNativeContinuation("AA:BB:CC:DD:EE:FF", declaration)
      throw AssertionError("Recording execution must refuse unavailable storage")
    } catch (expected: RustCoreRejection) {
      assertEquals("platform.failure", expected.code)
      assertEquals("continuation.recording.configure", expected.operation)
    }
    assertEquals(0, core.installCount)
  }
  @Test fun recordingConfigurationPreservesSafeStructuredStorageFailure() {
    val directory = java.nio.file.Files.createTempDirectory("ubm-recording-error").toFile()
    try {
      host.attachRecordingDirectory { directory }
      core.recordingConfigure = { """{"ok":false,"error":{"code":"platform.failure","domain":"platform","operation":"continuation.recording","detail":"storage unavailable","platform":{"domain":"sqlite","code":"14","message":"storage unavailable","metadata":{"sqliteExtendedCode":14}}}}""" }
      val reply = Captured()
      sessions.recordingControl("status", "capture", "", 0.0, 0.0, reply)
      val failure = reply.rejected.single()
      assertEquals("continuation.recording", failure.operation)
      assertEquals("sqlite", failure.platform?.get("domain"))
      assertEquals("14", failure.platform?.get("code"))
      assertFalse(failure.toJson().contains(directory.path))
      assertEquals(0, core.installCount)
    } finally { directory.delete() }
  }
  @Test fun durableControlConfiguresNativePrivateDirectoryWithoutOpeningBleSession() {
    val directory = java.nio.file.Files.createTempDirectory("ubm-recording-test").toFile()
    try {
      val order = mutableListOf<String>()
      host.attachRecordingDirectory { directory }
      core.recordingConfigure = { path ->
        assertEquals(directory.canonicalPath, path)
        order.add("configure")
        "{\"ok\":true,\"value\":null}"
      }
      core.recordingControl = { operation, id, token, items, bytes ->
        assertEquals("prepare", operation); assertEquals("recording_1", id)
        assertEquals("", token); assertEquals(2, items); assertEquals(4096, bytes)
        order.add("control")
        "{\"ok\":true,\"value\":{\"id\":\"recording_1\"}}"
      }
      val reply = Captured()
      sessions.recordingControl("prepare", "recording_1", "", 2.0, 4096.0, reply)
      assertTrue(reply.rejected.isEmpty())
      assertEquals(listOf("configure", "control"), order)
      assertTrue(core.openScopes.isEmpty())
      assertEquals("offline controls must not initialize BLE", 0, core.installCount)
    } finally { directory.delete() }
  }

  @Test fun privateDirectoryRefusalPreventsDurableControl() {
    val directory = java.nio.file.Files.createTempDirectory("ubm-recording-test").toFile()
    try {
      host.attachRecordingDirectory { directory }
      core.recordingConfigure = { "{\"ok\":false,\"error\":{\"code\":\"platform.failure\"}}" }
      core.recordingControl = { _, _, _, _, _ -> throw AssertionError("must not control unconfigured journal") }
      val reply = Captured()
      sessions.recordingControl("status", "recording_1", "", 0.0, 0.0, reply)
      assertEquals(1, reply.rejected.size)
      assertTrue(reply.resolved.isEmpty())
    } finally { directory.delete() }
  }

  @Test fun invalidDurableAndClaimBoundsNameTheirOwnOperationWithoutNativeEffects() {
    core.recordingControl = { _, _, _, _, _ -> throw AssertionError("invalid recording limits reached native storage") }
    val recording = Captured()
    sessions.recordingControl("prepare", "capture", "", 0.0, 4096.0, recording)
    assertEquals("argument.invalid", recording.rejected.single().code)
    assertEquals("continuation.recording.prepare", recording.rejected.single().operation)
    assertTrue(recording.resolved.isEmpty())
    assertEquals(0, core.installCount)

    val claim = Captured()
    sessions.prepareContinuationClaim(Double.NaN, 65536.0, claim)
    assertEquals("argument.invalid", claim.rejected.single().code)
    assertEquals("continuation.claim", claim.rejected.single().operation)
    assertTrue(claim.resolved.isEmpty())
    assertEquals(0, core.installCount)
  }
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
