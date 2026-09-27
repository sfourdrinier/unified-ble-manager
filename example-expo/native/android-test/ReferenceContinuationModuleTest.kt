package com.sfourdrinier.bleplxexample.continuation

import com.facebook.react.BaseReactPackage
import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.sfourdrinier.unifiedblemanager.rustcore.FakeCore
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreProcessHost
import com.ubm.core.MobileCoreBridge
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito.*
import org.mockito.ArgumentCaptor
import java.util.ArrayDeque
import java.util.concurrent.Executor

class ReferenceContinuationModuleTest {
  @Test fun packageUsesCurrentLazyModuleRegistrationWithoutChangingItsIdentity() {
    val reference = ReferenceContinuationPackage()
    assertTrue(BaseReactPackage::class.java.isAssignableFrom(reference.javaClass))
    assertEquals("UBMReferenceContinuation", ReferenceContinuationModule.NAME)
    val registered = reference as BaseReactPackage
    val context = mock(ReactApplicationContext::class.java)
    assertEquals("UBMReferenceContinuation", registered.getModule("UBMReferenceContinuation", context)?.name)
    assertNull(registered.getModule("not-this-module", context))
    val modules = registered.getReactModuleInfoProvider().getReactModuleInfos()
    assertEquals(setOf("UBMReferenceContinuation"), modules.keys)
    val info = checkNotNull(modules["UBMReferenceContinuation"])
    assertEquals("UBMReferenceContinuation", info.name)
    assertFalse(info.needsEagerInit)
    assertFalse(info.isTurboModule)
  }

  @Test fun wrapperRefusalsEmitCanonicalEnvelopesWithoutHostEffects() {
    val module = ReferenceContinuationModule(mock(ReactApplicationContext::class.java), Executor { it.run() }, { error("must not allocate a host") })
    for (operation in listOf("unknown", "x".repeat(100000))) {
      val promise = mock(Promise::class.java)
      module.invoke(operation, "", "", "", 0.0, 0.0, promise)
      val capture = ArgumentCaptor.forClass(String::class.java)
      verify(promise).resolve(capture.capture())
      println("reference-envelope=${capture.value}")
      assertTrue(capture.value.length < 512)
      assertTrue(capture.value.contains("\"operation\":\"continuation.invoke\""))
    }
    val rejected = ReferenceContinuationModule(mock(ReactApplicationContext::class.java), Executor { throw java.util.concurrent.RejectedExecutionException("worker refused") }, { error("must not allocate a host") })
    val promise = mock(Promise::class.java)
    rejected.invoke("status", "", "", "", 0.0, 0.0, promise)
    val capture = ArgumentCaptor.forClass(String::class.java)
    verify(promise).resolve(capture.capture())
    println("reference-envelope=${capture.value}")
  }
  @Test fun directExecuteRunsOnWorkerPreservesEnvelopeAndNeverCreatesWake() {
    val tasks = ArrayDeque<Runnable>()
    val core = FakeCore()
    val host = RustCoreProcessHost(core, { mock(MobileCoreBridge.RadioHost::class.java) }, { _, task -> task.run() }) {}
    val module = ReferenceContinuationModule(mock(ReactApplicationContext::class.java), Executor { tasks.add(it) }, { host })
    host.continuationStore().saveDeclaration("""{"onAppearance":"native","peerId":"AA:BB:CC:DD:EE:FF","resubscribe":[]}""")
    val promise = mock(Promise::class.java)
    val answer = "{\"ok\":true,\"value\":{\"event\":\"continuation.completed\",\"strategy\":\"native\",\"peerAddress\":\"AA:BB:CC:DD:EE:FF\",\"resubscribed\":0,\"link\":{\"original\":true}}}"
    var invoked = false
    core.continuationExecuteAnswer = { _, _, callback -> invoked = true; callback.onResult(answer) }
    module.invoke("execute", "AA:BB:CC:DD:EE:FF", """{"onAppearance":"native","peerId":"AA:BB:CC:DD:EE:FF","resubscribe":[]}""", "", 0.0, 0.0, promise)
    assertFalse(invoked)
    verifyNoInteractions(promise)
    tasks.removeFirst().run()
    assertTrue(invoked)
    verify(promise).resolve(answer)
    assertNull(host.continuationStore().lastWakeOutcome())
    assertTrue(core.invokes.isEmpty() && core.openScopes.isEmpty())
  }

  @Test fun asynchronousNativeCallsRemainBoundedUntilExactOnceCompletion() {
    val core = FakeCore()
    val host = RustCoreProcessHost(core, { mock(MobileCoreBridge.RadioHost::class.java) }, { _, task -> task.run() }) {}
    val json = """{"onAppearance":"native","peerId":"AA:BB:CC:DD:EE:FF","resubscribe":[]}"""
    host.continuationStore().saveDeclaration(json)
    val callbacks = mutableListOf<MobileCoreBridge.InvokeCallback>()
    core.continuationExecuteAnswer = { _, _, callback -> callbacks.add(callback) }
    val module = ReferenceContinuationModule(mock(ReactApplicationContext::class.java), Executor { it.run() }, { host })
    val promises = (0..16).map { mock(Promise::class.java) }
    for (promise in promises) module.invoke("execute", "AA:BB:CC:DD:EE:FF", json, "", 0.0, 0.0, promise)
    assertEquals(16, callbacks.size)
    verify(promises[16]).resolve(contains("lifecycle.invalid-state"))
    val capture = ArgumentCaptor.forClass(String::class.java)
    verify(promises[16]).resolve(capture.capture())
    println("reference-envelope=${capture.value}")
    callbacks[0].onResult("{\"ok\":true,\"value\":null}")
    callbacks[0].onResult("{\"ok\":true,\"value\":null}")
    verify(promises[0], times(1)).resolve(anyString())
    module.invoke("execute", "AA:BB:CC:DD:EE:FF", json, "", 0.0, 0.0, mock(Promise::class.java))
    module.invoke("execute", "AA:BB:CC:DD:EE:FF", json, "", 0.0, 0.0, mock(Promise::class.java))
    assertEquals(17, callbacks.size)
    callbacks.drop(1).forEach { it.onResult("{\"ok\":true,\"value\":null}") }
  }

  @Test fun hostStorageRefusalIsCanonicalAndDoesNotCreateWake() {
    val core = FakeCore()
    val host = RustCoreProcessHost(core, { mock(MobileCoreBridge.RadioHost::class.java) }, { _, task -> task.run() }) {}
    val json = """{"onAppearance":"native","peerId":"AA:BB:CC:DD:EE:FF","resubscribe":[],"recording":{"id":"direct","maxBytes":1048576,"maxRecords":1000}}"""
    host.continuationStore().saveDeclaration(json)
    val module = ReferenceContinuationModule(mock(ReactApplicationContext::class.java), Executor { it.run() }, { host })
    val promise = mock(Promise::class.java)
    module.invoke("execute", "AA:BB:CC:DD:EE:FF", json, "", 0.0, 0.0, promise)
    val capture = ArgumentCaptor.forClass(String::class.java)
    verify(promise).resolve(capture.capture())
    println("reference-envelope=${capture.value}")
    assertNull(host.continuationStore().lastWakeOutcome())
  }
}
