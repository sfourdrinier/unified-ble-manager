// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreSessionsTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.ubm.core.MobileCoreBridge
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.security.SecureRandom
import java.util.ArrayDeque
import java.util.concurrent.Executor

class RustCoreSessionsTest {
  @Test fun scanRecoveryCannotOpenUntilTrustedNativeBootstrapSelectsARecording() {
    val host = RustCoreProcessHost(FakeCore(), { unusedRadioHost() }, { _, task -> task.run() }) { }
    try {
      host.openSelectedScanFailureRecovery()
      fail("unselected recovery must not be opened")
    } catch (error: RustCoreRejection) {
      assertEquals("capability.unsupported", error.code)
      assertEquals("scan.failure-recovery.open", error.operation)
    }
  }

  @Test fun invokeAndDrainRequireContextOwnershipAndRejectDisposingSessions() {
    val native = FakeCore()
    val process = RustCoreProcessHost(native, { unusedRadioHost() }, { _, task -> task.run() }) { }
    val first = RustCoreSessions(native, process, DirectExecutor, {}, { null }, SecureRandom(), {})
    val second = RustCoreSessions(native, process, DirectExecutor, {}, { null }, SecureRandom(), {})
    Captured().also { first.openSession("first", "ubm-mobile-wire/3", it) }.single()
    native.openRecord = { "{\"sessionId\":8}" }
    Captured().also { second.openSession("second", "ubm-mobile-wire/3", it) }.single()
    val foreignInvoke = Captured(); second.invoke("7", "session.dispose", "{}", foreignInvoke)
    assertEquals("ownership.denied", foreignInvoke.rejection().code)
    val foreignDrain = Captured(); second.drain("7", 1.0, 128.0, foreignDrain)
    assertEquals("ownership.denied", foreignDrain.rejection().code)
    val reverse = Captured(); first.drain("8", 1.0, 128.0, reverse)
    assertEquals("ownership.denied", reverse.rejection().code)
    assertTrue(native.invokes.isEmpty())
    val disposal = Captured(); first.invoke("7", "session.dispose", "{}", disposal)
    val during = Captured(); first.drain("7", 1.0, 128.0, during)
    assertEquals("ownership.denied", during.rejection().code)
    native.callbacks.last().onResult("{\"ok\":true,\"value\":{\"state\":\"released\"}}")
    disposal.single()
    val after = Captured(); first.invoke("7", "adapter.state", "{}", after)
    assertEquals("ownership.denied", after.rejection().code)
    val close = Captured(); second.closeSession("8", close)
    val duringClose = Captured(); second.invoke("8", "adapter.state", "{}", duringClose)
    assertEquals("ownership.denied", duringClose.rejection().code)
    native.callbacks.last().onResult("{\"ok\":true,\"value\":{\"state\":\"released\"}}")
    close.single()
  }

  @Test fun queuedInvokeAndDrainCannotOutrunContextInvalidation() {
    val native = FakeCore()
    val queue = QueuedExecutor()
    val process = RustCoreProcessHost(native, { unusedRadioHost() }, { _, task -> task.run() }) { }
    val module = RustCoreSessions(native, process, queue, {}, { null }, SecureRandom(), {})
    val opened = Captured(); module.openSession("first", "ubm-mobile-wire/3", opened); queue.runAll(); opened.single()
    val invoke = Captured(); module.invoke("7", "adapter.state", "{}", invoke)
    val drain = Captured(); module.drain("7", 1.0, 128.0, drain)
    module.invalidate()
    queue.runAll()
    assertEquals("lifecycle.destroyed", invoke.rejection().code)
    assertEquals("lifecycle.destroyed", drain.rejection().code)
    assertEquals(listOf("session.dispose"), native.invokes.map { it.second })
    assertTrue(native.calls.none { it.startsWith("drain:") })
  }
  @Test fun retainedClaimV1UsesTheOwningPortAndReturnsNativeValues() {
    val calls = mutableListOf<String>()
    var retirement = "{\"ok\":true,\"value\":{}}"
    val native = object : MobileCorePort by FakeCore() {
      override fun continuationPrepareClaimVersioned(version: Int, maxItems: Int, maxBytes: Int, callback: MobileCoreBridge.InvokeCallback) {
        calls.add("prepare:$version:$maxItems:$maxBytes")
        callback.onResult("{\"ok\":true,\"value\":{\"claimToken\":\"claim\"}}")
      }
      override fun continuationAcknowledgeClaimV1(token: String, callback: MobileCoreBridge.InvokeCallback) {
        calls.add("ack:$token"); callback.onResult("{\"ok\":true,\"value\":{\"disposed\":true}}")
      }
      override fun continuationRetireClaimReceipt(token: String, callback: MobileCoreBridge.InvokeCallback) {
        calls.add("retire:$token"); callback.onResult(retirement)
      }
    }
    val process = RustCoreProcessHost(native, { unusedRadioHost() }, { _, task -> task.run() }) { }
    val module = RustCoreSessions(native, process, DirectExecutor, {}, { null }, SecureRandom(), {})
    val prepared = Captured(); module.prepareRetainedContinuationClaimV1(2.0, 4096.0, prepared)
    assertEquals("{\"claimToken\":\"claim\"}", prepared.single())
    val ack = Captured(); module.acknowledgeRetainedContinuationClaimV1("claim", ack)
    assertEquals("{\"disposed\":true}", ack.single())
    val retired = Captured(); module.retireRetainedContinuationClaimReceiptV1("claim", retired)
    assertEquals("{}", retired.single())
    assertEquals(listOf("prepare:1:2:4096", "ack:claim", "retire:claim"), calls)
    retirement = "{\"ok\":false,\"error\":{\"code\":\"ownership.denied\",\"domain\":\"restoration\",\"operation\":\"retire\",\"detail\":\"wrong owner\"}}"
    val refused = Captured(); module.retireRetainedContinuationClaimReceiptV1("foreign", refused)
    assertEquals("ownership.denied", refused.rejection().code)
    retirement = "{bad json"
    val malformed = Captured(); module.retireRetainedContinuationClaimReceiptV1("claim", malformed)
    assertEquals("protocol.malformed", malformed.rejection().code)
    val invalid = Captured(); module.prepareRetainedContinuationClaimV1(Double.NaN, 5.0, invalid)
    assertEquals("argument.invalid", invalid.rejection().code)
  }

