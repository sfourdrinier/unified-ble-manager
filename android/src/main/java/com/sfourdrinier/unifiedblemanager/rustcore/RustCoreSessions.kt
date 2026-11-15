// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustCoreSessions.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.presence.BackgroundContinuationDeclaration
import com.ubm.core.MobileCoreBridge
import java.security.SecureRandom
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executor
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

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
  @Suppress("UNUSED_PARAMETER") core: MobileCorePort,
  private val host: RustCoreProcessHost,
  private val executor: Executor,
  private val emitWake: (String) -> Unit,
  private val applicationId: () -> String?,
  private val random: SecureRandom,
  private val log: (String) -> Unit
) {
  // Retain the constructor parameter for existing callers, but always route
  // through the host: a failed unpublished install can replace its port.
  private val core: MobileCorePort get() = host.core
  /** A promise seam: exactly one of [resolve] / [reject] per call. */
  interface Reply {
    fun resolve(value: String?)
    fun reject(rejection: RustCoreRejection)
  }

  private val owned = ConcurrentHashMap.newKeySet<Long>()
  private val backgroundScope = "rn-module:" + ByteArray(16).also(random::nextBytes).joinToString("") { "%02x".format(it) }
  private val released = ConcurrentHashMap.newKeySet<Long>()
  private val disposing = ConcurrentHashMap.newKeySet<Long>()
  private val invalidated = AtomicBoolean(false)
  private val admissionLock = Any()
  @Volatile private var backgroundScopeUsed = false

  fun openSession(owner: String, expectedWireRevision: String, reply: Reply) = perform(reply, "session.open") {
    synchronized(admissionLock) {
      rejectIfInvalidated("session.open")
      host.ensureInstalled()
      // A presence wake with no live session persisted its peers; surface them
      // through the same restored records and events as a live ingest.
      host.drainPresenceAppearances()
      backgroundScopeUsed = true
      val record = core.openSession(owner, expectedWireRevision, backgroundScope)
      val admission = RustCoreJson.parse(record) as? Map<*, *>
      val sessionId = (admission?.get("sessionId") as? Long)
        ?: throw RustCoreRejection("protocol.malformed", "core", "session.open", "admission record has no numeric sessionId")
      owned.add(sessionId)
      host.route(sessionId) { id -> emitWake(id.toString()) }
      reply.resolve(record)
    }
  }

  fun invoke(sessionIdText: String, op: String, argsJson: String, reply: Reply) = perform(reply, op) {
    synchronized(admissionLock) {
      val sessionId = ownedRecordingSession(sessionIdText, op)
      if (op == "gatt.subscribe-recorded") host.configureRecordingStorage()
      if (op == DISPOSE) disposing.add(sessionId)
      try {
        core.invoke(sessionId, op, argsJson, MobileCoreBridge.InvokeCallback { envelope ->
          synchronized(admissionLock) {
            if (op == DISPOSE) disposing.remove(sessionId)
            if ((op == DISPOSE || op == "gatt.unsubscribe") && disposeReleased(envelope)) {
              if (op == DISPOSE && owned.contains(sessionId)) released.add(sessionId)
              host.notifyRecoveryHint()
            }
          }
          reply.resolve(envelope)
        })
      } catch (error: Throwable) {
        if (op == DISPOSE) disposing.remove(sessionId)
        throw error
      }
    }
  }

  fun drain(sessionIdText: String, maxItems: Double, maxBytes: Double, reply: Reply) = perform(reply, "session.drain") {
    synchronized(admissionLock) {
      val sessionId = ownedRecordingSession(sessionIdText, "session.drain")
      reply.resolve(core.drain(sessionId, positiveInt(maxItems, "maxItems", "session.drain"), positiveInt(maxBytes, "maxBytes", "session.drain")))
    }
  }

  /**
   * Ends the lease. When JS already ran `session.dispose` and it reported
   * `released`, the session is forgotten. Otherwise this runs
   * `session.dispose` itself: `released` (or a host that no longer knows
   * the session) resolves; `release-failed` rejects and keeps the session
   * for a retry. Closing an unknown or already-closed session resolves.
   */
  fun closeSession(sessionIdText: String, reply: Reply) = perform(reply, "session.close") {
    synchronized(admissionLock) {
      rejectIfInvalidated("session.close")
      val sessionId = sessionId(sessionIdText, "session.close")
      if (!owned.contains(sessionId)) {
        reply.resolve(null)
        return@synchronized
      }
      if (released.contains(sessionId)) {
        forget(sessionId)
        reply.resolve(null)
        return@synchronized
      }
      if (!disposing.add(sessionId)) throw RustCoreRejection("ownership.denied", "core", "session.close", "session is disposing")
      dispose(sessionId) { outcome ->
        synchronized(admissionLock) {
          disposing.remove(sessionId)
          if (outcome == null) forget(sessionId)
        }
        if (outcome == null) reply.resolve(null) else reply.reject(outcome)
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
    val declaration = try {
      BackgroundContinuationDeclaration.parse(declarationJson)
    } catch (error: IllegalArgumentException) {
      throw RustCoreRejection.invalid("continuation.declare", error.message ?: "declaration malformed")
    }
    if (declaration.recording != null) {
      host.ensureInstalled()
      host.configureRecordingStorage()
    }
    host.continuationExecutor().persistDeclaration(declaration) {
      host.continuationStore().saveDeclaration(declarationJson)
    }
    reply.resolve("{\"state\":\"declared\"}")
  }

  /** Probe the loaded Rust binary without installing a host or opening a journal. */
  fun orderedRecordingVersion(reply: Reply) = perform(reply, "ordered-recording.version") {
    reply.resolve(core.orderedRecordingVersion().toString())
  }

  private fun ownedRecordingSession(sessionIdText: String, operation: String): Long {
    val id = sessionId(sessionIdText, operation)
    if (id.toString() != sessionIdText) throw RustCoreRejection.invalid(operation, "sessionId must use canonical decimal")
    if (invalidated.get() || !owned.contains(id) || released.contains(id) || disposing.contains(id)) {
      throw RustCoreRejection("ownership.denied", "core", operation, "session is not owned by this module")
    }
    return id
  }

  fun negotiateScanEndVersion(sessionIdText: String, offeredJson: String, reply: Reply) = perform(reply, "scan-end-version") {
    val session = ownedRecordingSession(sessionIdText, "scan-end-version")
    reply.resolve(core.negotiateScanEndVersion(session, offeredJson))
  }

  fun orderedRecordingReceiptControl(sessionIdText: String, id: String, operation: String, reply: Reply) = perform(reply, "ordered-recording.$operation") {
    require(operation == "reserve" || operation == "status" || operation == "retire")
    val session = ownedRecordingSession(sessionIdText, "ordered-recording.$operation")
    core.orderedRecordingReceiptControl(session, operation, id, MobileCoreBridge.InvokeCallback { envelope -> reply.resolve(envelope) })
  }

  fun orderedRecordingStart(sessionIdText: String, handle: String, id: String, maxBytes: String, maxRecords: String,
                            declaration: String, budgetMs: Double, reply: Reply) = perform(reply, "ordered-recording.start") {
    val session = ownedRecordingSession(sessionIdText, "ordered-recording.start")
    if (!budgetMs.isFinite() || budgetMs < 0 || budgetMs > Int.MAX_VALUE || budgetMs != Math.rint(budgetMs)) {
      throw RustCoreRejection.invalid("ordered-recording.start", "budgetMs must be an integer within the native timeout range")
    }
    host.configureRecordingStorage()
    ownedRecordingSession(sessionIdText, "ordered-recording.start")
    core.orderedRecordingStart(session, handle, id, maxBytes, maxRecords, declaration, budgetMs.toLong(),
      MobileCoreBridge.InvokeCallback { envelope -> reply.resolve(envelope) })
  }

  fun orderedRecordingControl(sessionIdText: String, id: String, stop: Boolean, reply: Reply) = perform(reply, "ordered-recording.control") {
    val session = ownedRecordingSession(sessionIdText, "ordered-recording.control")
    host.configureRecordingStorage()
    ownedRecordingSession(sessionIdText, "ordered-recording.control")
    val callback = MobileCoreBridge.InvokeCallback { envelope -> reply.resolve(envelope) }
    if (stop) core.orderedRecordingStop(session, id, callback) else core.orderedRecordingContext(session, id, callback)
  }

  /** Rust atomically refuses offline access while any live owner exists. */
  fun orderedRecordingOffline(operation: String, id: String, reply: Reply) = perform(reply, "ordered-recording.offline-$operation") {
    require(operation == "context" || operation == "status" || operation == "stop")
    host.configureRecordingStorage()
    core.orderedRecordingOffline(operation, id, MobileCoreBridge.InvokeCallback { envelope -> reply.resolve(envelope) })
  }

  /** Offline journal access; this does not initialize or release the radio. */
  fun recordingControl(operation: String, id: String, token: String, maxItems: Double, maxBytes: Double, reply: Reply) =
    perform(reply, "continuation.recording.$operation") {
      val items = if (operation == "prepare") positiveInt(maxItems, "maxItems", "continuation.recording.$operation") else 0
      val bytes = if (operation == "prepare") positiveInt(maxBytes, "maxBytes", "continuation.recording.$operation") else 0
      reply.resolve(host.recordingControl(operation, id, token, items, bytes))
    }

  fun prepareRetainedContinuationClaimV1(maxItems: Double, maxBytes: Double, reply: Reply) = perform(reply, "continuation.claim.v1") {
    val items = positiveInt(maxItems, "maxItems", "continuation.claim.v1")
    val bytes = positiveInt(maxBytes, "maxBytes", "continuation.claim.v1")
    host.ensureInstalled()
    core.continuationPrepareClaimVersioned(1, items, bytes, continuationV1Reply(reply))
  }

  /**
   * Additive retained-prefix protocol. This has its own entrypoint so a
   * 5.2.1/V1 JavaScript decoder is never offered a V2 descriptor.
   */
  fun prepareRetainedContinuationClaimV2(maxItems: Double, maxBytes: Double, reply: Reply) = perform(reply, "continuation.claim.v2") {
    val items = positiveInt(maxItems, "maxItems", "continuation.claim.v2")
    val bytes = positiveInt(maxBytes, "maxBytes", "continuation.claim.v2")
    host.ensureInstalled()
    core.continuationPrepareClaimVersioned(2, items, bytes, continuationClaimReply("continuation.claim.v2", reply))
  }

  fun acknowledgeRetainedContinuationClaimV1(token: String, reply: Reply) = retainedClaimControlV1(token, reply, false)
  fun acknowledgeRetainedContinuationClaimV2(token: String, reply: Reply) = retainedClaimControlV2(token, reply)

  fun readPeerFailureRecovery(record: String, offset: String, reply: Reply) = perform(reply, "continuation.peer-source-failure.read") {
    if (record.isEmpty() || record.toByteArray(Charsets.UTF_8).size > 1_048_576) throw RustCoreRejection.invalid("continuation.peer-source-failure.read", "recovery record must be 1..1048576 UTF-8 bytes")
    if (offset.isEmpty() || offset.toULongOrNull()?.toString() != offset) throw RustCoreRejection.invalid("continuation.peer-source-failure.read", "offset must be canonical UInt64 decimal")
    host.ensureInstalled()
    core.continuationReadPeerFailureRecovery(record, offset, continuationClaimReply("continuation.peer-source-failure.read", reply))
  }

  fun acknowledgePeerFailureRecovery(record: String, reply: Reply) = perform(reply, "continuation.peer-source-failure.acknowledge") {
    if (record.isEmpty() || record.toByteArray(Charsets.UTF_8).size > 1_048_576) throw RustCoreRejection.invalid("continuation.peer-source-failure.acknowledge", "recovery record must be 1..1048576 UTF-8 bytes")
    host.ensureInstalled()
    core.continuationAcknowledgePeerFailureRecovery(record, continuationClaimReply("continuation.peer-source-failure.acknowledge", reply))
  }
  fun retireRetainedContinuationClaimReceiptV1(token: String, reply: Reply) = retainedClaimControlV1(token, reply, true)

  private fun retainedClaimControlV1(token: String, reply: Reply, retire: Boolean) = perform(reply, "continuation.claim.v1") {
    if (token.isEmpty() || token.length > 256) throw RustCoreRejection.invalid("continuation.claim.v1", "claimToken must be 1..256 bytes")
    host.ensureInstalled()
    val callback = continuationV1Reply(reply)
    if (retire) core.continuationRetireClaimReceipt(token, callback)
    else core.continuationAcknowledgeClaimV1(token, callback)
  }

  private fun retainedClaimControlV2(token: String, reply: Reply) = perform(reply, "continuation.claim.v2") {
    if (token.isEmpty() || token.length > 256) throw RustCoreRejection.invalid("continuation.claim.v2", "claimToken must be 1..256 bytes")
    host.ensureInstalled()
    core.continuationAcknowledgeClaimV2(token, continuationClaimReply("continuation.claim.v2", reply))
  }

  private fun continuationV1Reply(reply: Reply) = MobileCoreBridge.InvokeCallback { envelope ->
    continuationClaimReply("continuation.claim.v1", reply).onResult(envelope)
  }

  private fun continuationClaimReply(claimOperation: String, reply: Reply) = MobileCoreBridge.InvokeCallback { envelope ->
    val outcome = try {
      val root = RustCoreJson.parse(envelope) as? Map<*, *>
        ?: throw RustCoreRejection("protocol.malformed", "protocol", claimOperation, "Expected native envelope")
      when (root["ok"]) {
        true -> {
          if (!root.containsKey("value")) throw RustCoreRejection("protocol.malformed", "protocol", claimOperation, "Missing native value")
          Result.success(RustCoreJson.write(root["value"]))
        }
        false -> {
          val error = root["error"] as? Map<*, *>
            ?: throw RustCoreRejection("protocol.malformed", "protocol", claimOperation, "Missing native error")
          val code = error["code"] as? String
          val domain = error["domain"] as? String
          val nativeOperation = error["operation"] as? String
          if (code.isNullOrEmpty() || domain.isNullOrEmpty() || nativeOperation.isNullOrEmpty())
            throw RustCoreRejection("protocol.malformed", "protocol", claimOperation, "Malformed native error")
          Result.failure(RustCoreRejection(code, domain, nativeOperation, error["detail"] as? String, error["platform"] as? Map<*, *>))
        }
        else -> throw RustCoreRejection("protocol.malformed", "protocol", claimOperation, "Missing native outcome")
      }
    } catch (error: RuntimeException) {
      Result.failure(if (error is RustCoreRejection) error else RustCoreRejection("protocol.malformed", "protocol", claimOperation, "Invalid native response"))
    }
    outcome.fold(reply::resolve) { error ->
      reply.reject(if (error is RustCoreRejection) error else RustCoreRejection.platform(claimOperation, error))
    }
  }

  /**
   * Seals and prepares the continuation backlog with its loss accounting.
   * Acknowledgement releases the session; durable journal data is separate.
   */
  fun prepareContinuationClaim(maxItems: Double, maxBytes: Double, reply: Reply) =
    perform(reply, "continuation.claim") {
      val items = positiveInt(maxItems, "maxItems", "continuation.claim")
      val bytes = positiveInt(maxBytes, "maxBytes", "continuation.claim")
      val claim = host.continuationExecutor().prepareClaim(items, bytes)
      val response = linkedMapOf<String, Any?>(
        // Session-pinned authority for the consumer names in these batches.
        // The standing declaration may change before claim.
        "consumerCount" to claim.consumerCount,
        "selectors" to claim.selectors.map { selector ->
          linkedMapOf(
            "serviceUuid" to selector.serviceUuid,
            "serviceOccurrence" to selector.serviceOccurrence,
            "characteristicUuid" to selector.characteristicUuid,
            "characteristicOccurrence" to selector.characteristicOccurrence
          )
        },
        "batches" to claim.batches,
        "disposed" to false,
        // Attempts observed after the native cutoff are separate from
        // bounded-outbox loss: they belong to the handoff gap and must
        // be surfaced to the foreground owner explicitly.
        "afterCutoffLoss" to linkedMapOf(
          "items" to claim.afterCutoffLoss.items,
          "bytes" to claim.afterCutoffLoss.bytes
        ),
        // Why the session is still alive (null when disposed or when no
        // wake existed): a release-failed dispose or an incomplete drain
        // is retried by the next claim, never abandoned silently.
        "disposeFailure" to claim.disposeFailure
      )
      // No wake is the one valid tokenless response. Every prepared handoff
      // has a non-empty token and must be acknowledged by TypeScript.
      if (claim.claimToken.isNotEmpty()) response["claimToken"] = claim.claimToken
      claim.recordingId?.let { response["recording"] = linkedMapOf("id" to it) }
      reply.resolve(
        RustCoreJson.write(response)
      )
    }

  fun acknowledgeContinuationClaim(claimToken: String, reply: Reply) =
    perform(reply, "continuation.claim") {
      if (claimToken.isEmpty() || claimToken.length > 256) {
        throw RustCoreRejection.invalid("continuation.claim", "claimToken must be 1..256 bytes")
      }
      val acknowledgement = host.continuationExecutor().acknowledgeClaim(claimToken)
      reply.resolve(
        RustCoreJson.write(
          linkedMapOf(
            "disposed" to acknowledgement.disposed,
            "afterCutoffLoss" to linkedMapOf(
              "items" to acknowledgement.afterCutoffLoss.items,
              "bytes" to acknowledgement.afterCutoffLoss.bytes
            ),
            "disposeFailure" to acknowledgement.disposeFailure
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
          "lastRecovery" to host.continuationExecutor().lastRecovery(),
          "lastWake" to wake?.wire()
        )
      )
    )
  }

  /**
   * React context teardown: dispose every session this context still owns,
   * then end the module's background scope (its foreground-service leases).
   */
  fun invalidate() {
    val firstInvalidation = synchronized(admissionLock) {
      invalidated.compareAndSet(false, true)
    }
    if (!firstInvalidation) return
    executor.execute {
      val pending = AtomicInteger(1)
      fun finishOne() {
        if (pending.decrementAndGet() == 0 && backgroundScopeUsed) host.retainBackgroundScopeCleanup(backgroundScope)
      }
      try {
        owned.toList().forEach { sessionId ->
          host.unroute(sessionId)
          if (released.contains(sessionId)) {
            forget(sessionId)
            return@forEach
          }
          pending.incrementAndGet()
          val completed = AtomicBoolean(false)
          dispose(sessionId) { outcome ->
            if (completed.compareAndSet(false, true)) {
              try {
                if (outcome != null) {
                  host.retainSessionCleanup(sessionId, outcome.toJson())
                  log("session $sessionId dispose on invalidate transferred to process owner: ${outcome.toJson()}")
                }
                forget(sessionId)
              } finally {
                finishOne()
              }
            }
          }
        }
      } catch (error: Throwable) {
        log("background scope $backgroundScope invalidation threw: ${error.message}")
      } finally {
        finishOne()
      }
    }
  }

  fun ownedSessions(): Set<Long> = owned.toSet()

  private fun forget(sessionId: Long) {
    synchronized(admissionLock) {
      owned.remove(sessionId)
      released.remove(sessionId)
      disposing.remove(sessionId)
      host.unroute(sessionId)
    }
  }

  /** Runs `session.dispose`; [onOutcome] receives null when the lease is gone. */
  private fun dispose(sessionId: Long, onOutcome: (RustCoreRejection?) -> Unit) {
    try {
      core.invoke(sessionId, DISPOSE, "{}", MobileCoreBridge.InvokeCallback { envelope ->
        if (disposeReleased(envelope)) host.notifyRecoveryHint()
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
        rejectIfInvalidated(operation)
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

    private fun positiveInt(value: Double, name: String, operation: String): Int {
      if (value != Math.rint(value) || value < 1 || value > Int.MAX_VALUE) {
        throw RustCoreRejection.invalid(operation, "$name must be a positive integer")
      }
      return value.toInt()
    }

    private fun nativeRejection(error: RuntimeException, operation: String): RustCoreRejection = when (error) {
      is RustCoreRejection -> error
      is MobileCoreBridge.MobileCoreException -> RustCoreRejection.fromWire(error.message, operation)
      else -> RustCoreRejection.platform(operation, error)
    }

    internal fun disposeThrownOutcome(error: RuntimeException): RustCoreRejection? {
      val rejection = nativeRejection(error, DISPOSE)
      return if (rejection.code == LIFECYCLE_DESTROYED) null else rejection
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

  private fun rejectIfInvalidated(operation: String) {
    if (invalidated.get()) {
      throw RustCoreRejection(LIFECYCLE_DESTROYED, "core", operation, "React context is invalidated")
    }
  }
}
