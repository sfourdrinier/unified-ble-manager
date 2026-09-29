package com.sfourdrinier.bleplxexample.continuation

import android.util.Log
import com.facebook.react.bridge.Promise
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.bridge.ReactContextBaseJavaModule
import com.facebook.react.bridge.ReactMethod
import com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationDeclaration
import com.sfourdrinier.unifiedblemanager.presence.ContinuationStrategy
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreProcessHost
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreRejection
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.Executor
import java.util.concurrent.Semaphore
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit

/** Reference-app transport only. No OS appearance or lastWake is synthesized.
 * Accepted native work belongs to the process host, not this React context. */
class ReferenceContinuationModule(
  context: ReactApplicationContext,
  private val dispatcher: Executor = worker,
  private val processHost: () -> RustCoreProcessHost = { RustCoreProcessHost.shared(context.applicationContext) }
) : ReactContextBaseJavaModule(context) {
  override fun getName() = NAME

  @ReactMethod
  fun invoke(operation: String, peer: String, declarationJson: String, token: String,
             maxItems: Double, maxBytes: Double, promise: Promise) {
    if (operation !in setOf("execute", "status", "prepare", "acknowledge")) {
      promise.resolve(RustCoreRejection("argument.invalid", "restoration", "continuation.invoke",
        "Unknown continuation control").toContinuationEnvelope())
      return
    }
    val context = "continuation.$operation"
    if (!pending.tryAcquire()) {
      promise.resolve(RustCoreRejection("lifecycle.invalid-state", "restoration", context,
        "Reference continuation queue is full").toContinuationEnvelope())
      return
    }
    val settled = AtomicBoolean(false)
    fun finish(envelope: String) {
      if (!settled.compareAndSet(false, true)) {
        Log.w("UBMReferenceContinuation", "Ignored duplicate native control completion")
        return
      }
      pending.release()
      promise.resolve(envelope)
    }
    fun failure(error: RuntimeException) {
      val rejection = when (error) {
        is RustCoreRejection -> error
        is IllegalArgumentException -> RustCoreRejection("argument.invalid", "restoration", context, error.message ?: "Invalid request")
        is MobileCoreBridge.MobileCoreException -> RustCoreRejection.fromWire(error.message, context)
        else -> RustCoreRejection.platform(context, error)
      }
      finish(rejection.toContinuationEnvelope())
    }
    try {
      dispatcher.execute {
        try {
          require(peer.toByteArray(Charsets.UTF_8).size <= 128) { "Peer exceeds transport bound" }
          require(declarationJson.toByteArray(Charsets.UTF_8).size <= 65536) { "Declaration exceeds transport bound" }
          val host = processHost()
          val callback = MobileCoreBridge.InvokeCallback { finish(it) }
          when (operation) {
            "execute" -> {
              val declaration = BackgroundContinuationDeclaration.parse(declarationJson)
              require(declaration.strategy == ContinuationStrategy.NATIVE && declaration.peerId == peer) { "Exact native declaration peer required" }
              host.executeNativeContinuationRaw(peer, declaration, callback)
            }
            "status" -> host.continuationExecutor().describeBacklogRaw(callback)
            "prepare" -> {
              require(maxItems.isFinite() && maxItems > 0 && maxItems <= 2048 && maxItems == maxItems.toInt().toDouble()) { "Invalid claim item bound" }
              require(maxBytes.isFinite() && maxBytes > 0 && maxBytes <= 4194304 && maxBytes == maxBytes.toInt().toDouble()) { "Invalid claim byte bound" }
              host.continuationExecutor().prepareClaimRaw(maxItems.toInt(), maxBytes.toInt(), callback)
            }
            "acknowledge" -> {
              require(token.isNotEmpty() && token.toByteArray(Charsets.UTF_8).size <= 256) { "Invalid claim token" }
              host.continuationExecutor().acknowledgeClaimRaw(token, callback)
            }
            else -> throw IllegalArgumentException("Unknown continuation control")
          }
        } catch (error: RuntimeException) { failure(error) }
      }
    } catch (error: RuntimeException) { failure(error) }
  }

  companion object {
    const val NAME = "UBMReferenceContinuation"
    // The process owns accepted work. React reload must not cancel a native
    // acquisition or discard a prepared, not-yet-acknowledged handoff.
    private val worker = ThreadPoolExecutor(1, 1, 0, TimeUnit.MILLISECONDS,
      ArrayBlockingQueue<Runnable>(16), ThreadPoolExecutor.AbortPolicy())
    private val pending = Semaphore(16)
  }
}