  @Test fun retainedClaimV2UsesExplicitPrepareAndAcknowledgeSymbols() {
    val calls = mutableListOf<String>()
    val native = object : MobileCorePort by FakeCore() {
      override fun continuationPrepareClaimVersioned(version: Int, maxItems: Int, maxBytes: Int, callback: MobileCoreBridge.InvokeCallback) {
        calls.add("prepare:$version:$maxItems:$maxBytes")
        callback.onResult("{\"ok\":true,\"value\":{\"claimToken\":\"v2\",\"version\":2}}")
      }
      override fun continuationAcknowledgeClaimV2(token: String, callback: MobileCoreBridge.InvokeCallback) {
        calls.add("ack-v2:$token")
        callback.onResult("{\"ok\":true,\"value\":{\"disposed\":true}}")
      }
      override fun continuationAcknowledgeClaimV1(token: String, callback: MobileCoreBridge.InvokeCallback) {
        calls.add("ack-v1:$token")
        callback.onResult("{\"ok\":true,\"value\":{}}")
      }
    }
    val process = RustCoreProcessHost(native, { unusedRadioHost() }, { _, task -> task.run() }) { }
    val module = RustCoreSessions(native, process, DirectExecutor, {}, { null }, SecureRandom(), {})
    val prepared = Captured(); module.prepareRetainedContinuationClaimV2(2.0, 4096.0, prepared)
    assertEquals("{\"claimToken\":\"v2\",\"version\":2}", prepared.single())
    val acknowledged = Captured(); module.acknowledgeRetainedContinuationClaimV2("v2", acknowledged)
    assertEquals("{\"disposed\":true}", acknowledged.single())
    assertEquals(listOf("prepare:2:2:4096", "ack-v2:v2"), calls)
    val invalid = Captured(); module.acknowledgeRetainedContinuationClaimV2("", invalid)
    assertEquals("argument.invalid", invalid.rejection().code)
  }

  @Test fun independentProcessHostsRouteOverlappingSessionIdsAndInvalidateIndependently() {
    val a = FakeCore()
    val b = FakeCore()
    val firstHost = RustCoreProcessHost(a, { unusedRadioHost() }, { _, task -> task.run() }) { }
    val secondHost = RustCoreProcessHost(b, { unusedRadioHost() }, { _, task -> task.run() }) { }
    val wakesA = mutableListOf<String>()
    val wakesB = mutableListOf<String>()
    val first = RustCoreSessions(a, firstHost, DirectExecutor, { wakesA.add(it) }, { null }, SecureRandom(), {})
    val second = RustCoreSessions(b, secondHost, DirectExecutor, { wakesB.add(it) }, { null }, SecureRandom(), {})
    Captured().also { first.openSession("a", "ubm-mobile-wire/3", it) }.single()
    Captured().also { second.openSession("b", "ubm-mobile-wire/3", it) }.single()
    a.installedWake!!.onWake(7)
    assertEquals(listOf("7"), wakesA)
    assertTrue(wakesB.isEmpty())
    b.installedWake!!.onWake(7)
    assertEquals(listOf("7"), wakesB)
    first.invalidate()
    a.callbacks.last().onResult("{\"ok\":true,\"value\":{\"state\":\"released\"}}")
    b.installedWake!!.onWake(7)
    assertEquals(listOf("7", "7"), wakesB)
    assertTrue(b.releasedScopes.isEmpty())
  }

  @Test fun failedInstallKeepsItsPortUntilCleanupAndThenUsesAFreshPort() {
    val tasks = ArrayDeque<Runnable>()
    var cleanupAllowed = false
    var cleanups = 0
    val failed = object : MobileCorePort by FakeCore() {
      override fun installHost(radio: MobileCoreBridge.RadioHost, wake: MobileCoreBridge.WakeListener, owner: String, adapterLabel: String) {
        throw IllegalStateException("install failed after retaining radio")
      }
      override fun cleanupFailedInstall(): Boolean { cleanups++; return cleanupAllowed }
    }
    val replacement = FakeCore()
    var created = 0
    val process = RustCoreProcessHost(failed, { unusedRadioHost() }, { _, task -> tasks.add(task) }, log = {})
    process.setCoreFactory { created++; replacement }
    try { process.ensureInstalled(); throw AssertionError("install succeeded") } catch (_: IllegalStateException) { }
    assertEquals(0, created)
    tasks.removeFirst().run()
    assertEquals(1, cleanups)
    assertEquals(0, created)
    cleanupAllowed = true
    tasks.removeFirst().run()
    assertEquals(2, cleanups)
    assertEquals(1, created)
    process.ensureInstalled()
    assertEquals(1, replacement.installCount)
    assertTrue(process.core === replacement)
  }

  private val core = FakeCore()
  private val logs = mutableListOf<String>()
  private val radioHost = unusedRadioHost()
  private var radioHostBuilds = 0
  private val host = RustCoreProcessHost(core, { radioHostBuilds++; radioHost }, { _, task -> task.run() }) { logs.add(it) }
  private val wakes = mutableListOf<String>()
  private var packageName: String? = "com.example.app"
  private val sessions = RustCoreSessions(core, host, DirectExecutor, { wakes.add(it) }, { packageName }, SecureRandom(), { logs.add(it) })

