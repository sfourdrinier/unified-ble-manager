// android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.BluetoothGattCharacteristic
import android.content.Context
import android.os.Build
import android.os.SystemClock
import com.sfourdrinier.unifiedblemanager.protocol.generated.NATIVE_PROTOCOL_VERSION
import com.sfourdrinier.unifiedblemanager.protocol.generated.ConnectionIntents
import com.sfourdrinier.unifiedblemanager.protocol.generated.RecordKind
import com.sfourdrinier.unifiedblemanager.radio.GattConnectAttempt
import com.sfourdrinier.unifiedblemanager.radio.GattObservation
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.OwnedRadioTeardownFailure
import com.sfourdrinier.unifiedblemanager.radio.UbmGattCoreBinding
import com.sfourdrinier.unifiedblemanager.radio.UbmGattCentralBridge
import com.sfourdrinier.unifiedblemanager.radio.DeferredCoreShadow
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattOperationFailure
import com.sfourdrinier.unifiedblemanager.radio.BondedPeerSnapshot
import com.sfourdrinier.unifiedblemanager.radio.nextUuidOccurrence
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

/**
 * Owns protocol-v2 Android radio work and sends bytes only through the native protocol.
 *
 * R02 core authority: covered commands (scan/connect/discover/disconnect —
 * see [CoreCommandAuthority]) execute the platform radio ONLY after the core
 * admits the same transition through [coreShadow] (the real core JNI session:
 * [UbmGattCoreBinding] over [UbmGattCentralBridge] with the real `GattBridge`
 * natives). A missing/failed shadow or a refused post at command time fails
 * the command loud with a `core*`-coded ERROR terminal — never silent
 * radio-only execution. Core drain rejections (`ok:false`) bind still-pending
 * correlated commands the same way, so a command the core rejects never
 * reports radio success. Commands without a native op surface (GATT IO, link
 * quality, security, bonded enumeration) stay radio-only scoped exceptions,
 * documented in [CoreCommandAuthority]. Event-path mirrors (adapter reset,
 * services-changed, link reports, discovery completion reports) stay
 * fire-and-forget with diagnostics: they report physical facts and gate no
 * command.
 */
