package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.FakeCore
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreRejection
import com.ubm.core.MobileCoreBridge
import org.junit.Assert.*
import org.junit.Test

/** Kotlin owns transport mapping only; the JNI roundtrip exercises the actual shared owner. */
class RustCoreContinuationBindingTest {
  private val peer = "AA:BB:CC:DD:EE:FF"
  private val selector = ContinuationSelector(
    "0000180d-0000-1000-8000-00805f9b34fb", 1,
    "00002a37-0000-1000-8000-00805f9b34fb", 1
  )
  private val declaration = BackgroundContinuationDeclaration(
    ContinuationStrategy.NATIVE, peer, listOf(selector), null, null
  )
  private val core = FakeCore()
  private val logs = mutableListOf<String>()
  private val executor = NativeContinuationBinding(core, logs::add)

  @Test fun trustedRawExecutionPreservesCanonicalEnvelopeAndSeedRefusalWithoutWakeMapping() {
    val answer = "{\"ok\":false,\"error\":{\"code\":\"connection.failed\",\"domain\":\"connection\",\"operation\":\"connect\",\"detail\":\"refused\",\"platform\":{\"domain\":\"android\",\"code\":\"133\",\"message\":\"refused\",\"metadata\":{}}},\"commit\":null,\"retryability\":\"caller-decides\"}"
    var calls = 0
    core.continuationExecuteAnswer = { _, _, callback -> calls++; callback.onResult(answer) }
    var actual: String? = null
    executor.executeRaw(peer, declaration, MobileCoreBridge.InvokeCallback { actual = it })
    assertEquals(answer, actual)
    assertEquals(1, calls)
    core.continuationSeedAnswer = { answer }
    actual = null
    executor.executeRaw(peer, declaration, MobileCoreBridge.InvokeCallback { actual = it })
    assertEquals(answer, actual)
    assertEquals("seed refusal cannot dispatch", 1, calls)
    assertTrue(core.invokes.isEmpty() && core.openScopes.isEmpty())
  }

  @Test fun trustedRawHandoffPreservesCountersValuesAndFailureWithoutAutomaticAcknowledgement() {
    val prepared = prepared()
    var acknowledgements = 0
    core.continuationPrepareAnswer = { items, bytes, callback ->
      assertEquals(7, items)
      assertEquals(8192, bytes)
      callback.onResult(prepared)
    }
    val acknowledgement = "{\"ok\":false,\"error\":{\"code\":\"connection.failed\",\"domain\":\"connection\",\"operation\":\"disconnect\",\"detail\":\"retained\"}}"
    core.continuationAcknowledgeAnswer = { token, callback ->
      assertEquals("exact-token", token)
      acknowledgements++
      callback.onResult(acknowledgement)
    }
    core.continuationBacklog = "{\"ok\":true,\"value\":{\"counters\":{\"retainedByteBuffers\":9},\"continuationOutcome\":null}}"
    var actual: String? = null
    val callback = MobileCoreBridge.InvokeCallback { actual = it }
    executor.describeBacklogRaw(callback)
    assertEquals(core.continuationBacklog, actual)
    executor.prepareClaimRaw(7, 8192, callback)
    assertEquals(prepared, actual)
    assertEquals(0, acknowledgements)
    executor.acknowledgeClaimRaw("exact-token", callback)
    assertEquals(acknowledgement, actual)
    assertEquals(1, acknowledgements)
  }

  @Test fun platformExecutionRejectsCapturedDeclarationAfterSharedAuthorityChanged() {
    val captured = declaration.copy(strategy = ContinuationStrategy.HEADLESS_TASK, resubscribe = emptyList(), headlessTaskName = "BleWake")
    executor.persistDeclaration(BackgroundContinuationDeclaration.recordOnly()) {}
    core.continuationSeedAnswer = { "{\"ok\":false,\"error\":{\"code\":\"lifecycle.invalid-state\",\"detail\":\"captured declaration replaced\"}}" }
    var calls = 0
    val outcome = executor.executePlatform(captured) { calls++; ContinuationOutcome.Completed(captured.strategy, peer, 0, "task-dispatched") }
    assertEquals(0, calls)
    assertEquals("lifecycle.invalid-state", (outcome as ContinuationOutcome.Failed).code)
    assertEquals(captured.strategy, outcome.strategy)
  }

  @Test fun replacementAuthorityComesFromSharedOwner() {
    core.continuationReplacementFailure = "old generation still owns values"
    assertEquals("old generation still owns values", executor.declarationReplacementFailure(declaration))
    core.continuationReplacementFailure = null
    assertNull(executor.declarationReplacementFailure(declaration))
  }