  @Test fun scanVersionNegotiationRequiresTheExactOwningModule() {
    val calls = mutableListOf<Pair<Long, String>>()
    val versionCore = object : MobileCorePort by core {
      override fun negotiateScanEndVersion(sessionId: Long, offeredJson: String): String {
        calls.add(sessionId to offeredJson)
        return "{\"version\":1}"
      }
    }
    val process = RustCoreProcessHost(versionCore, { unusedRadioHost() }, { _, task -> task.run() }) { }
    val first = RustCoreSessions(versionCore, process, DirectExecutor, {}, { packageName }, SecureRandom(), {})
    val second = RustCoreSessions(versionCore, process, DirectExecutor, {}, { packageName }, SecureRandom(), {})
    Captured().also { first.openSession("first", "ubm-mobile-wire/3", it) }.single()
    val denied = Captured(); second.negotiateScanEndVersion("7", "[1,2]", denied)
    assertEquals("ownership.denied", denied.rejection().code)
    val nonCanonical = Captured(); first.negotiateScanEndVersion("07", "[1,2]", nonCanonical)
    assertEquals("argument.invalid", nonCanonical.rejection().code)
    assertTrue(calls.isEmpty())
    val accepted = Captured(); first.negotiateScanEndVersion("7", "[1,2]", accepted)
    assertEquals("{\"version\":1}", accepted.single())
    assertEquals(listOf(7L to "[1,2]"), calls)
  }

  @Test fun orderedRecordingRequiresOwningModuleAndPreservesNativeEnvelopes() {
    val calls = mutableListOf<String>()
    val callbacks = mutableListOf<MobileCoreBridge.InvokeCallback>()
    val recordingCore = object : MobileCorePort by core {
      override fun orderedRecordingVersion() = 0
      override fun orderedRecordingReceiptControl(sessionId: Long, operation: String, id: String, callback: MobileCoreBridge.InvokeCallback) {
        calls.add("$operation:$sessionId:$id"); callback.onResult("{\"ok\":true,\"value\":{\"version\":1,\"state\":\"reserved\",\"handle\":\"handle-1\"}}")
      }
      override fun orderedRecordingStart(sessionId: Long, handle: String, id: String, maxBytes: String, maxRecords: String, declaration: String, budgetMs: Long, callback: MobileCoreBridge.InvokeCallback) {
        calls.add("start:$sessionId:$handle:$id:$maxBytes:$maxRecords:$budgetMs"); callbacks.add(callback)
      }
      override fun orderedRecordingContext(sessionId: Long, id: String, callback: MobileCoreBridge.InvokeCallback) { calls.add("context:$sessionId:$id"); callbacks.add(callback) }
      override fun orderedRecordingStop(sessionId: Long, id: String, callback: MobileCoreBridge.InvokeCallback) { calls.add("stop:$sessionId:$id"); callbacks.add(callback) }
      override fun orderedRecordingOffline(operation: String, id: String, callback: MobileCoreBridge.InvokeCallback) { calls.add("offline:$operation:$id"); callbacks.add(callback) }
    }
    val recordingHost = RustCoreProcessHost(recordingCore, { unusedRadioHost() }, { _, task -> task.run() }) { logs.add(it) }
    val directory = java.nio.file.Files.createTempDirectory("ubm-ordered-routing").toFile()
    try {
      recordingHost.attachRecordingDirectory { directory }
      val first = RustCoreSessions(recordingCore, recordingHost, DirectExecutor, {}, { packageName }, SecureRandom(), { logs.add(it) })
      val second = RustCoreSessions(recordingCore, recordingHost, DirectExecutor, {}, { packageName }, SecureRandom(), { logs.add(it) })
      val probe = Captured(); first.orderedRecordingVersion(probe); assertEquals("0", probe.single())
      val opened = Captured(); first.openSession("first", "ubm-mobile-wire/3", opened); opened.single()
      core.openRecord = { "{\"sessionId\":8}" }
      val other = Captured(); second.openSession("second", "ubm-mobile-wire/3", other); other.single()
      val nonCanonical = Captured(); first.orderedRecordingReceiptControl("07", "recording", "status", nonCanonical)
      assertEquals("argument.invalid", nonCanonical.rejection().code)
      val deniedReceipt = Captured(); second.orderedRecordingReceiptControl("7", "recording", "status", deniedReceipt)
      assertEquals("ownership.denied", deniedReceipt.rejection().code)
      val denied = Captured(); second.orderedRecordingControl("7", "recording", true, denied)
      assertEquals("ownership.denied", denied.rejection().code)
      val deniedStart = Captured(); second.orderedRecordingStart("7", "handle-1", "recording", "9", "8", "{}", 10.0, deniedStart)
      assertEquals("ownership.denied", deniedStart.rejection().code)
      assertTrue(calls.isEmpty())
      val reserve = Captured(); first.orderedRecordingReceiptControl("7", "recording", "reserve", reserve)
      assertEquals("{\"ok\":true,\"value\":{\"version\":1,\"state\":\"reserved\",\"handle\":\"handle-1\"}}", reserve.single())
      val status = Captured(); first.orderedRecordingReceiptControl("7", "handle-1", "status", status)
      status.single(); assertEquals("status:7:handle-1", calls.last())
      val retire = Captured(); first.orderedRecordingReceiptControl("7", "handle-1", "retire", retire)
      retire.single(); assertEquals("retire:7:handle-1", calls.last())
      val start = Captured(); first.orderedRecordingStart("7", "handle-1", "recording", "18446744073709551615", "8", "{}", 10.0, start)
      assertEquals("start:7:handle-1:recording:18446744073709551615:8:10", calls.last())
      val pending = "{\"ok\":true,\"value\":{\"state\":\"waiting-source\"}}"
      callbacks.removeAt(0).onResult(pending); assertEquals(pending, start.single())
      val offline = Captured(); first.orderedRecordingOffline("context", "recording", offline)
      val refused = "{\"ok\":false,\"error\":{\"code\":\"ownership.denied\"}}"
      callbacks.removeAt(0).onResult(refused); assertEquals(refused, offline.single())
      val cold = Captured(); first.orderedRecordingOffline("context", "recording", cold)
      val recovered = "{\"ok\":true,\"value\":{\"state\":\"interrupted\"}}"
      callbacks.removeAt(0).onResult(recovered); assertEquals(recovered, cold.single())
      val offlineStatus = Captured(); first.orderedRecordingOffline("status", "recording", offlineStatus)
      assertEquals("offline:status:recording", calls.last())
      callbacks.removeAt(0).onResult(recovered); assertEquals(recovered, offlineStatus.single())
      val offlineStop = Captured(); first.orderedRecordingOffline("stop", "recording", offlineStop)
      assertEquals("offline:stop:recording", calls.last())
      val finished = "{\"ok\":true,\"value\":{\"state\":\"finished\"}}"
      callbacks.removeAt(0).onResult(finished); assertEquals(finished, offlineStop.single())
      val context = Captured(); first.orderedRecordingControl("7", "recording", false, context)
      val nativeContext = "{\"ok\":true,\"value\":{\"peerId\":\"peer\"}}"
      callbacks.removeAt(0).onResult(nativeContext); assertEquals(nativeContext, context.single())
      val stop = Captured(); first.orderedRecordingControl("7", "recording", true, stop)
      callbacks.removeAt(0).onResult(pending); assertEquals(pending, stop.single())
    } finally { directory.deleteRecursively() }
  }

