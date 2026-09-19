// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/CoreCommandAuthorityTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * R02 authority table pins (HOST-JVM, pure — no Android, no JNI).
 *
 * The lifecycle/link/scan/discovery slice with binding wire coverage is
 * admission-gated; every other command class is a documented scoped
 * exception (unwired GATT IO, no native op surface, or local lifecycle) and
 * must stay radio-only rather than fake core attestation.
 */
class CoreCommandAuthorityTest {

  @Test
  fun lifecycleLinkScanDiscoveryCommandsRequireAdmission() {
    listOf("scanStart", "scanStop", "connect", "disconnect", "discover").forEach { kind ->
      assertTrue("$kind must be admission-gated", CoreCommandAuthority.requiresAdmission(kind))
    }
  }

  @Test
  fun gattIoCommandsAreScopedExceptionAUnwiredSurface() {
    // Native verbs exist but the Android binding exposes no path/lease-context
    // posts for them: authority would be faked, so they stay radio-only.
    listOf(
      "read",
      "write",
      "readDescriptor",
      "writeDescriptor",
      "subscribe",
      "unsubscribe"
    ).forEach { kind ->
      assertFalse("$kind must not claim core admission", CoreCommandAuthority.requiresAdmission(kind))
      assertTrue(
        "$kind must carry no core event correlation",
        CoreCommandAuthority.coreEventsFor(kind).isEmpty()
      )
    }
  }

  @Test
  fun linkQualityCommandsAreScopedExceptionBNoOpSurface() {
    listOf(
      "readRssi",
      "requestMtu",
      "readMtu",
      "requestPriority",
      "readPhy",
      "requestPhy"
    ).forEach { kind ->
      assertFalse("$kind must not claim core admission", CoreCommandAuthority.requiresAdmission(kind))
      assertTrue(
        "$kind must carry no core event correlation",
        CoreCommandAuthority.coreEventsFor(kind).isEmpty()
      )
    }
  }

  @Test
  fun securityAndBondedCommandsAreScopedExceptionBNoOpSurface() {
    listOf(
      "securityState",
      "securityPair",
      "securityCancelPairing",
      "enumerateBondedPeers"
    ).forEach { kind ->
      assertFalse("$kind must not claim core admission", CoreCommandAuthority.requiresAdmission(kind))
      assertTrue(
        "$kind must carry no core event correlation",
        CoreCommandAuthority.coreEventsFor(kind).isEmpty()
      )
    }
  }

  @Test
  fun localLifecycleCommandsAreScopedExceptionCLocalOnly() {
    listOf("cancel", "destroy").forEach { kind ->
      assertFalse("$kind must not claim core admission", CoreCommandAuthority.requiresAdmission(kind))
      assertTrue(
        "$kind must carry no core event correlation",
        CoreCommandAuthority.coreEventsFor(kind).isEmpty()
      )
    }
  }

  @Test
  fun admissionEventsCoverTheBindingWireSlice() {
    assertEquals(setOf("scan.start"), CoreCommandAuthority.coreEventsFor("scanStart"))
    assertEquals(setOf("scan.stop"), CoreCommandAuthority.coreEventsFor("scanStop"))
    assertEquals(
      setOf("peer.resolve", "connect", "link.established"),
      CoreCommandAuthority.coreEventsFor("connect")
    )
    assertEquals(setOf("disconnect"), CoreCommandAuthority.coreEventsFor("disconnect"))
    assertEquals(
      setOf("discovery.begin", "discovery.complete", "discovery.fail"),
      CoreCommandAuthority.coreEventsFor("discover")
    )
  }

  @Test
  fun rejectionBindingIsTheInverseMap() {
    assertEquals(setOf("scanStart"), CoreCommandAuthority.commandKindsForCoreEvent("scan.start"))
    assertEquals(setOf("scanStop"), CoreCommandAuthority.commandKindsForCoreEvent("scan.stop"))
    assertEquals(setOf("connect"), CoreCommandAuthority.commandKindsForCoreEvent("peer.resolve"))
    assertEquals(setOf("connect"), CoreCommandAuthority.commandKindsForCoreEvent("connect"))
    assertEquals(setOf("connect"), CoreCommandAuthority.commandKindsForCoreEvent("link.established"))
    assertEquals(setOf("disconnect"), CoreCommandAuthority.commandKindsForCoreEvent("disconnect"))
    assertEquals(setOf("discover"), CoreCommandAuthority.commandKindsForCoreEvent("discovery.begin"))
    assertEquals(setOf("discover"), CoreCommandAuthority.commandKindsForCoreEvent("discovery.complete"))
    assertEquals(setOf("discover"), CoreCommandAuthority.commandKindsForCoreEvent("discovery.fail"))
  }

  @Test
  fun pureEventPathLinesBindNoCommand() {
    // Adapter reset, peer loss, services-changed and link release report
    // physical facts outside any pending command: diagnose only.
    listOf("adapter.reset", "peer.loss", "services-changed", "link.released").forEach { event ->
      assertTrue(
        "$event must bind no pending command",
        CoreCommandAuthority.commandKindsForCoreEvent(event).isEmpty()
      )
    }
  }

  @Test
  fun unknownKindsAndEventsDefaultToRadioOnly() {
    assertFalse(CoreCommandAuthority.requiresAdmission("unsupportedCommand"))
    assertTrue(CoreCommandAuthority.coreEventsFor("unsupportedCommand").isEmpty())
    assertTrue(CoreCommandAuthority.commandKindsForCoreEvent("no.such-verb").isEmpty())
  }

  @Test
  fun failureCodesAreActionableCoreIdentities() {
    assertEquals("coreUnavailable", CoreCommandAuthority.CODE_UNAVAILABLE)
    assertEquals("corePermissionDenied", CoreCommandAuthority.CODE_PERMISSION_DENIED)
    assertEquals("coreEnqueueFailed", CoreCommandAuthority.CODE_ENQUEUE_FAILED)
    assertEquals("coreScheduleFailed", CoreCommandAuthority.CODE_SCHEDULE_FAILED)
    assertEquals("coreRejected", CoreCommandAuthority.CODE_REJECTED)
  }
}
