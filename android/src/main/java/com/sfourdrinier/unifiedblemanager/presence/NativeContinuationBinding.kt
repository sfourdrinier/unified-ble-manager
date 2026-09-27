package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.rustcore.MobileCorePort
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreRejection
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.CountDownLatch
import java.util.concurrent.atomic.AtomicReference

/** Transport-only adapter. The shared Rust process owner owns admission, recovery and handoff. */
class NativeContinuationBinding(private val core: MobileCorePort, private val log: (String) -> Unit) {
  private val declarationGate = Any()
  fun execute(peer: String, declaration: BackgroundContinuationDeclaration): ContinuationOutcome = try {
    synchronized(declarationGate) {
      synchronous("seed") { core.continuationSeedDeclaration(declarationJson(declaration)) }
    }
    val value = objectValue(await("execute") { core.continuationExecute(peer, declarationJson(declaration), it) })
    if (value["event"] != "continuation.completed" || value["strategy"] != "native" || value["peerAddress"] != peer) {
      malformed("execution identity")
    }
    val count = integer(value["resubscribed"], "resubscribed")
    if (count != declaration.resubscribe.size.toLong()) malformed("execution subscription count")
    ContinuationOutcome.completed(ContinuationStrategy.NATIVE, peer, count.toInt())
  } catch (error: NativeContinuationFailure) {
    log("native continuation failed: ${error.message}")
    ContinuationOutcome.failed(ContinuationStrategy.NATIVE, error.code, error.message ?: "native continuation failed", error.platform?.let { RustCoreJson.write(it) })
  }

  fun declarationReplacementFailure(declaration: BackgroundContinuationDeclaration): String? =
    core.continuationDeclarationReplacementFailure(declarationJson(declaration))

  fun persistDeclaration(declaration: BackgroundContinuationDeclaration, persist: () -> Unit) = surface {
    synchronized(declarationGate) {
      val reservation = objectValue(synchronous("declare") { core.continuationReserveDeclaration(declarationJson(declaration)) })
      val token = reservation["reservationToken"] as? String ?: malformed("declaration reservation")
      try {
        persist()
      } catch (failure: Exception) {
        try { synchronous("declare") { core.continuationCancelDeclaration(token) } }
        catch (cancelFailure: Exception) {
          log("continuation declaration reservation cancellation failed; admission remains fenced: ${cancelFailure.message}")
          failure.addSuppressed(cancelFailure)
        }
        throw failure
      }
      synchronous("declare") { core.continuationCommitDeclaration(token) }
      Unit
    }
  }

  fun prepareClaim(maxItems: Int, maxBytes: Int): ContinuationClaim = surface {
    require(maxItems > 0 && maxBytes > 0) { "claim bounds must be positive" }
    val value = objectValue(await("prepare") { core.continuationPrepareClaim(maxItems, maxBytes, it) })
    val count = integer(value["consumerCount"], "consumerCount")
    if (count > Int.MAX_VALUE) malformed("consumer count exceeds integer range")
    val selectors = selectors(value["selectors"])
    if (count != selectors.size.toLong()) malformed("consumer count disagrees with selectors")
    val batches = value["batches"] as? List<*> ?: malformed("claim batches")
    val decodedBatches = batches.map { it as? String ?: malformed("claim batch must be a string") }
    if (value["disposed"] != false) malformed("prepared claim cannot already be disposed")
    val token = if (value.containsKey("claimToken")) value["claimToken"] as? String ?: malformed("claim token") else ""
    if (token.isEmpty() && (count != 0L || decodedBatches.isNotEmpty())) malformed("owned claim requires a token")
    ContinuationClaim(count.toInt(), decodedBatches, false, nullableText(value["disposeFailure"], "disposeFailure"),
      cutoff(value["afterCutoffLoss"]), selectors, token)
  }

  fun acknowledgeClaim(token: String): ContinuationAcknowledgement = surface {
    require(token.isNotEmpty() && token.length <= 256) { "invalid claim token" }
    val value = objectValue(await("acknowledge") { core.continuationAcknowledgeClaim(token, it) })
    val disposed = value["disposed"] as? Boolean ?: malformed("acknowledgement disposed")
    val failure = nullableText(value["disposeFailure"], "disposeFailure")
    if (disposed && failure != null) malformed("released acknowledgement carries a failure")
    ContinuationAcknowledgement(disposed, cutoff(value["afterCutoffLoss"]), failure)
  }

  fun describeBacklog(): BacklogCounts? {
    val result = await("backlog", core::continuationDescribeBacklog) ?: return null
    val value = objectValue(result)
    val counters = objectValue(value["counters"])
    val bytes = integer(counters["retainedByteBuffers"], "retainedByteBuffers")
    val native = objectValue(objectValue(value["process"])["native"])
    val drops = objectValue(native["ingressDrops"]).entries.associate { (name, count) ->
      (name as? String ?: malformed("ingress class")) to integer(count, "ingress drops")
    }
    return BacklogCounts(bytes, drops)
  }

  fun lastRecovery(): Map<*, *>? = surface {
    val value = await("backlog", core::continuationDescribeBacklog) ?: return@surface null
    objectValue(value)["continuationOutcome"]?.let(::objectValue)
  }