  @Test fun recoveryObserversSeeOnlyCompletedCleanupAndCanDetachWithoutRadio() {
    var first = 0
    var second = 0
    val observer = Runnable { first++ }
    val other = Runnable { second++ }
    host.addRecoveryObserver(observer)
    host.addRecoveryObserver(other)
    assertEquals(0, radioHostBuilds)
    open()
    val failed = Captured()
    sessions.invoke("7", "gatt.unsubscribe", "{}", failed)
    core.callbacks.last().onResult("{\"ok\":false,\"error\":{\"code\":\"platform.failure\"}}")
    assertEquals(0, first)
    val pending = Captured()
    sessions.invoke("7", "gatt.unsubscribe", "{}", pending)
    assertEquals(0, first) // initial recovery can run while the old cleanup is held
    core.callbacks.last().onResult("{\"ok\":true,\"value\":{\"state\":\"released\"}}")
    assertEquals(1, first)
    assertEquals(1, second)
    host.removeRecoveryObserver(observer)
    val dispose = Captured()
    sessions.invoke("7", "session.dispose", "{}", dispose)
    core.callbacks.last().onResult("{\"ok\":true,\"value\":{\"state\":\"released\"}}")
    assertEquals(1, first)
    assertEquals(2, second)
  }

  @Test fun recoveryHintWaitsForRetainedCleanupRetrySuccess() {
    val tasks = QueuedExecutor()
    val processHost = RustCoreProcessHost(core, { unusedRadioHost() }, { _, task -> tasks.execute(task) }, { logs.add(it) })
    var hints = 0
    processHost.addRecoveryObserver(Runnable { hints++ })
    processHost.retainSessionCleanup(7L, "failed")
    tasks.runNext()
    core.callbacks[0].onResult("{\"ok\":true,\"value\":{\"state\":\"release-failed\",\"failures\":[]}}")
    assertEquals(0, hints)
    tasks.runNext()
    core.callbacks[1].onResult("{\"ok\":true,\"value\":{\"state\":\"released\",\"failures\":[]}}")
    assertEquals(1, hints)
    assertTrue(processHost.retainedCleanupSessions().isEmpty())
  }

  @Test fun trustedDirectRecordingRefusesMissingStorageBeforeNativeAcquisition() {
    val declaration = com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationDeclaration.parse(
      """{"onAppearance":"native","peerId":"AA:BB:CC:DD:EE:FF","resubscribe":[],"recording":{"id":"direct","maxBytes":1048576,"maxRecords":1000}}"""
    )
    var executions = 0
    host.continuationStore().saveDeclaration("""{"onAppearance":"native","peerId":"AA:BB:CC:DD:EE:FF","resubscribe":[],"recording":{"id":"direct","maxBytes":1048576,"maxRecords":1000}}""")
    core.continuationExecuteAnswer = { _, _, _ -> executions++ }
    var result: String? = null
    host.executeNativeContinuationRaw("AA:BB:CC:DD:EE:FF", declaration, MobileCoreBridge.InvokeCallback { result = it })
    val envelope = RustCoreJson.parse(result!!) as Map<*, *>
    assertEquals(false, envelope["ok"])
    assertEquals("capability.unsupported", (envelope["error"] as Map<*, *>)["code"])
    assertEquals(0, executions)
    assertEquals(0, radioHostBuilds)
    assertNull(host.continuationStore().lastWakeOutcome())
  }