  @Test fun failedPersistenceCancelsReservationWithoutCommit() {
    val failure = IllegalStateException("storage refused")
    try {
      executor.persistDeclaration(declaration) { throw failure }
      fail("persistence failure must propagate")
    } catch (actual: IllegalStateException) { assertSame(failure, actual) }
    assertEquals(listOf("cancel:reservation-1"), core.declarationCalls)
  }

  @Test fun successfulPersistenceCommitsOnlyAfterStorageCompletes() {
    executor.persistDeclaration(declaration) {
      assertTrue("commit must follow persistence", core.declarationCalls.isEmpty())
    }
    assertEquals(listOf("commit:reservation-1"), core.declarationCalls)
  }

  @Test fun executionPassesPublicOccurrenceIdentityUnchangedToSharedOwner() {
    core.continuationExecuteAnswer = { actualPeer, json, callback ->
      assertEquals(peer, actualPeer)
      val parsed = BackgroundContinuationDeclaration.parse(json)
      assertEquals(declaration, parsed)
      callback.onResult("{\"ok\":true,\"value\":{\"event\":\"continuation.completed\",\"strategy\":\"native\",\"peerAddress\":\"$peer\",\"resubscribed\":1}}")
    }
    assertEquals(ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1), executor.execute(peer, declaration))
    assertTrue("Kotlin must not issue its own session or radio operations", core.invokes.isEmpty() && core.openScopes.isEmpty())
  }

  @Test fun declarationTransportRefusalIsAnOutcomeWithoutRadioAdmission() {
    core.continuationSeedAnswer = { throw IllegalStateException("JNI owner unavailable") }
    val outcome = executor.execute(peer, declaration)
    if (outcome !is ContinuationOutcome.Failed) throw AssertionError("expected a typed failure")
    assertEquals("platform.failure", outcome.code)
    assertTrue(outcome.reason.contains("JNI owner unavailable"))
    assertTrue(core.invokes.isEmpty() && core.openScopes.isEmpty())
  }

  @Test fun setupIsForwardedToTheSharedOwnerWithoutDroppingAnyField() {
    val declared = declaration.copy(setup = listOf(
      ContinuationSetupStep(selector, listOf(0, 255), 20000,
        ContinuationSetupResponse(0, listOf(240, 2), 4, 5, 3, listOf(0, 255),
          ContinuationSetupTrailing(4, listOf(0)))),
      ContinuationSetupStep(selector, listOf(1), 1, null)
    ), link = ContinuationLinkMtu(517, 20000, "continue"),
      recording = ContinuationRecording("session_1", 1048576, 1000000))
    core.continuationExecuteAnswer = { _, json, callback ->
      assertEquals(declared, BackgroundContinuationDeclaration.parse(json))
      callback.onResult("{\"ok\":true,\"value\":{\"event\":\"continuation.completed\",\"strategy\":\"native\",\"peerAddress\":\"$peer\",\"resubscribed\":1}}")
    }
    assertEquals(ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, 1), executor.execute(peer, declared))
  }

  @Test fun refusedExecutionPreservesPlatformFailure() {
    core.continuationExecuteAnswer = { _, _, callback ->
      callback.onResult("{\"ok\":false,\"error\":{\"code\":\"connection.failed\",\"detail\":\"radio refused\",\"platform\":{\"nativeCode\":\"133\"}}}")
    }
    val result = executor.execute(peer, declaration)
    if (result !is ContinuationOutcome.Failed) throw AssertionError("expected a typed failure")
    assertEquals("connection.failed", result.code)
    assertEquals("radio refused", result.reason)
    assertEquals(mapOf("nativeCode" to "133"), RustCoreJson.parse(result.platform!!))
  }

  @Test fun malformedSuccessCannotBecomeACompletedWake() {
    core.continuationExecuteAnswer = { _, _, callback -> callback.onResult("{\"ok\":true,\"value\":{}}") }
    val result = executor.execute(peer, declaration)
    assertTrue(result is ContinuationOutcome.Failed)
  }

  private fun prepared(overrides: Map<String, Any?> = emptyMap()): String {
    val fields = linkedMapOf<String, Any?>(
      "consumerCount" to 1,
      "selectors" to listOf(mapOf("serviceUuid" to selector.serviceUuid,
        "serviceOccurrence" to 1, "characteristicUuid" to selector.characteristicUuid,
        "characteristicOccurrence" to 1)),
      "batches" to listOf("{\"more\":false,\"controlLost\":0,\"records\":[]}"),
      "disposed" to false, "disposeFailure" to null,
      "afterCutoffLoss" to mapOf("items" to 2, "bytes" to 8), "claimToken" to "claim-7"
    )
    fields.putAll(overrides)
    return RustCoreJson.write(mapOf("ok" to true, "value" to fields))
  }

  @Test fun preparingForwardsBoundsAndDoesNotAcknowledge() {
    core.continuationPrepareAnswer = { items, bytes, callback ->
      assertEquals(128, items)
      assertEquals(4096, bytes)
      callback.onResult(prepared())
    }
    val result = executor.prepareClaim(128, 4096)
    assertEquals("claim-7", result.claimToken)
    assertEquals(listOf(selector), result.selectors)
    assertEquals(CutoffLoss(2, 8), result.afterCutoffLoss)
    assertFalse(result.disposed)
    assertEquals(1, result.batches.size)
  }

  @Test fun invalidClaimIdentityCannotAuthorizeAcknowledgement() {
    for (overrides in listOf(mapOf("consumerCount" to 2), mapOf("claimToken" to ""),
      mapOf("batches" to listOf(1)), mapOf("disposed" to true), mapOf("consumerCount" to 1.5))) {
      core.continuationPrepareAnswer = { _, _, callback -> callback.onResult(prepared(overrides)) }
      try {
        executor.prepareClaim(128, 4096)
        fail("malformed prepared response was accepted: $overrides")
      } catch (expected: RuntimeException) {
        assertNotNull(expected.message)
      }
    }
  }

  @Test fun claimCarriesOnlyDurableRecordingIdentityWithoutAcknowledgingItsCursor() {
    core.continuationPrepareAnswer = { _, _, callback -> callback.onResult(prepared(mapOf("recording" to mapOf("id" to "session_1")))) }
    assertEquals("session_1", executor.prepareClaim(128, 4096).recordingId)
    for (reference in listOf(mapOf("id" to "../escape"), mapOf("id" to "session_1", "cursor" to "unauthorized"))) {
      core.continuationPrepareAnswer = { _, _, callback -> callback.onResult(prepared(mapOf("recording" to reference))) }
      assertThrows(RuntimeException::class.java) { executor.prepareClaim(128, 4096) }
    }
  }

  @Test fun failedCleanupReceiptRetainsTheNativeOutcomeAndLoss() {
    core.continuationAcknowledgeAnswer = { token, callback ->
      assertEquals("claim-7", token)
      callback.onResult("{\"ok\":true,\"value\":{\"disposed\":false,\"disposeFailure\":\"retry native release\",\"afterCutoffLoss\":{\"items\":3,\"bytes\":9}}}")
    }
    val result = executor.acknowledgeClaim("claim-7")
    assertFalse(result.disposed)
    assertEquals("retry native release", result.disposeFailure)
    assertEquals(CutoffLoss(3, 9), result.afterCutoffLoss)
  }

  @Test fun backlogFailureIsNotConvertedIntoNoOwnedSession() {
    core.continuationBacklog = "{\"ok\":false,\"error\":{\"code\":\"platform.failure\",\"detail\":\"counter unavailable\"}}"
    try {
      executor.describeBacklog()
      fail("unreadable backlog must be reported")
    } catch (expected: RuntimeException) {
      assertEquals("counter unavailable", expected.message)
    }
    core.continuationBacklog = "{\"ok\":true,\"value\":null}"
    assertNull(executor.describeBacklog())
  }

  @Test fun bridgeThrowBecomesAnExplicitFailure() {
    core.continuationExecuteAnswer = { _, _, _ -> throw IllegalStateException("JNI rejected") }
    val result = executor.execute(peer, declaration)
    if (result !is ContinuationOutcome.Failed) throw AssertionError("expected bridge failure")
    assertEquals("platform.failure", result.code)
    assertTrue(result.reason.contains("JNI rejected"))
  }

  @Test fun asynchronousInvokeThrowPreservesStructuredNativeFailure() {
    core.continuationPrepareAnswer = { _, _, _ ->
      throw MobileCoreBridge.MobileCoreException("permission.denied|permissions|native.claim|denied | by policy")
    }
    try {
      executor.prepareClaim(128, 4096)
      fail("native exception must remain a rejection")
    } catch (failure: RustCoreRejection) {
      assertEquals("permission.denied", failure.code)
      assertEquals("permissions", failure.domain)
      assertEquals("native.claim", failure.operation)
      assertEquals("denied | by policy", failure.detail)
    }
  }

  @Test fun synchronousInvokeThrowPreservesStructuredNativeFailureWithoutPersistence() {
    core.continuationReserveAnswer = {
      throw MobileCoreBridge.MobileCoreException("lifecycle.invalid-state|lifecycle|native.declaration|ownership retained")
    }
    var persisted = false
    try {
      executor.persistDeclaration(declaration) { persisted = true }
      fail("native exception must remain a rejection")
    } catch (failure: RustCoreRejection) {
      assertEquals("lifecycle.invalid-state", failure.code)
      assertEquals("lifecycle", failure.domain)
      assertEquals("native.declaration", failure.operation)
      assertEquals("ownership retained", failure.detail)
    }
    assertFalse(persisted)
    assertTrue(core.declarationCalls.isEmpty())
  }
}