  /** Wait only on the service/module worker, never a UI/radio callback thread.
   * Rust owns operation deadlines. Inventing a second deadline here can contradict its outcome.
   */
  private fun await(operation: String, invoke: (MobileCoreBridge.InvokeCallback) -> Unit): Any? {
    val response = AtomicReference<String?>(null)
    val done = CountDownLatch(1)
    try {
      invoke(MobileCoreBridge.InvokeCallback { envelope ->
        response.compareAndSet(null, envelope)
        done.countDown()
      })
      done.await()
    } catch (error: InterruptedException) {
      Thread.currentThread().interrupt()
      throw NativeContinuationFailure("operation.cancelled", "continuation $operation waiter interrupted; native ownership retained", null)
    } catch (error: RuntimeException) {
      throw bridgeFailure(operation, error)
    }
    return decode(operation, response.get() ?: malformed("missing bridge response"))
  }

  private fun decode(operation: String, response: String): Any? {
    val root = try {
      objectValue(RustCoreJson.parse(response))
    } catch (error: IllegalArgumentException) {
      throw NativeContinuationFailure("protocol.malformed", "continuation $operation invalid JSON: ${error.message}", null)
    }
    return when (root["ok"]) {
      true -> if (root.containsKey("value")) root["value"] else malformed("missing response value")
      false -> {
        val error = objectValue(root["error"])
        val code = error["code"] as? String ?: malformed("missing error code")
        val detail = error["detail"] as? String ?: error["operation"] as? String ?: "continuation $operation refused"
        val platform = error["platform"]?.let { objectValue(it) }
        throw NativeContinuationFailure(code, detail, platform,
          error["domain"] as? String ?: "restoration", error["operation"] as? String ?: "continuation.$operation")
      }
      else -> malformed("missing response outcome")
    }
  }

  private fun synchronous(operation: String, invoke: () -> String): Any? {
    val response = try { invoke() } catch (error: RuntimeException) {
      throw bridgeFailure(operation, error)
    }
    return decode(operation, response)
  }

  private fun bridgeFailure(operation: String, error: RuntimeException): NativeContinuationFailure {
    val failure = when (error) {
      is MobileCoreBridge.MobileCoreException -> RustCoreRejection.fromWire(error.message, "continuation.$operation")
      else -> RustCoreRejection.platform("continuation.$operation", error)
    }
    return NativeContinuationFailure(failure.code, failure.detail, failure.platform, failure.domain, failure.operation)
  }

  private fun selectors(value: Any?): List<ContinuationSelector> {
    val entries = value as? List<*> ?: malformed("selectors")
    return entries.map { entry ->
      val item = objectValue(entry)
      val service = item["serviceUuid"] as? String ?: malformed("service UUID")
      val characteristic = item["characteristicUuid"] as? String ?: malformed("characteristic UUID")
      val serviceOccurrence = integer(item["serviceOccurrence"], "service occurrence")
      val characteristicOccurrence = integer(item["characteristicOccurrence"], "characteristic occurrence")
      if (serviceOccurrence == 0L || characteristicOccurrence == 0L) malformed("occurrences must be positive")
      ContinuationSelector(service, serviceOccurrence, characteristic, characteristicOccurrence)
    }
  }

  private fun cutoff(value: Any?): CutoffLoss {
    val loss = objectValue(value)
    return CutoffLoss(integer(loss["items"], "cutoff items"), integer(loss["bytes"], "cutoff bytes"))
  }

  private fun objectValue(value: Any?): Map<*, *> = value as? Map<*, *> ?: malformed("expected object")
  private fun nullableText(value: Any?, field: String): String? =
    if (value == null) null else value as? String ?: malformed(field)

  private fun integer(value: Any?, field: String): Long {
    val number = value as? Number ?: malformed(field)
    val result = number.toLong()
    if (result < 0 || result > 9_007_199_254_740_991L || result.toDouble() != number.toDouble()) malformed(field)
    return result
  }

  private fun malformed(detail: String): Nothing = throw NativeContinuationFailure("protocol.malformed", detail, null)

  private fun <T> surface(block: () -> T): T = try {
    block()
  } catch (error: NativeContinuationFailure) {
    throw RustCoreRejection(error.code, error.domain, error.operation, error.message, error.platform)
  }

  private fun declarationJson(declaration: BackgroundContinuationDeclaration): String {
    val value = linkedMapOf<String, Any?>("onAppearance" to declaration.strategy.wire,
      "resubscribe" to declaration.resubscribe.map { selector -> linkedMapOf(
        "serviceUuid" to selector.serviceUuid, "serviceOccurrence" to selector.serviceOccurrence,
        "characteristicUuid" to selector.characteristicUuid, "characteristicOccurrence" to selector.characteristicOccurrence
      ) })
    declaration.peerId?.let { value["peerId"] = it }
    declaration.headlessTaskName?.let { value["headlessTaskName"] = it }
    declaration.foregroundService?.let { service ->
      val notification = linkedMapOf<String, Any?>("channelId" to service.notification.channelId,
        "channelName" to service.notification.channelName, "title" to service.notification.title)
      service.notification.body?.let { notification["body"] = it }
      service.notification.icon?.let { notification["icon"] = it }
      value["foregroundService"] = linkedMapOf("notification" to notification)
    }
    return RustCoreJson.write(value)
  }
}

private class NativeContinuationFailure(
  val code: String, message: String?, val platform: Map<*, *>?,
  val domain: String = "restoration", val operation: String = "continuation"
) : RuntimeException(message)