  @Test fun directColdExecutionRequiresPersistedMatchingOrderAndExplicitReplacement() {
    val json = """{"onAppearance":"native","peerId":"AA:BB:CC:DD:EE:FF","resubscribe":[]}"""
    val declaration = com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationDeclaration.parse(json)
    var calls = 0
    core.continuationExecuteAnswer = { _, _, callback -> calls++; callback.onResult("{\"ok\":true,\"value\":{}}") }
    var result: String? = null
    val callback = MobileCoreBridge.InvokeCallback { result = it }
    host.executeNativeContinuationRaw("AA:BB:CC:DD:EE:FF", declaration, callback)
    assertTrue(result!!.contains("Explicit matching persisted"))
    assertEquals(0, radioHostBuilds)
    host.continuationStore().saveDeclaration(json.replace("AA:BB:CC:DD:EE:FF", "00:11:22:33:44:55"))
    host.executeNativeContinuationRaw("AA:BB:CC:DD:EE:FF", declaration, callback)
    assertEquals(0, calls)
    host.continuationExecutor().persistDeclaration(declaration) { host.continuationStore().saveDeclaration(json) }
    host.executeNativeContinuationRaw("AA:BB:CC:DD:EE:FF", declaration, callback)
    assertEquals(1, calls)
    assertNull(host.continuationStore().lastWakeOutcome())
    val recordOnly = com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationDeclaration.recordOnly()
    host.continuationExecutor().persistDeclaration(recordOnly) { host.continuationStore().saveDeclaration("{\"onAppearance\":\"record-only\"}") }
    host.executeNativeContinuationRaw("AA:BB:CC:DD:EE:FF", declaration, callback)
    assertEquals(1, calls)
    val envelope = RustCoreJson.parse(result!!) as Map<*, *>
    assertEquals(setOf("ok", "error", "commit", "retryability"), envelope.keys)
    assertEquals("never", envelope["retryability"])
  }

  private class Captured : RustCoreSessions.Reply {
    val resolved = mutableListOf<String?>()
    val rejected = mutableListOf<RustCoreRejection>()
    override fun resolve(value: String?) {
      resolved.add(value)
    }

    override fun reject(rejection: RustCoreRejection) {
      rejected.add(rejection)
    }

    fun single(): String? {
      assertEquals("rejections: ${rejected.map { it.toJson() }}", 0, rejected.size)
      assertEquals(1, resolved.size)
      return resolved[0]
    }

    fun rejection(): RustCoreRejection {
      assertEquals(0, resolved.size)
      assertEquals(1, rejected.size)
      return rejected[0]
    }
  }

  private fun open(): Captured = Captured().also { sessions.openSession("manager-a", "ubm-mobile-wire/3", it) }

  private class QueuedExecutor : Executor {
    private val tasks = ArrayDeque<Runnable>()

    override fun execute(command: Runnable) {
      tasks.addLast(command)
    }

    fun runAll() {
      while (tasks.isNotEmpty()) tasks.removeFirst().run()
    }

    fun runNext() {
      tasks.removeFirst().run()
    }
  }

  @Test
  fun openInstallsTheProcessHostOnceAndResolvesTheAdmissionVerbatim() {
    assertEquals("{\"sessionId\":7,\"owner\":\"manager-a\"}", open().single())
    core.openRecord = { "{\"sessionId\":8}" }
    open().single()
    assertEquals(1, core.installCount)
    assertEquals(1, radioHostBuilds)
    assertEquals(setOf(7L, 8L), sessions.ownedSessions())
  }

  @Test
  fun admissionRefusalRejectsWithStructuredJson() {
    core.openFailure = MobileCoreBridge.MobileCoreException(
      "protocol.incompatible|core|ubm-mobile.session.open|caller speaks x, owner speaks ubm-mobile-wire/3"
    )
    val rejection = open().rejection()
    assertEquals("protocol.incompatible", rejection.code)
    assertEquals(
      "{\"code\":\"protocol.incompatible\",\"domain\":\"core\",\"operation\":\"ubm-mobile.session.open\"," +
        "\"detail\":\"caller speaks x, owner speaks ubm-mobile-wire/3\"}",
      rejection.toJson()
    )
    assertTrue(sessions.ownedSessions().isEmpty())
  }

  @Test
  fun emptyDetailBecomesNull() {
    assertEquals(
      "{\"code\":\"lifecycle.destroyed\",\"domain\":\"core\",\"operation\":\"mobile.session.drain\",\"detail\":null}",
      RustCoreRejection.fromWire("lifecycle.destroyed|core|mobile.session.drain|", "x").toJson()
    )
  }

  @Test
  fun invokePassesArgumentsAndEnvelopeVerbatim() {
    open()
    val reply = Captured()
    sessions.invoke("7", "gatt.read", "{\"peerId\":\"P\"}", reply)
    assertEquals(listOf(Triple(7L, "gatt.read", "{\"peerId\":\"P\"}")), core.invokes)
    assertTrue(reply.resolved.isEmpty())
    core.callbacks.single().onResult("{\"ok\":true,\"value\":{\"valueB64\":\"AA==\"}}")
    assertEquals("{\"ok\":true,\"value\":{\"valueB64\":\"AA==\"}}", reply.single())
  }

  @Test
  fun malformedSessionIdsAreRejectedBeforeRust() {
    listOf("", "-1", "7.0", "abc", "123456789012345678901").forEach { id ->
      val reply = Captured()
      sessions.invoke(id, "adapter.state", "{}", reply)
      assertEquals("argument.invalid", reply.rejection().code)
    }
    assertTrue(core.invokes.isEmpty())
  }

  @Test
  fun unknownSessionIsRejectedBeforeRust() {
    core.invokeFailure = MobileCoreBridge.MobileCoreException("lifecycle.destroyed|core|mobile.session.invoke|unknown or disposed session")
    val reply = Captured()
    sessions.invoke("99", "adapter.state", "{}", reply)
    assertEquals("ownership.denied", reply.rejection().code)
    assertTrue(core.invokes.isEmpty())
  }

