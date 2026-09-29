package com.sfourdrinier.unifiedblemanager.presence

import com.sfourdrinier.unifiedblemanager.background.ConnectedDeviceForegroundServiceLeaseRegistry
import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceControlException
import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceNotificationConfiguration
import com.sfourdrinier.unifiedblemanager.rustcore.RustCoreJson
import com.sfourdrinier.unifiedblemanager.rustcore.classifyBackgroundFailure
import com.sfourdrinier.unifiedblemanager.rustcore.RadioFailureKind
import com.sfourdrinier.unifiedblemanager.rustcore.RadioPortFailure

/** Process-owned presence leases share the ordinary manager's service authority. */
class PresenceForegroundContinuation(private val registry: ConnectedDeviceForegroundServiceLeaseRegistry) {
  private class Entry(val configuration: ForegroundServiceNotificationConfiguration) {
    var wanted = true
    var acquiring = true
    var releasing = false
    var lease: String? = null
  }
  private val entries = mutableMapOf<String, Entry>()
  private val lock = Any()

  fun execute(peer: String, declaration: BackgroundContinuationDeclaration): ContinuationOutcome {
    val notification = declaration.foregroundService?.notification
      ?: return ContinuationOutcome.failed(ContinuationStrategy.FOREGROUND_SERVICE, "argument.invalid", "Missing foreground-service notification", null)
    val configuration = ForegroundServiceNotificationConfiguration.fromValues(notification.channelId, notification.channelName, notification.title, notification.body, notification.icon, false)
    val entry = synchronized(lock) {
      entries[peer]?.let { existing ->
        if (existing.wanted && !existing.acquiring && !existing.releasing && existing.configuration == configuration) return completed(peer)
        return ContinuationOutcome.failed(ContinuationStrategy.FOREGROUND_SERVICE, "lifecycle.invalid-state", "A presence lease is pending cleanup or owns a different notification", null)
      }
      Entry(configuration).also { entries[peer] = it }
    }
    val lease = try { registry.acquire("companion-presence", configuration) } catch (error: RuntimeException) {
      synchronized(lock) { if (entries[peer] === entry) entries.remove(peer) }
      return continuationPlatformFailure(ContinuationStrategy.FOREGROUND_SERVICE, error)
    }
    val wanted = synchronized(lock) { entry.lease = lease; entry.acquiring = false; entry.wanted }
    if (!wanted) {
      release(peer)?.let { return it }
      return ContinuationOutcome.failed(ContinuationStrategy.FOREGROUND_SERVICE, "operation.aborted", "Peer disappeared before foreground-service promotion settled", null)
    }
    return completed(peer)
  }

  /** Null confirms no remaining obligation; failures retain the exact lease for retry. */
  fun release(peer: String): ContinuationOutcome.Failed? {
    val entry = synchronized(lock) { entries[peer]?.also { it.wanted = false } }
    if (entry == null) {
      return try { registry.retryPendingStartCleanup(); null }
      catch (error: RuntimeException) { continuationPlatformFailure(ContinuationStrategy.FOREGROUND_SERVICE, error) }
    }
    val lease: String
    synchronized(lock) {
      if (entry.acquiring) return null // The accepted acquire compensates when it settles.
      if (entry.releasing) return ContinuationOutcome.Failed(ContinuationStrategy.FOREGROUND_SERVICE, "lifecycle.invalid-state", "Presence lease release already in progress", null)
      lease = checkNotNull(entry.lease)
      entry.releasing = true
    }
    return try {
      registry.release(lease)
      synchronized(lock) { if (entries[peer] === entry) entries.remove(peer) }
      null
    } catch (error: RuntimeException) {
      synchronized(lock) { entry.releasing = false }
      continuationPlatformFailure(ContinuationStrategy.FOREGROUND_SERVICE, error)
    }
  }

  private fun completed(peer: String) = ContinuationOutcome.Completed(ContinuationStrategy.FOREGROUND_SERVICE, peer, 0, "foreground-service-started")

  fun releaseOrThrow(peer: String) {
    val failure = release(peer) ?: return
    val detail = failure.platform?.let { RustCoreJson.parse(it) as? Map<*, *> }
    throw RadioPortFailure(
      if (failure.code == "permission.denied") RadioFailureKind.PERMISSION_DENIED else RadioFailureKind.PLATFORM,
      failure.reason, nativeCode = detail?.get("code") as? String
    )
  }
}

internal fun continuationPlatformFailure(strategy: ContinuationStrategy, error: Throwable): ContinuationOutcome.Failed {
  val cause = error.cause ?: error
  val classified = classifyBackgroundFailure(error)
  val reason = classified.detail
  val code = when (classified.kind) {
    RadioFailureKind.PERMISSION_DENIED -> "permission.denied"
    RadioFailureKind.UNSUPPORTED -> "capability.unsupported"
    else -> "platform.failure"
  }
  val metadata = linkedMapOf<String, Any?>()
  if (error.suppressed.isNotEmpty()) {
    metadata["cleanupFailureCount"] = error.suppressed.size
    error.suppressed.forEachIndexed { index, cleanup ->
      metadata["cleanupFailure${index}Code"] = cleanup.javaClass.name
      metadata["cleanupFailure${index}Message"] = cleanup.message ?: cleanup.javaClass.name
    }
    if (error is ForegroundServiceControlException && error.cleanupRequired) metadata["cleanupPending"] = true
  }
  return ContinuationOutcome.Failed(strategy, code, reason, RustCoreJson.write(linkedMapOf("domain" to "android", "code" to (classified.nativeCode ?: cause.javaClass.name), "message" to reason, "metadata" to metadata)))
}