class UnifiedBleProtocolAndroidDispatcher
constructor(
  context: Context,
  private val nativeHandle: Long,
  coreShadowFactory: ((Context, (GattObservation) -> Unit) -> UbmGattCoreBinding?)? = null,
  coreShadowOpener: Executor?
) {
  constructor(context: Context, nativeHandle: Long) : this(context, nativeHandle, null, defaultCoreShadowOpener())

  constructor(
    context: Context,
    nativeHandle: Long,
    coreShadowFactory: ((Context, (GattObservation) -> Unit) -> UbmGattCoreBinding?)?
  ) : this(context, nativeHandle, coreShadowFactory, defaultCoreShadowOpener())

  companion object {
    private fun defaultCoreShadowOpener(): Executor = Executors.newSingleThreadExecutor { runnable ->
      Thread(runnable, "ubm-core-shadow-opener").apply { isDaemon = true }
    }
  }
  private val radio = OwnedAndroidGattRadio(context.applicationContext)
  // R02: the shadow opens off the constructing (JS) thread — construction
  // here must never touch JNI (first touch loads libubm5_jni_echo.so).
  // The gate retries a missing/failed shadow on demand and reports each
  // distinct cause once; [admitCoreCommand] hardens this seam into the
  // authority readiness gate (missing/failed shadow at command time fails
  // the command loud instead of dropping to radio-only).
  private val shadowGate = DeferredCoreShadow(
    factory = {
      if (coreShadowFactory != null) {
        coreShadowFactory.invoke(context.applicationContext, this::onCoreRejection)
      } else {
        UbmGattCoreBinding(context.applicationContext, onCoreRejection = this::onCoreRejection)
      }
    },
    diagnose = { code, detail ->
      UnifiedBleProtocolJsiBinding.emitDiagnostic(nativeHandle, code, detail)
    },
    opener = coreShadowOpener
  )
  private val coreShadow: UbmGattCoreBinding?
    get() = shadowGate.current()

  /**
   * R02 authority verdict log. Every drained core rejection is journaled with
   * a monotonic sequence so admission ([admitCoreCommand]) and pending-bind
   * ([onCoreRejection]) can tell verdicts that predate a command from ones
   * that bind it. Bounded: the oldest entry is evicted past the cap.
   */
  private val coreVerdictSeq = AtomicLong(0)
  private data class CoreRejection(val seq: Long, val observation: GattObservation)
  private val coreRejectionJournal = ConcurrentLinkedQueue<CoreRejection>()

  /**
   * Admission sequence per pending operation key, recorded by
   * [admitCoreCommand] and removed on every terminal (see the emitters).
   * Lets a late rejection bind only commands admitted before it.
   */
  private val coreAdmissionSeqByOp = ConcurrentHashMap<String, Long>()

  /** The admitted core shadow plus the verdict sequence at admission time. */
  private data class CoreAdmission(
    val shadow: UbmGattCoreBinding,
    val seqBefore: Long,
    val schedulerFailure: String? = null
  )

  private fun onCoreRejection(observation: GattObservation) {
    UnifiedBleProtocolJsiBinding.emitDiagnostic(
      nativeHandle,
      "coreShadowRejected",
      "${observation.code}|${observation.domain}|${observation.operation} ${observation.detail ?: ""}".trim()
    )
    // Authority bind: a core rejection must never leave a correlated pending
    // command reporting radio success. Fail every still-pending command of the
    // mapped kinds admitted before this rejection (claim-guarded: settled
    // commands are untouched). Attribution is by command kind — a rejection
    // line carries no peer or dispatcher op key — see CoreCommandAuthority.
    val seq = coreVerdictSeq.incrementAndGet()
    while (coreRejectionJournal.size >= 128) coreRejectionJournal.poll()
    coreRejectionJournal.offer(CoreRejection(seq, observation))
    val kinds = CoreCommandAuthority.commandKindsForCoreEvent(observation.event)
    if (kinds.isEmpty()) return
    pendingCommands.values.toList().forEach { command ->
      val kind = try {
        command.requiredString(3)
      } catch (_: IllegalArgumentException) {
        return@forEach
      }
      if (kind !in kinds) return@forEach
      val admittedSeq = try {
        coreAdmissionSeqByOp[operationKey(command)] ?: -1L
      } catch (_: IllegalArgumentException) {
        -1L
      }
      if (admittedSeq >= seq) return@forEach
      emitFailure(command, CoreCommandAuthority.CODE_REJECTED, coreRejectionMessage(kind, observation))
    }
  }

  private fun coreRejectionMessage(kind: String, rejection: GattObservation): String =
    "Android core rejected $kind " +
      "(${rejection.code}|${rejection.domain}|${rejection.operation} ${rejection.detail ?: ""}".trim() +
      "); radio success is not reported"

  /** Latest rejection of one of [events] admitted after [seqBefore], if any. */
  private fun coreRejectionSince(seqBefore: Long, events: Set<String>): GattObservation? =
    coreRejectionJournal.firstOrNull { entry ->
      entry.seq > seqBefore && entry.observation.event in events
    }?.observation

  /**
   * Drops the authority admission record once a command reaches a terminal.
   * Called from every terminal path (see the emitters); never throws.
   */
  private fun forgetCoreAdmission(command: ProtocolWireRecord) {
    try {
      coreAdmissionSeqByOp.remove(operationKey(command))
    } catch (_: IllegalArgumentException) {
    }
  }

  /**
   * R02 authority gate for covered commands (see
   * [CoreCommandAuthority.requiresAdmission]).
   *
   * Posts the core transition BEFORE the radio runs and returns the admission
   * (with the verdict sequence at admission time) only when the core admitted
   * it ([UbmGattCentralBridge.PostResult.Queued]) with no same-tick rejection
   * for the command's wire events (synchronous attestation drain). Every
   * other outcome emits the fail-loud terminal and returns null — the caller
   * must then return WITHOUT touching the radio:
   * - missing/failed/withdrawn shadow → `coreUnavailable` (quoting the
   *   seam's recorded cause);
   * - refused post → `corePermissionDenied` / `coreEnqueueFailed` /
   *   `coreScheduleFailed` / `coreUnavailable`;
   * - same-tick core rejection → `coreRejected`.
   *
   * The one legitimate null post is `scanStop` with a healthy shadow and no
   * tracked core op (documented no-op shadow): it proceeds radio-only.
   */
  private fun admitCoreCommand(
    command: ProtocolWireRecord,
    commandKind: String,
    post: (UbmGattCoreBinding) -> UbmGattCentralBridge.PostResult?
  ): CoreAdmission? {
    val seqBefore = coreVerdictSeq.get()
    val shadow = coreShadow
    if (shadow == null || shadow.openFailure != null || !shadow.isOpen) {
      val unavailableCause = when {
        shadow == null -> shadowGate.lastCause() ?: "shadow unavailable"
        shadow.openFailure != null -> shadow.openFailure
        else -> "session not open"
      }
      emitFailure(
        command,
        CoreCommandAuthority.CODE_UNAVAILABLE,
        "Android core shadow is unavailable ($unavailableCause); $commandKind was not executed"
      )
      return null
    }
    val result = try {
      post(shadow)
    } catch (th: Throwable) {
      emitFailure(
        command,
        CoreCommandAuthority.CODE_ENQUEUE_FAILED,
        "Android core failed to enqueue $commandKind (${th.message ?: th.javaClass.simpleName}); $commandKind was not executed"
      )
      return null
    }
    if (result == null) {
      if (commandKind == "scanStop" && shadow.isOpen && shadow.openFailure == null) {
        coreAdmissionSeqByOp[operationKey(command)] = seqBefore
        return CoreAdmission(shadow, seqBefore)
      }
      emitFailure(
        command,
        CoreCommandAuthority.CODE_UNAVAILABLE,
        "Android core refused $commandKind (shadow withdrawn); $commandKind was not executed"
      )
      return null
    }
    // Synchronous attestation: drain same-tick verdicts before the radio
    // runs, so a core rejection binds now instead of racing radio success.
    // drainNow never throws: drain failures surface as data observations and
    // observation consumers are isolated inside the bridge.
    shadow.bridge.drainNow()
    coreRejectionSince(seqBefore, CoreCommandAuthority.coreEventsFor(commandKind))?.let { rejection ->
      // The pending-bind in onCoreRejection already attempted this terminal
      // during the drain; the claim guard makes this a safe deterministic
      // second attempt with the same code.
      emitFailure(command, CoreCommandAuthority.CODE_REJECTED, coreRejectionMessage(commandKind, rejection))
      return null
    }
    when (result) {
      is UbmGattCentralBridge.PostResult.Queued -> {
        coreAdmissionSeqByOp[operationKey(command)] = seqBefore
        return CoreAdmission(shadow, seqBefore)
      }
      is UbmGattCentralBridge.PostResult.PermissionDenied -> emitFailure(
        command,
        CoreCommandAuthority.CODE_PERMISSION_DENIED,
        "Android core denied $commandKind (${result.identity}); $commandKind was not executed"
      )
      is UbmGattCentralBridge.PostResult.EnqueueFailed -> emitFailure(
        command,
        CoreCommandAuthority.CODE_ENQUEUE_FAILED,
        "Android core failed to enqueue $commandKind (${result.message}); $commandKind was not executed"
      )
      is UbmGattCentralBridge.PostResult.ScheduleFailed -> {
        if (commandKind == "connect") {
          // The line is queued and the synchronous drain above may already
          // have produced the exact connect admission. Let the caller consume
          // that receipt; never discard a real core lease merely because the
          // asynchronous worker could not be scheduled.
          coreAdmissionSeqByOp[operationKey(command)] = seqBefore
          return CoreAdmission(shadow, seqBefore, result.message)
        }
        emitFailure(
          command,
          CoreCommandAuthority.CODE_SCHEDULE_FAILED,
          "Android core queued $commandKind but no drain was scheduled (${result.message}); $commandKind was not executed"
        )
      }
      is UbmGattCentralBridge.PostResult.Shutdown -> emitFailure(
        command,
        CoreCommandAuthority.CODE_UNAVAILABLE,
        "Android core shadow is released; $commandKind was not executed"
      )
    }
    return null
  }

  /** Posts a cleanup transition and attests its same-tick core verdict. */
  private fun attestCoreTransition(
    shadow: UbmGattCoreBinding,
    kind: String,
    post: (UbmGattCoreBinding) -> UbmGattCentralBridge.PostResult?,
    events: Set<String>
  ): String? {
    val seqBefore = coreVerdictSeq.get()
    val result = try {
      post(shadow)
    } catch (error: Throwable) {
      return "Android core failed to enqueue $kind (${error.message ?: error.javaClass.simpleName})"
    }
    if (result !is UbmGattCentralBridge.PostResult.Queued) {
      return "Android core did not queue $kind (${result ?: "shadow withdrawn"})"
    }
    shadow.bridge.drainNow()
    coreRejectionSince(seqBefore, events)?.let { rejection ->
      return coreRejectionMessage(kind, rejection)
    }
    return null
  }

  private fun scopedReleaseFailure(
    shadow: UbmGattCoreBinding?,
    peerId: String,
    generation: String
  ): OwnedRadioTeardownFailure? {
    if (shadow == null || shadow.openFailure != null || !shadow.isOpen) {
      return OwnedRadioTeardownFailure(
        "coreLinkReleased",
        IllegalStateException("Android core shadow unavailable while releasing $peerId")
      )
    }
    val failure = attestCoreTransition(
      shadow,
      "linkReleased",
      { it.postLinkReleased(peerId, generation) },
      setOf("link.released", "link.released.scoped")
    )
    return failure?.let { OwnedRadioTeardownFailure("coreLinkReleased", IllegalStateException(it)) }
  }

  private fun completeReservationCleanup(
    reservation: DispatchReservation,
    peerId: String,
    radioFailure: OwnedRadioTeardownFailure?,
    shadow: UbmGattCoreBinding?,
    callbackClaim: AtomicBoolean,
    releaseCore: Boolean = true,
    physicalCleanupCompleted: Boolean? = null
  ) {
    if (!callbackClaim.compareAndSet(false, true)) return
    if (physicalCleanupCompleted != null) {
      synchronized(ownershipGuard) { reservation.physicalCleanupComplete = physicalCleanupCompleted }
    }
    val failure = radioFailure ?: if (releaseCore) {
      scopedReleaseFailure(shadow, peerId, reservation.generation)
    } else {
      null
    }
    val waiters = synchronized(ownershipGuard) {
      if (failure == null) {
        reservation.phase = DispatchPhase.Returned
        dispatchReservations.remove(peerId.uppercase(), reservation)
        returnedReservations.remove(operationKey(reservation.command), reservation)
      } else {
        reservation.phase = DispatchPhase.Cleaning
        reservation.cleanupStarted = false
      }
      reservation.disconnectWaiters.toList().also { reservation.disconnectWaiters.clear() }
    }
    waiters.forEach { waiter ->
      try {
        waiter(failure)
      } catch (error: Throwable) {
        UnifiedBleProtocolJsiBinding.emitDiagnostic(
          nativeHandle,
          "disconnectWaiterFailed",
          error.message ?: error.javaClass.simpleName
        )
      }
    }
    failure?.let { radio.reportCleanupFailure(it) }
  }

  private fun coreLeaseFor(deviceId: String, operation: String): String =
    "android-link-${deviceId.uppercase()}-$operation"
  private val pendingCommands = ConcurrentHashMap<String, ProtocolWireRecord>()
  /**
   * A connect command and the radio attempt token it handed to `radio.connect`. The token is what
   * ties a GATT observation to this command: a prior generation's loss for the same peer carries a
   * different (or no) token and can never settle this command.
   */
  private class PendingProtocolConnect(
    val command: ProtocolWireRecord,
    val attempt: GattConnectAttempt,
    var lease: String = "",
    var generation: String = ""
  )

  private val pendingConnects = ConcurrentHashMap<String, PendingProtocolConnect>()
  private val ownershipGuard = Any()
  private enum class DispatchPhase { Reserved, Dispatching, Returned, Withdrawn, Cleaning }
  private data class DispatchReservation(
    val command: ProtocolWireRecord,
    val attempt: GattConnectAttempt,
    var lease: String = "",
    var generation: String = "",
    var phase: DispatchPhase = DispatchPhase.Reserved,
    var nativeReturned: Boolean = false,
    var nativeStarted: Boolean = false,
    var physicalCleanupComplete: Boolean = false,
    var cleanupStarted: Boolean = false,
    val disconnectWaiters: MutableList<(OwnedRadioTeardownFailure?) -> Unit> = mutableListOf()
  )
  private val dispatchReservations = ConcurrentHashMap<String, DispatchReservation>()
  private val returnedReservations = ConcurrentHashMap<String, DispatchReservation>()
  private val compensatedConnects = ConcurrentHashMap.newKeySet<String>()

  private fun removePendingConnect(deviceKey: String, command: ProtocolWireRecord) {
    synchronized(ownershipGuard) {
      pendingConnects[deviceKey]?.takeIf { it.command === command }?.let { pendingConnects.remove(deviceKey, it) }
    }
  }
  private data class EstablishedProtocolConnection(
    val connection: ProtocolWireRecord,
    val attempt: GattConnectAttempt,
    val lease: String,
    val generation: String
  )

  private sealed interface ConnectionOutcome {
    data class Pending(val pending: PendingProtocolConnect) : ConnectionOutcome
    data class Established(
      val connection: EstablishedProtocolConnection,
      val replacement: ProtocolWireRecord?
    ) : ConnectionOutcome
    object Unknown : ConnectionOutcome
  }

  private val establishedConnections = ConcurrentHashMap<String, EstablishedProtocolConnection>()
  private val activeDatabases = ConcurrentHashMap<String, ProtocolWireRecord>()
  private val activeSubscriptions = ConcurrentHashMap<String, SubscriptionRoute>()
  private val pendingSubscriptions = ConcurrentHashMap<String, SubscriptionRoute>()
  private val radioOperationIds = ConcurrentHashMap<String, Long>()
  private val activeScanCommand = AtomicReference<ProtocolWireRecord?>(null)
  private val cancelledScanCommands = ConcurrentHashMap<String, ProtocolWireRecord>()
  private val attachmentCloseRequested = AtomicBoolean(false)
  /** Security events are enabled only after a security-aware JS peer sends a security command. */
  private val securityEventsEnabled = AtomicBoolean(false)
  private var attachmentRecord: ProtocolWireRecord? = null

  init {
    radio.onAdapterState = { adapterState ->
      clearGattProtocolOwnershipForAdapterLoss(adapterState)
      if (adapterState == "PoweredOff" || adapterState == "Resetting") {
        coreShadow?.postAdapterReset()
      }
      emitCurrentAdapterState()
    }
    radio.onCleanupFailure = { failure ->
      UnifiedBleProtocolJsiBinding.emitDiagnostic(
        nativeHandle,
        "cleanupRetryable",
        "Android cleanup remains retryable operation=${failure.operation}: " +
          (failure.throwable.message ?: "unknown error")
      )
    }
    radio.onSecurityState = { deviceId, state ->
      if (securityEventsEnabled.get()) emitSecurityStateChanged(deviceId, state.bond)
    }
    radio.onServicesChanged = { deviceId ->
      clearSubscriptionRoutesForDevice(deviceId)
      coreShadow?.postServicesChanged(deviceId)
      activeDatabases[deviceId.uppercase()]?.let { database ->
        emitDatabaseChanged(database)
      }
    }
    radio.registerBondStateReceiver()
    radio.registerAdapterStateReceiver()
    radio.onConnectionOutcome = outcome@{ deviceId, connected, status, attempt ->
      val deviceKey = deviceId.uppercase()
      val resolution = synchronized(ownershipGuard) {
        val pending = pendingConnects[deviceKey]
        val ownedPending = pending?.takeIf { attempt != null && it.attempt === attempt }
        if (ownedPending != null) {
          pendingConnects.remove(deviceKey, ownedPending)
          returnedReservations.remove(operationKey(ownedPending.command))
          if (connected && status == 0 && ownedPending.generation.isNotEmpty()) {
            establishedConnections[deviceKey] = EstablishedProtocolConnection(
              ownedPending.command.requiredRecord(10),
              ownedPending.attempt,
              ownedPending.lease,
              ownedPending.generation
            )
          }
          ConnectionOutcome.Pending(ownedPending)
        } else if (!connected) {
          val established = establishedConnections[deviceKey]
            ?.takeIf { attempt != null && it.attempt === attempt }
          if (established != null) {
            establishedConnections.remove(deviceKey, established)
            ConnectionOutcome.Established(established, pending?.command)
          } else {
            ConnectionOutcome.Unknown
          }
        } else {
          ConnectionOutcome.Unknown
        }
      }
      if (resolution === ConnectionOutcome.Unknown) {
        UnifiedBleProtocolJsiBinding.emitDiagnostic(nativeHandle, "unknownConnectionOutcome", deviceId)
        return@outcome
      }
      if (resolution is ConnectionOutcome.Pending) {
        val ownedPending = resolution.pending
        val command = ownedPending.command
        val admittedSeq = try {
          coreAdmissionSeqByOp[operationKey(command)]
        } catch (_: IllegalArgumentException) {
          null
        }
        if (connected && status == 0) {
          if (ownedPending.generation.isEmpty()) {
            UnifiedBleProtocolJsiBinding.emitDiagnostic(nativeHandle, "connectAdmissionMissing", deviceId)
            emitFailure(command, CoreCommandAuthority.CODE_REJECTED, "Android core admission generation was unavailable")
          } else {
            coreShadow?.postLinkEstablished(deviceId, ownedPending.generation)
            val rejection = admittedSeq?.let { seq ->
              coreRejectionSince(seq, CoreCommandAuthority.coreEventsFor("connect"))
            }
            if (rejection != null) {
              synchronized(ownershipGuard) {
                establishedConnections[deviceKey]?.takeIf { it.attempt === ownedPending.attempt }?.let {
                  establishedConnections.remove(deviceKey, it)
                }
              }
              coreShadow?.postLinkReleased(deviceId, ownedPending.generation)
              emitFailure(command, CoreCommandAuthority.CODE_REJECTED, coreRejectionMessage("connect", rejection))
            } else {
              emitSuccess(command, "connected")
            }
          }
        } else {
          emitFailure(command, "connectionFailed", "Android GATT connection failed with status $status")
        }
        return@outcome
      }
      if (resolution is ConnectionOutcome.Established) {
        val established = resolution.connection
        val affected = synchronized(ownershipGuard) {
          activeDatabases[deviceKey]?.takeIf { database ->
            databaseConnectionMatches(database, established.connection)
          }?.let { database -> activeDatabases.remove(deviceKey, database) }
          val commands = pendingCommands.values.filter { command ->
            command !== resolution.replacement &&
              command.requiredString(3) != "disconnect" &&
              connectionIdentityMatches(command, established.connection)
          }
          pendingSubscriptions.entries.removeIf { entry ->
            connectionIdentityMatchesRecords(entry.value.connection, established.connection)
          }
          activeSubscriptions.entries.removeIf { entry ->
            connectionIdentityMatchesRecords(entry.value.connection, established.connection)
          }
          commands
        }
        affected.forEach { command -> emitFailure(command, "connectionLost", "Android GATT link was lost") }
        coreShadow?.postLinkReleased(deviceId, established.generation)
        emitConnectionLost(established.connection, status)
      }
    }
    radio.onScanFailed = { errorCode ->
      val failedCommand = activeScanCommand.get()
      val stopFailure = radio.stopScan()
      if (stopFailure == null) {
        failedCommand?.let { activeScanCommand.compareAndSet(it, null) }
        completeCancelledScanCommands()
      }
      if (stopFailure != null) {
        radio.reportCleanupFailure(stopFailure)
        UnifiedBleProtocolJsiBinding.emitDiagnostic(
          nativeHandle,
          "scanStopFailed",
          "Android scan failure cleanup failed: ${stopFailure.throwable.message ?: "unknown error"}"
        )
      }
      if (failedCommand != null) {
        emitFailure(
          failedCommand,
          "scanFailed",
          "Android scan failed code=$errorCode"
        )
      }
      UnifiedBleProtocolJsiBinding.emitDiagnostic(nativeHandle, "scanFailed", "Android scan failed code=$errorCode")
    }
    radio.onProtocolScanResult = { advertisement ->
      if (activeScanCommand.get() != null) {
        UnifiedBleProtocolJsiBinding.emitAdvertisement(
          nativeHandle,
          advertisement.deviceId,
          advertisement.name,
          advertisement.rssi,
          advertisement.txPower ?: 0,
          advertisement.txPower != null,
          advertisement.connectable.toNativeConnectableState(),
          advertisement.appearance?.toLong() ?: 0L,
          advertisement.appearance != null,
          advertisement.rawRecord,
          advertisement.serviceUuids?.toTypedArray(),
          advertisement.solicitedServiceUuids?.toTypedArray(),
          advertisement.serviceData?.map { entry -> entry.serviceUuid }?.toTypedArray(),
          advertisement.serviceData?.map { entry -> entry.value }?.toTypedArray(),
          advertisement.manufacturerData?.map { entry -> entry.companyIdentifier }?.toIntArray(),
          advertisement.manufacturerData?.map { entry -> entry.value }?.toTypedArray()
        )
      }
    }
    radio.onProtocolNotification = { deviceId, characteristic, value ->
      (pendingSubscriptions.values.asSequence() + activeSubscriptions.values.asSequence())
        .filter { route -> route.matches(deviceId, characteristic, radio) }
        .forEach { route ->
          UnifiedBleProtocolJsiBinding.emitNotification(nativeHandle, route.subscriptionId, value)
        }
    }
  }

  private fun clearGattProtocolOwnershipForAdapterLoss(adapterState: String) {
    val state = radio.currentProtocolAdapterState()
    if (state.availability == "available" && state.authorization == "granted" && state.power == "on") return
    val failure = when (adapterState) {
      "PoweredOff" -> AdapterLossConnectFailure(
        "adapterPoweredOff",
        "Android Bluetooth was powered off while the connection was pending"
      )
      "Resetting" -> AdapterLossConnectFailure(
        "adapterResetting",
        "Android Bluetooth reset while the connection was pending"
      )
      else -> AdapterLossConnectFailure(
        "adapterUnavailable",
        "Android Bluetooth became unavailable while the connection was pending"
      )
    }
    val pending = synchronized(ownershipGuard) {
      pendingConnects.entries.toList().also { entries -> entries.forEach { entry -> pendingConnects.remove(entry.key, entry.value) } }
    }
    pending.forEach { entry -> emitFailure(entry.value.command, failure.code, failure.message) }
    synchronized(ownershipGuard) {
      establishedConnections.clear()
      dispatchReservations.clear()
      returnedReservations.clear()
    }
    activeDatabases.clear()
    pendingSubscriptions.clear()
    activeSubscriptions.clear()
  }

  fun emitCurrentAdapterState() {
    val state = radio.currentProtocolAdapterState()
    val fields = mutableMapOf<Int, ProtocolWireValue>(
      1 to ProtocolWireValue.StringValue(state.availability),
      2 to ProtocolWireValue.StringValue(state.authorization),
      3 to ProtocolWireValue.StringValue(state.power)
    )
    if (state.safeReason != null) {
      fields[4] = ProtocolWireValue.StringValue(state.safeReason)
    }
    UnifiedBleProtocolJsiBinding.emitAdapterState(
      nativeHandle,
      ProtocolWireEncoder.encode(ProtocolWireRecord(RecordKind.ADAPTER_STATE_SNAPSHOT, fields))
    )
  }

  fun dispatch(encodedCommand: ByteArray) {
    check(!attachmentCloseRequested.get()) {
      "Android protocol attachment close is in progress; retry close before dispatching another command"
    }
    val command = try {
      ProtocolCommandDecoder.decodeCommand(encodedCommand)
    } catch (error: IllegalArgumentException) {
      UnifiedBleProtocolJsiBinding.emitDispatcherFailure(nativeHandle, error.message ?: "Malformed command")
      throw error
    }
    val operationKey = operationKey(command)
    attachmentRecord = command.requiredRecord(2).requiredRecord(1)
    val prior = pendingCommands.putIfAbsent(operationKey, command)
    if (prior != null) {
      UnifiedBleProtocolJsiBinding.emitDiagnostic(
        nativeHandle,
        "duplicateOperation",
        "Android dispatcher received an already-pending protocol correlation"
      )
      return
    }
    try {
      when (command.requiredString(3)) {
        "scanStart" -> startScan(command)
        "scanStop" -> stopScan(command)
        "connect" -> connect(command)
        "disconnect" -> disconnect(command)
        "discover" -> discover(command)
        "read" -> read(command)
        "write" -> write(command)
        "readDescriptor" -> readDescriptor(command)
        "writeDescriptor" -> writeDescriptor(command)
        "readRssi" -> readRssi(command)
        "requestMtu" -> requestMtu(command)
        "readMtu" -> readMtu(command)
        "requestPriority" -> requestPriority(command)
        "readPhy" -> readPhy(command)
        "requestPhy" -> requestPhy(command)
        "securityState" -> {
          securityEventsEnabled.set(true)
          securityState(command)
        }
        "securityPair" -> {
          securityEventsEnabled.set(true)
          securityPair(command)
        }
        "securityCancelPairing" -> {
          securityEventsEnabled.set(true)
          securityCancelPairing(command)
        }
        "enumerateBondedPeers" -> enumerateBondedPeers(command)
        "subscribe" -> subscribe(command, true)
        "unsubscribe" -> subscribe(command, false)
        "cancel" -> cancel(command)
        "destroy" -> destroy(command)
        else -> emitFailure(command, "unsupportedCommand", "Command is not implemented by Android protocol-v2")
      }
    } catch (error: IllegalArgumentException) {
      emitFailure(command, "invalidCommand", error.message ?: "Android command is invalid")
    } catch (error: IllegalStateException) {
      emitFailure(command, "radioFailure", error.message ?: "Android radio rejected the command")
    } catch (error: SecurityException) {
      emitFailure(command, "permissionDenied", "Android Bluetooth permission is required for this operation")
    } catch (error: Exception) {
      emitFailure(command, "platformFailure", error.message ?: "Android platform operation failed")
    }
  }

  fun close(): Boolean {
    attachmentCloseRequested.set(true)
    securityEventsEnabled.set(false)
    radio.onSecurityState = null
    shadowGate.release()
    val result = radio.destroy()
    if (!result.isSuccessful) {
      UnifiedBleProtocolJsiBinding.emitDiagnostic(
        nativeHandle,
        "radioDestroyFailed",
        "Android radio destroy reported ${result.failures.size} failure(s)"
      )
    }
    if (result.isSuccessful) {
      pendingCommands.values.toList().forEach { pending ->
        emitFailure(pending, "attachmentClosed", "Android protocol attachment was closed")
      }
      synchronized(ownershipGuard) {
        pendingConnects.clear()
        establishedConnections.clear()
        dispatchReservations.clear()
        returnedReservations.clear()
      }
      activeDatabases.clear()
      pendingSubscriptions.clear()
      activeSubscriptions.clear()
      activeScanCommand.set(null)
    }
    return result.isSuccessful
  }

  private fun startScan(command: ProtocolWireRecord) {
    val options = command.requiredRecord(12)
    val serviceUuids = options.requiredStringList(1).toTypedArray()
    require(activeScanCommand.compareAndSet(null, command)) { "A protocol scan is already active" }
    // Set only once the core admitted the scan: compensation runs for an
    // admitted scan whose radio start then refuses, never for option parsing.
    var admission: CoreAdmission? = null
    try {
      val allowDuplicates = options.requiredBoolean(2)
      // Authority: admit the core scan BEFORE touching the radio. A refused
      // command releases the active-scan claim here and never scans.
      admission = admitCoreCommand(command, "scanStart") { shadow ->
        shadow.postScanStart(serviceUuids.toList(), allowDuplicates)
      } ?: run {
        activeScanCommand.compareAndSet(command, null)
        return
      }
      radio.startScan(
        serviceUuids = serviceUuids,
        scanMode = options.requiredSignedInteger(3).toInt(),
        callbackType = options.requiredSignedInteger(4).toInt(),
        legacyScan = options.requiredBoolean(5),
        allowDuplicates = allowDuplicates,
        deviceAddresses = options.optionalStringList(6).toTypedArray()
      )
      // Re-check before reporting: a worker verdict may have landed between
      // admission and radio start. On rejection stop the radio scan rather
      // than reporting success over a core refusal.
      val admitted = admission
      coreRejectionSince(admitted.seqBefore, CoreCommandAuthority.coreEventsFor("scanStart"))?.let { rejection ->
        activeScanCommand.compareAndSet(command, null)
        radio.stopScan()?.let { failure -> radio.reportCleanupFailure(failure) }
        emitFailure(command, CoreCommandAuthority.CODE_REJECTED, coreRejectionMessage("scanStart", rejection))
        return
      }
      emitSuccess(command, "scanStarted")
    } catch (error: Exception) {
      // Core-first compensation: the admitted core scan has no radio peer, so
      // attempt its stop rather than orphaning the core op. Best effort — the
      // rethrown radio error stays the loud terminal.
      if (admission != null) compensateCoreScanStop()
      if (!radio.hasScanCleanupOwnership()) {
        activeScanCommand.compareAndSet(command, null)
      }
      throw error
    }
  }

  /** Best-effort core scan-stop compensation after a radio refusal. */
  private fun compensateCoreScanStop() {
    val compensation = try {
      coreShadow?.postScanStop()
    } catch (_: Throwable) {
      null
    }
    if (compensation !is UbmGattCentralBridge.PostResult.Queued) {
      UnifiedBleProtocolJsiBinding.emitDiagnostic(
        nativeHandle,
        "coreCompensationFailed",
        "Android core scan compensation failed after radio refusal"
      )
    }
  }

  private fun stopScan(command: ProtocolWireRecord) {
    // Authority: admit (or attest the no-op of) the core stop before the
    // radio stop. A refused command never touches the radio.
    val admission = admitCoreCommand(command, "scanStop") { shadow -> shadow.postScanStop() } ?: return
    val failure = radio.stopScan()
    if (failure == null) {
      activeScanCommand.set(null)
      coreRejectionSince(admission.seqBefore, CoreCommandAuthority.coreEventsFor("scanStop"))?.let { rejection ->
        emitFailure(command, CoreCommandAuthority.CODE_REJECTED, coreRejectionMessage("scanStop", rejection))
        return
      }
      completeCancelledScanCommands()
      emitSuccess(command, "accepted")
    } else {
      radio.reportCleanupFailure(failure)
      emitFailure(command, "scanStopFailed", failure.throwable.message ?: "Android scan stop failed")
    }
  }

  private fun connect(command: ProtocolWireRecord) {
    val connection = command.requiredRecord(10)
    val peerId = connection.requiredString(2)
    val attempt = GattConnectAttempt()
    val prior = synchronized(ownershipGuard) {
      val key = peerId.uppercase()
      val existing = dispatchReservations[key]
      if (existing != null) existing.command else {
        dispatchReservations[key] = DispatchReservation(command, attempt)
        pendingConnects[key] = PendingProtocolConnect(command, attempt)
        null
      }
    }
    require(prior == null) { "A protocol connect is already reserved for this peer" }
    val lease = coreLeaseFor(peerId, operationKey(command))
    var admittedShadow: UbmGattCoreBinding? = null
    var admittedGeneration = ""
    try {
      // Authority: admit the core connect BEFORE touching the radio.
      val admission = admitCoreCommand(command, "connect") { shadow ->
        shadow.postConnect(peerId, lease)
      } ?: run {
        synchronized(ownershipGuard) {
          removePendingConnect(peerId.uppercase(), command)
          dispatchReservations[peerId.uppercase()]?.takeIf { it.command === command }?.let {
            dispatchReservations.remove(peerId.uppercase(), it)
          }
        }
        return
      }
      admittedShadow = admission.shadow
      val connectAdmission = when (val result = admission.shadow.takeConnectAdmission(peerId, lease)) {
        is com.sfourdrinier.unifiedblemanager.radio.ConnectAdmissionResult.Admitted -> result.admission.also {
          admittedGeneration = it.generation
        }
        is com.sfourdrinier.unifiedblemanager.radio.ConnectAdmissionResult.Rejected -> {
          synchronized(ownershipGuard) {
            removePendingConnect(peerId.uppercase(), command)
            dispatchReservations[peerId.uppercase()]?.takeIf { it.command === command }?.let {
              dispatchReservations.remove(peerId.uppercase(), it)
            }
          }
          emitFailure(command, CoreCommandAuthority.CODE_REJECTED, "Android core rejected connect $peerId (${result.detail})")
          return
        }
        com.sfourdrinier.unifiedblemanager.radio.ConnectAdmissionResult.Missing -> {
          synchronized(ownershipGuard) {
            removePendingConnect(peerId.uppercase(), command)
            dispatchReservations[peerId.uppercase()]?.takeIf { it.command === command }?.let {
              dispatchReservations.remove(peerId.uppercase(), it)
            }
          }
          emitFailure(command, CoreCommandAuthority.CODE_REJECTED, "Android core did not admit connect $peerId")
          return
        }
      }
      if (admission.schedulerFailure != null) {
        val reservation = synchronized(ownershipGuard) {
          dispatchReservations[peerId.uppercase()]?.takeIf { it.command === command }?.also {
            it.lease = lease
            it.generation = connectAdmission.generation
            it.phase = DispatchPhase.Cleaning
            it.cleanupStarted = true
          }
        }
        val compensationFailure = compensateConnect(
          admission.shadow,
          peerId,
          lease,
          connectAdmission.generation
        )
        reservation?.let {
          completeReservationCleanup(
            it,
            peerId,
            compensationFailure,
            admission.shadow,
            AtomicBoolean(false),
            releaseCore = false
          )
        }
        emitFailure(
          command,
          CoreCommandAuthority.CODE_SCHEDULE_FAILED,
          "Android core queued connect but scheduling failed (${admission.schedulerFailure}); native radio was not started"
        )
        return
      }
      var compensateConnectAfterAdmission = false
      synchronized(ownershipGuard) {
        val admittedPending = pendingConnects[peerId.uppercase()]
        val reservation = dispatchReservations[peerId.uppercase()]
        val cancelledBeforeAdmission = admittedPending == null && reservation?.command === command &&
          reservation.phase == DispatchPhase.Cleaning && reservation.lease.isEmpty()
        if (cancelledBeforeAdmission) {
          reservation.lease = lease
          reservation.generation = connectAdmission.generation
          reservation.cleanupStarted = true
          compensateConnectAfterAdmission = true
        } else if (admittedPending == null || admittedPending.command !== command || reservation == null ||
          reservation.command !== command || reservation.phase == DispatchPhase.Withdrawn
        ) {
          admittedPending?.takeIf { it.command === command }?.let { pendingConnects.remove(peerId.uppercase(), it) }
          reservation?.takeIf { it.command === command }?.let { dispatchReservations.remove(peerId.uppercase(), it) }
          // A re-entrant cancellation may already have completed the exact
          // core compensation while this admission callback was unwinding.
          // Only compensate an operation that is still pending here.
          compensateConnectAfterAdmission = pendingCommands[operationKey(command)] === command &&
            !compensatedConnects.remove(operationKey(command))
        } else {
          admittedPending.lease = lease
          admittedPending.generation = connectAdmission.generation
          reservation.lease = lease
          reservation.generation = connectAdmission.generation
        }
      }
      if (compensateConnectAfterAdmission) {
        val reservation = synchronized(ownershipGuard) {
          dispatchReservations[peerId.uppercase()]?.takeIf { it.command === command }
        }
        val failure = compensateConnect(admission.shadow, peerId, lease, connectAdmission.generation)
        if (reservation != null) {
          completeReservationCleanup(
            reservation,
            peerId,
            failure,
            admission.shadow,
            AtomicBoolean(false),
            releaseCore = false
          )
        }
        return
      }
      val autoConnect = when (connectionIntent(command.requiredString(20))) {
        ConnectionIntents.DIRECT -> false
        ConnectionIntents.WHEN_AVAILABLE -> true
      }
      val withdrawn = synchronized(ownershipGuard) {
        val reservation = dispatchReservations[peerId.uppercase()]
        if (reservation == null || reservation.command !== command || reservation.phase != DispatchPhase.Reserved) true
        else {
          reservation.phase = DispatchPhase.Dispatching
          reservation.nativeStarted = true
          false
        }
      }
      if (withdrawn) return
      val dispatchReservation = synchronized(ownershipGuard) {
        dispatchReservations[peerId.uppercase()]?.takeIf { it.command === command }
      }
      require(dispatchReservation != null) { "Android connect dispatch reservation disappeared" }
      try {
        radio.connect(peerId, autoConnect, 0, attempt)
      } finally {
        val cleanupReservation = synchronized(ownershipGuard) {
          if (dispatchReservations[peerId.uppercase()] === dispatchReservation) {
            dispatchReservation.nativeReturned = true
            val shouldCleanup = !dispatchReservation.cleanupStarted &&
              (dispatchReservation.phase == DispatchPhase.Withdrawn ||
                dispatchReservation.phase == DispatchPhase.Cleaning)
            dispatchReservation.phase = if (shouldCleanup) DispatchPhase.Cleaning else DispatchPhase.Returned
            if (shouldCleanup) dispatchReservation.cleanupStarted = true
            if (!shouldCleanup) {
              dispatchReservations.remove(peerId.uppercase(), dispatchReservation)
              if (pendingConnects[peerId.uppercase()]?.command === command) {
                returnedReservations[operationKey(command)] = dispatchReservation
              }
            }
            if (shouldCleanup) dispatchReservation else null
          } else null
        }
        if (cleanupReservation != null) {
          val cleanupDelivered = AtomicBoolean(false)
          fun completeCleanup(failure: OwnedRadioTeardownFailure?) {
            completeReservationCleanup(
              cleanupReservation,
              peerId,
              failure,
              coreShadow,
              cleanupDelivered,
              physicalCleanupCompleted = failure == null
            )
          }
          val immediateFailure = radio.disconnect(peerId, ::completeCleanup)
          immediateFailure?.let { completeCleanup(it) }
        }
      }
    } catch (error: Exception) {
      // Core-first compensation: the admitted core connect has no radio peer,
      // so release it rather than orphaning the core op. Best effort — the
      // rethrown radio error stays the loud terminal.
      val shadow = admittedShadow ?: coreShadow
      val reservation = synchronized(ownershipGuard) {
        (dispatchReservations[peerId.uppercase()]
          ?: returnedReservations[operationKey(command)])?.takeIf { it.command === command }?.also {
          returnedReservations.remove(operationKey(command), it)
          dispatchReservations[peerId.uppercase()] = it
          it.lease = lease
          it.generation = admittedGeneration
          it.phase = DispatchPhase.Cleaning
          it.cleanupStarted = true
        }
      }
      val compensationFailure = if (shadow != null && admittedGeneration.isNotEmpty()) {
        compensateConnect(shadow, peerId, lease, admittedGeneration)
      } else if (admittedGeneration.isEmpty()) {
        null
      } else {
        OwnedRadioTeardownFailure(
          "coreCompensation",
          IllegalStateException("Android core shadow unavailable after radio refusal for $peerId")
        )
      }
      if (reservation != null) {
        completeReservationCleanup(
          reservation,
          peerId,
          compensationFailure,
          shadow,
          AtomicBoolean(false),
          releaseCore = false
        )
      } else if (compensationFailure != null) {
        radio.reportCleanupFailure(compensationFailure)
      }
      removePendingConnect(peerId.uppercase(), command)
      throw error
    }
  }

  private fun compensateConnect(
    shadow: UbmGattCoreBinding,
    peerId: String,
    lease: String,
    generation: String
  ): OwnedRadioTeardownFailure? {
    val disconnectFailure = attestCoreTransition(
      shadow,
      "disconnect compensation",
      { it.postDisconnect(peerId, lease) },
      setOf("disconnect", "connection.stale")
    )
    if (disconnectFailure != null) {
      return OwnedRadioTeardownFailure("coreCompensation", IllegalStateException(disconnectFailure))
    }
    return if (generation.isEmpty()) null else scopedReleaseFailure(shadow, peerId, generation)
  }

  private fun disconnect(command: ProtocolWireRecord) {
    val requestedConnection = command.requiredRecord(10)
    val peerId = requestedConnection.requiredString(2)
    var withdrawnCommand: ProtocolWireRecord? = null
    val ownership = synchronized(ownershipGuard) {
      val peerKey = peerId.uppercase()
      val pending = pendingConnects[peerKey]
      val returned = pending?.let { returnedReservations[operationKey(it.command)] }
      val reservation = (dispatchReservations[peerKey] ?: returned)?.takeIf { reservation ->
        reservationConnectionMatches(reservation, requestedConnection)
      }
      val established = establishedConnections[peerId.uppercase()]?.takeIf {
        connectionIdentityMatchesRecords(it.connection, requestedConnection)
      }
      if (reservation != null && pending?.command === reservation.command) {
        reservation.phase = DispatchPhase.Withdrawn
        if (returned === reservation) {
          returnedReservations.remove(operationKey(reservation.command), reservation)
          dispatchReservations[peerKey] = reservation
        }
        withdrawnCommand = pending.command
        pendingConnects.remove(peerKey, pending)
      }
      established?.let { it.lease }
        ?: reservation?.lease
    }
    withdrawnCommand?.let { emitCancelled(it) }
    val lease = ownership ?: run {
      emitFailure(command, CoreCommandAuthority.CODE_REJECTED, "Android disconnect has no owned connection lease for $peerId")
      return
    }
    // Authority: admit the core disconnect BEFORE touching the radio.
    val admission = admitCoreCommand(command, "disconnect") { shadow ->
      shadow.postDisconnect(peerId, lease)
    } ?: return
    val events = CoreCommandAuthority.coreEventsFor("disconnect")
    val completion: (OwnedRadioTeardownFailure?) -> Unit = { cleanupFailure ->
      if (cleanupFailure == null) {
        val rejection = coreRejectionSince(admission.seqBefore, events)
        if (rejection != null) {
          emitFailure(command, CoreCommandAuthority.CODE_REJECTED, coreRejectionMessage("disconnect", rejection))
        } else {
          emitSuccess(command, "accepted")
        }
      } else {
        emitFailure(
          command,
          "disconnectCleanupFailed",
          cleanupFailure.throwable.message ?: "Android GATT cleanup failed"
        )
      }
    }
    var startDeferredCleanup = false
    var retryCoreOnly = false
    var retryCompensation = false
    val deferred = synchronized(ownershipGuard) {
      dispatchReservations[peerId.uppercase()]?.takeIf {
        (it.phase == DispatchPhase.Withdrawn || it.phase == DispatchPhase.Cleaning) &&
          reservationConnectionMatches(it, requestedConnection) && it.lease == lease
      }?.also {
        it.disconnectWaiters += completion
        if (it.cleanupStarted) {
          // The current cleanup owner will deliver this waiter with the same
          // result; never start a second physical or core teardown.
        } else if (it.phase == DispatchPhase.Withdrawn && it.nativeReturned) {
          it.phase = DispatchPhase.Cleaning
          it.cleanupStarted = true
          startDeferredCleanup = true
        } else if (it.phase == DispatchPhase.Cleaning) {
          it.cleanupStarted = true
          retryCompensation = !it.nativeStarted
          retryCoreOnly = it.nativeStarted && it.physicalCleanupComplete
          startDeferredCleanup = !retryCompensation && !retryCoreOnly
        }
      }
    }
    if (deferred != null) {
      if (retryCompensation) {
        val failure = coreShadow?.let { compensateConnect(it, peerId, deferred.lease, deferred.generation) }
          ?: OwnedRadioTeardownFailure("coreCompensation", IllegalStateException("Android core shadow unavailable while retrying $peerId"))
        completeReservationCleanup(deferred, peerId, failure, coreShadow, AtomicBoolean(false), releaseCore = false)
      } else if (retryCoreOnly) {
        val failure = scopedReleaseFailure(coreShadow, peerId, deferred.generation)
        completeReservationCleanup(
          deferred,
          peerId,
          failure,
          coreShadow,
          AtomicBoolean(false),
          releaseCore = false,
          physicalCleanupCompleted = true
        )
      } else if (startDeferredCleanup) {
        val cleanupDelivered = AtomicBoolean(false)
        synchronized(ownershipGuard) { deferred.cleanupStarted = true }
        val cleanupFailure = radio.disconnect(peerId) { failure ->
          completeReservationCleanup(
            deferred,
            peerId,
            failure,
            coreShadow,
            cleanupDelivered,
            physicalCleanupCompleted = failure == null
          )
        }
        cleanupFailure?.let { failure ->
          completeReservationCleanup(
            deferred,
            peerId,
            failure,
            coreShadow,
            cleanupDelivered,
            physicalCleanupCompleted = false
          )
        }
      }
      return
    }
    val failure = radio.disconnect(peerId, completion)
    if (failure != null) return
  }

  private fun discover(command: ProtocolWireRecord) {
    val connection = command.requiredRecord(10)
    val database = command.requiredRecord(11)
    val peerId = connection.requiredString(2)
    // Authority: admit the core discovery BEFORE touching the radio.
    val admission = admitCoreCommand(command, "discover") { shadow ->
      shadow.postDiscoveryBegin(peerId)
    } ?: return
    val events = CoreCommandAuthority.coreEventsFor("discover")
    val radioOperationId = try {
      radio.discover(peerId) { result ->
        if (result.isFailure) {
          coreShadow?.postDiscoveryFail(peerId)
          emitFailure(command, "discoverFailed", "Android GATT service discovery failed")
          return@discover
        }
        val snapshot = databaseSnapshot(database, connection.requiredString(2))
        activeDatabases[connection.requiredString(2).uppercase()] = database
        coreShadow?.postDiscoveryComplete(peerId)
        coreRejectionSince(admission.seqBefore, events)?.let { rejection ->
          emitFailure(command, CoreCommandAuthority.CODE_REJECTED, coreRejectionMessage("discover", rejection))
          return@discover
        }
        emitSuccess(command, "database", mapOf(4 to ProtocolWireValue.RecordValue(database), 12 to ProtocolWireValue.RecordValue(snapshot)))
      }
    } catch (error: Exception) {
      // Core-first compensation: the begun core discovery will never complete,
      // so fail it rather than wedging core discovery state. Best effort.
      val compensation = try {
        admission.shadow.postDiscoveryFail(peerId)
      } catch (_: Throwable) {
        null
      }
      if (compensation !is UbmGattCentralBridge.PostResult.Queued) {
        UnifiedBleProtocolJsiBinding.emitDiagnostic(
          nativeHandle,
          "coreCompensationFailed",
          "Android core discovery compensation failed after radio refusal for $peerId"
        )
      }
      throw error
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun read(command: ProtocolWireRecord) {
    val endpoint = characteristicEndpoint(command.requiredRecord(4))
    val radioOperationId = radio.readCharacteristicExact(
      endpoint.deviceId,
      endpoint.serviceUuid,
      endpoint.serviceOccurrence,
      endpoint.characteristicUuid,
      endpoint.characteristicOccurrence
    ) { result ->
      result.fold(
        onSuccess = { value ->
          if (!isPending(command)) return@fold
          UnifiedBleProtocolJsiBinding.emitRead(
            nativeHandle,
            commandEpoch(command),
            commandNonce(command),
            value ?: byteArrayOf()
          )
          pendingCommands.remove(operationKey(command), command)
          radioOperationIds.remove(operationKey(command))
        },
        onFailure = { error -> emitFailure(command, "readFailed", error.message ?: "Android GATT read failed") }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun write(command: ProtocolWireRecord) {
    val endpoint = characteristicEndpoint(command.requiredRecord(4))
    val value = UnifiedBleProtocolJsiBinding.copyCommandBinary(
      nativeHandle,
      commandEpoch(command),
      commandNonce(command)
    )
    val withResponse = when (command.requiredString(13)) {
      "withResponse" -> true
      "withoutResponse" -> false
      else -> throw IllegalArgumentException("Native protocol write mode is invalid")
    }
    val radioOperationId = radio.writeCharacteristicExact(
      endpoint.deviceId,
      endpoint.serviceUuid,
      endpoint.serviceOccurrence,
      endpoint.characteristicUuid,
      endpoint.characteristicOccurrence,
      value,
      withResponse
    ) { result ->
      result.fold(
        onSuccess = { emitSuccess(command, "write") },
        onFailure = { error ->
          emitGattOperationFailure(command, "writeFailed", error, "Android GATT write failed")
        }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun readDescriptor(command: ProtocolWireRecord) {
    val endpoint = descriptorEndpoint(command.requiredRecord(5))
    val radioOperationId = radio.readDescriptorExact(
      endpoint.deviceId,
      endpoint.serviceUuid,
      endpoint.serviceOccurrence,
      endpoint.characteristicUuid,
      endpoint.characteristicOccurrence,
      endpoint.descriptorUuid,
      endpoint.descriptorOccurrence
    ) { result ->
      result.fold(
        onSuccess = { value ->
          if (!isPending(command)) return@fold
          UnifiedBleProtocolJsiBinding.emitDescriptorRead(
            nativeHandle,
            commandEpoch(command),
            commandNonce(command),
            value ?: byteArrayOf()
          )
          pendingCommands.remove(operationKey(command), command)
          radioOperationIds.remove(operationKey(command))
        },
        onFailure = { error ->
          emitFailure(command, "readDescriptorFailed", error.message ?: "Android GATT descriptor read failed")
        }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun writeDescriptor(command: ProtocolWireRecord) {
    val endpoint = descriptorEndpoint(command.requiredRecord(5))
    val value = UnifiedBleProtocolJsiBinding.copyCommandBinary(
      nativeHandle,
      commandEpoch(command),
      commandNonce(command)
    )
    val radioOperationId = radio.writeDescriptorExact(
      endpoint.deviceId,
      endpoint.serviceUuid,
      endpoint.serviceOccurrence,
      endpoint.characteristicUuid,
      endpoint.characteristicOccurrence,
      endpoint.descriptorUuid,
      endpoint.descriptorOccurrence,
      value
    ) { result ->
      result.fold(
        onSuccess = { emitSuccess(command, "descriptorWrite", mapOf(15 to ProtocolWireValue.RecordValue(command.requiredRecord(5)))) },
        onFailure = { error ->
          emitFailure(command, "writeDescriptorFailed", error.message ?: "Android GATT descriptor write failed")
        }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun readRssi(command: ProtocolWireRecord) {
    val deviceId = command.requiredRecord(10).requiredString(2)
    val radioOperationId = radio.readRemoteRssi(deviceId) { result ->
      result.fold(
        onSuccess = { rssi ->
          emitSuccess(command, "rssi", mapOf(13 to ProtocolWireValue.SignedIntegerValue(rssi.toLong())))
        },
        onFailure = { error -> emitFailure(command, "readRssiFailed", error.message ?: "Android RSSI read failed") }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun requestMtu(command: ProtocolWireRecord) {
    val deviceId = command.requiredRecord(10).requiredString(2)
    val requestedMtu = command.requiredUnsigned(14)
    require(requestedMtu in 23L..517L) { "Requested ATT MTU is outside the canonical range" }
    val radioOperationId = radio.requestMtu(deviceId, requestedMtu.toInt()) { result ->
      result.fold(
        onSuccess = { negotiatedMtu ->
          emitSuccess(command, "mtu", mapOf(14 to ProtocolWireValue.UnsignedIntegerValue(negotiatedMtu.toLong())))
        },
        onFailure = { error -> emitFailure(command, "requestMtuFailed", error.message ?: "Android MTU request failed") }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun readMtu(command: ProtocolWireRecord) {
    val deviceId = command.requiredRecord(10).requiredString(2)
    val radioOperationId = radio.readEffectiveMtu(deviceId) { result ->
      result.fold(
        onSuccess = { effectiveMtu ->
          val fields = if (effectiveMtu === null) {
            emptyMap()
          } else {
            mapOf(22 to ProtocolWireValue.UnsignedIntegerValue(effectiveMtu.toLong()))
          }
          emitSuccess(command, "mtu", fields)
        },
        onFailure = { error -> emitFailure(command, "readMtuFailed", error.message ?: "Android effective MTU read failed") }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun requestPriority(command: ProtocolWireRecord) {
    val deviceId = command.requiredRecord(10).requiredString(2)
    val connectionPriority = when (command.requiredString(16)) {
      "lowPower" -> android.bluetooth.BluetoothGatt.CONNECTION_PRIORITY_LOW_POWER
      "balanced" -> android.bluetooth.BluetoothGatt.CONNECTION_PRIORITY_BALANCED
      "highThroughput" -> android.bluetooth.BluetoothGatt.CONNECTION_PRIORITY_HIGH
      else -> throw IllegalArgumentException("Android connection priority is unsupported")
    }
    val radioOperationId = radio.requestConnectionPriority(deviceId, connectionPriority) { result ->
      result.fold(
        onSuccess = { accepted ->
          emitSuccess(
            command,
            "priority",
            mapOf(18 to ProtocolWireValue.BooleanValue(accepted))
          )
        },
        onFailure = { error ->
          emitFailure(
            command,
            "requestPriorityFailed",
            error.message ?: "Android connection priority request failed"
          )
        }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun readPhy(command: ProtocolWireRecord) {
    requirePhyAvailable()
    val deviceId = command.requiredRecord(10).requiredString(2)
    val radioOperationId = radio.readPhy(deviceId) { result ->
      result.fold(
        onSuccess = { phy ->
          emitSuccess(
            command,
            "phy",
            mapOf(
              19 to ProtocolWireValue.StringValue(phy.txPhy),
              20 to ProtocolWireValue.StringValue(phy.rxPhy)
            )
          )
        },
        onFailure = { error -> emitFailure(command, "readPhyFailed", error.message ?: "Android PHY read failed") }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun requestPhy(command: ProtocolWireRecord) {
    requirePhyAvailable()
    val deviceId = command.requiredRecord(10).requiredString(2)
    val txPhy = OwnedAndroidGattRadio.phyMaskValue(command.optionalString(17))
    val rxPhy = OwnedAndroidGattRadio.phyMaskValue(command.optionalString(18))
    val radioOperationId = radio.requestPhy(deviceId, txPhy, rxPhy) { result ->
      result.fold(
        onSuccess = { phy ->
          val fields = mutableMapOf<Int, ProtocolWireValue>(
            21 to ProtocolWireValue.BooleanValue(phy !== null)
          )
          if (phy !== null) {
            fields[19] = ProtocolWireValue.StringValue(phy.txPhy)
            fields[20] = ProtocolWireValue.StringValue(phy.rxPhy)
          }
          emitSuccess(command, "phy", fields)
        },
        onFailure = { error -> emitFailure(command, "requestPhyFailed", error.message ?: "Android PHY request failed") }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun subscribe(command: ProtocolWireRecord, enable: Boolean) {
    val subscriptionId = command.requiredString(7)
    if (!enable && !activeSubscriptions.containsKey(subscriptionId)) {
      emitSuccess(command, "unsubscribed")
      return
    }
    val endpoint = characteristicEndpoint(command.requiredRecord(4))
    val connection = connectionRecordForCommand(command)
      ?: throw IllegalArgumentException("Android subscription is missing its connection path")
    val route = SubscriptionRoute(
      subscriptionId,
      endpoint,
      connection,
      command.optionalString(21)
    )
    if (enable) {
      pendingSubscriptions[subscriptionId] = route
    }
    val radioOperationId = radio.setNotifyExact(
      endpoint.deviceId,
      endpoint.serviceUuid,
      endpoint.serviceOccurrence,
      endpoint.characteristicUuid,
      endpoint.characteristicOccurrence,
      enable,
      subscriptionType = command.optionalString(21)
    ) { result ->
      result.fold(
        onSuccess = {
          if (!isPending(command)) {
            pendingSubscriptions.remove(subscriptionId, route)
            if (enable) {
              radio.setNotifyExact(
                endpoint.deviceId,
                endpoint.serviceUuid,
                endpoint.serviceOccurrence,
                endpoint.characteristicUuid,
                endpoint.characteristicOccurrence,
                false
              ) { disableResult ->
                disableResult.exceptionOrNull()?.let { error ->
                  UnifiedBleProtocolJsiBinding.emitDiagnostic(
                    nativeHandle,
                    "cancelledSubscriptionDisableFailed",
                    error.message ?: "Android GATT cancellation cleanup failed"
                  )
                }
              }
            }
            return@fold
          }
          if (enable) {
            pendingSubscriptions.remove(subscriptionId, route)
            activeSubscriptions[subscriptionId] = route
          } else {
            pendingSubscriptions.remove(subscriptionId)
            activeSubscriptions.remove(subscriptionId)
          }
          emitSuccess(command, if (enable) "subscribed" else "unsubscribed")
        },
        onFailure = { error ->
          pendingSubscriptions.remove(subscriptionId, route)
          emitGattOperationFailure(command, "subscriptionFailed", error, "Android CCCD operation failed")
        }
      )
    }
    radioOperationIds[operationKey(command)] = radioOperationId
  }

  private fun destroy(command: ProtocolWireRecord) {
    securityEventsEnabled.set(false)
    radio.onSecurityState = null
    shadowGate.release()
    val pendingBeforeDestroy = pendingCommands.values
      .filter { it !== command }
      .toList()
    val result = radio.destroy()
    pendingBeforeDestroy.forEach { pending ->
      emitFailure(pending, "destroyed", "Android radio was destroyed before the operation completed")
    }
    synchronized(ownershipGuard) {
      pendingConnects.clear()
      establishedConnections.clear()
      dispatchReservations.clear()
    }
    activeDatabases.clear()
    pendingSubscriptions.clear()
    activeSubscriptions.clear()
    if (result.isSuccessful) {
      activeScanCommand.set(null)
      completeCancelledScanCommands()
      emitSuccess(command, "destroyed")
    } else {
      emitFailure(command, "destroyFailed", "Android radio destroy reported ${result.failures.size} failure(s)")
    }
  }

  fun cancelPendingOperation(dispatchEpoch: Long, nonce: String) {
    val command = pendingCommands["$dispatchEpoch:$nonce"] ?: return
    val commandKind = command.requiredString(3)
    val operationKey = operationKey(command)
    val radioOperationId = radioOperationIds[operationKey]
    try {
      var cleanupNow = false
      var reservedCancellation = false
      var reservedCompensation = false
      val withdrawnConnect = if (commandKind == "connect") {
        val deviceId = command.requiredRecord(10).requiredString(2)
        synchronized(ownershipGuard) {
          val reservation = (dispatchReservations[deviceId.uppercase()] ?: returnedReservations[operationKey(command)])
            ?.takeIf { it.command === command }
          reservation?.let {
            val wasReserved = reservation.phase == DispatchPhase.Reserved
            reservedCancellation = wasReserved
            cleanupNow = reservation.phase == DispatchPhase.Returned
            reservedCompensation = wasReserved && reservation.lease.isNotEmpty() && !reservation.cleanupStarted
            reservation.phase = if (cleanupNow || wasReserved) DispatchPhase.Cleaning else DispatchPhase.Withdrawn
            if (cleanupNow || reservedCompensation) reservation.cleanupStarted = true
            if (cleanupNow) {
              returnedReservations.remove(operationKey(command), it)
              dispatchReservations[deviceId.uppercase()] = it
            }
          }
          pendingConnects[deviceId.uppercase()]
            ?.takeIf { it.command === command }
            ?.also { pendingConnects.remove(deviceId.uppercase(), it) }
          reservation
        }
      } else {
        null
      }
      if (commandKind == "scanStart") {
        val cleanupFailure = radio.stopScan()
        if (cleanupFailure != null) {
          cancelledScanCommands[operationKey] = command
          radio.reportCleanupFailure(cleanupFailure)
          UnifiedBleProtocolJsiBinding.emitDiagnostic(
            nativeHandle,
            "scanCancellationCleanupRetryable",
            cleanupFailure.throwable.message ?: "Android scan cancellation cleanup remains retryable"
          )
          return
        }
        activeScanCommand.set(null)
        emitCancelled(command)
        return
      }
      emitCancelled(command)
      if (radioOperationId != null) {
        radio.cancelOperation(radioOperationId)
      }
      if (commandKind == "connect") {
        val deviceId = command.requiredRecord(10).requiredString(2)
        if (withdrawnConnect != null && withdrawnConnect.lease.isNotEmpty() && !reservedCancellation) {
          val shadow = coreShadow
          if (shadow != null) {
            attestCoreTransition(
              shadow,
              "disconnect cancellation",
              { it.postDisconnect(deviceId, withdrawnConnect.lease) },
              setOf("disconnect", "connection.stale")
            )
          }
        }
        if (reservedCompensation && withdrawnConnect != null && withdrawnConnect.lease.isNotEmpty()) {
          compensatedConnects.add(operationKey(command))
          val failure = coreShadow?.let {
            compensateConnect(it, deviceId, withdrawnConnect.lease, withdrawnConnect.generation)
          } ?: OwnedRadioTeardownFailure(
            "coreCompensation",
            IllegalStateException("Android core shadow unavailable while cancelling $deviceId")
          )
          completeReservationCleanup(
            withdrawnConnect,
            deviceId,
            failure,
            coreShadow,
            AtomicBoolean(false),
            releaseCore = false
          )
        }
        if (cleanupNow) {
          val cleanupDelivered = AtomicBoolean(false)
          val cleanupFailure = radio.disconnect(deviceId) { failure ->
            completeReservationCleanup(
              withdrawnConnect!!,
              deviceId,
              failure,
              coreShadow,
              cleanupDelivered,
              physicalCleanupCompleted = failure == null
            )
          }
          cleanupFailure?.let { failure ->
            completeReservationCleanup(
              withdrawnConnect!!,
              deviceId,
              failure,
              coreShadow,
              cleanupDelivered,
              physicalCleanupCompleted = false
            )
          }
        }
      }
      if (commandKind == "scanStop") {
        radio.stopScan()?.let { failure ->
          radio.reportCleanupFailure(failure)
        } ?: run {
          activeScanCommand.set(null)
          completeCancelledScanCommands()
        }
      }
      if (commandKind == "unsubscribe") {
        val subscriptionId = command.requiredString(7)
        val route = activeSubscriptions[subscriptionId]
        if (route != null) {
          radio.setNotifyExact(
            route.endpoint.deviceId,
            route.endpoint.serviceUuid,
            route.endpoint.serviceOccurrence,
            route.endpoint.characteristicUuid,
            route.endpoint.characteristicOccurrence,
            true,
            subscriptionType = route.mode
          ) { result ->
            result.exceptionOrNull()?.let { error ->
              radio.reportCleanupFailure(
                OwnedRadioTeardownFailure("cancelledUnsubscribeRestore", error)
              )
            }
          }
        }
      }
      if (commandKind == "subscribe") {
        val endpoint = characteristicEndpoint(command.requiredRecord(4))
        val subscriptionId = command.requiredString(7)
        pendingSubscriptions.remove(subscriptionId)
        activeSubscriptions.remove(subscriptionId)
        radio.setNotifyExact(
          endpoint.deviceId,
          endpoint.serviceUuid,
          endpoint.serviceOccurrence,
          endpoint.characteristicUuid,
          endpoint.characteristicOccurrence,
          false
        ) { result ->
          result.exceptionOrNull()?.let { error ->
            radio.reportCleanupFailure(OwnedRadioTeardownFailure("cancelledSubscriptionDisable", error))
          }
        }
      }
    } catch (error: Exception) {
      UnifiedBleProtocolJsiBinding.emitDiagnostic(
        nativeHandle,
        "cancellationCleanupFailed",
        error.message ?: "Android cancellation cleanup failed"
      )
    }
  }

  private fun completeCancelledScanCommands() {
    cancelledScanCommands.values.toList().forEach { command ->
      emitCancelled(command)
    }
    cancelledScanCommands.clear()
  }

  private fun cancel(command: ProtocolWireRecord) {
    val target = command.requiredRecord(8)
    val dispatchEpoch = target.requiredUnsigned(2)
    val nonce = target.requiredString(3)
    val state = UnifiedBleProtocolJsiBinding.requestCancellation(nativeHandle, dispatchEpoch, nonce)
    if (state == "cancellationRequested") {
      cancelPendingOperation(dispatchEpoch, nonce)
    }
    emitCancellationAcknowledgement(command, state)
  }

  private fun securityState(command: ProtocolWireRecord) {
    val peerId = command.requiredString(15)
    val state = radio.securityState(peerId)
    emitSuccess(command, "securityState", securityFields(peerId, state.bond))
  }

  private fun securityPair(command: ProtocolWireRecord) {
    val peerId = command.requiredString(15)
    val pairTransport = command.requiredString(19)
    val operationId = radio.pair(peerId, pairTransport) { outcome, state ->
      if (!isPending(command)) return@pair
      if (outcome == "rejected") {
        emitFailure(command, "pairRejected", "Android rejected the system bond request")
      } else if (outcome == "unknown") {
        emitFailure(command, "bondStateUnknown", "Android did not report a recognized terminal bond state")
      } else {
        emitSuccess(command, "securityPair", securityFields(peerId, state.bond))
      }
    }
    if (operationId != 0L) radioOperationIds[operationKey(command)] = operationId
  }

  private fun securityCancelPairing(command: ProtocolWireRecord) {
    val peerId = command.requiredString(15)
    val pendingPair = pendingCommands.values.firstOrNull { candidate ->
      candidate.requiredString(3) == "securityPair" && candidate.requiredString(15).equals(peerId, ignoreCase = true)
    }
    if (pendingPair != null) {
      radio.clearPendingBondPair(peerId)
      emitCancelled(pendingPair)
    }
    // Compile-SDK 36 cannot physically cancel the Android system ceremony.
    // This command only releases library ownership; the public cancellation
    // capability remains unregistered until a public API is compiled in.
    emitSuccess(command, "accepted")
  }

  private fun enumerateBondedPeers(command: ProtocolWireRecord) {
    val snapshots = bondedPeerSnapshotRecords(radio.bondedPeerSnapshots())
    emitSuccess(
      command,
      "bondedPeers",
      mapOf(23 to ProtocolWireValue.RecordListValue(snapshots))
    )
  }

  private fun emitSuccess(command: ProtocolWireRecord, kind: String, additions: Map<Int, ProtocolWireValue> = emptyMap()) {
    if (kind == "bondedPeers") {
      // Bonded enumeration reads Android metadata synchronously and can block while
      // JSI cancellation or attachment teardown runs concurrently. Claim first so
      // only one terminal path owns this command's result.
      if (!claimExactPendingCommand(pendingCommands, operationKey(command), command)) return
      val records = additions[23]
      require(records is ProtocolWireValue.RecordListValue) {
        "Android bonded peer result is missing its peer snapshot list"
      }
      UnifiedBleProtocolJsiBinding.emitRecord(
        nativeHandle,
        ProtocolWireEncoder.encode(bondedPeerResultRecord(command.requiredRecord(2), records.value))
      )
      radioOperationIds.remove(operationKey(command))
      forgetCoreAdmission(command)
      return
    }
    if (!isPending(command)) return
    val fields = mutableMapOf<Int, ProtocolWireValue>(
      1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
      2 to ProtocolWireValue.StringValue(kind),
      3 to ProtocolWireValue.RecordValue(terminal(command, "succeeded"))
    )
    when (kind) {
      "connected" -> fields[11] = ProtocolWireValue.RecordValue(command.requiredRecord(10))
      "subscribed", "unsubscribed" -> {
        fields[5] = ProtocolWireValue.RecordValue(command.requiredRecord(4))
        fields[7] = ProtocolWireValue.StringValue(command.requiredString(7))
      }
    }
    fields.putAll(additions)
    val result = ProtocolWireRecord(RecordKind.RESULT, fields)
    UnifiedBleProtocolJsiBinding.emitRecord(nativeHandle, ProtocolWireEncoder.encode(result))
    claimExactPendingCommand(pendingCommands, operationKey(command), command)
    radioOperationIds.remove(operationKey(command))
    forgetCoreAdmission(command)
  }

  private fun emitGattOperationFailure(
    command: ProtocolWireRecord,
    fallbackCode: String,
    error: Throwable,
    fallbackMessage: String
  ) {
    emitFailure(
      command,
      androidGattOperationFailureCode(error, fallbackCode),
      error.message ?: fallbackMessage,
      androidGattOperationFailureStatus(error)
    )
  }

  private fun emitFailure(
    command: ProtocolWireRecord,
    code: String,
    message: String,
    androidGattStatus: Int? = null
  ) {
    // Claim before constructing or emitting the terminal. Link loss can race a
    // native GATT operation callback; a check followed by a later claim lets
    // both paths publish a result for the same operation.
    if (!claimExactPendingCommand(pendingCommands, operationKey(command), command)) return
    val errorFields = mutableMapOf<Int, ProtocolWireValue>(
        1 to ProtocolWireValue.StringValue(code),
        2 to ProtocolWireValue.StringValue("android"),
        3 to ProtocolWireValue.StringValue(command.requiredString(3)),
        4 to ProtocolWireValue.StringValue("notRetryable"),
        7 to ProtocolWireValue.StringValue(message)
      )
    androidGattStatus?.let { status ->
      errorFields[8] = ProtocolWireValue.SignedIntegerValue(status.toLong())
    }
    val error = ProtocolWireRecord(
      RecordKind.ERROR,
      errorFields
    )
    val result = ProtocolWireRecord(
      RecordKind.RESULT,
      mapOf(
        1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
        2 to ProtocolWireValue.StringValue(dispatcherResultKindFor(command.requiredString(3))),
        3 to ProtocolWireValue.RecordValue(terminal(command, "failed", code)),
        10 to ProtocolWireValue.RecordValue(error)
      )
    )
    UnifiedBleProtocolJsiBinding.emitRecord(nativeHandle, ProtocolWireEncoder.encode(result))
    radioOperationIds.remove(operationKey(command))
    forgetCoreAdmission(command)
  }

  private fun emitCancelled(command: ProtocolWireRecord) {
    val bondedPeerCommand = command.requiredString(3) == "enumerateBondedPeers"
    if (bondedPeerCommand) {
      if (!claimExactPendingCommand(pendingCommands, operationKey(command), command)) return
    } else if (!isPending(command)) return
    val result = ProtocolWireRecord(
      RecordKind.RESULT,
      mapOf(
        1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
        2 to ProtocolWireValue.StringValue("cancelled"),
        3 to ProtocolWireValue.RecordValue(terminal(command, "failed", "cancelled")),
        10 to ProtocolWireValue.RecordValue(
          ProtocolWireRecord(
            RecordKind.ERROR,
            mapOf(
              1 to ProtocolWireValue.StringValue("cancelled"),
              2 to ProtocolWireValue.StringValue("android"),
              3 to ProtocolWireValue.StringValue(command.requiredString(3)),
              4 to ProtocolWireValue.StringValue("notRetryable"),
              7 to ProtocolWireValue.StringValue("Android operation was cancelled")
            )
          )
        )
      )
    )
    UnifiedBleProtocolJsiBinding.emitRecord(nativeHandle, ProtocolWireEncoder.encode(result))
    if (!bondedPeerCommand) claimExactPendingCommand(pendingCommands, operationKey(command), command)
    radioOperationIds.remove(operationKey(command))
    forgetCoreAdmission(command)
  }

  private fun emitCancellationAcknowledgement(command: ProtocolWireRecord, state: String) {
    if (!isPending(command)) return
    val result = ProtocolWireRecord(
      RecordKind.RESULT,
      mapOf(
        1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
        2 to ProtocolWireValue.StringValue("cancelled"),
        3 to ProtocolWireValue.RecordValue(terminal(command, "succeeded")),
        8 to ProtocolWireValue.StringValue(state)
      )
    )
    UnifiedBleProtocolJsiBinding.emitRecord(nativeHandle, ProtocolWireEncoder.encode(result))
    claimExactPendingCommand(pendingCommands, operationKey(command), command)
    forgetCoreAdmission(command)
  }

  private fun emitConnectionLost(connection: ProtocolWireRecord, status: Int) {
    val event = connectionLostEvent(nativeHandle, connection, status, 0L, SystemClock.elapsedRealtime())
    UnifiedBleProtocolJsiBinding.emitRecord(nativeHandle, ProtocolWireEncoder.encode(event))
  }

  private fun emitDatabaseChanged(database: ProtocolWireRecord) {
    val event = databaseChangedEvent(nativeHandle, database, 0L, SystemClock.elapsedRealtime())
    UnifiedBleProtocolJsiBinding.emitRecord(nativeHandle, ProtocolWireEncoder.encode(event))
  }

  private fun emitSecurityStateChanged(peerId: String, bondState: String) {
    val attachment = attachmentRecord ?: return
    val event = ProtocolWireRecord(
      RecordKind.EVENT,
      mapOf(
        1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
        2 to ProtocolWireValue.StringValue("native-security-state-${SystemClock.elapsedRealtimeNanos()}"),
        3 to ProtocolWireValue.StringValue("securityStateChanged"),
        4 to ProtocolWireValue.RecordValue(attachment),
        5 to ProtocolWireValue.UnsignedIntegerValue(0),
        6 to ProtocolWireValue.UnsignedIntegerValue(SystemClock.elapsedRealtime()),
        16 to ProtocolWireValue.StringValue(peerId),
        17 to ProtocolWireValue.StringValue(bondState)
      )
    )
    UnifiedBleProtocolJsiBinding.emitRecord(nativeHandle, ProtocolWireEncoder.encode(event))
  }

  private fun securityFields(peerId: String, bondState: String): Map<Int, ProtocolWireValue> = mapOf(
    16 to ProtocolWireValue.StringValue(peerId),
    17 to ProtocolWireValue.StringValue(bondState)
  )

  private fun terminal(command: ProtocolWireRecord, outcome: String, cause: String? = null): ProtocolWireRecord {
    val fields = mutableMapOf<Int, ProtocolWireValue>(
      1 to ProtocolWireValue.RecordValue(command.requiredRecord(2)),
      2 to ProtocolWireValue.StringValue(outcome)
    )
    if (cause != null) fields[3] = ProtocolWireValue.StringValue(cause)
    return ProtocolWireRecord(RecordKind.TERMINAL, fields)
  }

  private fun databaseSnapshot(database: ProtocolWireRecord, deviceId: String): ProtocolWireRecord {
    val services = mutableListOf<ProtocolWireRecord>()
    val characteristics = mutableListOf<ProtocolWireRecord>()
    val descriptors = mutableListOf<ProtocolWireRecord>()
    val serviceOccurrenceCounts = mutableMapOf<UUID, Int>()
    for (service in radio.services(deviceId)) {
      val serviceOccurrence = nextUuidOccurrence(serviceOccurrenceCounts, service.uuid)
      val servicePath = ProtocolWireRecord(
        RecordKind.SERVICE_PATH,
        mapOf(
          1 to ProtocolWireValue.RecordValue(database),
          2 to ProtocolWireValue.StringValue(service.uuid.toString()),
          3 to ProtocolWireValue.StringValue(serviceOccurrence.toString())
        )
      )
      services.add(servicePath)
      val characteristicOccurrenceCounts = mutableMapOf<UUID, Int>()
      for (characteristic in service.characteristics) {
        val characteristicOccurrence = nextUuidOccurrence(characteristicOccurrenceCounts, characteristic.uuid)
        val characteristicPath = ProtocolWireRecord(
          RecordKind.CHARACTERISTIC_PATH,
          mapOf(
            1 to ProtocolWireValue.RecordValue(servicePath),
            2 to ProtocolWireValue.StringValue(characteristic.uuid.toString()),
            3 to ProtocolWireValue.StringValue(characteristicOccurrence.toString())
          )
        )
        characteristics.add(
          ProtocolWireRecord(
            RecordKind.CHARACTERISTIC_SNAPSHOT,
            mapOf(
              1 to ProtocolWireValue.RecordValue(characteristicPath),
              2 to ProtocolWireValue.BooleanValue(
                (characteristic.properties and BluetoothGattCharacteristic.PROPERTY_READ) != 0
              ),
              3 to ProtocolWireValue.BooleanValue(
                (characteristic.properties and BluetoothGattCharacteristic.PROPERTY_WRITE) != 0
              ),
              4 to ProtocolWireValue.BooleanValue(
                (characteristic.properties and BluetoothGattCharacteristic.PROPERTY_WRITE_NO_RESPONSE) != 0
              ),
              5 to ProtocolWireValue.BooleanValue(
                (characteristic.properties and BluetoothGattCharacteristic.PROPERTY_NOTIFY) != 0
              ),
              6 to ProtocolWireValue.BooleanValue(
                (characteristic.properties and BluetoothGattCharacteristic.PROPERTY_INDICATE) != 0
              )
            )
          )
        )
        val descriptorOccurrenceCounts = mutableMapOf<UUID, Int>()
        for (descriptor in characteristic.descriptors) {
          val descriptorOccurrence = nextUuidOccurrence(descriptorOccurrenceCounts, descriptor.uuid)
          descriptors.add(
            ProtocolWireRecord(
              RecordKind.DESCRIPTOR_PATH,
              mapOf(
                1 to ProtocolWireValue.RecordValue(characteristicPath),
                2 to ProtocolWireValue.StringValue(descriptor.uuid.toString()),
                3 to ProtocolWireValue.StringValue(descriptorOccurrence.toString())
              )
            )
          )
        }
      }
    }
    return ProtocolWireRecord(
      RecordKind.DATABASE_SNAPSHOT,
      mapOf(
        1 to ProtocolWireValue.RecordValue(database),
        2 to ProtocolWireValue.RecordListValue(services),
        3 to ProtocolWireValue.RecordListValue(characteristics),
        4 to ProtocolWireValue.RecordListValue(descriptors)
      )
    )
  }

  private fun characteristicEndpoint(path: ProtocolWireRecord): CharacteristicEndpoint {
    val service = path.requiredRecord(1)
    val database = service.requiredRecord(1)
    val connection = database.requiredRecord(1)
    return CharacteristicEndpoint(
      connection.requiredString(2),
      UUID.fromString(service.requiredString(2)),
      service.requiredString(3).toInt(),
      UUID.fromString(path.requiredString(2)),
      path.requiredString(3).toInt()
    )
  }

  private fun descriptorEndpoint(path: ProtocolWireRecord): DescriptorEndpoint {
    val characteristic = path.requiredRecord(1)
    val endpoint = characteristicEndpoint(characteristic)
    return DescriptorEndpoint(
      endpoint.deviceId,
      endpoint.serviceUuid,
      endpoint.serviceOccurrence,
      endpoint.characteristicUuid,
      endpoint.characteristicOccurrence,
      UUID.fromString(path.requiredString(2)),
      path.requiredString(3).toInt()
    )
  }

  private fun commandEpoch(command: ProtocolWireRecord): Long = command.requiredRecord(2).requiredUnsigned(2)
  private fun commandNonce(command: ProtocolWireRecord): String = command.requiredRecord(2).requiredString(3)
  private fun operationKey(command: ProtocolWireRecord): String = "${commandEpoch(command)}:${commandNonce(command)}"
  private fun isPending(command: ProtocolWireRecord): Boolean = pendingCommands[operationKey(command)] === command

  private fun commandDeviceId(command: ProtocolWireRecord): String? {
    return try {
      when (command.requiredString(3)) {
        "connect", "disconnect", "discover", "readRssi", "requestMtu", "readMtu", "requestPriority" ->
          command.requiredRecord(10).requiredString(2)
        "read", "write", "subscribe", "unsubscribe" -> characteristicEndpoint(command.requiredRecord(4)).deviceId
        "readDescriptor", "writeDescriptor" -> descriptorEndpoint(command.requiredRecord(5)).deviceId
        "securityState", "securityPair", "securityCancelPairing" -> command.requiredString(15)
        else -> null
      }
    } catch (error: IllegalArgumentException) {
      UnifiedBleProtocolJsiBinding.emitDiagnostic(
        nativeHandle,
        "cancellationTargetInvalid",
        error.message ?: "Android cancellation target is invalid"
      )
      null
    }
  }

  private fun connectionRecordForCommand(command: ProtocolWireRecord): ProtocolWireRecord? = try {
    when (command.requiredString(3)) {
      "connect", "disconnect", "discover", "readRssi", "requestMtu", "readMtu", "requestPriority" ->
        command.requiredRecord(10)
      "read", "write", "subscribe", "unsubscribe" ->
        command.requiredRecord(4).requiredRecord(1).requiredRecord(1).requiredRecord(1)
      "readDescriptor", "writeDescriptor" ->
        command.requiredRecord(5).requiredRecord(1).requiredRecord(1).requiredRecord(1).requiredRecord(1)
      else -> null
    }
  } catch (_: IllegalArgumentException) {
    null
  }

  private fun connectionIdentityMatchesRecords(
    candidate: ProtocolWireRecord,
    connection: ProtocolWireRecord
  ): Boolean = try {
    (2..5).all { field -> candidate.requiredString(field) == connection.requiredString(field) }
  } catch (_: IllegalArgumentException) {
    false
  }

  private fun reservationConnectionMatches(
    reservation: DispatchReservation,
    requestedConnection: ProtocolWireRecord
  ): Boolean = try {
    val commandConnection = connectionRecordForCommand(reservation.command) ?: return false
    commandConnection.requiredString(2) == requestedConnection.requiredString(2) &&
      commandConnection.requiredString(3) == requestedConnection.requiredString(3) &&
      reservation.lease == requestedConnection.requiredString(4) &&
      reservation.generation == requestedConnection.requiredString(5)
  } catch (_: IllegalArgumentException) {
    false
  }

  private fun connectionIdentityMatches(command: ProtocolWireRecord, connection: ProtocolWireRecord): Boolean =
    connectionRecordForCommand(command)?.let { candidate ->
      connectionIdentityMatchesRecords(candidate, connection)
    } == true

  private fun databaseConnectionMatches(database: ProtocolWireRecord, connection: ProtocolWireRecord): Boolean = try {
    connectionIdentityMatchesRecords(database.requiredRecord(1), connection)
  } catch (_: IllegalArgumentException) {
    false
  }

  private fun failPendingCommandsForDevice(
    deviceId: String,
    message: String,
    except: ProtocolWireRecord? = null
  ) {
    pendingCommands.values.toList().forEach { command ->
      if (command !== except &&
        command.requiredString(3) != "disconnect" &&
        commandDeviceId(command).equals(deviceId, ignoreCase = true)
      ) {
        emitFailure(command, "connectionLost", message)
      }
    }
  }

  private fun clearSubscriptionRoutesForDevice(deviceId: String) {
    pendingSubscriptions.entries.forEach { entry ->
      if (entry.value.endpoint.deviceId.equals(deviceId, ignoreCase = true)) {
        pendingSubscriptions.remove(entry.key, entry.value)
      }
    }
    activeSubscriptions.entries.forEach { entry ->
      if (entry.value.endpoint.deviceId.equals(deviceId, ignoreCase = true)) {
        activeSubscriptions.remove(entry.key, entry.value)
      }
    }
  }

  private data class CharacteristicEndpoint(
    val deviceId: String,
    val serviceUuid: UUID,
    val serviceOccurrence: Int,
    val characteristicUuid: UUID,
    val characteristicOccurrence: Int
  )

  private data class DescriptorEndpoint(
    val deviceId: String,
    val serviceUuid: UUID,
    val serviceOccurrence: Int,
    val characteristicUuid: UUID,
    val characteristicOccurrence: Int,
    val descriptorUuid: UUID,
    val descriptorOccurrence: Int
  )

  private data class SubscriptionRoute(
    val subscriptionId: String,
    val endpoint: CharacteristicEndpoint,
    val connection: ProtocolWireRecord,
    val mode: String?
  ) {
    fun matches(
      deviceId: String,
      characteristic: BluetoothGattCharacteristic,
      radio: OwnedAndroidGattRadio
    ): Boolean {
      if (!endpoint.deviceId.equals(deviceId, ignoreCase = true)) return false
      val service = characteristic.service ?: return false
      if (service.uuid != endpoint.serviceUuid || characteristic.uuid != endpoint.characteristicUuid) return false
      val serviceOccurrence = radio.services(deviceId)
        .asSequence()
        .filter { candidate -> candidate.uuid == service.uuid }
        .takeWhile { candidate -> candidate !== service }
        .count()
      if (serviceOccurrence != endpoint.serviceOccurrence) return false
      val characteristicOccurrence = service.characteristics
        .asSequence()
        .filter { candidate -> candidate.uuid == characteristic.uuid }
        .takeWhile { candidate -> candidate !== characteristic }
        .count()
      return characteristicOccurrence == endpoint.characteristicOccurrence
    }
  }

  private data class AdapterLossConnectFailure(
    val code: String,
    val message: String
  )

  private fun requirePhyAvailable() {
    check(Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      "Android PHY requires API 26"
    }
  }
}

internal fun connectionLostEvent(
  nativeHandle: Long,
  connection: ProtocolWireRecord,
  status: Int,
  ingressOrdinal: Long,
  monotonicTimestamp: Long
): ProtocolWireRecord {
  val safeMessage = "Android GATT connection lost with status $status"
  val error = ProtocolWireRecord(
    RecordKind.ERROR,
    mapOf(
      1 to ProtocolWireValue.StringValue("connectionLost"),
      2 to ProtocolWireValue.StringValue("android"),
      3 to ProtocolWireValue.StringValue("connection"),
      4 to ProtocolWireValue.StringValue("notRetryable"),
      7 to ProtocolWireValue.StringValue(safeMessage),
      8 to ProtocolWireValue.SignedIntegerValue(status.toLong())
    )
  )

  return ProtocolWireRecord(
    RecordKind.EVENT,
    mapOf(
      1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
      2 to ProtocolWireValue.StringValue("native-connection-lost-$nativeHandle-$ingressOrdinal"),
      3 to ProtocolWireValue.StringValue("connectionLost"),
      4 to ProtocolWireValue.RecordValue(connection.requiredRecord(1)),
      5 to ProtocolWireValue.UnsignedIntegerValue(ingressOrdinal),
      6 to ProtocolWireValue.UnsignedIntegerValue(monotonicTimestamp),
      7 to ProtocolWireValue.RecordValue(connection),
      14 to ProtocolWireValue.RecordValue(error)
    )
  )
}

internal fun androidGattOperationFailureCode(error: Throwable, fallbackCode: String): String {
  val gattFailure = error as? AndroidGattOperationFailure
  return if (gattFailure?.isLinkLoss == true) "connectionLost" else fallbackCode
}

internal fun androidGattOperationFailureStatus(error: Throwable): Int? =
  (error as? AndroidGattOperationFailure)?.gattStatus

internal fun databaseChangedEvent(
  nativeHandle: Long,
  database: ProtocolWireRecord,
  ingressOrdinal: Long,
  monotonicTimestamp: Long
): ProtocolWireRecord {
  val attachment = database.requiredRecord(1).requiredRecord(1)
  return ProtocolWireRecord(
    RecordKind.EVENT,
    mapOf(
      1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
      2 to ProtocolWireValue.StringValue("native-database-changed-$nativeHandle-$ingressOrdinal"),
      3 to ProtocolWireValue.StringValue("databaseChanged"),
      4 to ProtocolWireValue.RecordValue(attachment),
      5 to ProtocolWireValue.UnsignedIntegerValue(ingressOrdinal),
      6 to ProtocolWireValue.UnsignedIntegerValue(monotonicTimestamp),
      8 to ProtocolWireValue.RecordValue(database)
    )
  )
}

internal fun dispatcherResultKindFor(commandKind: String): String = when (commandKind) {
  "scanStart" -> "scanStarted"
  "connect" -> "connected"
  "discover" -> "database"
  "read" -> "read"
  "write" -> "write"
  "readDescriptor" -> "descriptorRead"
  "writeDescriptor" -> "descriptorWrite"
  "readRssi" -> "rssi"
  "requestMtu" -> "mtu"
  "readMtu" -> "mtu"
  "requestPriority" -> "priority"
  "readPhy", "requestPhy" -> "phy"
  "subscribe" -> "subscribed"
  "unsubscribe" -> "unsubscribed"
  "securityState" -> "securityState"
  "securityPair" -> "securityPair"
  "enumerateBondedPeers" -> "bondedPeers"
  "destroy" -> "destroyed"
  else -> "accepted"
}

internal fun bondedPeerResultRecord(
  correlation: ProtocolWireRecord,
  snapshots: List<ProtocolWireRecord>
): ProtocolWireRecord = ProtocolWireRecord(
  RecordKind.RESULT,
  mapOf(
    1 to ProtocolWireValue.UnsignedIntegerValue(NATIVE_PROTOCOL_VERSION.toLong()),
    2 to ProtocolWireValue.StringValue("bondedPeers"),
    3 to ProtocolWireValue.RecordValue(
      ProtocolWireRecord(
        RecordKind.TERMINAL,
        mapOf(
          1 to ProtocolWireValue.RecordValue(correlation),
          2 to ProtocolWireValue.StringValue("succeeded")
        )
      )
    ),
    23 to ProtocolWireValue.RecordListValue(snapshots)
  )
)

internal fun bondedPeerSnapshotRecords(
  snapshots: Iterable<BondedPeerSnapshot>
): List<ProtocolWireRecord> = snapshots.map { peer ->
  ProtocolWireRecord(
    RecordKind.BONDED_PEER_SNAPSHOT,
    buildMap {
      put(1, ProtocolWireValue.StringValue(peer.nativePeerId))
      peer.displayName?.let { name -> put(2, ProtocolWireValue.StringValue(name)) }
    }
  )
}

/** Atomically claim one pending command by operation key and object identity. */
internal fun claimExactPendingCommand(
  pending: ConcurrentHashMap<String, ProtocolWireRecord>,
  key: String,
  command: ProtocolWireRecord
): Boolean {
  var removed = false
  pending.computeIfPresent(key) { _, candidate ->
    if (candidate === command) {
      removed = true
      null
    } else {
      candidate
    }
  }
  return removed
}

private fun ProtocolWireRecord.requiredBoolean(fieldId: Int): Boolean {
  val value = fields[fieldId]
  return if (value is ProtocolWireValue.BooleanValue) value.value else throw IllegalArgumentException("Boolean field is missing")
}

private fun connectionIntent(value: String): ConnectionIntents {
  return when (value) {
    "direct" -> ConnectionIntents.DIRECT
    "whenAvailable" -> ConnectionIntents.WHEN_AVAILABLE
    else -> throw IllegalArgumentException("Connection intent is invalid")
  }
}

private fun ProtocolWireRecord.requiredSignedInteger(fieldId: Int): Long {
  val value = fields[fieldId]
  return if (value is ProtocolWireValue.SignedIntegerValue) value.value else throw IllegalArgumentException("Signed field is missing")
}

private fun ProtocolWireRecord.requiredStringList(fieldId: Int): List<String> {
  val value = fields[fieldId]
  return if (value is ProtocolWireValue.StringListValue) value.value else throw IllegalArgumentException("String list field is missing")
}

private fun ProtocolWireRecord.optionalStringList(fieldId: Int): List<String> {
  val value = fields[fieldId] ?: return emptyList()
  return if (value is ProtocolWireValue.StringListValue) value.value else throw IllegalArgumentException("String list field is malformed")
}

private fun Boolean?.toNativeConnectableState(): Int = when (this) {
  true -> 1
  false -> 0
  null -> -1
}