  @Test
  fun drainValidatesBoundsAndResolvesVerbatim() {
    open()
    val reply = Captured()
    sessions.drain("7", 256.0, 65536.0, reply)
    assertEquals("{\"more\":false,\"records\":[]}", reply.single())
    assertEquals(listOf("drain:7:256:65536"), core.calls)
    val bad = Captured()
    sessions.drain("7", 0.5, 65536.0, bad)
    assertEquals("argument.invalid", bad.rejection().code)
  }

  @Test
  fun wakesRouteToTheOwningSessionAsDecimalStrings() {
    open()
    host.wake.onWake(7)
    host.wake.onWake(12)
    assertEquals(listOf("7"), wakes)
    assertEquals(1L, host.unroutedWakeCount())
  }

  @Test
  fun closeAfterJsDisposeForgetsWithoutASecondDispose() {
    open()
    sessions.invoke("7", "session.dispose", "{}", Captured())
    core.callbacks.single().onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    val close = Captured()
    sessions.closeSession("7", close)
    assertNull(close.single())
    assertEquals(1, core.invokes.size)
    host.wake.onWake(7)
    assertTrue(wakes.isEmpty())
  }

  @Test
  fun closeWithoutDisposeRunsDisposeAndIsIdempotent() {
    open()
    val close = Captured()
    sessions.closeSession("7", close)
    assertEquals(Triple(7L, "session.dispose", "{}"), core.invokes.single())
    core.callbacks.single().onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    assertNull(close.single())
    val again = Captured()
    sessions.closeSession("7", again)
    assertNull(again.single())
    assertEquals(1, core.invokes.size)
  }

  @Test
  fun releaseFailedKeepsTheSessionForARetry() {
    open()
    val close = Captured()
    sessions.closeSession("7", close)
    core.callbacks.single().onResult(
      "{\"ok\":true,\"value\":{\"failures\":[{\"resourceKind\":\"connection\",\"code\":\"platform.failure\"}],\"state\":\"release-failed\"}}"
    )
    val rejection = close.rejection()
    assertEquals("platform.failure", rejection.code)
    assertTrue(rejection.detail!!.contains("release-failed"))
    assertEquals(setOf(7L), sessions.ownedSessions())
  }

  @Test
  fun closeOfASessionRustNoLongerKnowsResolves() {
    open()
    core.invokeFailure = MobileCoreBridge.MobileCoreException("lifecycle.destroyed|core|mobile.session.invoke|unknown or disposed session")
    val close = Captured()
    sessions.closeSession("7", close)
    assertNull(close.single())
    assertTrue(sessions.ownedSessions().isEmpty())
  }

  @Test
  fun everySessionOfOneModuleSharesItsBackgroundScope() {
    open().single()
    core.openRecord = { "{\"sessionId\":8}" }
    open().single()
    val other = RustCoreSessions(core, host, DirectExecutor, { }, { packageName }, SecureRandom(), { })
    core.openRecord = { "{\"sessionId\":9}" }
    Captured().also { other.openSession("manager-b", "ubm-mobile-wire/3", it) }.single()
    val scopes = core.openScopes
    assertEquals(3, scopes.size)
    assertEquals("a manager destroy/recreate stays in the module's scope", scopes[0], scopes[1])
    assertTrue("another module never shares it", scopes[2] != scopes[0])
    assertTrue(scopes.all { it.isNotEmpty() })
  }

  @Test
  fun closingAManagerKeepsTheModulesForegroundServiceAndInvalidateReleasesIt() {
    open().single()
    val close = Captured().also { sessions.closeSession("7", it) }
    core.callbacks.single().onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    close.single()
    assertTrue("manager destroy never ends the scope", core.releasedScopes.isEmpty())
    sessions.invalidate()
    assertEquals(listOf(core.openScopes.single()), core.releasedScopes)
  }

  @Test
  fun aScopeReleaseFailureOnInvalidateIsLoggedNotSwallowed() {
    val cleanupTasks = QueuedExecutor()
    val processHost = RustCoreProcessHost(core, { unusedRadioHost() },
      { _, task -> cleanupTasks.execute(task) }, { logs.add(it) })
    val moduleSessions = RustCoreSessions(core, processHost, DirectExecutor, {}, { packageName }, SecureRandom(), { logs.add(it) })
    Captured().also { moduleSessions.openSession("manager-a", "ubm-mobile-wire/3", it) }.single()
    core.scopeRelease = "{\"failures\":[{\"resourceKind\":\"background\",\"code\":\"platform.failure\"}],\"state\":\"release-failed\"}"
    moduleSessions.invalidate()
    core.callbacks.forEach { it.onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}") }
    cleanupTasks.runNext()
    assertTrue(logs.toString(), logs.any { it.contains("Scope") && it.contains("release-failed") })
  }

  @Test
  fun invalidationRetriesFailedScopeAfterSessionDisposalWithoutAnotherModule() {
    val cleanupTasks = QueuedExecutor()
    val processHost = RustCoreProcessHost(core, { unusedRadioHost() },
      { _, task -> cleanupTasks.execute(task) }, { logs.add(it) })
    val first = RustCoreSessions(core, processHost, DirectExecutor, {}, { packageName }, SecureRandom(), { logs.add(it) })
    val second = RustCoreSessions(core, processHost, DirectExecutor, {}, { packageName }, SecureRandom(), { logs.add(it) })
    Captured().also { first.openSession("first", "ubm-mobile-wire/3", it) }.single()
    core.openRecord = { "{\"sessionId\":8}" }
    Captured().also { second.openSession("second", "ubm-mobile-wire/3", it) }.single()
    val firstScope = core.openScopes[0]
    val secondScope = core.openScopes[1]
    core.scopeRelease = "{\"failures\":[{\"resourceKind\":\"background\",\"code\":\"platform.failure\"}],\"state\":\"release-failed\"}"

    first.invalidate()
    assertTrue("scope awaits native disposal", core.releasedScopes.isEmpty())
    core.callbacks.single().onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    cleanupTasks.runNext()
    assertEquals(listOf(firstScope), core.releasedScopes)
    assertEquals(setOf(firstScope), processHost.retainedCleanupScopes())
    core.scopeRelease = "{\"failures\":[],\"state\":\"released\"}"
    cleanupTasks.runAll()
    assertEquals(listOf(firstScope, firstScope), core.releasedScopes)
    assertTrue(processHost.retainedCleanupScopes().isEmpty())
    assertTrue("other module remains active", secondScope !in core.releasedScopes)
  }

