// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreSessionsTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.ubm.core.MobileCoreBridge
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.security.SecureRandom
import java.util.ArrayDeque
import java.util.concurrent.Executor

class RustCoreSessionsTest {
  private val core = FakeCore()
  private val logs = mutableListOf<String>()
  private val radioHost = unusedRadioHost()
  private var radioHostBuilds = 0
  private val host = RustCoreProcessHost(core, { radioHostBuilds++; radioHost }) { logs.add(it) }
  private val wakes = mutableListOf<String>()
  private var packageName: String? = "com.example.app"
  private val sessions = RustCoreSessions(core, host, DirectExecutor, { wakes.add(it) }, { packageName }, SecureRandom(), { logs.add(it) })

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

  private fun open(): Captured = Captured().also { sessions.openSession("manager-a", "ubm-mobile-wire/1", it) }

  private class QueuedExecutor : Executor {
    private val tasks = ArrayDeque<Runnable>()

    override fun execute(command: Runnable) {
      tasks.addLast(command)
    }

    fun runAll() {
      while (tasks.isNotEmpty()) tasks.removeFirst().run()
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
      "protocol.incompatible|core|ubm-mobile.session.open|caller speaks x, owner speaks ubm-mobile-wire/1"
    )
    val rejection = open().rejection()
    assertEquals("protocol.incompatible", rejection.code)
    assertEquals(
      "{\"code\":\"protocol.incompatible\",\"domain\":\"core\",\"operation\":\"ubm-mobile.session.open\"," +
        "\"detail\":\"caller speaks x, owner speaks ubm-mobile-wire/1\"}",
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
  fun unknownSessionFromRustIsAStructuredRejection() {
    core.invokeFailure = MobileCoreBridge.MobileCoreException("lifecycle.destroyed|core|mobile.session.invoke|unknown or disposed session")
    val reply = Captured()
    sessions.invoke("99", "adapter.state", "{}", reply)
    assertEquals("lifecycle.destroyed", reply.rejection().code)
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
    Captured().also { other.openSession("manager-b", "ubm-mobile-wire/1", it) }.single()
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
    open().single()
    core.scopeRelease = "{\"failures\":[{\"resourceKind\":\"background\",\"code\":\"platform.failure\"}],\"state\":\"release-failed\"}"
    sessions.invalidate()
    core.callbacks.forEach { it.onResult("{\"ok\":true,\"value\":{\"failures\":[],\"state\":\"released\"}}") }
    assertTrue(logs.toString(), logs.any { it.contains("background scope") && it.contains("release-failed") })
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
    val queuedHost = RustCoreProcessHost(core, { unusedRadioHost() }, log = { logs.add(it) })
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

    queuedSessions.openSession("queued-manager", "ubm-mobile-wire/1", admission)
    queuedSessions.invalidate()
    queued.runAll()

    assertEquals("lifecycle.destroyed", admission.rejection().code)
    assertTrue(core.openScopes.isEmpty())
    assertTrue(queuedSessions.ownedSessions().isEmpty())
    queuedHost.wake.onWake(7)
    assertTrue(queuedWakes.isEmpty())
    assertEquals(1L, queuedHost.unroutedWakeCount())
    assertEquals(1, core.releasedScopes.size)
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
    Captured().also { moduleSessions.openSession("manager-a", "ubm-mobile-wire/1", it) }.single()

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
    assertEquals("ubm-mobile-wire/1", wire.single())
  }

  @Test
  fun linkageFailuresRejectInsteadOfCrashing() {
    val broken = object : MobileCorePort by core {
      override fun wireRevision(): String = throw UnsatisfiedLinkError("libubm5_jni_echo.so not found")
    }
    val reply = Captured()
    RustCoreSessions(broken, host, DirectExecutor, {}, { null }, SecureRandom(), {}).wireRevision(reply)
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
