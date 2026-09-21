// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreSessions.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationDeclaration
import com.ubm.core.MobileCoreBridge
import java.security.SecureRandom
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executor

/**
 * Everything the `UnifiedBleRustCore` TurboModule does, without React
 * Native types (JVM-tested). One instance per React context: it owns the
 * sessions that context opened, routes their wakes to [emitWake], and
 * disposes them when the context is invalidated. Arguments and results
 * cross verbatim — this class never parses an operation's arguments.
 *
 * Foreground-service leases belong to this module instance, not to one
 * manager (legacy held them on the native module until `invalidate()`):
 * every session opens in this instance's background scope, so destroying
 * and recreating a manager keeps the service, and [invalidate] ends it.
 *
 * Every call runs on [executor]; nothing blocks the module thread.
 * Rejections carry the structured JSON of [RustCoreRejection].
 */
class RustCoreSessions(
  private val core: MobileCorePort,
  private val host: RustCoreProcessHost,
  private val executor: Executor,
  private val emitWake: (String) -> Unit,
  private val applicationId: () -> String?,
  private val random: SecureRandom,
  private val log: (String) -> Unit
) {
  /** A promise seam: exactly one of [resolve] / [reject] per call. */
  interface Reply {
    fun resolve(value: String?)
    fun reject(rejection: RustCoreRejection)
  }

  private val owned = ConcurrentHashMap.newKeySet<Long>()
  private val backgroundScope = "rn-module:" + ByteArray(16).also(random::nextBytes).joinToString("") { "%02x".format(it) }
  private val released = ConcurrentHashMap.newKeySet<Long>()

  fun openSession(owner: String, expectedWireRevision: String, reply: Reply) = perform(reply, "session.open") {
    host.ensureInstalled()
    // A presence wake with no live session persisted its peers; surface them
    // through the same restored records and events as a live ingest.
    host.drainPresenceAppearances()
    val record = core.openSession(owner, expectedWireRevision, backgroundScope)
    val admission = RustCoreJson.parse(record) as? Map<*, *>
    val sessionId = (admission?.get("sessionId") as? Long)
      ?: throw RustCoreRejection("protocol.malformed", "core", "session.open", "admission record has no numeric sessionId")
    owned.add(sessionId)
    host.route(sessionId) { id -> emitWake(id.toString()) }
    reply.resolve(record)
  }

  fun invoke(sessionIdText: String, op: String, argsJson: String, reply: Reply) = perform(reply, op) {
    val sessionId = sessionId(sessionIdText, op)
    core.invoke(sessionId, op, argsJson, MobileCoreBridge.InvokeCallback { envelope ->
      if (op == DISPOSE && disposeReleased(envelope)) released.add(sessionId)
      reply.resolve(envelope)
    })
  }

  fun drain(sessionIdText: String, maxItems: Double, maxBytes: Double, reply: Reply) = perform(reply, "session.drain") {
    val sessionId = sessionId(sessionIdText, "session.drain")
    reply.resolve(core.drain(sessionId, positiveInt(maxItems, "maxItems"), positiveInt(maxBytes, "maxBytes")))
  }

  /**
   * Ends the lease. When JS already ran `session.dispose` and it reported
   * `released`, the session is forgotten. Otherwise this runs
   * `session.dispose` itself: `released` (or a host that no longer knows
   * the session) resolves; `release-failed` rejects and keeps the session
   * for a retry. Closing an unknown or already-closed session resolves.
   */
  fun closeSession(sessionIdText: String, reply: Reply) = perform(reply, "session.close") {
    val sessionId = sessionId(sessionIdText, "session.close")
    if (!owned.contains(sessionId)) {
      reply.resolve(null)
      return@perform
    }
    if (released.contains(sessionId)) {
      forget(sessionId)
      reply.resolve(null)
      return@perform
    }
    dispose(sessionId) { outcome ->
      if (outcome == null) {
        forget(sessionId)
        reply.resolve(null)
      } else {
        reply.reject(outcome)
      }
    }
  }

  fun nativeBuildIdentity(reply: Reply) = perform(reply, "identity.build") { reply.resolve(core.buildIdentityJson()) }

  fun contractRevision(reply: Reply) = perform(reply, "identity.contract") { reply.resolve(core.contractRevision()) }

  fun wireRevision(reply: Reply) = perform(reply, "identity.wire") { reply.resolve(core.wireRevision()) }

  fun randomBytes(length: Double, reply: Reply) = perform(reply, "random.bytes") {
    if (length != Math.rint(length) || length < 1 || length > RustCorePlatformValues.MAX_RANDOM_BYTES) {
      throw RustCoreRejection.invalid("random.bytes", "length must be an integer in 1..${RustCorePlatformValues.MAX_RANDOM_BYTES}")
    }
    reply.resolve(RustCorePlatformValues.randomBytesBase64(length.toInt(), random))
  }

  fun restorationIdentity(requestJson: String, reply: Reply) = perform(reply, "restoration.identity") {
    reply.resolve(RustCorePlatformValues.restorationIdentity(applicationId(), requestJson))
  }

  /**
   * Persists the declared background standing order (BGS4) in the process
   * owner so an OS wake with no JavaScript can execute it. The payload is
   * the binding's canonical JSON; it is parsed here before persisting, so
   * a malformed declaration is refused with no effect.
   */
  fun declareContinuation(declarationJson: String, reply: Reply) = perform(reply, "continuation.declare") {
    if (declarationJson.isEmpty() || declarationJson.length > MAX_CONTINUATION_JSON) {
      throw RustCoreRejection.invalid("continuation.declare", "declaration must be 1..$MAX_CONTINUATION_JSON bytes")
    }
    try {
      BackgroundContinuationDeclaration.parse(declarationJson)
    } catch (error: IllegalArgumentException) {
      throw RustCoreRejection.invalid("continuation.declare", error.message ?: "declaration malformed")
    }
    host.continuationStore().saveDeclaration(declarationJson)
    reply.resolve("{\"state\":\"declared\"}")
  }

  /**
   * Drains the continuation backlog (verbatim drain batches for the JS
   * codec) and disposes the continuation session, so the app's own session
   * connects next. Values queued with no JS session drain here with the
   * drain contract's own loss accounting — nothing silently dropped.
   */
  fun claimContinuation(maxItems: Double, maxBytes: Double, reply: Reply) =
    perform(reply, "continuation.claim") {
      val items = positiveInt(maxItems, "maxItems")
      val bytes = positiveInt(maxBytes, "maxBytes")
      val claim = host.continuationExecutor().claimAndDispose(items, bytes)
      reply.resolve(
        RustCoreJson.write(
          linkedMapOf(
            "batches" to claim.batches,
            "disposed" to claim.disposed,
            // Why the session is still alive (null when disposed or when no
            // wake existed): a release-failed dispose or an incomplete drain
            // is retried by the next claim, never abandoned silently.
            "disposeFailure" to claim.disposeFailure
          )
        )
      )
    }

  /**
   * Reports the continuation posture for Diagnostics: the declared strategy
   * and the last wake outcome. Never fails for an undeclared order.
   */
  fun continuationStatus(reply: Reply) = perform(reply, "continuation.status") {
    val store = host.continuationStore()
    val declaration = store.loadDeclaration()
    val wake = store.lastWakeOutcome()
    reply.resolve(
      RustCoreJson.write(
        linkedMapOf(
          "strategy" to declaration.strategy.wire,
          "peerId" to declaration.peerId,
          "resubscribe" to declaration.resubscribe.size,
          "malformedDeclarations" to store.malformedDeclarationCount(),
          "lastWake" to if (wake == null) null else linkedMapOf(
            "observedAtMs" to wake.observedAtMs,
            "event" to wake.event,
            "strategy" to wake.strategy.wire,
            "peerAddress" to wake.peerAddress,
            "code" to wake.code,
            "reason" to wake.reason
          )
        )
      )
    )
  }

  /**
   * React context teardown: dispose every session this context still owns,
   * then end the module's background scope (its foreground-service leases).
   */
  fun invalidate() {
    owned.toList().forEach { sessionId ->
      host.unroute(sessionId)
      if (released.contains(sessionId)) {
        forget(sessionId)
        return@forEach
      }
      executor.execute {
        try {
          dispose(sessionId) { outcome ->
            forget(sessionId)
            if (outcome != null) log("session $sessionId dispose on invalidate failed: ${outcome.toJson()}")
          }
        } catch (error: Throwable) {
          log("session $sessionId dispose on invalidate threw: ${error.message}")
        }
      }
    }
    executor.execute {
      try {
        val record = core.releaseBackgroundScope(backgroundScope)
        if (envelope(record)?.get("state") != "released") {
          log("background scope $backgroundScope release on invalidate: $record")
        }
      } catch (error: Throwable) {
        log("background scope $backgroundScope release on invalidate threw: ${error.message}")
      }
    }
  }

  fun ownedSessions(): Set<Long> = owned.toSet()

  private fun forget(sessionId: Long) {
    owned.remove(sessionId)
    released.remove(sessionId)
    host.unroute(sessionId)
  }

  /** Runs `session.dispose`; [onOutcome] receives null when the lease is gone. */
  private fun dispose(sessionId: Long, onOutcome: (RustCoreRejection?) -> Unit) {
    try {
      core.invoke(sessionId, DISPOSE, "{}", MobileCoreBridge.InvokeCallback { envelope ->
        onOutcome(disposeOutcome(envelope))
      })
    } catch (error: RuntimeException) {
      val rejection = nativeRejection(error, DISPOSE)
      onOutcome(if (rejection.code == LIFECYCLE_DESTROYED) null else rejection)
    }
  }

  private fun perform(reply: Reply, operation: String, body: () -> Unit) {
    val task = Runnable {
      try {
        body()
      } catch (rejection: RustCoreRejection) {
        reply.reject(rejection)
      } catch (error: RuntimeException) {
        reply.reject(nativeRejection(error, operation))
      } catch (error: LinkageError) {
        reply.reject(RustCoreRejection.platform(operation, error))
      }
    }
    try {
      executor.execute(task)
    } catch (error: RuntimeException) {
      reply.reject(RustCoreRejection.platform(operation, error))
    }
  }

  companion object {
    const val DISPOSE = "session.dispose"
    private const val LIFECYCLE_DESTROYED = "lifecycle.destroyed"
    /** The canonical declaration JSON is small; anything larger is not ours. */
    private const val MAX_CONTINUATION_JSON = 65536

    private fun sessionId(text: String, operation: String): Long {
      if (text.isEmpty() || text.length > 20 || !text.all { it in '0'..'9' }) {
        throw RustCoreRejection.invalid(operation, "sessionId must be the decimal session id")
      }
      return text.toLongOrNull() ?: throw RustCoreRejection.invalid(operation, "sessionId is out of range")
    }

    private fun positiveInt(value: Double, name: String): Int {
      if (value != Math.rint(value) || value < 1 || value > Int.MAX_VALUE) {
        throw RustCoreRejection.invalid("session.drain", "$name must be a positive integer")
      }
      return value.toInt()
    }

    private fun nativeRejection(error: RuntimeException, operation: String): RustCoreRejection = when (error) {
      is RustCoreRejection -> error
      is MobileCoreBridge.MobileCoreException -> RustCoreRejection.fromWire(error.message, operation)
      else -> RustCoreRejection.platform(operation, error)
    }

    private fun envelope(text: String): Map<*, *>? = try {
      RustCoreJson.parse(text) as? Map<*, *>
    } catch (_: IllegalArgumentException) {
      null
    }

    internal fun disposeReleased(text: String): Boolean {
      val parsed = envelope(text) ?: return false
      return parsed["ok"] == true && (parsed["value"] as? Map<*, *>)?.get("state") == "released"
    }

    /** null = the lease is gone; otherwise why it is kept. */
    internal fun disposeOutcome(text: String): RustCoreRejection? {
      val parsed = envelope(text)
        ?: return RustCoreRejection("protocol.malformed", "core", DISPOSE, "dispose envelope is not JSON")
      if (parsed["ok"] == true) {
        val value = parsed["value"] as? Map<*, *>
        if (value?.get("state") == "released") return null
        return RustCoreRejection(
          "platform.failure",
          "platform",
          DISPOSE,
          "session.dispose reported release-failed; the session is kept for a retry: ${RustCoreJson.write(value)}"
        )
      }
      val error = parsed["error"] as? Map<*, *>
      val code = error?.get("code") as? String ?: "protocol.malformed"
      if (code == LIFECYCLE_DESTROYED) return null
      return RustCoreRejection(
        code,
        error?.get("domain") as? String ?: "core",
        error?.get("operation") as? String ?: DISPOSE,
        error?.get("detail") as? String
      )
    }
  }
}