  @Test
  fun acceptedBackgroundAcquisitionCannotOutliveTheScopeReleaseBoundary() {
    val cleanupTasks = QueuedExecutor()
    val processHost = RustCoreProcessHost(core, { unusedRadioHost() },
      { _, task -> cleanupTasks.execute(task) }, { logs.add(it) })
    val moduleSessions = RustCoreSessions(core, processHost, DirectExecutor, {}, { packageName }, SecureRandom(), { logs.add(it) })
    Captured().also { moduleSessions.openSession("manager", "ubm-mobile-wire/3", it) }.single()
    val acquisition = Captured()
    moduleSessions.invoke("7", "background.acquire", "{\"kind\":\"connected-device\"}", acquisition)
    assertEquals(1, core.callbacks.size)

    moduleSessions.invalidate()
    assertEquals(2, core.callbacks.size)
    assertTrue(core.releasedScopes.isEmpty())
    core.callbacks[0].onResult("{\"ok\":true,\"value\":{\"leaseId\":\"background-1\"}}")
    acquisition.single()
    cleanupTasks.runAll()
    assertTrue("accepted acquisition alone cannot release the scope", core.releasedScopes.isEmpty())
    core.callbacks[1].onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    cleanupTasks.runAll()
    assertEquals(listOf(core.openScopes.single()), core.releasedScopes)
  }

  @Test
  fun retainedSessionRetryStaysSingleFlightAndIgnoresStaleCallback() {
    val cleanupTasks = QueuedExecutor()
    val processHost = RustCoreProcessHost(core, { unusedRadioHost() },
      { _, task -> cleanupTasks.execute(task) }, { logs.add(it) })
    processHost.retainSessionCleanup(7L, "failure")
    cleanupTasks.runAll()
    assertEquals(1, core.callbacks.size)
    repeat(3) { processHost.ensureInstalled() }
    cleanupTasks.runAll()
    assertEquals("in-flight native disposal is not retried", 1, core.callbacks.size)
    core.callbacks.single().onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    assertTrue(processHost.retainedCleanupSessions().isEmpty())
    core.callbacks.single().onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    assertTrue(processHost.retainedCleanupSessions().isEmpty())
  }

  @Test
  fun staleSessionCallbackCannotCompleteANewerRetry() {
    val cleanupTasks = QueuedExecutor()
    val processHost = RustCoreProcessHost(core, { unusedRadioHost() },
      { _, task -> cleanupTasks.execute(task) }, { logs.add(it) })
    processHost.retainSessionCleanup(7L, "failure")
    cleanupTasks.runNext()
    core.callbacks[0].onResult("{\"ok\":true,\"value\":{\"failures\":[{\"code\":\"platform.failure\"}],\"state\":\"release-failed\"}}")
    cleanupTasks.runNext()
    assertEquals(2, core.callbacks.size)
    core.callbacks[0].onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    assertEquals(setOf(7L), processHost.retainedCleanupSessions())
    core.callbacks[1].onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    assertTrue(processHost.retainedCleanupSessions().isEmpty())
  }

  @Test
  fun invalidateDisposesEverySessionTheContextOwns() {
    open()
    core.openRecord = { "{\"sessionId\":8}" }
    open()
    sessions.invalidate()
    assertEquals(listOf(7L, 8L), core.invokes.map { it.first }.sorted())
    assertTrue(core.invokes.all { it.second == "session.dispose" })
    core.callbacks.forEach { it.onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}") }
    assertTrue(sessions.ownedSessions().isEmpty())
    host.wake.onWake(7)
    assertTrue(wakes.isEmpty())
  }

  @Test
  fun invalidateRejectsAnAcceptedOpenThatWasAlreadyQueuedBeforeTeardown() {
    val queued = QueuedExecutor()
    val queuedWakes = mutableListOf<String>()
    val queuedHost = RustCoreProcessHost(core, { unusedRadioHost() },
      { _, task -> task.run() }, { logs.add(it) })
    val queuedSessions = RustCoreSessions(
      core,
      queuedHost,
      queued,
      { queuedWakes.add(it) },
      { packageName },
      SecureRandom(),
      { logs.add(it) }
    )
    val admission = Captured()

    queuedSessions.openSession("queued-manager", "ubm-mobile-wire/3", admission)
    queuedSessions.invalidate()
    queued.runAll()

    assertEquals("lifecycle.destroyed", admission.rejection().code)
    assertTrue(core.openScopes.isEmpty())
    assertTrue(queuedSessions.ownedSessions().isEmpty())
    queuedHost.wake.onWake(7)
    assertTrue(queuedWakes.isEmpty())
    assertEquals(1L, queuedHost.unroutedWakeCount())
    assertTrue("an unopened module has no scope to release", core.releasedScopes.isEmpty())
  }

