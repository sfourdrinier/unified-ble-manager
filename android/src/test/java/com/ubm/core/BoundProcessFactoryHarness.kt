package com.ubm.core

import com.sfourdrinier.unifiedblemanager.rustcore.JniBoundMobileCorePort
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreProcessHost
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreSessions
import java.security.SecureRandom
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.Executor
import java.util.concurrent.TimeUnit

/** Production process factory and sessions over synthetic JNI radios, never Android BLE. */
object BoundProcessFactoryHarness {
  @JvmStatic fun main(args: Array<String>) {
    val radios = mutableListOf<BoundMobileCorePortHarness.Radio>()
    fun process(): RustCoreProcessHost = RustCoreProcessHost.bound({ port ->
      check(port is JniBoundMobileCorePort)
      BoundMobileCorePortHarness.Radio(port).also { radios.add(it) }.host
    }, log = {})
    val a = process()
    val b = process()
    val wakesA = ArrayBlockingQueue<String>(4)
    val wakesB = ArrayBlockingQueue<String>(4)
    val executor = Executor { it.run() }
    fun sessions(host: RustCoreProcessHost, wakes: ArrayBlockingQueue<String>) = RustCoreSessions(
      host.core, host, executor, { wakes.add(it) }, { null }, SecureRandom(), {})
    class Reply : RustCoreSessions.Reply {
      val result = ArrayBlockingQueue<String>(1)
      override fun resolve(value: String?) { result.add(value ?: "null") }
      override fun reject(rejection: com.sfourdrinier.unifiedblemanager.rustcore.RustCoreRejection) { throw rejection }
    }
    val sa = sessions(a, wakesA)
    val sb = sessions(b, wakesB)
    val ra = Reply(); val rb = Reply()
    sa.openSession("first", a.core.wireRevision(), ra)
    sb.openSession("second", b.core.wireRevision(), rb)
    check(ra.result.take().contains("\"sessionId\":1"))
    check(rb.result.take().contains("\"sessionId\":1"))
    val ia = Reply(); val ib = Reply()
    sa.invoke("1", "adapter.state", "{}", ia)
    sb.invoke("1", "adapter.state", "{}", ib)
    val firstRequest = radios[0].requests.poll(5, TimeUnit.SECONDS) ?: error("first request missing")
    val secondRequest = radios[1].requests.poll(5, TimeUnit.SECONDS) ?: error("second request missing")
    check(a.core.completeAdapter(firstRequest, "available", "granted", "on", null) == 0)
    check(ia.result.poll(5, TimeUnit.SECONDS) != null)
    check(ib.result.poll(200, TimeUnit.MILLISECONDS) == null)
    check(b.core.completeAdapter(secondRequest, "unavailable", "granted", "off", null) == 0)
    check(ib.result.poll(5, TimeUnit.SECONDS) != null)
    check(a.core.ingestAdapterState(com.sfourdrinier.unifiedblemanager.rustcore.AdapterFacts("available", "granted", "on", null)) == 0)
    check(wakesA.poll(5, TimeUnit.SECONDS) == "1")
    check(wakesB.poll(200, TimeUnit.MILLISECONDS) == null)
    check(a.core.drain(1, 64, 8192).contains("\"power\":\"on\""))
    check(!b.core.drain(1, 64, 8192).contains("\"power\":\"on\""))

    val retries = java.util.ArrayDeque<Runnable>()
    var builds = 0
    val recovery = RustCoreProcessHost.bound({ port ->
      check(port is JniBoundMobileCorePort)
      builds++
      val radio = BoundMobileCorePortHarness.Radio(port, builds == 1)
      if (builds != 1) radio.host else MobileCoreBridge.RadioHost::class.java.cast(java.lang.reflect.Proxy.newProxyInstance(
        MobileCoreBridge.RadioHost::class.java.classLoader,
        arrayOf(MobileCoreBridge.RadioHost::class.java)
      ) { proxy, method, arguments ->
        if (method.name == "orderedAdmissionOwner" && arguments[0] == 3) "invalid-owner"
        else radio.invoke(proxy, method, arguments)
      })
    }, { _, task -> retries.add(task) }, {})
    val failedPort = recovery.core
    try { recovery.ensureInstalled(); error("invalid install succeeded") }
    catch (_: MobileCoreBridge.MobileCoreException) { }
    check(retries.size == 1)
    retries.removeFirst().run()
    check(recovery.core === failedPort && retries.size == 1)
    retries.removeFirst().run()
    check(recovery.core !== failedPort)
    recovery.ensureInstalled()
    check(builds == 2 && recovery.core.hostInstalled())
    println("bound production process factory JNI isolation passed")
    kotlin.system.exitProcess(0)
  }
}