  @Test
  fun invalidateTransfersFailedDisposalToTheProcessOwnerUntilARetryReleasesIt() {
    val cleanupTasks = QueuedExecutor()
    val processHost = RustCoreProcessHost(
      core,
      { unusedRadioHost() },
      { _, task -> cleanupTasks.execute(task) },
      { logs.add(it) }
    )
    val moduleSessions = RustCoreSessions(
      core,
      processHost,
      DirectExecutor,
      {},
      { packageName },
      SecureRandom(),
      { logs.add(it) }
    )
    Captured().also { moduleSessions.openSession("manager-a", "ubm-mobile-wire/3", it) }.single()

    moduleSessions.invalidate()
    core.callbacks.single().onResult(
      "{\"ok\":true,\"value\":{\"failures\":[{\"resourceKind\":\"connection\",\"code\":\"platform.failure\"}],\"state\":\"release-failed\"}}"
    )

    assertTrue(moduleSessions.ownedSessions().isEmpty())
    assertEquals(setOf(7L), processHost.retainedCleanupSessions())
    cleanupTasks.runAll()
    assertEquals(2, core.callbacks.size)
    core.callbacks[1].onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}")
    assertTrue(processHost.retainedCleanupSessions().isEmpty())
  }

  @Test
  fun identitiesAreRustAnswered() {
    val build = Captured().also { sessions.nativeBuildIdentity(it) }
    val contract = Captured().also { sessions.contractRevision(it) }
    val wire = Captured().also { sessions.wireRevision(it) }
    assertEquals(core.buildIdentityJson(), build.single())
    assertEquals("C-UBM.test", contract.single())
    assertEquals("ubm-mobile-wire/3", wire.single())
  }

  @Test
  fun linkageFailuresRejectInsteadOfCrashing() {
    val broken = object : MobileCorePort by core {
      override fun wireRevision(): String = throw UnsatisfiedLinkError("libubm5_jni_echo.so not found")
    }
    val reply = Captured()
    val brokenHost = RustCoreProcessHost(broken, { unusedRadioHost() }) { }
    RustCoreSessions(broken, brokenHost, DirectExecutor, {}, { null }, SecureRandom(), {}).wireRevision(reply)
    assertEquals("platform.failure", reply.rejection().code)
  }

  @Test
  fun randomBytesAreStrictPaddedBase64OfTheRequestedLength() {
    val reply = Captured()
    sessions.randomBytes(5.0, reply)
    val text = reply.single()!!
    assertTrue(text, Regex("^[A-Za-z0-9+/]{7}=$").matches(text))
    listOf(0.0, 1025.0, 2.5, -1.0).forEach { length ->
      val bad = Captured()
      sessions.randomBytes(length, bad)
      assertEquals("argument.invalid", bad.rejection().code)
    }
  }

  @Test
  fun randomBytesUsesTheInjectedCsprng() {
    val fixed = object : SecureRandom() {
      override fun nextBytes(bytes: ByteArray) {
        bytes.indices.forEach { bytes[it] = (0xfb + it).toByte() }
      }
    }
    assertEquals("+/w=", RustCorePlatformValues.randomBytesBase64(2, fixed))
    assertEquals("+/z9", RustCorePlatformValues.randomBytesBase64(3, fixed))
  }

  @Test
  fun restorationIdentityMatchesTheLegacyDerivation() {
    val reply = Captured()
    sessions.restorationIdentity("{\"restorationId\":\"polar-h10\",\"generation\":\"g1\"}", reply)
    assertEquals(
      "{\"applicationId\":\"com.example.app\",\"restorationId\":\"polar-h10\",\"generation\":\"g1\"," +
        "\"restoreIdentifier\":\"com.example.app.ubm.bAJhamvIRtxLfB4DiasYDO\"," +
        "\"namespaceValue\":\"ubm-ns:yDbEzUt4lL1INi-T2zFWQsUbRKNjerFs2rsMGLJ3xpY\"," +
        "\"clientId\":\"ubm-client:1EUm4_KmPwR91cMa4X5TLzQqUlvuGd-SuRGvWjHyCkA\"," +
        "\"hostSessionScope\":\"ubm-host:0T4aOQ6z3p7XtLKRbX7aWhzLULSrzv63NQkAXDTf6yg\"}",
      reply.single()
    )
  }

  @Test
  fun restorationIdentityAnswersNullForTheConfiguredIdentity() {
    val reply = Captured()
    sessions.restorationIdentity("{}", reply)
    assertEquals("null", reply.single())
  }

  @Test
  fun restorationIdentityRefusesMalformedRequestsAsPlatformFailure() {
    listOf(
      "not json",
      "{\"restorationId\":\"polar\"}",
      "{\"restorationId\":\"polar\",\"generation\":\"g1\",\"extra\":1}",
      "{\"restorationId\":\"-leading-dash\",\"generation\":\"g1\"}",
      "{\"restorationId\":\"polar\",\"generation\":7}"
    ).forEach { request ->
      val reply = Captured()
      sessions.restorationIdentity(request, reply)
      val rejection = reply.rejection()
      assertEquals(request, "platform.failure", rejection.code)
      assertEquals("restoration.identity", rejection.operation)
    }
    packageName = null
    val reply = Captured()
    sessions.restorationIdentity("{\"restorationId\":\"polar\",\"generation\":\"g1\"}", reply)
    assertEquals("platform.failure", reply.rejection().code)
  }

  private fun unusedRadioHost(): MobileCoreBridge.RadioHost =
    java.lang.reflect.Proxy.newProxyInstance(
      javaClass.classLoader,
      arrayOf(MobileCoreBridge.RadioHost::class.java)
    ) { _, method, _ -> throw AssertionError("radio host must not be driven here: ${method.name}") } as MobileCoreBridge.RadioHost
}
