// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/RustRadioHostAdapter.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceControlException
import com.sfourdrinier.unifiedblemanager.presence.PresencePort
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattLinkLost
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattNotSubmitted
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattOperationFailure
import com.sfourdrinier.unifiedblemanager.radio.GattConnectAttempt
import com.sfourdrinier.unifiedblemanager.radio.OrderedRegistrationStale
import com.sfourdrinier.unifiedblemanager.radio.OrderedOpenNotCreated
import com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancelled
import com.sfourdrinier.unifiedblemanager.radio.AndroidNativeGraphIdentity
import com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedOpenSlots
import com.sfourdrinier.unifiedblemanager.radio.OrderedLocalCleanupStatus
import com.sfourdrinier.unifiedblemanager.radio.OrderedEnableNotApplied
import com.ubm.core.MobileCoreBridge
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executor
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import java.util.UUID

/**
 * The Android platform radio of the process-owned Rust mobile host
 * (`MobileCoreBridge.RadioHost`, `docs/MOBILE_RUST_WIRE.md`).
 *
 * Rust asks, this adapter drives the OS through [AndroidRadioPort] and
 * answers every request exactly once with one `nativeComplete*`. OS facts
 * enter Rust through `nativeIngest*`. No decision about the BLE contract is
 * taken here: the adapter translates, and reports what the OS did.
 *
 * Threading: `RadioHost` methods are called from Rust executor threads and
 * return at once; work runs on [radioExecutor] (GATT/scan) or
 * [serviceExecutor] (foreground-service promotion and the companion
 * chooser, which may block). Driver callbacks arrive on the main/binder
 * threads and only touch concurrent state.
 *
 * Every non-zero status the host returns (late or mismatched completion,
 * dropped ingress, host closed, no host) is counted in [statusCounts] and
 * logged — never discarded silently.
 */
class RustRadioHostAdapter internal constructor(
  private val core: MobileCorePort,
  private val radio: AndroidRadioPort,
  private val background: BackgroundPort,
  private val companion: () -> CompanionPort?,
  private val presence: () -> PresencePort?,
  private val radioExecutor: Executor,
  private val serviceExecutor: Executor,
  private val log: (String) -> Unit
) : MobileCoreBridge.RadioHost {
  // A v4 fence has to linearize with the transition from the owner-wide
  // coordinator to the legacy slot ledger.  Neither ledger can make that
  // transition atomic by itself: a callback admitted by the coordinator must
  // reserve its slot work before a later fence closes the slots, while a
  // callback after the fence must not reserve either ledger.
  private val ownerAdmissionLock = Any()
  private data class OwnerControlAdmission(
    val slots: AndroidOrderedOpenSlots,
    val coordinator: AndroidOwnerQuiescenceCoordinator?,
    val v4: Boolean,
    val rejection: String?
  )

  private data class OwnerCallbackAdmission(
    val slots: AndroidOrderedOpenSlots?,
    val work: AndroidOrderedOpenSlots.EffectWork?,
    val quiescence: AndroidOwnerQuiescenceCoordinator.Lease?,
    val begun: Boolean = false
  )

  private data class OwnerOrderedOpenAdmission(
    val slots: AndroidOrderedOpenSlots,
    val work: AndroidOrderedOpenSlots.AdapterWork,
    val quiescence: AndroidOwnerQuiescenceCoordinator.Lease?
  )
  override fun acceptBindingRoute(version: Int, binding: Long): Boolean =
    (core as? JniBoundMobileCorePort)?.acceptBindingRoute(version, binding) ?: false

  override fun orderedAdmissionOwner(version: Int): String =
    (radio as? OrderedGroupPort)?.orderedAdmissionOwner(version) ?: ""

  override fun ownerFence(requestId: Long, ownerIncarnation: String, retirementId: String) =
    ownerControl(requestId, ownerIncarnation, retirementId, true)

  override fun ownerStatus(requestId: Long, ownerIncarnation: String, retirementId: String) =
    ownerControl(requestId, ownerIncarnation, retirementId, false)

  private fun ownerControl(requestId: Long, owner: String, retirement: String, fence: Boolean) {
    inFlight.add(requestId)
    try {
      fun identity(value: String): UUID {
        val parsed = UUID.fromString(value)
        require(parsed.toString() == value && parsed.version() == 4 && parsed.variant() == 2) { "invalid owner retirement identity" }
        return parsed
      }
      val ownerId = identity(owner)
      val retirementId = identity(retirement)
      val orderedPort = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
      val control = synchronized(ownerAdmissionLock) {
        val slots = orderedPort.orderedSlots()
        val coordinator = orderedPort.ownerQuiescenceCoordinator()
        val v4 = coordinator?.owner() == ownerId
        val rejection = if (v4) {
          val coordinatorRejection = if (fence) coordinator.fence(ownerId, retirementId)
            else if (coordinator.status(ownerId, retirementId) == null) "conflicting-retirement" else null
          if (coordinatorRejection == null) slots.ownerControl(slots.incarnation, retirementId, fence) else coordinatorRejection
        } else slots.ownerControl(ownerId, retirementId, fence)
        OwnerControlAdmission(slots, coordinator, v4, rejection)
      }
      val slots = control.slots
      val coordinator = control.coordinator
      val v4 = control.v4
      val rejection = control.rejection
      if (rejection == null && fence) {
        synchronized(scanControlLock) { scanOwnerFenced = true }
        (radio as? OwnerGraphFencePort)?.fenceGraphOwner()
        val servicePort = background as? ForegroundServiceBackgroundPort
        check(servicePort == null || servicePort.fence(retirement)) { "conflicting background retirement" }
        synchronized(backgroundOwnershipLock) { backgroundRetirement = retirement }
      }
      val previousScanFailure = synchronized(scanControlLock) { scanCleanupFailure }
      // An inline retry can clear the registry before this status is assembled.
      // Keep failures already observed by the status that initiated that retry.
      val previousService = backgroundOwnershipSnapshot()
      if (rejection == null) scheduleRetiredScanCleanup()
      if (rejection == null) scheduleRetiredBackgroundCleanup()
      val scanFailure = previousScanFailure ?: synchronized(scanControlLock) { scanCleanupFailure }
      val slotStatus = if (rejection == null) slots.ownerStatus(if (v4) slots.incarnation else ownerId, retirementId) else null
      // Admitted effects can still need the bond/adapter receivers to settle.
      val receiverRetirement = if (slotStatus != null && slotStatus.executing == 0 &&
        slotStatus.adapterWork == 0 && slotStatus.effectWork == 0 &&
        (!v4 || coordinator?.status(ownerId, retirementId)?.inFlight == 0))
        (radio as? OwnerReceiverRetirementPort)?.retireReceivers() else null
      val result = MobileCoreBridge.OwnerRetirementResult().apply {
        ownerIncarnation = owner
        this.retirementId = retirement
        if (rejection != null) { state = "rejected"; reason = rejection }
        else {
          val quiescence = coordinator?.status(ownerId, retirementId)
          val graph = (radio as? OwnerGraphFencePort)?.graphOwnerStatus()
          val store = (radio as? OwnerOrderedStoreSnapshotPort)?.orderedStoreSnapshot()
          val service = backgroundOwnershipSnapshot()
          val connectFailures = synchronized(connectGuard) {
            pendingConnects.values.flatMap { it.cleanupFailures } +
              uncertainConnectCleanupFailures.values + pendingConnectFailureDeliveries.values + connectDeliveryFailures.toList()
          }
          val cleanupFailures = (listOfNotNull(scanFailure) + connectFailures + (receiverRetirement?.failures ?: emptyList()) +
            previousService.acquireFailures.map { it.second } + previousService.releaseFailures.values.flatten() + previousService.registryFailures +
            service.acquireFailures.map { it.second } + service.releaseFailures.values.flatten() + service.registryFailures).distinct()
          failures = cleanupFailures.map { error ->
            val failure = classify(error)
            MobileCoreBridge.OrderedResult().apply {
              failureKind = failure.kind.wire
              detail = failure.detail
              gattStatus = failure.gattStatus ?: MobileCoreBridge.ABSENT_INT
              nativeName = failure.nativeCode
              dispatched = failure.dispatched
            }
          }.toTypedArray()
          // This status request itself is in-flight until its result crosses
          // the core boundary; it cannot make an otherwise idle owner pending.
          val localWork = inFlight.count { it != requestId } + pendingAssociations.size + enablements.size +
            synchronized(connectGuard) { pendingConnects.size + establishedConnections.size } +
            synchronized(scanControlLock) { scanStarting + if (scanMayOwn) 1 else 0 }
          val slotsBusy = slotStatus != null && (slotStatus.executing != 0 || slotStatus.adapterWork != 0 || slotStatus.effectWork != 0 || slotStatus.terminal != 0)
          val storeBusy = store != null && (store.openAdmissions != 0 || store.routes != 0 || store.installed != 0 ||
            store.batches != 0 || store.markerReceipts != 0 || store.removalReceipts != 0 || store.uncertainRemovals != 0)
          val graphBusy = graph != null && (graph.callsInFlight != 0 || graph.authorities != 0 || graph.authorityReservations != 0 ||
            graph.authorityOpenReceipts != 0 || graph.permitReservations != 0 || graph.permitReceipts != 0 || graph.admissionGates != 0 ||
            graph.activePermits != 0 || graph.deferredCallbacks != 0 || graph.drainingGates != 0 || graph.discoveryWork != 0 ||
            graph.nativeGraphs != 0 || graph.detachedSources != 0)
          val coordinatorBusy = quiescence != null && quiescence.inFlight != 0
          val backgroundBusy = service.inFlight != 0 || service.leases.isNotEmpty() || service.pendingStartCleanup
          val receiverBusy = receiverRetirement?.callbacks ?: 0
          val v4Quiescent = v4 && quiescence != null && quiescence.fenced && !slotsBusy && !storeBusy && !graphBusy &&
            !coordinatorBusy && localWork == 0 && !backgroundBusy && receiverBusy == 0 && cleanupFailures.isEmpty()
          if (v4Quiescent) {
            state = "quiescent"
            outstandingClasses = emptyArray()
          } else {
            state = "fenced-pending"
            outstandingClasses = arrayOf("attachments", "reservations", "native-work", "callbacks", "cleanup")
          }
        }
      }
      answer(requestId, "owner-retirement") { core.completeOwnerRetirement(requestId, result) }
    } catch (error: Throwable) { fail(requestId, error, dispatched = false) }
  }

  private fun slotIdentity(input: MobileCoreBridge.OrderedOpenIdentity): AndroidOrderedOpenSlots.Identity {
    val owner = UUID.fromString(input.owner)
    require(owner.toString() == input.owner && owner.version() == 4) { "invalid ordered owner incarnation" }
    val size = input.members.size
    require(size > 0 && input.peers.size == size && input.serviceUuids.size == size && input.services.size == size &&
      input.characteristicUuids.size == size && input.characteristics.size == size) { "invalid ordered member descriptor" }
    require(input.connection.isNotEmpty() && input.database.isNotEmpty() && input.peer.isNotEmpty())
    val members = input.members.indices.map { i ->
      AndroidOrderedOpenSlots.Member(input.members[i].toULong(), input.peers[i], input.serviceUuids[i],
        input.services[i].toULong(), input.characteristicUuids[i], input.characteristics[i].toULong())
    }
    require(members.map { it.memberId }.toSet().size == size) { "duplicate ordered member ID" }
    return AndroidOrderedOpenSlots.Identity(input.version, owner, input.slot, input.generation.toULong(),
      input.original.toULong(), input.group.toULong(), input.peer,
      AndroidOrderedOpenSlots.Spec(input.connection, input.database,
        AndroidOrderedOpenSlots.Snapshot(input.snapshot, input.attachment.toULong(), input.revision.toULong()),
        members, input.itemCapacity.toUInt(), input.byteCapacity.toUInt()))
  }

  private fun slotResult(observation: AndroidOrderedOpenSlots.Observation): MobileCoreBridge.OrderedResult {
    val result = MobileCoreBridge.OrderedResult().apply {
      kind = "open-result"; ownerIncarnation = observation.slot.ownerIncarnation.toString()
      slot = observation.slot.slot; slotGeneration = observation.slot.generation.toLong()
    }
    fun opened(value: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner.Opened) {
      result.generationKind = "counter"; result.generationValue = value.generation.toString()
      result.members = value.members.map { OrderedGroupBridgeMapper.registration(OrderedPortRegistration(it.memberId, it.sourceMember, it.epoch)) }.toTypedArray()
    }
    fun failure(error: Throwable) {
      val value = classifyOrderedOpenError(error)
      result.failureKind = value.kind.wire; result.gattStatus = value.gattStatus ?: MobileCoreBridge.ABSENT_INT
      result.detail = value.detail; result.dispatched = value.dispatched; result.nativeName = value.nativeCode
      value.gattStatus?.let { result.nativeDomain = "android"; result.hasNativeCode = true; result.nativeCode = it.toLong() }
    }
    when (val value = observation.result) {
      AndroidOrderedOpenSlots.Result.Pending -> result.status = "pending"
      is AndroidOrderedOpenSlots.Result.Opened -> { result.status = "opened"; opened(value.opened) }
      is AndroidOrderedOpenSlots.Result.Failed -> {
        result.status = "failed"; failure(value.error)
        when (val ownership = value.ownership) {
          AndroidOrderedOpenSlots.Ownership.NotCreated -> result.ownershipKind = "not-created"
          is AndroidOrderedOpenSlots.Ownership.Created -> { result.ownershipKind = "created"; opened(ownership.opened) }
        }
      }
      is AndroidOrderedOpenSlots.Result.Indeterminate -> { result.status = "indeterminate"; result.reasonKind = "failure"; failure(value.reason) }
    }
    return result
  }

  private fun slotControl(requestId: Long, action: () -> MobileCoreBridge.OrderedResult) {
    inFlight.add(requestId)
    try {
      val result = action()
      answer(requestId, "ordered-slot") { core.completeOrdered(requestId, result) }
    } catch (error: Throwable) { fail(requestId, error, dispatched = false) }
  }

  override fun orderedRecoverOpenV2(requestId: Long, identity: MobileCoreBridge.OrderedOpenIdentity) = slotControl(requestId) {
    val slots = (radio as? OrderedGroupPort ?: error("ordered source port is unavailable")).orderedSlots()
    slotResult(slots.fence(slotIdentity(identity)))
  }

  override fun orderedAcknowledgeOpenV2(requestId: Long, identity: MobileCoreBridge.OrderedOpenIdentity) = slotControl(requestId) {
    val slots = (radio as? OrderedGroupPort ?: error("ordered source port is unavailable")).orderedSlots()
    val retired = slots.acknowledge(slotIdentity(identity))
    MobileCoreBridge.OrderedResult().apply {
      kind = "open-retired"; ownerIncarnation = retired.slot.ownerIncarnation.toString()
      slot = retired.slot.slot; slotGeneration = retired.slot.generation.toLong()
    }
  }

  override fun orderedOpenV2(requestId: Long, identity: MobileCoreBridge.OrderedOpenIdentity) {
    inFlight.add(requestId)
    val port: OrderedGroupPort
    val slots: AndroidOrderedOpenSlots
    val descriptor: AndroidOrderedOpenSlots.Identity
    val work: AndroidOrderedOpenSlots.AdapterWork
    val quiescence: AndroidOwnerQuiescenceCoordinator.Lease?
    try {
      require(requestId > 0 && requestId.toULong() == identity.original.toULong()) { "invalid original ordered request" }
      port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
      descriptor = slotIdentity(identity)
      val admission = admitOrderedOpen(port, descriptor)
      if (admission == null) {
        rejectRetiredOwnerRequest(requestId)
        return
      }
      slots = admission.slots
      work = admission.work
      quiescence = admission.quiescence
    } catch (error: Throwable) { fail(requestId, error, dispatched = false); return }
    check(quiescence == null || quiescenceWork.putIfAbsent(requestId, quiescence) == null) { "duplicate owner request" }
    orderedOpenRequests.add(requestId)
    val task = Runnable {
      slots.startAdapterWork(work)
      try {
        val permit = try { slots.begin(descriptor) } catch (error: Throwable) { fail(requestId, error, dispatched = false); return@Runnable }
        if (permit == null) {
          try { val current = slots.observe(descriptor); answer(requestId, "ordered-slot") { core.completeOrdered(requestId, slotResult(current)) } }
          catch (error: Throwable) { fail(requestId, error, dispatched = false) }
          return@Runnable
        }
        val outcome = try {
          port.orderedOpenSlot(descriptor, onWakeFailure = { generation, group, _ ->
            relayOrderedSourceCallback("ordered-wake-failure") {
              val status = core.ingestOrderedWakeFailure(generation.toLong(), group.toLong())
              check(status == MobileCoreBridge.STATUS_ACCEPTED) { "ordered source failure signal was rejected ($status)" }
            }
          }) { generation, group ->
            relayOrderedSourceCallback("ordered-wake") {
              val status = core.ingestOrderedWake(generation.toLong(), group.toLong())
              check(status == MobileCoreBridge.STATUS_ACCEPTED) { "ordered source wake was rejected ($status)" }
            }
          }
        } catch (error: Throwable) {
          AndroidOrderedOpenSlots.Result.Indeterminate(error)
        }
        slots.complete(permit, outcome)
        try { val current = slots.observe(descriptor); answer(requestId, "ordered-slot") { core.completeOrdered(requestId, slotResult(current)) } }
        catch (error: Throwable) { fail(requestId, error, dispatched = false) }
      } finally {
        slots.finishAdapterWork(work)
      }
    }
    try { radioExecutor.execute(task) } catch (error: Throwable) {
      try {
        slots.rejectBeforeExecution(descriptor, AndroidGattNotSubmitted("Android radio executor refused the request: ${detailOf(error)}"))
        try { val current = slots.observe(descriptor); answer(requestId, "ordered-slot") { core.completeOrdered(requestId, slotResult(current)) } }
        catch (failure: Throwable) { fail(requestId, failure, dispatched = false) }
      } finally {
        if (error is RejectedExecutionException) slots.rejectAdapterWork(work)
      }
    }
  }

  private data class InstanceKey(
    val peer: String,
    val service: String,
    val serviceOccurrence: Long,
    val characteristic: String,
    val characteristicOccurrence: Long
  )

  private class Enablement(val instance: CharacteristicInstance, val epoch: Long, val connection: EstablishedConnection) {
    @Volatile var databaseInvalidated = false
  }
  private data class EnablementKey(val instance: InstanceKey, val epoch: Long)
  private data class OrderedActivationKey(
    val instance: CharacteristicInstance, val connection: String, val generation: ULong,
    val group: ULong, val member: OrderedPortRegistration, val epoch: Long
  )
  private sealed interface OrderedActivationState {
    data object Pending : OrderedActivationState
    data class Enabled(val delivery: String) : OrderedActivationState
    data class Uncertain(val failure: RadioFailure, val localRollbackEligible: Boolean = false) : OrderedActivationState
    data class Released(val failure: RadioFailure?) : OrderedActivationState
  }
  private val orderedActivationLock = Any()
  private val orderedActivations = mutableMapOf<OrderedActivationKey, OrderedActivationState>()

  private fun retryOrderedLocalRelease(key: OrderedActivationKey, failure: RadioFailure): OrderedActivationState {
    val port = radio as? OrderedGroupPort ?: return OrderedActivationState.Uncertain(failure)
    return when (port.orderedRetryLocalCleanup(key.generation, key.group, key.instance, key.epoch)) {
      OrderedLocalCleanupStatus.RELEASED -> {
        val physicalKey = EnablementKey(instanceKey(key.instance), key.epoch)
        enablements[physicalKey]?.let { owner ->
          if (owner.connection.expectedGeneration == key.connection) enablements.remove(physicalKey, owner)
        }
        OrderedActivationState.Released(failure)
      }
      OrderedLocalCleanupStatus.PENDING, OrderedLocalCleanupStatus.UNKNOWN -> OrderedActivationState.Uncertain(failure)
    }
  }

  /**
   * A connect request and the driver token it handed to `radio.connect`. The token, not the peer
   * address, ties a link observation to the request: a prior generation's loss for the same peer
   * carries a different (or no) token and can never settle this request. Identity-compared, so a
   * release or claim removes exactly this entry and never a later request's.
   */
  private enum class PendingConnectState { DISPATCHING, RETURNED, WITHDRAWN, CLEANING, CLEANUP_FAILED }

  private class PendingConnect(
    val peerId: String,
    val requestId: Long,
    val attempt: GattConnectAttempt,
    val expectedGeneration: String,
    val ownerWork: OwnerRequestWork?
  ) {
    var state = PendingConnectState.DISPATCHING
    val disconnectRequests = mutableListOf<Long>()
    var established: EstablishedConnection? = null
    var outcomeObserved = false
    val cleanupFailures = mutableListOf<Throwable>()
    var cleanupWork: ConnectCleanupWork? = null
    var cleanupAttempt: ConnectCleanupAttempt? = null
    var cleanupCallbacks = 0
    var cleanupLastSucceeded = false
  }

  private data class EstablishedConnection(val attempt: GattConnectAttempt, val expectedGeneration: String)

  private sealed interface ConnectionTransition {
    data class Pending(val value: PendingConnect) : ConnectionTransition
    data class Established(val value: EstablishedConnection) : ConnectionTransition
  }

  private val inFlight = ConcurrentHashMap.newKeySet<Long>()
  /** Version-4 work is admitted before any executor/native boundary and released only after its exact answer. */
  private val quiescenceWork = ConcurrentHashMap<Long, AndroidOwnerQuiescenceCoordinator.Lease>()
  /** Both dispatch and the foreign completion must return before an effect can retire. */
  private class OwnerRequestWork(val slots: AndroidOrderedOpenSlots, val work: AndroidOrderedOpenSlots.EffectWork) {
    private var dispatchReturned = false
    private var completionReturned = false
    private var callbacks = 0
    private var released = false
    @Synchronized fun beginCallback(): Boolean {
      if (released) return false
      callbacks++
      return true
    }
    @Synchronized fun endCallback() { check(callbacks > 0); callbacks--; releaseIfDone() }
    @Synchronized fun dispatchDone() { dispatchReturned = true; releaseIfDone() }
    @Synchronized fun completionDone() { completionReturned = true; releaseIfDone() }
    private fun releaseIfDone() {
      if (!released && dispatchReturned && completionReturned && callbacks == 0) {
        released = true
        slots.finishEffectWork(work)
      }
    }
  }
  /** Retains the exact native obligation across retries, dispatch and foreign completions. */
  private class ConnectCleanupWork(private val request: OwnerRequestWork) {
    private var dispatches = 0
    private var callbacks = 0
    private var succeeded = false
    private var released = false
    @Synchronized fun beginAttempt() { check(!released); dispatches++; callbacks++ }
    // A later failure supersedes an earlier retry's success until another success reconciles it.
    @Synchronized fun observeOutcome(success: Boolean) { succeeded = success }
    @Synchronized fun callbackDone() { check(callbacks > 0); callbacks--; releaseIfDone() }
    @Synchronized fun dispatchDone() { check(dispatches > 0); dispatches--; releaseIfDone() }
    private fun releaseIfDone() {
      if (!released && succeeded && dispatches == 0 && callbacks == 0) {
        released = true
        request.endCallback()
      }
    }
  }
  private class ConnectCleanupAttempt { var callbackClaimed = false }
  // A throw after dispatch may still be followed by its callback, even after a retry succeeds.
  private val uncertainConnectCleanupFailures = mutableMapOf<ConnectCleanupAttempt, Throwable>()
  private val ownerRequestWork = ConcurrentHashMap<Long, OwnerRequestWork>()
  internal data class BackgroundOwnershipSnapshot(
    val ownerIncarnation: UUID?, val leases: Set<String>, val inFlight: Int,
    val releaseFailures: Map<String, List<Throwable>>, val acquireFailures: List<Pair<Long, Throwable>>,
    val unknownOwnership: Boolean, val pendingStartCleanup: Boolean = false, val pendingStartId: String? = null,
    val registryFailures: List<Throwable> = emptyList()
  )
  private val backgroundOwnershipLock = Any()
  private val backgroundLeases = mutableSetOf<String>()
  private val backgroundReleaseFailures = mutableMapOf<String, MutableList<Throwable>>()
  private val backgroundAcquireFailures = mutableListOf<Pair<Long, Throwable>>()
  // BackgroundPort does not expose the shared registry's per-owner state or
  // pending start cleanup. Local receipts cannot certify that state is empty.
  private var backgroundUnknownOwnership = true
  private val backgroundRequests = mutableSetOf<Long>()
  private var backgroundRetirement: String? = null
  private var backgroundCleanupScheduled = false
  private var backgroundCleanupSchedulingFailure: Throwable? = null
  internal fun backgroundOwnershipSnapshot() = synchronized(backgroundOwnershipLock) {
    val owner = (radio as? OrderedGroupPort)?.orderedSlots()?.incarnation
    val authoritative = (background as? ForegroundServiceBackgroundPort)?.snapshot()
    // Registry work can be admitted through the same port outside this adapter.
    // These counts can overlap; both must be zero before ownership is clear.
    BackgroundOwnershipSnapshot(owner, authoritative?.leases ?: backgroundLeases.toSet(),
      backgroundRequests.size + (if (backgroundCleanupScheduled) 1 else 0) + (authoritative?.inFlight ?: 0),
      if (authoritative == null) backgroundReleaseFailures.mapValues { it.value.toList() } else emptyMap(),
      backgroundAcquireFailures.toList(),
      (authoritative == null && backgroundUnknownOwnership) || owner == null,
      authoritative?.pendingStartCleanup ?: false,
      authoritative?.pendingStartId,
      (authoritative?.failures ?: emptyList()) + listOfNotNull(backgroundCleanupSchedulingFailure))
  }
  private fun scheduleRetiredBackgroundCleanup() {
    val port = background as? ForegroundServiceBackgroundPort ?: return
    val retirement = synchronized(backgroundOwnershipLock) {
      val id = backgroundRetirement
      if (id == null || backgroundRequests.isNotEmpty() || backgroundCleanupScheduled) null
      else {
        val snapshot = port.snapshot()
        if (snapshot.inFlight != 0 || (snapshot.leases.isEmpty() && !snapshot.pendingStartCleanup)) null
        else { backgroundCleanupScheduled = true; id }
      }
    } ?: return
    try {
      serviceExecutor.execute {
        try { port.retry(retirement) }
        catch (error: Throwable) { log("retired Android background cleanup remains retryable: ${detailOf(error)}") }
        finally { synchronized(backgroundOwnershipLock) { backgroundCleanupScheduled = false } }
      }
      synchronized(backgroundOwnershipLock) { backgroundCleanupSchedulingFailure = null }
    } catch (error: RejectedExecutionException) {
      synchronized(backgroundOwnershipLock) { backgroundCleanupScheduled = false; backgroundCleanupSchedulingFailure = error }
    }
  }
  private inner class BackgroundRequestWork(private val requestId: Long) {
    private var dispatchReturned = false
    private var completionReturned = false
    @Synchronized fun dispatchDone() { dispatchReturned = true; releaseIfDone() }
    @Synchronized fun completionDone() { completionReturned = true; releaseIfDone() }
    private fun releaseIfDone() {
      if (dispatchReturned && completionReturned) {
        synchronized(backgroundOwnershipLock) { backgroundRequests.remove(requestId) }
        scheduleRetiredBackgroundCleanup()
      }
    }
  }
  private val backgroundRequestWork = ConcurrentHashMap<Long, BackgroundRequestWork>()
  private val orderedEnableRequests = ConcurrentHashMap.newKeySet<Long>()
  private val orderedEnableKeys = ConcurrentHashMap<Long, OrderedActivationKey>()
  private val orderedOpenRequests = ConcurrentHashMap.newKeySet<Long>()
  private data class OpenReservation(val port: OrderedGroupPort, val admission: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner.OpenAdmission)
  private val orderedOpenReservations = ConcurrentHashMap<Long, OpenReservation>()
  /**
   * A legacy open is queued before it has a native-owner admission.  Recovery
   * must be able to fence that exact queued request without inventing a
   * native receipt, while a real owner fence must still prevent the queued
   * task from creating an admission later.
   */
  private data class LegacyOpenIntent(val group: ULong, val request: ULong, val peer: String, var fenced: Boolean = false)
  private val legacyOpenIntentLock = Any()
  private val legacyOpenIntents = ConcurrentHashMap<Long, LegacyOpenIntent>()
  private val markerCancellations = ConcurrentHashMap<Long, com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation>()
  private data class MarkerReservation(val port: OrderedGroupPort, val generation: ULong, val group: ULong, val marker: ULong, val created: Boolean)
  private val markerReservations = ConcurrentHashMap<Long, MarkerReservation>()
  /** A write that is queued but has not yet created its source receipt. */
  private data class QueuedMarkerIntent(
    val request: Long, val generationKind: String, val generationValue: String, val group: Long, val marker: Long,
    val connection: String, val snapshotOwner: String, val snapshotAttachment: Long, val snapshotRevision: Long,
    val peer: String, val service: String, val serviceOccurrence: Long, val characteristic: String,
    val characteristicOccurrence: Long, val value: ByteArray, val withResponse: Boolean,
    val cancellation: com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation,
    var fencedBeforeAdmission: Boolean = false
  )
  private val queuedMarkerIntentLock = Any()
  private val queuedMarkerIntents = ConcurrentHashMap<Long, QueuedMarkerIntent>()
  private val cancelRequested = ConcurrentHashMap.newKeySet<Long>()
  private val started = ConcurrentHashMap.newKeySet<Long>()
  private val driverOperations = ConcurrentHashMap<Long, Long>()
  private val pendingConnects = ConcurrentHashMap<String, PendingConnect>()
  /** Guards pending/established connect ownership transitions, never native or core calls. */
  private val connectGuard = Any()
  private val connectDeliveryFailures = mutableListOf<Throwable>()
  // A retry can finish while a previous failure completion is still in foreign code.
  private val pendingConnectFailureDeliveries = mutableMapOf<Long, Throwable>()
  private val establishedConnections = HashMap<String, EstablishedConnection>()
  private val fencedAttempts = mutableSetOf<GattConnectAttempt>()
  private val pendingAssociations = ConcurrentHashMap<Long, Pair<CompanionPort, (Result<CompanionAssociation>) -> Unit>>()
  private val enablements = ConcurrentHashMap<EnablementKey, Enablement>()
  private val counts = ConcurrentHashMap<String, AtomicLong>()
  private val scanControlLock = Any()
  private var scanOwnerFenced = false
  private var scanStarting = 0
  private var scanMayOwn = false
  private var scanCleanupScheduled = false
  private var scanRevision = 0L
  private var scanCleanupFailure: Throwable? = null

  /** Keep uncertain cleanup for an exact retry on a later retirement observation. */
  private fun scheduleRetiredScanCleanup() {
    val schedule = synchronized(scanControlLock) {
      if (!scanOwnerFenced || !scanMayOwn || scanStarting != 0 || scanCleanupScheduled) false
      else { scanCleanupScheduled = true; true }
    }
    if (!schedule) return
    try {
      radioExecutor.execute {
        val revision = synchronized(scanControlLock) { scanRevision }
        val failure = try { radio.stopScan() } catch (error: Throwable) { error }
        val retryForNewerScan = synchronized(scanControlLock) {
          if (scanRevision == revision) {
            scanCleanupFailure = failure
            if (failure == null) scanMayOwn = false
          }
          scanCleanupScheduled = false
          failure == null && scanRevision != revision && scanMayOwn && scanStarting == 0
        }
        if (failure != null) log("retired Android scan cleanup remains retryable: ${detailOf(failure)}")
        if (retryForNewerScan) scheduleRetiredScanCleanup()
      }
    } catch (error: RejectedExecutionException) {
      synchronized(scanControlLock) { scanCleanupScheduled = false; scanCleanupFailure = error }
      log("retired Android scan cleanup executor refused the request: ${detailOf(error)}")
    }
  }

  init {
    radio.setEvents(Events())
  }

  /** Non-delivered host statuses and adapter-side suppressions, by label. */
  fun statusCounts(): Map<String, Long> = counts.mapValues { it.value.get() }

  // -- requests -----------------------------------------------------------

  override fun orderedRecoverOpen(requestId: Long, groupId: Long, openRequestId: Long, peerId: String) = perform(requestId) {
    val port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
    val recovery = synchronized(legacyOpenIntentLock) {
      val intent = legacyOpenIntents[openRequestId]
      if (intent != null && intent.group == groupId.toULong() && intent.request == openRequestId.toULong() &&
        intent.peer == peerId.uppercase() && !orderedOpenReservations.containsKey(openRequestId)) {
        // No native admission exists yet.  This exact recovery is the fence
        // for the queued legacy effect, so its later task reports not-created.
        intent.fenced = true
        port.orderedFenceQueuedOpen(groupId.toULong(), openRequestId.toULong(), peerId)
      } else {
        port.orderedRecoverOpenExact(groupId.toULong(), openRequestId.toULong(), peerId)
      }
    }
    val result = orderedRecoveryResult(recovery, groupId)
    answer(requestId, "ordered-recover-open") { core.completeOrdered(requestId, result) }
  }

  override fun orderedAcknowledgeOpen(requestId: Long, version: Int, groupId: Long, openRequestId: Long, peerId: String) = perform(requestId) {
    require(version == 1) { "unsupported ordered open acknowledgement version" }
    val port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
    port.orderedAcknowledgeOpen(groupId.toULong(), openRequestId.toULong(), peerId)
    answer(requestId, "ordered-acknowledge-open") { core.completeUnit(requestId) }
  }

  private fun orderedRecoveryResult(recovery: OrderedPortOpenRecovery, group: Long): MobileCoreBridge.OrderedResult = when (recovery) {
    is OrderedPortOpenRecovery.Created -> OrderedGroupBridgeMapper.opened(recovery.opened)
    is OrderedPortOpenRecovery.Failed -> orderedOpenFailure(classifyOrderedOpenError(recovery.error), recovery.ownership, group)
    else -> orderedOpenFailure(RadioFailure(RadioFailureKind.PLATFORM, null, "ordered open ownership is not available", false), recovery, group)
  }

  private fun classifyOrderedOpenError(error: Throwable): RadioFailure =
    when (error) {
      is AndroidOrderedOpenSlots.Fenced -> RadioFailure(RadioFailureKind.CANCELLED, null, detailOf(error), dispatched = false)
      is OrderedOpenNotCreated -> error.cause?.let { classify(it).copy(dispatched = false) } ?: classify(error).copy(dispatched = false)
      else -> classify(error)
    }

  private fun orderedOpenFailure(failure: RadioFailure, recovery: OrderedPortOpenRecovery, recoveryKey: Long) = MobileCoreBridge.OrderedResult().apply {
    kind = "open-failed"; failureKind = failure.kind.wire; gattStatus = failure.gattStatus ?: MobileCoreBridge.ABSENT_INT
    detail = failure.detail; dispatched = failure.dispatched; nativeName = failure.nativeCode
    when (recovery) {
      OrderedPortOpenRecovery.NotCreated -> ownershipKind = "not-created"
      OrderedPortOpenRecovery.Indeterminate -> { ownershipKind = "indeterminate"; this.recoveryKey = recoveryKey }
      is OrderedPortOpenRecovery.Created -> {
        ownershipKind = "created"; generationKind = "counter"; generationValue = recovery.opened.generation.toString()
        members = recovery.opened.members.map(OrderedGroupBridgeMapper::registration).toTypedArray()
      }
      is OrderedPortOpenRecovery.Failed -> error("nested ordered open failure")
    }
  }

  override fun orderedEnable(requestId: Long, operation: String, peerId: String, serviceUuid: String,
                             serviceOccurrence: Long, characteristicUuid: String, characteristicOccurrence: Long,
                             connectionGeneration: String, generationKind: String, generationValue: String,
                             group: Long, member: Long, sourceMember: Long, memberEpoch: Long, activationEpoch: Long,
                             requested: String?, preferred: String?) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    val key = try {
      OrderedActivationKey(instance, connectionGeneration, OrderedGroupBridgeMapper.generation(generationKind, generationValue),
        group.toULong(), OrderedPortRegistration(member.toULong(), sourceMember.toULong(), memberEpoch.toULong()), activationEpoch)
    } catch (error: Throwable) { perform(requestId) { throw error }; return }
    when (operation) {
      "enable" -> {
        // Record a request-local activation intent before it enters the
        // executor.  The intent is not a physical/source admission: that
        // still happens under the owner-effect lease below.  It does give a
        // pre-dispatch cancellation, executor refusal, or owner fence an
        // exact activation identity whose release can be reported to Rust.
        // Without it, cancel(142) can only see an ordinary queued request and
        // returns a generic failure instead of the retained enable release.
        synchronized(orderedActivationLock) {
          check(key !in orderedActivations) { "ordered activation already exists" }
          check(orderedActivations.size < 64) { "ordered activation receipt capacity exhausted" }
          orderedActivations[key] = OrderedActivationState.Pending
          check(orderedEnableKeys.putIfAbsent(requestId, key) == null) { "duplicate ordered enable request" }
          check(orderedEnableRequests.add(requestId)) { "duplicate ordered enable request" }
        }
        // Claim validation and source admission remain inside the shared
        // owner lease so a queued enable can never recreate native source
        // work after ownerFence has begun retirement.
        performOwnerEffect(requestId) {
          try {
            val port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
            check(port.orderedHasClaim(key.generation, key.group, key.member, instance)) { "ordered physical claim is not current" }
            val admission = port.orderedAdmission(key.generation, key.group, key.member, instance)
            synchronized(connectGuard) {
              val current = establishedConnections[peerKey(peerId)]
              check(current?.expectedGeneration == connectionGeneration && current.attempt !in fencedAttempts) { "ordered connection generation is not current" }
            }
            enableNotificationsImpl(requestId, peerId, serviceUuid, serviceOccurrence, characteristicUuid,
              characteristicOccurrence, activationEpoch, requested, preferred, true, admission, ownerEffectAlreadyHeld = true) { result ->
              synchronized(orderedActivationLock) {
                if (orderedActivations.containsKey(key) && orderedActivations[key] !is OrderedActivationState.Released) {
                  orderedActivations[key] = result.fold({ OrderedActivationState.Enabled(it) }, {
                    if (it is OrderedEnableNotApplied) OrderedActivationState.Released(classify(it.original))
                    else OrderedActivationState.Uncertain(classify(it), localRollbackEligible = true)
                  })
                }
              }
            }
          } catch (error: Throwable) {
            settleOrderedEnableBeforeEffect(requestId, classify(error).copy(dispatched = false))
          }
        }
      }
      "settlement" -> perform(requestId) {
        val recorded = synchronized(orderedActivationLock) { checkNotNull(orderedActivations[key]) { "ordered activation receipt is unavailable" } }
        val current = synchronized(connectGuard) { establishedConnections[peerKey(peerId)] }
        val state = if (recorded is OrderedActivationState.Released) recorded else if (current?.expectedGeneration != connectionGeneration) OrderedActivationState.Uncertain(
          RadioFailure(RadioFailureKind.PATH_STALE, null, "ordered activation connection changed; cleanup is not confirmed", dispatched = false)
        ) else if (recorded is OrderedActivationState.Uncertain && recorded.localRollbackEligible) {
          try { retryOrderedLocalRelease(key, recorded.failure) } catch (error: Throwable) {
            log("ordered activation cleanup retry failed: ${detailOf(error)}")
            recorded
          }.also { retried -> synchronized(orderedActivationLock) { if (orderedActivations[key] == recorded) orderedActivations[key] = retried } }
        } else recorded
        val result = MobileCoreBridge.OrderedResult().apply {
          kind = "enable-status"
          when (state) {
            OrderedActivationState.Pending -> status = "pending"
            is OrderedActivationState.Enabled -> { status = "enabled"; delivery = state.delivery }
            is OrderedActivationState.Uncertain -> {
              status = "uncertain"; hasFailure = true; failureKind = state.failure.kind.wire
              gattStatus = state.failure.gattStatus ?: MobileCoreBridge.ABSENT_INT; detail = state.failure.detail
              dispatched = state.failure.dispatched; nativeName = state.failure.nativeCode
            }
            is OrderedActivationState.Released -> {
              status = "released"
              state.failure?.let { failure ->
                hasFailure = true; failureKind = failure.kind.wire; gattStatus = failure.gattStatus ?: MobileCoreBridge.ABSENT_INT
                detail = failure.detail; dispatched = failure.dispatched; nativeName = failure.nativeCode
              }
            }
          }
        }
        answer(requestId, "ordered-enable-status") { core.completeOrdered(requestId, result) }
      }
      "disable" -> {
        orderedEnableRequests.add(requestId)
        perform(requestId) {
          val state = synchronized(orderedActivationLock) { checkNotNull(orderedActivations[key]) { "ordered activation receipt is unavailable" } }
          if (state is OrderedActivationState.Released) {
            answer(requestId, "unit") { core.completeUnit(requestId) }
            return@perform
          }
          if (state is OrderedActivationState.Uncertain) {
            val retried = if (state.localRollbackEligible) retryOrderedLocalRelease(key, state.failure) else state
            synchronized(orderedActivationLock) { if (orderedActivations[key] == state) orderedActivations[key] = retried }
            if (retried is OrderedActivationState.Released) {
              answer(requestId, "unit") { core.completeUnit(requestId) }
              return@perform
            }
          }
          check(state is OrderedActivationState.Enabled || state is OrderedActivationState.Uncertain) { "ordered activation is still pending" }
          val enablementKey = EnablementKey(instanceKey(instance), activationEpoch)
          val owner = checkNotNull(enablements[enablementKey]) { "ordered physical owner is unavailable" }
          check(connectionIsCurrent(owner) && !owner.databaseInvalidated && owner.connection.expectedGeneration == connectionGeneration) {
            "ordered activation generation is not current"
          }
          val physical = radio as? OrderedPhysicalPort ?: error("ordered physical port is unavailable")
          track(requestId, physical.setOrderedNotification(instance, false, null, activationEpoch, null) { result ->
            result.fold(onSuccess = {
              enablements.remove(enablementKey, owner)
              synchronized(orderedActivationLock) {
                if (orderedActivations.containsKey(key)) orderedActivations[key] = OrderedActivationState.Released(null)
              }
              answer(requestId, "unit") { core.completeUnit(requestId) }
            }, onFailure = { error ->
              synchronized(orderedActivationLock) {
                if (orderedActivations.containsKey(key) && orderedActivations[key] !is OrderedActivationState.Released) orderedActivations[key] = OrderedActivationState.Uncertain(classify(error))
              }
              fail(requestId, error)
            })
          })
        }
      }
      "forget-receipt" -> perform(requestId) {
        synchronized(orderedActivationLock) {
          val state = orderedActivations[key]
          check(state == null || state is OrderedActivationState.Released) { "ordered activation cleanup is not confirmed" }
          orderedActivations.remove(key)
        }
        answer(requestId, "unit") { core.completeUnit(requestId) }
      }
      else -> perform(requestId) { throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "unknown ordered activation operation") }
    }
  }

  override fun orderedOpen(
    requestId: Long, group: Long, memberIds: LongArray, peers: Array<String>, services: Array<String>,
    serviceOccurrences: LongArray, characteristics: Array<String>, characteristicOccurrences: LongArray,
    itemCapacity: Int, byteCapacity: Int, connectionGeneration: String, databaseGeneration: String,
    snapshotOwner: String, snapshotAttachment: Long, snapshotRevision: Long
  ) {
    orderedOpenRequests.add(requestId)
    val peer = peers.firstOrNull()?.uppercase() ?: ""
    legacyOpenIntents[requestId] = LegacyOpenIntent(group.toULong(), requestId.toULong(), peer)
    // Legacy opens predate the slot protocol, but their admission still
    // creates exact source ownership.  Reserve it only after the same owner
    // lease used by the v2 slot path has begun.
    performOwnerEffect(requestId) {
      val reservation = synchronized(legacyOpenIntentLock) {
        val intent = checkNotNull(legacyOpenIntents[requestId]) { "legacy ordered open intent is missing" }
        if (intent.fenced) throw OrderedOpenNotCreated(IllegalStateException("ordered open was recovered before native admission"))
        val port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
        OpenReservation(port, port.orderedAdmitOpen(group.toULong(), requestId.toULong(), peers.firstOrNull() ?: ""))
      }
      orderedOpenReservations[requestId] = reservation
    try {
    val size = memberIds.size
    require(size > 0 && peers.size == size && services.size == size && serviceOccurrences.size == size &&
      characteristics.size == size && characteristicOccurrences.size == size)
    val members = LinkedHashMap<ULong, CharacteristicInstance>()
    for (index in memberIds.indices) {
      val previous = members.put(memberIds[index].toULong(), CharacteristicInstance(peers[index], services[index],
        serviceOccurrences[index], characteristics[index], characteristicOccurrences[index]))
      require(previous == null) { "duplicate ordered member ID" }
    }
    require(connectionGeneration.isNotEmpty() && databaseGeneration.isNotEmpty())
    val opened = reservation.port.orderedOpenAdmitted(reservation.admission, AndroidNativeGraphIdentity(snapshotOwner, snapshotAttachment, snapshotRevision.toULong()), members, itemCapacity, byteCapacity, onWakeFailure = { generation, groupId, _ ->
      relayOrderedSourceCallback("ordered-wake-failure") {
        val status = core.ingestOrderedWakeFailure(generation.toLong(), groupId.toLong())
        check(status == MobileCoreBridge.STATUS_ACCEPTED) { "ordered source failure signal was rejected ($status)" }
      }
    }) { generation, groupId ->
      relayOrderedSourceCallback("ordered-wake") {
        val status = core.ingestOrderedWake(generation.toLong(), groupId.toLong())
        check(status == MobileCoreBridge.STATUS_ACCEPTED) { "ordered source wake was rejected ($status)" }
      }
    }
    answer(requestId, "ordered-opened") { core.completeOrdered(requestId, OrderedGroupBridgeMapper.opened(opened)) }
    } catch (error: Throwable) {
      val recovery = try {
        reservation.port.orderedFailAdmission(reservation.admission, error)
      } catch (recoveryError: Throwable) {
        log("ordered open recovery failed: ${detailOf(recoveryError)}")
        OrderedPortOpenRecovery.Indeterminate
      }
      val result = if (recovery is OrderedPortOpenRecovery.Failed) orderedRecoveryResult(recovery, group)
        else orderedOpenFailure(classifyOrderedOpenError(error), recovery, group)
      answer(requestId, "ordered-open-failed") { core.completeOrdered(requestId, result) }
    }
    }
  }

  private fun markerStatus(state: com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State): MobileCoreBridge.OrderedResult =
    MobileCoreBridge.OrderedResult().apply {
      kind = "marker-write-status"
      fun setFailure(error: Throwable, preEffect: Boolean = false) {
        val failure = classify(error).let { if (preEffect) it.copy(dispatched = false) else it }
        hasFailure = true; failureKind = failure.kind.wire
        gattStatus = failure.gattStatus ?: MobileCoreBridge.ABSENT_INT
        detail = failure.detail; dispatched = failure.dispatched; nativeName = failure.nativeCode
      }
      when (state) {
        com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State.Reserved -> { admission = "pending"; writeState = "not-started" }
        is com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State.NotAdmitted -> { admission = "not-admitted"; writeState = "not-started"; setFailure(state.error, true) }
        is com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State.Rejected -> { admission = "rejected"; writeState = "not-started"; setFailure(state.error, true) }
        is com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State.AdmittedNotStarted -> { admission = "admitted"; ordinal = state.ordinal.toLong(); writeState = "not-started"; setFailure(state.error, true) }
        is com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State.Initiating -> { admission = "admitted"; ordinal = state.ordinal.toLong(); writeState = "initiating" }
        is com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State.Started -> { admission = "admitted"; ordinal = state.ordinal.toLong(); writeState = "started"; state.uncertainty?.let { setFailure(it) } }
        is com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State.Settled -> { admission = "admitted"; ordinal = state.ordinal.toLong(); writeState = "settled"; state.result.exceptionOrNull()?.let { setFailure(it) } }
      }
    }

  // Graph admission mutates the exact owner store.  It must take the same
  // admission permit as a radio effect: otherwise a graph request queued
  // before ownerFence can enter the graph store after the fence's status
  // observation has already started.
  override fun orderedGraphPermitReservation(requestId: Long, operation: String, authorityId: Long, request: Long, peerId: String, connectionGeneration: String, snapshotOwner: String, snapshotAttachment: Long, snapshotRevision: Long) = performOwnerEffect(requestId) {
    val port = radio as? OrderedGroupPort ?: error("ordered graph owner is unavailable")
    val key = com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitRequest(
      authorityId.toULong(), request.toULong(), peerId, connectionGeneration,
      AndroidNativeGraphIdentity(snapshotOwner, snapshotAttachment, snapshotRevision.toULong()))
    val state = port.orderedGraphPermitReservation(operation, key)
    if (state == null) {
      answer(requestId, "graph-permit-reservation") { core.completeUnit(requestId) }
    } else {
      val response = MobileCoreBridge.OrderedResult().apply {
        when (state) {
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus.Pending -> { kind = "graph-reserve-status"; status = "pending" }
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus.NotCreated -> { kind = "graph-reserve-status"; status = "not-created" }
          is com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus.Issued -> {
            kind = if (operation == "reserve") "graph-permit-reserved" else "graph-reserve-status"
            status = "issued"; this.authorityId = state.permit.toLong()
          }
        }
      }
      answer(requestId, "graph-permit-reservation") { core.completeOrdered(requestId, response) }
    }
  }

  override fun orderedGraphAuthority(requestId: Long, operation: String, authorityValue: Long) = performOwnerEffect(requestId) {
    val port = radio as? OrderedGroupPort ?: error("ordered graph owner is unavailable")
    val outcome = port.orderedGraphAuthority(operation, authorityValue.toULong())
    if (outcome == GraphAuthorityPortResponse.Unit) return@performOwnerEffect answer(requestId, "graph-authority") { core.completeUnit(requestId) }
    val response = MobileCoreBridge.OrderedResult().apply {
      when (outcome) {
        is GraphAuthorityPortResponse.Reservation -> when (val state = outcome.status) {
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus.Pending -> { kind = "graph-reserve-status"; status = "pending" }
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus.NotCreated -> { kind = "graph-reserve-status"; status = "not-created" }
          is com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus.Issued -> {
            kind = if (operation == "reserve") "graph-authority-reserved" else "graph-reserve-status"
            status = "issued"; authorityId = state.permit.toLong()
          }
        }
        is GraphAuthorityPortResponse.Open -> when (val state = outcome.status) {
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Pending -> { kind = "graph-authority-status"; status = "pending" }
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphAuthorityOpenStatus.NotCreated -> { kind = "graph-authority-status"; status = "not-created" }
          is com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Opened -> {
            kind = if (operation == "open") "graph-authority-opened" else "graph-authority-status"
            status = "opened"; authorityId = state.authority.toLong()
          }
          is com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphAuthorityOpenStatus.Closed -> { kind = "graph-authority-status"; status = "closed"; authorityId = state.authority.toLong() }
        }
        GraphAuthorityPortResponse.Unit -> error("unit response already handled")
      }
    }
    answer(requestId, "graph-authority") { core.completeOrdered(requestId, response) }
  }

  override fun orderedGraphPermit(requestId: Long, operation: String, peerId: String, connectionGeneration: String,
                                  snapshotOwner: String, snapshotAttachment: Long, snapshotRevision: Long, authorityId: Long, permitId: Long) = performOwnerEffect(requestId) {
    val port = radio as? OrderedGroupPort ?: error("ordered graph owner is unavailable")
    val key = com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitKey(
      authorityId.toULong(), peerId, connectionGeneration, AndroidNativeGraphIdentity(snapshotOwner, snapshotAttachment, snapshotRevision.toULong()), permitId.toULong())
    val outcome = port.orderedGraphPermit(operation, key) {
      synchronized(connectGuard) {
        val current = establishedConnections[peerKey(peerId)]
        current?.expectedGeneration == connectionGeneration && current.attempt !in fencedAttempts
      }
    }
    val response = MobileCoreBridge.OrderedResult().apply {
      if (outcome == com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitResult.INVALIDATED ||
          (operation != "acquire" && outcome == com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitResult.STALE)) {
        val cause = port.orderedGraphPermitInvalidation(key)
        kind = "graph-permit-invalidated"
        failureKind = when (cause) {
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitInvalidation.SERVICE_CHANGED -> "path-stale"
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitInvalidation.CONNECTION_LOST -> "not-connected"
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitInvalidation.OVERFLOW -> "busy"
          com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitInvalidation.TEARDOWN -> "cancelled"
          null -> "platform"
        }
        nativeName = cause?.name ?: "MISSING_INVALIDATION_CAUSE"
        detail = "native graph permit invalidated before handoff"; dispatched = false
        gattStatus = MobileCoreBridge.ABSENT_INT
      } else { kind = "graph-permit"; status = outcome.name.lowercase() }
    }
    answer(requestId, "graph-permit") { core.completeOrdered(requestId, response) }
  }

  override fun orderedMarkerWrite(requestId: Long, operation: String, generationKind: String, generationValue: String,
                                  group: Long, markerId: Long, connectionGeneration: String, snapshotOwner: String,
                                  snapshotAttachment: Long, snapshotRevision: Long, peerId: String, serviceUuid: String,
                                  serviceOccurrence: Long, characteristicUuid: String, characteristicOccurrence: Long,
                                  value: ByteArray, withResponse: Boolean) {
    val cancellation = com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation()
    markerCancellations[requestId] = cancellation
    val ownedValue = value.copyOf()
    val intent = QueuedMarkerIntent(requestId, generationKind, generationValue, group, markerId, connectionGeneration,
      snapshotOwner, snapshotAttachment, snapshotRevision, peerId, serviceUuid, serviceOccurrence, characteristicUuid,
      characteristicOccurrence, ownedValue, withResponse, cancellation)
    queuedMarkerIntents[requestId] = intent
    // Marker reservation changes the source store.  It must happen after the
    // owner effect lease begins, not in this caller thread: otherwise a fence
    // can win after this method enters but before the serialized write runs.
    performOwnerEffect(requestId) {
      require(operation == "write")
      val port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
      val generation = OrderedGroupBridgeMapper.generation(generationKind, generationValue)
      val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
      val graph = AndroidNativeGraphIdentity(snapshotOwner, snapshotAttachment, snapshotRevision.toULong())
      val reservation = synchronized(queuedMarkerIntentLock) {
        val existing = markerReservations[requestId]
        if (intent.fencedBeforeAdmission) {
          checkNotNull(existing) { "cancelled marker receipt is missing" }
          // The queued adapter task has now returned without submitting a
          // native cell, so this is the only point that may release the
          // retained adapter-work evidence.
          port.orderedCancelMarkerWrite(generation, group.toULong(), markerId.toULong(),
            com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancelled())
          com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.Reservation(
            port.orderedMarkerWriteSettlement(generation, group.toULong(), markerId.toULong()), false, cancellation)
        } else {
          port.orderedReserveMarkerWrite(generation, group.toULong(), markerId.toULong(), connectionGeneration,
            graph, instance, ownedValue, withResponse, cancellation).also {
              markerReservations[requestId] = MarkerReservation(port, generation, group.toULong(), markerId.toULong(), it.created)
            }
        }
      }
      if (!reservation.created) {
        val current = port.orderedMarkerWriteSettlement(generation, group.toULong(), markerId.toULong())
        answer(requestId, "marker-write-status") { core.completeOrdered(requestId, markerStatus(current)) }
        return@performOwnerEffect
      }
      val operationId = port.orderedMarkerWrite(generation, group.toULong(), markerId.toULong(), connectionGeneration,
        graph, instance, ownedValue, withResponse, validate = {
          synchronized(connectGuard) {
            val current = establishedConnections[peerKey(peerId)]
            check(current?.expectedGeneration == connectionGeneration && current.attempt !in fencedAttempts) { "ordered connection generation is not current" }
          }
        }, cancellation = cancellation) { state ->
        answer(requestId, "marker-write-status") { core.completeOrdered(requestId, markerStatus(state)) }
      }
      if (operationId != null) {
        track(requestId, operationId)
        if (cancelRequested.contains(requestId)) radio.cancel(operationId)
      }
    }
  }

  override fun orderedRequest(
    requestId: Long, operation: String, generationKind: String, generationValue: String,
    group: Long, member: Long, sourceMember: Long, epoch: Long, batch: Long
  ) {
    // Cancellation and settlement must remain available while the serialized
    // radio executor is waiting for the original marker/write admission.
    val executor = if (operation == "cancel-marker-write" || operation == "marker-write-settlement") Executor { it.run() } else radioExecutor
    val body: () -> Unit = {
    val port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
    val generation = OrderedGroupBridgeMapper.generation(generationKind, generationValue)
    val groupId = group.toULong()
    when (operation) {
      "register" -> {
        val result = port.orderedRegister(generation, groupId, OrderedPortRegistration(member.toULong(), sourceMember.toULong(), epoch.toULong()))
        answer(requestId, "ordered-registered") { core.completeOrdered(requestId, OrderedGroupBridgeMapper.registered(result)) }
      }
      "retain-claim" -> {
        val result = port.orderedRetainClaim(generation, groupId,
          OrderedPortRegistration(member.toULong(), sourceMember.toULong(), epoch.toULong()))
        answer(requestId, "ordered-claim") { core.completeOrdered(requestId, OrderedGroupBridgeMapper.claim(generation, result)) }
      }
      "drain" -> {
        val result = port.orderedDrain(generation, groupId)
        answer(requestId, "ordered-records") { core.completeOrdered(requestId, OrderedGroupBridgeMapper.records(result)) }
      }
      "retire" -> { port.orderedRetire(generation, groupId, OrderedPortRegistration(member.toULong(), sourceMember.toULong(), epoch.toULong())); answer(requestId, "unit") { core.completeUnit(requestId) } }
      "cancel-marker-write" -> {
        val marker = batch.toULong()
        val pending = queuedMarkerIntents.values.singleOrNull {
          it.generationKind == generationKind && it.generationValue == generationValue && it.group.toULong() == groupId && it.marker == batch
        }
        val result = if (pending == null) {
          port.orderedCancelMarkerWriteExact(generation, groupId, marker)
        } else synchronized(queuedMarkerIntentLock) {
          val existing = markerReservations[pending.request]
          if (existing != null) {
            port.orderedCancelMarkerWriteExact(generation, groupId, marker)
          } else {
            pending.fencedBeforeAdmission = true
            val instance = CharacteristicInstance(pending.peer, pending.service, pending.serviceOccurrence,
              pending.characteristic, pending.characteristicOccurrence)
            val graph = AndroidNativeGraphIdentity(pending.snapshotOwner, pending.snapshotAttachment, pending.snapshotRevision.toULong())
            val reservation = port.orderedReserveMarkerWrite(generation, groupId, marker, pending.connection, graph,
              instance, pending.value, pending.withResponse, pending.cancellation)
            markerReservations[pending.request] = MarkerReservation(port, generation, groupId, marker, reservation.created)
            port.orderedCancelMarkerWriteExact(generation, groupId, marker)
          }
        }
        answer(requestId, "marker-write-status") { core.completeOrdered(requestId, markerStatus(result)) }
      }
      "marker-write-settlement" -> { val result = port.orderedMarkerWriteSettlement(generation, groupId, batch.toULong()); answer(requestId, "marker-write-status") { core.completeOrdered(requestId, markerStatus(result)) } }
      "forget-marker-write-receipt" -> { port.orderedForgetMarkerWriteReceipt(generation, groupId, batch.toULong()); answer(requestId, "unit") { core.completeUnit(requestId) } }
      "barrier" -> { port.orderedBarrier(generation, groupId, batch.toULong()); answer(requestId, "unit") { core.completeUnit(requestId) } }
      "acknowledge-batch" -> { port.orderedAcknowledge(generation, groupId, batch.toULong()); answer(requestId, "unit") { core.completeUnit(requestId) } }
      "close" -> { port.orderedClose(generation, groupId); answer(requestId, "unit") { core.completeUnit(requestId) } }
      "seal" -> { port.orderedSeal(generation, groupId); answer(requestId, "unit") { core.completeUnit(requestId) } }
      "confirm-cleanup" -> {
        port.orderedConfirmCleanup(generation, groupId, OrderedPortRegistration(member.toULong(), sourceMember.toULong(), epoch.toULong()))
        answer(requestId, "unit") { core.completeUnit(requestId) }
      }
      "remove" -> { port.orderedRemove(generation, groupId); answer(requestId, "unit") { core.completeUnit(requestId) } }
      "forget-removal-receipt" -> { port.orderedForgetRemovalReceipt(generation, groupId); answer(requestId, "unit") { core.completeUnit(requestId) } }
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "unknown ordered source operation")
    }
    }
    // Registration and claim retention create new source ownership. Cleanup,
    // drain and settlement controls remain runnable after retirement so they
    // can settle the exact receipts already admitted before the fence.
    if (operation == "register" || operation == "retain-claim" || operation == "barrier") {
      performOwnerEffect(requestId, executor, body = body)
    } else {
      submit(requestId, executor, body = body)
    }
  }

  override fun adapterState(requestId: Long) = perform(requestId) {
    val state = radio.adapterState()
    answer(requestId, "adapter") {
      core.completeAdapter(requestId, state.availability, state.authorization, state.power, state.safeReason)
    }
  }

  override fun startScan(
    requestId: Long,
    serviceUuids: Array<String>?,
    deviceAddresses: Array<String>?,
    scanMode: String?,
    callbackType: String?,
    legacy: Int,
    reportDelayMs: Long,
    phy: String?
  ) = performOwnerEffect(requestId) {
    val mode = when (scanMode) {
      null, "low-latency" -> SCAN_MODE_LOW_LATENCY
      "low-power" -> SCAN_MODE_LOW_POWER
      "balanced" -> SCAN_MODE_BALANCED
      "opportunistic" -> SCAN_MODE_OPPORTUNISTIC
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "scan mode '$scanMode' is not an Android scan mode")
    }
    val callback = when (callbackType) {
      null, "all-matches" -> CALLBACK_TYPE_ALL_MATCHES
      "first-match" -> CALLBACK_TYPE_FIRST_MATCH
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "scan callback type '$callbackType' is not supported")
    }
    if (reportDelayMs < 0 || reportDelayMs > Int.MAX_VALUE) {
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "scan report delay is outside the wire range")
    }
    val scanPhy = when (phy) {
      null -> null
      "all-supported" -> SCAN_PHY_ALL_SUPPORTED
      "1m" -> SCAN_PHY_LE_1M
      "coded" -> SCAN_PHY_LE_CODED
      else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "scan PHY '$phy' is not an Android scan PHY")
    }
    if (scanPhy != null && legacy != 0) {
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "scan PHY selection requires legacy=false")
    }
    val scanIdentity = requestId.toString()
    synchronized(scanControlLock) { scanStarting++; scanMayOwn = true; scanRevision++ }
    try {
      val services = serviceUuids?.toList() ?: emptyList()
      val addresses = deviceAddresses?.toList() ?: emptyList()
      if (radio is ScanIdentityRadioPort) {
        radio.startScanWithIdentity(services, addresses, mode, callback, legacy != 0, reportDelayMs, scanPhy, scanIdentity)
      } else {
        radio.startScan(services, addresses, mode, callback, legacy != 0, reportDelayMs, scanPhy)
      }
      answer(requestId, "unit") { core.completeScanStarted(requestId, scanIdentity) }
    } catch (error: Throwable) {
      (error.suppressed.firstOrNull() ?: error.cause?.suppressed?.firstOrNull())?.let { failure ->
        synchronized(scanControlLock) { scanCleanupFailure = failure }
      }
      throw error
    } finally {
      synchronized(scanControlLock) { scanStarting-- }
      scheduleRetiredScanCleanup()
    }
  }

  override fun stopScan(requestId: Long) = perform(requestId) {
    val revision = synchronized(scanControlLock) { scanRevision }
    val failure = radio.stopScan()
    synchronized(scanControlLock) {
      if (scanRevision == revision) {
        scanCleanupFailure = failure
        if (failure == null) scanMayOwn = false
      }
    }
    if (failure == null) answer(requestId, "unit") { core.completeUnit(requestId) } else fail(requestId, failure)
  }

  override fun connect(requestId: Long, peerId: String, autoConnect: Boolean, preferredPhy: Array<String>, expectedGeneration: String) = performOwnerEffect(requestId) {
    require(expectedGeneration.isNotEmpty()) { "Connect requires its admitted core generation" }
    val phyMask = connectPhyMask(preferredPhy)
    if (phyMask != 0) {
      // A PHY preference is honoured by establishing the link on it, or
      // refused before any effect; never connected without it.
      if (autoConnect) {
        throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "Android does not apply a connect PHY with autoConnect")
      }
      if (!radio.supportsConnectPhy()) {
        throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "connectGatt with a PHY preference needs API 26")
      }
    }
    val key = peerKey(peerId)
    if (synchronized(connectGuard) { establishedConnections.containsKey(key) }) {
      if (phyMask != 0) {
        throw RadioPortFailure(
          RadioFailureKind.UNSUPPORTED,
          "the link to $peerId is already established; its PHYs were chosen then"
        )
      }
      // The OS already holds this link (a live link this owner opened):
      // answered from the live link, never by a second connectGatt.
      answer(requestId, "unit") { core.completeUnit(requestId) }
      return@performOwnerEffect
    }
    // Installed before radio.connect: the driver can publish a prior generation's loss from
    // inside connect(), and that observation must find the request already owned (and not its own).
    val pending = PendingConnect(peerId, requestId, GattConnectAttempt(), expectedGeneration, ownerRequestWork[requestId])
    synchronized(connectGuard) {
      if (pendingConnects[key] != null) {
        throw RadioPortFailure(RadioFailureKind.BUSY, "a connect is already pending for $peerId")
      }
      pendingConnects[key] = pending
    }
    try {
      radio.connect(peerId, autoConnect, phyMask, pending.attempt)
    } catch (error: Throwable) {
      val withdrawn = synchronized(connectGuard) {
        if (pendingConnects[key] !== pending) false
        else if (pending.state == PendingConnectState.WITHDRAWN) {
          pending.state = PendingConnectState.RETURNED
          true
        } else {
          pendingConnects.remove(key, pending)
          false
        }
      }
      if (withdrawn) finishWithdrawnPending(key, pending)
      throw error
    }
    val withdrawn = synchronized(connectGuard) {
      if (pendingConnects[key] !== pending) {
        null
      } else if (pending.state == PendingConnectState.WITHDRAWN) {
        pending.state = PendingConnectState.RETURNED
        pending
      } else {
        pending.state = PendingConnectState.RETURNED
        if (pending.outcomeObserved) pendingConnects.remove(key, pending)
        null
      }
    }
    if (withdrawn != null) finishWithdrawnPending(key, withdrawn)
  }

  override fun disconnect(requestId: Long, peerId: String) = perform(requestId) {
    val key = peerKey(peerId)
    val pending = synchronized(connectGuard) {
      pendingConnects[key]?.let { it to withdrawPendingLocked(key, it, requestId) }
    }
    if (pending != null) {
      failPendingConnect(
        pending.first,
        RadioPortFailure(RadioFailureKind.CANCELLED, "connect withdrawn by explicit disconnect")
      )
      if (pending.second) finishWithdrawnPending(key, pending.first)
      return@perform
    }
    radio.disconnect(peerId) { failure ->
      if (failure == null) answer(requestId, "unit") { core.completeUnit(requestId) } else fail(requestId, failure)
    }
  }

  override fun discover(requestId: Long, peerId: String) = performOwnerEffect(requestId) {
    requireConnected(peerId)
    val scoped = radio as? ScopedDiscoveryPort
    if (scoped != null) {
      track(requestId, scoped.discoverScoped(peerId) { result ->
        result.fold(onSuccess = { graph ->
          val tree = encodeDiscovery(graph.services)
          answer(requestId, "discovered-scoped") {
            core.completeDiscoveredScoped(requestId, tree.levels, tree.uuids, tree.occurrences, tree.properties,
              graph.identity.owner, graph.identity.attachment, graph.identity.revision.toLong())
          }
        }, onFailure = { error -> fail(requestId, error) })
      })
      return@performOwnerEffect
    }
    track(requestId, radio.discover(peerId) { result ->
      result.fold(
        onSuccess = { services ->
          val tree = encodeDiscovery(services)
          answer(requestId, "discovered") {
            core.completeDiscovered(requestId, tree.levels, tree.uuids, tree.occurrences, tree.properties)
          }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun read(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long
  ) = performOwnerEffect(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.read(instance) { result ->
      result.fold(
        // `onCharacteristicRead` is Android's own answer to this read: never a notification.
        onSuccess = { value -> answer(requestId, "read") { core.completeRead(requestId, value, READ_RESPONSE) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun write(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    value: ByteArray,
    withResponse: Boolean
  ) = performOwnerEffect(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.write(instance, value.copyOf(), withResponse) { result ->
      result.fold(
        onSuccess = { answer(requestId, "unit") { core.completeUnit(requestId) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun readDescriptor(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    descriptorUuid: String,
    descriptorOccurrence: Long
  ) = performOwnerEffect(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.readDescriptor(DescriptorInstance(instance, descriptorUuid, descriptorOccurrence)) { result ->
      result.fold(
        onSuccess = { value -> answer(requestId, "bytes") { core.completeBytes(requestId, value) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun writeDescriptor(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    descriptorUuid: String,
    descriptorOccurrence: Long,
    value: ByteArray
  ) = performOwnerEffect(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    requirePath(instance)
    track(requestId, radio.writeDescriptor(DescriptorInstance(instance, descriptorUuid, descriptorOccurrence), value.copyOf()) { result ->
      result.fold(
        onSuccess = { answer(requestId, "unit") { core.completeUnit(requestId) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun enableNotifications(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    epoch: Long,
    requested: String?,
    preferred: String?
  ) = enableNotificationsImpl(requestId, peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence, epoch, requested, preferred, false)

  private fun enableNotificationsImpl(
    requestId: Long, peerId: String, serviceUuid: String, serviceOccurrence: Long,
    characteristicUuid: String, characteristicOccurrence: Long, epoch: Long,
    requested: String?, preferred: String?, ordered: Boolean,
    admission: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedAdmission? = null,
    ownerEffectAlreadyHeld: Boolean = false,
    onSettlement: ((Result<String>) -> Unit)? = null
  ) {
    if (ordered) orderedEnableRequests.add(requestId)
    val body = {
    var dispatchAttempted = false
    try {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    val facts = requirePath(instance)
    val mode = resolveCccdMode(requested, preferred, facts.properties)
    val orderedPort = if (ordered) radio as? OrderedPhysicalPort ?: error("ordered physical port is unavailable") else null
    val key = EnablementKey(instanceKey(instance), epoch)
    // Stamped before the CCCD write: values Android stages while the
    // enable is in flight are delivered with this enable's epoch.
    val owner = synchronized(connectGuard) {
      val connection = establishedConnections[peerKey(peerId)]
        ?: throw RadioPortFailure(RadioFailureKind.NOT_CONNECTED, "$peerId is not connected")
      Enablement(instance, epoch, connection).also { enablements[key] = it }
    }
    val callback: (Result<Unit>) -> Unit = { result ->
      result.fold(
        onSuccess = {
          val delivery = if (facts.hasCccd) mode else DELIVERY_UNKNOWN
          onSettlement?.invoke(Result.success(delivery))
          answer(requestId, "notify-enabled") { core.completeNotifyEnabled(requestId, delivery) }
        },
        onFailure = { error ->
          onSettlement?.invoke(Result.failure(error))
          synchronized(connectGuard) {
            if ((!ordered && !owner.databaseInvalidated) || (ordered && error is OrderedEnableNotApplied)) enablements.remove(key, owner)
          }
          if (ordered && error is OrderedEnableNotApplied) {
            val failure = classify(error.original).copy(dispatched = false)
            answer(requestId, "ordered-enable-released") { core.completeOrderedEnableReleased(requestId, failure) }
          } else fail(requestId, error)
        }
      )
    }
    val operation = if (ordered) {
      dispatchAttempted = true
      checkNotNull(orderedPort).setOrderedNotification(instance, true, mode, epoch, admission, callback)
    } else {
      dispatchAttempted = true
      radio.setNotification(instance, true, mode, epoch, callback)
    }
    track(requestId, operation)
    } catch (error: Throwable) {
      if (ordered && !dispatchAttempted) settleOrderedEnableBeforeEffect(requestId, classify(error).copy(dispatched = false))
      else { onSettlement?.invoke(Result.failure(error)); throw error }
    }
    }
    if (ownerEffectAlreadyHeld) body() else performOwnerEffect(requestId, body = body)
  }

  override fun disableNotifications(
    requestId: Long,
    peerId: String,
    serviceUuid: String,
    serviceOccurrence: Long,
    characteristicUuid: String,
    characteristicOccurrence: Long,
    epoch: Long
  ) = perform(requestId) {
    val instance = CharacteristicInstance(peerId, serviceUuid, serviceOccurrence, characteristicUuid, characteristicOccurrence)
    val key = EnablementKey(instanceKey(instance), epoch)
    val owner = enablements[key]
    if (owner != null && !connectionIsCurrent(owner)) {
      enablements.remove(key, owner)
      answer(requestId, "unit") { core.completeUnit(requestId) }
      return@perform
    }
    if (owner?.databaseInvalidated == true) {
      throw RadioPortFailure(RadioFailureKind.PATH_STALE, "notification cleanup remains uncertain after the GATT database changed")
    }
    requirePath(instance)
    track(requestId, radio.setNotification(instance, false, null, epoch) { result ->
      result.fold(
        onSuccess = {
          if (owner != null) enablements.remove(key, owner)
          answer(requestId, "unit") { core.completeUnit(requestId) }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun readMtu(requestId: Long, peerId: String) = performOwnerEffect(requestId) {
    requireConnected(peerId)
    track(requestId, radio.readMtu(peerId) { result ->
      result.fold(
        onSuccess = { mtu -> answer(requestId, "mtu") { core.completeMtu(requestId, mtu ?: 0) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  /**
   * Android's per-mode single-write limits. The stack performs a long
   * (prepared) write for a with-response request larger than one ATT
   * payload, up to the ATT maximum attribute value; a command must fit one
   * ATT payload of the MTU `onMtuChanged` reported, or of the ATT default
   * LE MTU before any exchange (the link's MTU until one completes).
   */
  /**
   * Android has no write-without-response queue callback. A GATT commit is
   * not that signal, so the request fails before any radio effect.
   */
  override fun readWriteReadiness(requestId: Long, peerId: String) = perform(requestId) {
    throw RadioPortFailure(
      RadioFailureKind.UNSUPPORTED,
      "Android reports no write-without-response readiness"
    )
  }

  override fun readWriteLimits(requestId: Long, peerId: String) = performOwnerEffect(requestId) {
    requireConnected(peerId)
    track(requestId, radio.readMtu(peerId) { result ->
      result.fold(
        onSuccess = { mtu ->
          val withoutResponse = minOf((mtu ?: ATT_DEFAULT_LE_MTU) - ATT_WRITE_HEADER_BYTES, ATT_MAX_ATTRIBUTE_VALUE)
          answer(requestId, "write-limits") {
            core.completeWriteLimits(requestId, ATT_MAX_ATTRIBUTE_VALUE, withoutResponse)
          }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun requestMtu(requestId: Long, peerId: String, mtu: Int) = performOwnerEffect(requestId) {
    requireConnected(peerId)
    track(requestId, radio.requestMtu(peerId, mtu) { result ->
      result.fold(
        onSuccess = { negotiated -> answer(requestId, "mtu") { core.completeMtu(requestId, negotiated) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun readRssi(requestId: Long, peerId: String) = performOwnerEffect(requestId) {
    requireConnected(peerId)
    track(requestId, radio.readRssi(peerId) { result ->
      result.fold(
        onSuccess = { rssi -> answer(requestId, "rssi") { core.completeRssi(requestId, rssi) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun requestConnectionPriority(requestId: Long, peerId: String, priority: String) = performOwnerEffect(requestId) {
    requireConnected(peerId)
    track(requestId, radio.requestConnectionPriority(peerId, priority) { result ->
      result.fold(
        onSuccess = { accepted -> answer(requestId, "accepted") { core.completeAccepted(requestId, accepted) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun subrateAvailable(): Boolean = radio.supportsSubrate()

  override fun requestSubrate(requestId: Long, peerId: String, mode: String) = performOwnerEffect(requestId) {
    if (!radio.supportsSubrate()) {
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "Android subrate requests require SDK 36.1")
    }
    if (mode !in setOf("default", "low-latency", "low-power", "high-throughput")) {
      throw IllegalArgumentException("unknown subrate request mode '$mode'")
    }
    requireConnected(peerId)
    track(requestId, radio.requestSubrate(peerId, mode) { result ->
      result.fold(
        onSuccess = { status -> answer(requestId, "subrate-status") { core.completeSubrateStatus(requestId, status) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun readPhy(requestId: Long, peerId: String) = performOwnerEffect(requestId) {
    requirePhy()
    requireConnected(peerId)
    track(requestId, radio.readPhy(peerId) { result ->
      result.fold(
        onSuccess = { phy -> answer(requestId, "phy") { core.completePhy(requestId, phy.tx, phy.rx) } },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun requestPhy(requestId: Long, peerId: String, tx: String?, rx: String?) = performOwnerEffect(requestId) {
    requirePhy()
    requireConnected(peerId)
    track(requestId, radio.requestPhy(peerId, tx, rx) { result ->
      result.fold(
        onSuccess = { phy ->
          answer(requestId, "phy-request") { core.completePhyRequest(requestId, phy != null, phy?.tx, phy?.rx) }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    })
  }

  override fun securityState(requestId: Long, peerId: String) = perform(requestId) {
    val state = radio.securityState(peerId)
    answer(requestId, "security") { core.completeSecurity(requestId, state) }
  }

  override fun createBond(requestId: Long, peerId: String, transport: String) = performOwnerEffect(requestId) {
    radio.createBond(peerId, transport) { result ->
      result.fold(
        onSuccess = { state -> answer(requestId, "security") { core.completeSecurity(requestId, state) } },
        onFailure = { error -> fail(requestId, error) }
      )
    }
  }

  override fun cancelBond(requestId: Long, peerId: String) = perform(requestId) {
    // BluetoothDevice.cancelBondProcess is hidden API and the public
    // cancellation arrives only after the compiled API-36 boundary; the
    // legacy route registered no cancel-pairing capability either.
    throw RadioPortFailure(
      RadioFailureKind.UNSUPPORTED,
      "Android exposes no public bonding cancellation before API 37 for $peerId"
    )
  }

  override fun resolvePeer(requestId: Long, peerId: String) = perform(requestId) {
    val peer = radio.resolvePeer(peerId)
    answer(requestId, "resolve-peer") { core.completeResolvedPeer(requestId, peer?.peerId, peer?.name) }
  }

  override fun connectedPeers(requestId: Long, services: Array<String>) = perform(requestId) {
    if (services.isNotEmpty()) throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "Android system GATT inventory has no service-filter query")
    val peers = radio.connectedPeers()
    answer(requestId, "connected-peers") { core.completeConnectedPeers(requestId, peers.map { it.peerId }.toTypedArray(), peers.map { it.name }.toTypedArray()) }
  }

  override fun bondedPeers(requestId: Long) = perform(requestId) {
    val peers = radio.bondedPeers()
    answer(requestId, "bonded-peers") {
      core.completeBondedPeers(requestId, peers.map { it.peerId }.toTypedArray(), peers.map { it.name }.toTypedArray())
    }
  }

  override fun acquireBackground(requestId: Long, kind: String, reason: String) = performOwnerEffect(requestId, serviceExecutor, backgroundWork = true) {
    val leaseId = try { background.acquire(kind, reason) } catch (error: Throwable) {
      synchronized(backgroundOwnershipLock) {
        backgroundUnknownOwnership = true
        backgroundAcquireFailures.add(requestId to error)
      }
      throw error
    }
    synchronized(backgroundOwnershipLock) { if (!backgroundLeases.add(leaseId)) backgroundUnknownOwnership = true }
    answer(requestId, "lease") { core.completeLease(requestId, leaseId) }
  }

  override fun releaseBackground(requestId: Long, leaseId: String) = submit(requestId, serviceExecutor, backgroundWork = true) {
    synchronized(backgroundOwnershipLock) { if (leaseId !in backgroundLeases) backgroundUnknownOwnership = true }
    try { background.release(leaseId) } catch (error: Throwable) {
      synchronized(backgroundOwnershipLock) { backgroundReleaseFailures.getOrPut(leaseId) { mutableListOf() }.add(error) }
      throw error
    }
    synchronized(backgroundOwnershipLock) { backgroundLeases.remove(leaseId); backgroundReleaseFailures.remove(leaseId) }
    answer(requestId, "unit") { core.completeUnit(requestId) }
  }

  override fun updateBackgroundNotification(requestId: Long, leaseId: String, title: String, body: String?) =
    performOwnerEffect(requestId, serviceExecutor, backgroundWork = true) {
      background.update(leaseId, title, body)
      answer(requestId, "unit") { core.completeUnit(requestId) }
    }

  override fun associateCompanion(requestId: Long, name: String?, serviceUuid: String?, filtersJson: String?) = performOwnerEffect(requestId, serviceExecutor) {
    val chooser = companion()
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "no React Native activity host is attached to launch the Companion Device Manager chooser"
      )
    val callback: (Result<CompanionAssociation>) -> Unit = { result ->
      pendingAssociations.remove(requestId)
      result.fold(
        onSuccess = { association ->
          answer(requestId, "companion") {
            core.completeCompanion(
              requestId,
              association.associationId,
              association.peerId,
              association.displayName,
              association.alreadyAssociated
            )
          }
        },
        onFailure = { error -> fail(requestId, error) }
      )
    }
    pendingAssociations[requestId] = Pair(chooser, callback)
    if (filtersJson == null) chooser.associate(name, serviceUuid, callback)
    else chooser.associateWithFilters(filtersJson, callback)
  }

  override fun listCompanion(requestId: Long) = runService(requestId) {
    val port = companion()
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "companion association listing is not attached on this host"
      )
    val records = port.listAssociations()
    answer(requestId, "companion-list") {
      core.completeCompanionList(
        requestId,
        records.map { it.associationId }.toLongArray(),
        records.map { it.peerId }.toTypedArray(),
        records.map { it.displayName }.toTypedArray()
      )
    }
  }

  override fun disassociateCompanion(requestId: Long, associationId: Long) = runService(requestId) {
    val port = companion()
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "companion association removal is not attached on this host"
      )
    port.disassociate(associationId)
    answer(requestId, "unit") { core.completeUnit(requestId) }
  }

  override fun observePresence(requestId: Long, peerId: String) = performOwnerEffect(requestId, serviceExecutor) {
    val port = presence()
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "device presence observation is not attached on this host"
      )
    port.observe(peerId) { result ->
      result.fold(
        onSuccess = { answer(requestId, "unit") { core.completeUnit(requestId) } },
        onFailure = { error -> fail(requestId, error) }
      )
    }
  }

  override fun unobservePresence(requestId: Long, peerId: String) = runService(requestId) {
    val port = presence()
      ?: throw RadioPortFailure(
        RadioFailureKind.UNSUPPORTED,
        "device presence observation is not attached on this host"
      )
    port.unobserve(peerId) { result ->
      result.fold(
        onSuccess = { answer(requestId, "unit") { core.completeUnit(requestId) } },
        onFailure = { error -> fail(requestId, error) }
      )
    }
  }

  /**
   * Ingests presence-restored peers into the live owner. Returns false when
   * the owner refused them (the caller keeps them persisted).
   */
  fun ingestPresenceRestored(peers: List<PresenceRestoredPeer>): Boolean {
    if (peers.isEmpty()) return true
    val coordinator = (radio as? OrderedGroupPort)?.ownerQuiescenceCoordinator()
    val lease = coordinator?.admit("presence-restored")
    // The service retains its durable restore record when the live owner has
    // fenced; a rejected restore is never silently consumed or re-created.
    if (coordinator != null && lease == null) return false
    try {
      val status = core.ingestRestored(peers)
      recordIngress("restored", status)
      return status == MobileCoreBridge.STATUS_ACCEPTED
    } finally {
      lease?.close()
    }
  }

  override fun close(requestId: Long) = perform(requestId) {
    val live = enablements.values.toList()
    if (live.isEmpty()) {
      answer(requestId, "closed") { core.completeClosed(requestId, emptyList()) }
      return@perform
    }
    val failures = ConcurrentHashMap<EnablementKey, CloseFailure>()
    val remaining = AtomicInteger(live.size)
    fun settle() {
      if (remaining.decrementAndGet() == 0) {
        answer(requestId, "closed") { core.completeClosed(requestId, failures.values.toList()) }
      }
    }
    live.forEach { enablement ->
      val key = EnablementKey(instanceKey(enablement.instance), enablement.epoch)
      try {
        if (!connectionIsCurrent(enablement)) {
          enablements.remove(key, enablement)
          settle()
          return@forEach
        }
        if (enablement.databaseInvalidated) {
          failures[key] = CloseFailure(enablement.instance, "notification cleanup remains uncertain after the GATT database changed")
          settle()
          return@forEach
        }
        radio.setNotification(enablement.instance, false, null, enablement.epoch) { result ->
          result.fold(
            onSuccess = { enablements.remove(key, enablement) },
            onFailure = { error -> failures[key] = CloseFailure(enablement.instance, detailOf(error)) }
          )
          settle()
        }
      } catch (error: Throwable) {
        failures[key] = CloseFailure(enablement.instance, detailOf(error))
        settle()
      }
    }
  }

  override fun cancel(requestId: Long) {
    markerCancellations[requestId]?.cancel()
    if (requestId in orderedEnableRequests) {
      if (!started.contains(requestId)) cancelRequested.add(requestId)
      else bump("ordered-request-retained-after-cancel")
      return
    }
    if (requestId in orderedOpenRequests) {
      bump("ordered-request-retained-after-cancel")
      return
    }
    if (!inFlight.contains(requestId)) {
      bump("cancel-after-answer")
      return
    }
    cancelRequested.add(requestId)
    if (!started.contains(requestId)) return
    val association = pendingAssociations[requestId]
    if (association != null && association.first.cancelAssociation(association.second)) {
      // UI cancellation is scheduled on the Activity queue. Keep the owner
      // until its callback actually reports release/refusal; admission alone
      // must not fabricate a completed native cleanup.
      return
    }
    val connect = synchronized(connectGuard) {
      pendingConnects.entries.firstOrNull { it.value.requestId == requestId }?.let { (peer, pending) ->
        val returned = pending.state == PendingConnectState.RETURNED
        withdrawPendingLocked(peer, pending, null)
        pending to (peer to returned)
      }
    }
    if (connect != null) {
      val (pending, peerAndReturned) = connect
      val (connectPeer, returned) = peerAndReturned
      failPendingConnect(pending, RadioPortFailure(RadioFailureKind.CANCELLED, "connect cancelled; pending GATT release deferred"))
      if (returned) finishWithdrawnPending(connectPeer, pending)
      return
    }
    val operation = driverOperations[requestId]
    if (operation == null || !radio.cancel(operation)) {
      // Nothing cancellable on the OS side (synchronous verb, bonding
      // ceremony, foreground-service promotion, chooser UI): the request
      // keeps running and answers truthfully; Rust counts that answer late.
      bump("cancel-not-cancellable")
    }
  }

  // -- plumbing -----------------------------------------------------------

  private fun perform(requestId: Long, body: () -> Unit) = submit(requestId, radioExecutor, body = body)

  private fun performOwnerEffect(requestId: Long, executor: Executor = radioExecutor, backgroundWork: Boolean = false, body: () -> Unit) {
    val admission = admitOwnerEffect("host-effect")
    if (admission == null) {
      inFlight.add(requestId)
      rejectRetiredOwnerRequest(requestId)
      return
    }
    if (admission.slots == null || admission.work == null) {
      submit(requestId, executor, backgroundWork = backgroundWork, quiescence = admission.quiescence, body = body)
      return
    }
    submit(requestId, executor, admission.slots to admission.work, backgroundWork, admission.quiescence, body)
  }

  private fun runService(requestId: Long, body: () -> Unit) = submit(requestId, serviceExecutor, body = body)

  private fun submit(requestId: Long, executor: Executor,
                     ownerEffect: Pair<AndroidOrderedOpenSlots, AndroidOrderedOpenSlots.EffectWork>? = null,
                     backgroundWork: Boolean = false,
                     quiescence: AndroidOwnerQuiescenceCoordinator.Lease? = null,
                     body: () -> Unit) {
    if (quiescence != null) check(quiescenceWork.putIfAbsent(requestId, quiescence) == null) { "duplicate owner request" }
    val requestWork = ownerEffect?.let { OwnerRequestWork(it.first, it.second) }
    if (requestWork != null) check(ownerRequestWork.putIfAbsent(requestId, requestWork) == null) { "duplicate owner request" }
    val serviceWork = if (backgroundWork) BackgroundRequestWork(requestId) else null
    if (serviceWork != null) {
      synchronized(backgroundOwnershipLock) { check(backgroundRequests.add(requestId)) { "duplicate background request" } }
      check(backgroundRequestWork.putIfAbsent(requestId, serviceWork) == null) { "duplicate background request" }
    }
    inFlight.add(requestId)
    val task = Runnable {
      try {
        if (ownerEffect != null && !ownerEffect.first.beginEffectWork(ownerEffect.second)) {
          rejectRetiredOwnerRequest(requestId)
          return@Runnable
        }
        started.add(requestId)
        if (cancelRequested.contains(requestId)) {
          // Cancelled before the adapter touched the OS: no effect at all.
          if (markerReservations.containsKey(requestId)) {
            settleMarkerBeforeEffect(requestId, OrderedMarkerCancelled())
            return@Runnable
          }
          if (orderedEnableKeys.containsKey(requestId)) {
            settleOrderedEnableBeforeEffect(requestId, RadioFailure(RadioFailureKind.CANCELLED, null, "cancelled before dispatch", dispatched = false))
            return@Runnable
          }
          fail(requestId, RadioPortFailure(RadioFailureKind.CANCELLED, "cancelled before dispatch"), dispatched = false)
          return@Runnable
        }
        try {
          body()
        } catch (error: Throwable) {
          if (markerReservations.containsKey(requestId)) settleMarkerBeforeEffect(requestId, error)
          else fail(requestId, error, dispatched = false)
        }
      } finally { requestWork?.dispatchDone(); serviceWork?.dispatchDone() }
    }
    try {
      executor.execute(task)
    } catch (error: RejectedExecutionException) {
      try {
        val queuedMarker = queuedMarkerIntents[requestId]
        if (queuedMarker != null) {
          val state = synchronized(queuedMarkerIntentLock) {
            val port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
            val generation = OrderedGroupBridgeMapper.generation(queuedMarker.generationKind, queuedMarker.generationValue)
            val group = queuedMarker.group.toULong()
            val marker = queuedMarker.marker.toULong()
            if (markerReservations[requestId] == null) {
              val instance = CharacteristicInstance(queuedMarker.peer, queuedMarker.service, queuedMarker.serviceOccurrence,
                queuedMarker.characteristic, queuedMarker.characteristicOccurrence)
              val graph = AndroidNativeGraphIdentity(queuedMarker.snapshotOwner, queuedMarker.snapshotAttachment,
                queuedMarker.snapshotRevision.toULong())
              val reservation = port.orderedReserveMarkerWrite(generation, group, marker, queuedMarker.connection,
                graph, instance, queuedMarker.value, queuedMarker.withResponse, queuedMarker.cancellation)
              markerReservations[requestId] = MarkerReservation(port, generation, group, marker, reservation.created)
            }
            port.orderedCancelMarkerWrite(generation, group, marker, error)
          }
          answer(requestId, "marker-write-status") { core.completeOrdered(requestId, markerStatus(state)) }
          return
        }
        if (markerReservations.containsKey(requestId)) {
          settleMarkerBeforeEffect(requestId, error)
          return
        }
        if (orderedEnableKeys.containsKey(requestId)) {
          settleOrderedEnableBeforeEffect(requestId, RadioFailure(RadioFailureKind.PLATFORM, null, "Android radio executor refused the request", dispatched = false))
          return
        }
        if (requestId in orderedOpenRequests) {
          val failure = AndroidGattNotSubmitted("Android radio executor refused the request")
          val reservation = orderedOpenReservations[requestId] ?: synchronized(legacyOpenIntentLock) {
            val intent = legacyOpenIntents[requestId]
            if (intent == null || intent.fenced) null else try {
              val port = radio as? OrderedGroupPort ?: error("ordered source port is unavailable")
              OpenReservation(port, port.orderedAdmitOpen(intent.group, intent.request, intent.peer)).also {
                orderedOpenReservations[requestId] = it
              }
            } catch (_: OrderedOpenNotCreated) {
              null
            }
          }
          val recovery = reservation?.port?.orderedFailAdmission(reservation.admission, failure) ?: OrderedPortOpenRecovery.NotCreated
          val group = reservation?.admission?.group?.toLong() ?: legacyOpenIntents[requestId]?.group?.toLong() ?: 0
          answer(requestId, "ordered-open-failed") {
            val result = if (recovery is OrderedPortOpenRecovery.Failed) orderedRecoveryResult(recovery, group)
              else orderedOpenFailure(classify(failure).copy(dispatched = false), recovery, group)
            core.completeOrdered(requestId, result)
          }
          return
        }
        fail(
          requestId,
          RadioPortFailure(RadioFailureKind.PLATFORM, "Android radio executor refused the request", cause = error),
          dispatched = false
        )
      } finally { requestWork?.dispatchDone(); serviceWork?.dispatchDone() }
    }
  }

  private fun rejectRetiredOwnerRequest(requestId: Long) {
    val failure = RadioFailure(RadioFailureKind.CANCELLED, null, "owner is retiring", dispatched = false)
    if (orderedEnableKeys.containsKey(requestId)) settleOrderedEnableBeforeEffect(requestId, failure)
    else fail(requestId, RadioPortFailure(RadioFailureKind.CANCELLED, failure.detail), dispatched = false)
  }

  private fun settleMarkerBeforeEffect(requestId: Long, error: Throwable) {
    val marker = markerReservations[requestId] ?: return
    val state = if (marker.created) marker.port.orderedCancelMarkerWrite(marker.generation, marker.group, marker.marker, error)
      else marker.port.orderedMarkerWriteSettlement(marker.generation, marker.group, marker.marker)
    answer(requestId, "marker-write-status") { core.completeOrdered(requestId, markerStatus(state)) }
  }

  private fun settleOrderedEnableBeforeEffect(requestId: Long, failure: RadioFailure) {
    orderedEnableKeys[requestId]?.let { key ->
      synchronized(orderedActivationLock) {
        if (orderedActivations[key] !is OrderedActivationState.Released) orderedActivations[key] = OrderedActivationState.Released(failure)
      }
    }
    answer(requestId, "ordered-enable-released") { core.completeOrderedEnableReleased(requestId, failure) }
  }

  private fun track(requestId: Long, operationId: Long) {
    if (operationId != 0L && inFlight.contains(requestId)) driverOperations[requestId] = operationId
  }

  /** Withdraw a pending owner only after its native dispatch has returned. */
  private fun finishWithdrawnPending(key: String, pending: PendingConnect) {
    val attempt = ConnectCleanupAttempt()
    val cleanupWork = synchronized(connectGuard) {
      if (pendingConnects[key] !== pending ||
        (pending.state != PendingConnectState.WITHDRAWN && pending.state != PendingConnectState.RETURNED)
      ) return
      pending.state = PendingConnectState.CLEANING
      pending.cleanupAttempt = attempt
      pending.cleanupCallbacks++
      pending.cleanupWork?.also { it.beginAttempt() }
    }
    fun complete(failure: Throwable?, callback: Boolean): Boolean {
      val requests = synchronized(connectGuard) {
        if (pendingConnects[key] !== pending || (!callback &&
          (pending.cleanupAttempt !== attempt || attempt.callbackClaimed))) return false
        if (callback) {
          check(pending.cleanupCallbacks > 0)
          pending.cleanupCallbacks--
          pending.cleanupLastSucceeded = failure == null
          cleanupWork?.observeOutcome(failure == null)
          uncertainConnectCleanupFailures.remove(attempt)
        }
        val current = pending.cleanupAttempt === attempt
        if (!current && !callback) return false
        if (failure != null) pending.cleanupFailures.add(failure)
        else if (callback) pending.cleanupFailures.clear()
        // A successful retry cannot retire this exact owner while an earlier callback
        // can still report a failure. Keep it available for that result and a retry.
        if (callback && pending.cleanupCallbacks == 0 && pending.cleanupLastSucceeded) {
          pendingConnects.remove(key, pending)
          pending.cleanupWork = null
        } else if (pending.cleanupCallbacks == 0 || (current && failure != null)) {
          pending.state = PendingConnectState.CLEANUP_FAILED
        }
        if (current || (callback && pending.cleanupCallbacks == 0)) {
          pending.disconnectRequests.toList().also { requests ->
            pending.disconnectRequests.clear()
            if (failure != null) requests.forEach { pendingConnectFailureDeliveries[it] = failure }
          }
        } else emptyList()
      }
      requests.forEach { requestId ->
        try { settleDisconnect(requestId, failure) }
        catch (error: Throwable) {
          synchronized(connectGuard) {
            failure?.let { connectDeliveryFailures.add(it) }
            connectDeliveryFailures.add(error)
          }
        } finally {
          synchronized(connectGuard) { pendingConnectFailureDeliveries.remove(requestId) }
        }
      }
      return failure == null
    }
    try {
      radio.cancelConnect(pending.peerId, pending.attempt) { failure ->
        val claimed = synchronized(connectGuard) {
          if (attempt.callbackClaimed) false else {
            attempt.callbackClaimed = true
            true
          }
        }
        if (claimed) {
          try {
            complete(failure, callback = true)
            if (failure != null) log("withdrawn connect cleanup failed for $key: ${detailOf(failure)}")
          } finally {
            cleanupWork?.callbackDone()
          }
        }
      }
    } catch (error: Throwable) {
      val claimed = synchronized(connectGuard) {
        attempt.callbackClaimed.also { if (!it) uncertainConnectCleanupFailures[attempt] = error }
      }
      if (!claimed) complete(error, callback = false)
      else synchronized(connectGuard) { connectDeliveryFailures.add(error) }
      log("withdrawn connect cleanup threw for $key: ${detailOf(error)}")
    } finally {
      // A throw does not prove that a previously submitted native callback
      // cannot still arrive, so it does not substitute for callbackDone.
      cleanupWork?.dispatchDone()
    }
  }

  private fun failPendingConnect(pending: PendingConnect, failure: Throwable) {
    try { fail(pending.requestId, failure) }
    catch (error: Throwable) { synchronized(connectGuard) { connectDeliveryFailures.add(error) } }
  }

  private fun settleDisconnect(requestId: Long, failure: Throwable?) {
    if (failure == null) answer(requestId, "unit") { core.completeUnit(requestId) } else fail(requestId, failure)
  }

  /** Marks a pending owner withdrawn while preserving its dispatch reservation. */
  private fun withdrawPendingLocked(key: String, pending: PendingConnect, disconnectRequestId: Long?): Boolean {
    if (disconnectRequestId != null && !pending.disconnectRequests.contains(disconnectRequestId)) {
      pending.disconnectRequests += disconnectRequestId
    }
    if (pending.state == PendingConnectState.CLEANING) return false
    val wasReturned = pending.state == PendingConnectState.RETURNED || pending.state == PendingConnectState.CLEANUP_FAILED
    if (pending.state != PendingConnectState.WITHDRAWN) {
      // Public cancellation can complete before cancelConnect has actually
      // released its exact native attempt. Keep the original owner work until
      // that cleanup callback and any disconnect completions have returned.
      if (pending.cleanupWork == null) {
        pending.cleanupWork = pending.ownerWork?.takeIf { it.beginCallback() }?.let { ConnectCleanupWork(it) }
      }
      pending.state = PendingConnectState.WITHDRAWN
      pending.established?.let { owner ->
        if (establishedConnections[key] === owner) establishedConnections.remove(key)
      }
      pending.established = null
    }
    return wasReturned
  }

  /** Delivers exactly one answer per request id; later answers are counted, not sent. */
  private fun answer(requestId: Long, shape: String, deliver: () -> Int) {
    if (!inFlight.remove(requestId)) {
      bump("suppressed-second-answer")
      log("second answer ($shape) for request $requestId suppressed")
      return
    }
    driverOperations.remove(requestId)
    pendingAssociations.remove(requestId)
    cancelRequested.remove(requestId)
    started.remove(requestId)
    orderedEnableRequests.remove(requestId)
    orderedEnableKeys.remove(requestId)
    orderedOpenRequests.remove(requestId)
    orderedOpenReservations.remove(requestId)
    legacyOpenIntents.remove(requestId)
    markerCancellations.remove(requestId)
    markerReservations.remove(requestId)
    queuedMarkerIntents.remove(requestId)
    try {
      val status = deliver()
      recordStatus("complete:$shape", status)
      if (shape == "failure" && status == MobileCoreBridge.STATUS_DELIVERED) {
        synchronized(backgroundOwnershipLock) { backgroundAcquireFailures.removeAll { it.first == requestId } }
      }
    } finally {
      ownerRequestWork.remove(requestId)?.completionDone()
      backgroundRequestWork.remove(requestId)?.completionDone()
      quiescenceWork.remove(requestId)?.close()
    }
  }

  /** [dispatched] overrides the classification when the adapter knows the request never left. */
  private fun fail(requestId: Long, error: Throwable, dispatched: Boolean? = null) {
    val classified = classify(error)
    val failure = (
      if (cancelRequested.contains(requestId)) {
        RadioFailure(RadioFailureKind.CANCELLED, null, "cancelled: ${detailOf(error)}", classified.dispatched)
      } else {
        classified
      }
    ).let { if (dispatched == null) it else it.copy(dispatched = dispatched) }
    answer(requestId, "failure") { core.completeFailure(requestId, failure) }
  }

  private fun recordStatus(label: String, status: Int) {
    if (status == MobileCoreBridge.STATUS_DELIVERED) return
    val name = when (status) {
      MobileCoreBridge.STATUS_LATE -> "late"
      MobileCoreBridge.STATUS_MISMATCHED -> "mismatched"
      MobileCoreBridge.STATUS_DROPPED_CONTROL -> "dropped-control"
      MobileCoreBridge.STATUS_CLOSED -> "closed"
      MobileCoreBridge.STATUS_NO_HOST -> "no-host"
      else -> "status-$status"
    }
    bump("$label:$name")
    log("host answered $name to $label")
  }

  private fun recordIngress(label: String, status: Int) {
    if (status == MobileCoreBridge.STATUS_ACCEPTED) return
    val name = when (status) {
      MobileCoreBridge.STATUS_DROPPED_ADVERTISEMENT -> "dropped-advertisement"
      MobileCoreBridge.STATUS_DROPPED_NOTIFICATION -> "dropped-notification"
      MobileCoreBridge.STATUS_DROPPED_CONTROL -> "dropped-control"
      MobileCoreBridge.STATUS_CLOSED -> "closed"
      MobileCoreBridge.STATUS_NO_HOST -> "no-host"
      else -> "status-$status"
    }
    bump("ingest:$label:$name")
    // Queue-full drops are already counted and surfaced by Rust as
    // `ingress-drop` records; closed/no-host facts only this side can see.
    if (status == MobileCoreBridge.STATUS_CLOSED || status == MobileCoreBridge.STATUS_NO_HOST) {
      log("host refused $label ingress: $name")
    }
  }

  private fun bump(label: String) {
    counts.computeIfAbsent(label) { AtomicLong() }.incrementAndGet()
  }

  /** LE PHY control is API 26; legacy refused it below as capability.unsupported (139, AN-5). */
  private fun requirePhy() {
    if (!radio.supportsConnectPhy()) {
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "Android PHY requires API 26")
    }
  }

  private fun requireConnected(peerId: String) {
    if (!synchronized(connectGuard) { establishedConnections[peerKey(peerId)]?.let { it.attempt !in fencedAttempts } == true }) {
      throw RadioPortFailure(RadioFailureKind.NOT_CONNECTED, "$peerId is not connected")
    }
  }

  private fun requirePath(instance: CharacteristicInstance): CharacteristicFacts {
    requireConnected(instance.peerId)
    return radio.characteristic(instance)
      ?: throw RadioPortFailure(
        RadioFailureKind.PATH_STALE,
        "${instance.serviceUuid}#${instance.serviceOccurrence}/${instance.characteristicUuid}#${instance.characteristicOccurrence} is not in the current database"
      )
  }

  private fun connectionIsCurrent(owner: Enablement): Boolean = synchronized(connectGuard) {
    establishedConnections[peerKey(owner.instance.peerId)] === owner.connection && owner.connection.attempt !in fencedAttempts
  }

  /**
   * Acquires the v4 coordinator lease and its legacy effect-work counterpart
   * as one owner-control transaction. The fence takes [ownerAdmissionLock]
   * while closing both ledgers, so it can neither close slots after the
   * coordinator accepted this effect nor leave a post-fence effect in either
   * ledger. Callback work can be begun in this transaction; queued host work
   * remains reserve-only until its executor owns it.
   */
  private fun admitOwnerEffect(label: String, reserveSlot: Boolean = true, beginSlotWork: Boolean = false): OwnerCallbackAdmission? =
    synchronized(ownerAdmissionLock) {
      val orderedPort = radio as? OrderedGroupPort
      val coordinator = orderedPort?.ownerQuiescenceCoordinator()
      val quiescence = coordinator?.admit(label)
      if (coordinator != null && quiescence == null) return@synchronized null
      try {
        // Connection callbacks historically have no slot obligation. Keep
        // that v3 behavior while retaining the v4 coordinator lease.
        val slots = if (reserveSlot) orderedPort?.orderedSlots() else null
        val work = slots?.reserveEffectWork()
        if (slots != null && work == null) {
          quiescence?.close()
          null
        } else if (beginSlotWork && slots != null && work != null && !slots.beginEffectWork(work)) {
          slots.finishEffectWork(work)
          quiescence?.close()
          null
        } else OwnerCallbackAdmission(slots, work, quiescence, beginSlotWork && slots != null && work != null)
      } catch (error: Throwable) {
        quiescence?.close()
        throw error
      }
    }

  /** Same atomic handoff as [admitOwnerEffect], for the separate adapter-work ledger. */
  private fun admitOrderedOpen(
    port: OrderedGroupPort,
    descriptor: AndroidOrderedOpenSlots.Identity
  ): OwnerOrderedOpenAdmission? = synchronized(ownerAdmissionLock) {
    val coordinator = port.ownerQuiescenceCoordinator()
    val quiescence = coordinator?.admit("ordered-open")
    if (coordinator != null && quiescence == null) return@synchronized null
    try {
      val slots = port.orderedSlots()
      OwnerOrderedOpenAdmission(slots, slots.reserveAdapterWork(descriptor), quiescence)
    } catch (error: Throwable) {
      quiescence?.close()
      throw error
    }
  }

  /**
   * A native ordered source can invoke these callbacks after its open request
   * has answered.  Its ingress therefore needs the same admission edge as a
   * queued host effect: otherwise an owner fence can observe an empty slot
   * table and a late wake can still enter Rust.
   */
  private fun relayOrderedSourceCallback(label: String, body: () -> Unit) {
    // Keep the coordinator admission and legacy slot reservation in the same
    // critical section as ownerControl's two fences. A source wake admitted
    // before a v4 fence must keep its slot obligation; a wake admitted after
    // the fence must not enter either ledger.
    val admission = admitOwnerEffect("callback:$label", beginSlotWork = true)
    if (admission == null) {
      bump("ingest:$label:retired-owner")
      return
    }
    try {
      if (!admission.begun && admission.slots != null && admission.work != null && !admission.slots.beginEffectWork(admission.work)) {
        bump("ingest:$label:retired-owner")
        return
      }
      body()
    } finally {
      if (admission.slots != null && admission.work != null) admission.slots.finishEffectWork(admission.work)
      admission.quiescence?.close()
    }
  }

  private fun invalidateEnablementsFor(peerId: String) {
    val peer = peerKey(peerId)
    synchronized(connectGuard) {
      enablements.forEach { (key, owner) -> if (key.instance.peer == peer) owner.databaseInvalidated = true }
    }
  }

  private inner class Events : RadioPortEvents {
    private fun relayOwnerCallback(label: String, reserveSlot: Boolean = true, body: () -> Unit) {
      val admission = admitOwnerEffect("callback:$label", reserveSlot, beginSlotWork = reserveSlot)
      if (admission == null) {
        bump("ingest:$label:retired-owner")
        return
      }
      try {
        if (!admission.begun && admission.slots != null && admission.work != null && !admission.slots.beginEffectWork(admission.work)) {
          bump("ingest:$label:retired-owner")
          return
        }
        body()
      } finally {
        if (admission.slots != null && admission.work != null) admission.slots.finishEffectWork(admission.work)
        admission.quiescence?.close()
      }
    }
    override fun onOrderedLinkFence(peerId: String, attempt: GattConnectAttempt?) = relayOwnerCallback("ordered-link-fence") {
      val connection = synchronized(connectGuard) {
        val current = establishedConnections[peerKey(peerId)]
        if (attempt != null && current?.attempt === attempt) fencedAttempts.add(attempt)
        current
      }
      if (attempt == null || connection?.attempt !== attempt) return@relayOwnerCallback
      synchronized(orderedActivationLock) {
        orderedActivations.keys.filter { it.instance.peerId.equals(peerId, ignoreCase = true) && it.connection == connection.expectedGeneration }
          .forEach { orderedActivations[it] = OrderedActivationState.Released(null) }
      }
    }
    override fun onAdvertisement(advertisement: AdvertisementFacts) = relayOwnerCallback("advertisement") {
      recordIngress("advertisement", core.ingestAdvertisement(advertisement))
    }

    override fun onConnection(peerId: String, connected: Boolean, gattStatus: Int, attempt: GattConnectAttempt?) =
      relayOwnerCallback("connection", reserveSlot = false) { relayConnection(peerId, connected, gattStatus, attempt) }

    private fun relayConnection(peerId: String, connected: Boolean, gattStatus: Int, attempt: GattConnectAttempt?) {
      val key = peerKey(peerId)
      // The connection result can complete the request before the same callback
      // relays its connection observation. Keep its admitted work until both return.
      val callbackWork = synchronized(connectGuard) {
        val pending = pendingConnects[key]
        if (attempt != null && pending?.attempt === attempt)
          pending.ownerWork?.takeIf { it.beginCallback() }
        else null
      }
      try {
        val transition = synchronized(connectGuard) {
          val current = pendingConnects[key]
          val owner = establishedConnections[key]
          when {
            connected && gattStatus == GATT_SUCCESS &&
              current != null && attempt != null && current.attempt === attempt &&
              (current.state == PendingConnectState.DISPATCHING || current.state == PendingConnectState.RETURNED) -> {
              val established = EstablishedConnection(attempt, current.expectedGeneration)
              current.established = established
              current.outcomeObserved = true
              establishedConnections[key] = established
              if (current.state == PendingConnectState.RETURNED) pendingConnects.remove(key, current)
              ConnectionTransition.Pending(current)
            }
            !connected && attempt != null && owner?.attempt === attempt -> {
              establishedConnections.remove(key)
              fencedAttempts.remove(attempt)
              if (current?.established === owner) {
                current.established = null
                current.outcomeObserved = true
              }
              ConnectionTransition.Established(owner)
            }
            !connected && current != null && attempt != null && current.attempt === attempt &&
              (current.state == PendingConnectState.DISPATCHING || current.state == PendingConnectState.RETURNED) -> {
              current.outcomeObserved = true
              if (current.state == PendingConnectState.RETURNED) pendingConnects.remove(key, current)
              ConnectionTransition.Pending(current)
            }
            else -> null
          }
        }
        if (connected && gattStatus == GATT_SUCCESS) {
          when (transition) {
            is ConnectionTransition.Pending -> {
              val pending = transition.value
              answer(pending.requestId, "unit") { core.completeUnit(pending.requestId) }
              recordIngress("connection", core.ingestConnectionForGeneration(peerId, pending.expectedGeneration, true, gattStatus))
            }
            else -> {
              supersededOutcome(peerId, "connected", gattStatus)
            }
          }
          return
        }
        when (transition) {
          is ConnectionTransition.Pending -> {
            val pending = transition.value
            fail(
              pending.requestId,
              RadioPortFailure(
                if (gattStatus == GATT_SUCCESS) RadioFailureKind.PLATFORM else RadioFailureKind.GATT_STATUS,
                "Android GATT connection failed with status $gattStatus",
                gattStatus
              )
            )
          }
          is ConnectionTransition.Established -> {
            val established = transition.value
            recordIngress("connection", core.ingestConnectionForGeneration(peerId, established.expectedGeneration, false, gattStatus))
          }
          null -> {
            val activePending = synchronized(connectGuard) {
              pendingConnects[key]?.state == PendingConnectState.DISPATCHING ||
                pendingConnects[key]?.state == PendingConnectState.RETURNED
            }
            if (activePending) supersededOutcome(peerId, "link", gattStatus) else {
              bump("disconnect-without-link")
              log("disconnect for $peerId without an owned link or pending connect (status $gattStatus)")
            }
          }
        }
      } finally { callbackWork?.endCallback() }
    }

    private fun supersededOutcome(peerId: String, what: String, gattStatus: Int) {
      bump("superseded-connect-outcome")
      log("superseded Android GATT generation $what for $peerId (status $gattStatus) while a replacement connect is pending")
    }

    override fun onServicesChanged(peerId: String) = relayOwnerCallback("services-changed") {
      invalidateEnablementsFor(peerId)
      recordIngress("services-changed", core.ingestServicesChanged(peerId))
    }

    override fun onNotification(instance: CharacteristicInstance, epoch: Long, value: ByteArray) = relayOwnerCallback("notification") {
      val routedInstance = enablements[EnablementKey(instanceKey(instance), epoch)]?.instance ?: instance
      recordIngress("notification", core.ingestNotification(routedInstance, epoch, value))
    }

    override fun onNotificationSourceFailed(instance: CharacteristicInstance, epoch: Long, failure: RadioFailure) = relayOwnerCallback("notification-source-failed") {
      if (enablements[EnablementKey(instanceKey(instance), epoch)] == null) {
        log("notification source failure for absent or stale enable; forwarding its original epoch")
      }
      val routedInstance = enablements[EnablementKey(instanceKey(instance), epoch)]?.instance ?: instance
      val status = core.ingestNotificationSourceFailed(routedInstance, epoch, failure)
      recordIngress("notification-source-failed", status)
      if (status != MobileCoreBridge.STATUS_ACCEPTED) {
        log("notification source failure ingress refused: $status")
      }
    }

    override fun onAdapterState(state: AdapterFacts) = relayOwnerCallback("adapter") {
      if (state.power == "on" && state.authorization == "granted") return@relayOwnerCallback
      // The driver force-closes every GATT on adapter loss without a
      // per-link callback: report each link's end and each pending connect.
      val pendingAtReset = synchronized(connectGuard) {
        pendingConnects.entries.map { (key, pending) ->
          val wasReturned = pending.state == PendingConnectState.RETURNED || pending.state == PendingConnectState.CLEANUP_FAILED
          withdrawPendingLocked(key, pending, null)
          Triple(key, pending, wasReturned)
        }
      }
      val establishedAtReset = synchronized(connectGuard) {
        val owners = establishedConnections.toMap()
        establishedConnections.clear()
        owners
      }
      recordIngress("adapter", core.ingestAdapterState(state))
      pendingAtReset.forEach { (key, pending, returned) ->
        failPendingConnect(pending, adapterLossFailure(state))
        if (returned) finishWithdrawnPending(key, pending)
      }
      if (state.power == "on") return@relayOwnerCallback
      establishedAtReset.forEach { (peer, owner) ->
        recordIngress("connection", core.ingestConnectionForGeneration(peer, owner.expectedGeneration, false, null))
      }
    }

    override fun onScanFailed(errorCode: Int) = relayScanFailure(errorCode, {
      RadioScanStop(false, radio.stopScan())
    })

    override fun onScanFailed(errorCode: Int, stop: () -> RadioScanStop) = relayScanFailure(errorCode, stop)

    override fun onScanFailed(errorCode: Int, scanIdentity: String, stop: () -> RadioScanStop) = relayScanFailure(errorCode, stop, scanIdentity)

    private fun relayScanFailure(errorCode: Int, stop: () -> RadioScanStop, scanIdentity: String? = null) =
      relayOwnerCallback("scan-failed") {
        val stopped = stop()
        if (stopped.stale) {
          bump("ingest:scan-failed:stale-session")
          return@relayOwnerCallback
        }
        val stopFailure = stopped.failure
        if (stopFailure != null) synchronized(scanControlLock) { scanCleanupFailure = stopFailure }
        val detail = if (stopFailure == null) {
          "scan failed code=$errorCode"
        } else {
          "scan failed code=$errorCode; scan cleanup failed: ${detailOf(stopFailure)}"
        }
        recordIngress("scan-failed", if (scanIdentity == null) core.ingestScanFailed(detail) else core.ingestScanFailed(detail, scanIdentity, errorCode))
      }

    override fun onSecurity(peerId: String, security: SecurityFacts) = relayOwnerCallback("security") {
      recordIngress("security", core.ingestSecurity(peerId, security))
    }

    override fun onSecurityFailure(peerId: String?, error: Throwable) = relayOwnerCallback("security-failed") {
      recordIngress("security-failed", core.ingestSecurityFailure(peerId, classify(error),
        if (error is com.sfourdrinier.unifiedblemanager.radio.AndroidEncryptionFailure) error.status else null))
    }

    override fun onDropped(ingressClass: String, detail: String, count: Long) = relayOwnerCallback("dropped") {
      recordIngress("dropped", core.ingestDroppedCount(ingressClass, detail, count))
    }
  }

  private fun adapterLossFailure(state: AdapterFacts): RadioPortFailure = when {
    state.authorization == "restricted" ->
      RadioPortFailure(RadioFailureKind.PERMISSION_RESTRICTED, "Bluetooth use is restricted")
    state.authorization == "not-determined" ->
      RadioPortFailure(RadioFailureKind.PERMISSION_NOT_DETERMINED, "Bluetooth permission is not determined")
    state.authorization != "granted" ->
      RadioPortFailure(RadioFailureKind.PERMISSION_DENIED, state.safeReason ?: "Bluetooth permission was revoked")
    state.power == "resetting" -> RadioPortFailure(RadioFailureKind.ADAPTER_RESETTING, "Bluetooth adapter is resetting")
    state.power == "off" -> RadioPortFailure(RadioFailureKind.ADAPTER_OFF, "Bluetooth adapter is off")
    else -> RadioPortFailure(RadioFailureKind.ADAPTER_UNAVAILABLE, state.safeReason ?: "Bluetooth adapter is ${state.power}")
  }

  /** Pre-order discovery tree in the `nativeCompleteDiscovered` encoding. */
  internal class DiscoveryTree(
    val levels: IntArray,
    val uuids: Array<String>,
    val occurrences: LongArray,
    val properties: IntArray
  )

  companion object {
    const val SCAN_MODE_OPPORTUNISTIC = -1
    const val SCAN_MODE_LOW_POWER = 0
    const val SCAN_MODE_BALANCED = 1
    const val SCAN_MODE_LOW_LATENCY = 2
    const val CALLBACK_TYPE_ALL_MATCHES = 1
    const val CALLBACK_TYPE_FIRST_MATCH = 2
    /** ScanSettings/BluetoothDevice scan PHY values, distinct from connect masks. */
    const val SCAN_PHY_ALL_SUPPORTED = 255
    const val SCAN_PHY_LE_1M = 1
    const val SCAN_PHY_LE_CODED = 3
    const val GATT_SUCCESS = 0
    const val PROPERTY_NOTIFY = 0x10
    const val PROPERTY_INDICATE = 0x20
    const val DELIVERY_NOTIFICATION = "notification"
    const val DELIVERY_INDICATION = "indication"
    const val DELIVERY_UNKNOWN = "unknown"
    /** Read provenance: the platform attributed the value to the ATT read response. */
    const val READ_RESPONSE = "read-response"
    const val INGRESS_NOTIFICATION = "notification"

    /** Core spec Vol 3 Part F §3.2.8: the LE ATT_MTU before any exchange. */
    const val ATT_DEFAULT_LE_MTU = 23
    /** Core spec Vol 3 Part F §3.2.9: the longest attribute value. */
    const val ATT_MAX_ATTRIBUTE_VALUE = 512
    /** Opcode and handle of an ATT write request/command. */
    const val ATT_WRITE_HEADER_BYTES = 3

    /** `BluetoothDevice.PHY_LE_1M_MASK` / `PHY_LE_2M_MASK` / `PHY_LE_CODED_MASK`. */
    const val PHY_LE_1M_MASK = 1
    const val PHY_LE_2M_MASK = 2
    const val PHY_LE_CODED_MASK = 4

    /** The `connectGatt` PHY mask for wire PHY names; an unknown name is refused, never dropped. */
    internal fun connectPhyMask(preferredPhy: Array<String>): Int =
      preferredPhy.fold(0) { mask, name ->
        mask or when (name) {
          "le-1m" -> PHY_LE_1M_MASK
          "le-2m" -> PHY_LE_2M_MASK
          "le-coded" -> PHY_LE_CODED_MASK
          else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "'$name' is not an LE PHY")
        }
      }

    /** `BluetoothStatusCodes.ERROR_GATT_WRITE_REQUEST_BUSY` (API 33 write submission). */
    const val ANDROID_ERROR_GATT_WRITE_REQUEST_BUSY = 201

    private fun peerKey(peerId: String): String = peerId.uppercase()

    private fun instanceKey(instance: CharacteristicInstance) = InstanceKey(
      peerKey(instance.peerId),
      instance.serviceUuid.lowercase(),
      instance.serviceOccurrence,
      instance.characteristicUuid.lowercase(),
      instance.characteristicOccurrence
    )

    /**
     * The CCCD mode to write. A `requested` mode is hard: the characteristic
     * must carry that property or the enable is refused before any effect.
     * Otherwise `preferred` (default notification) wins when allowed, else
     * the other mode (legacy `resolveCccdPayload` rule).
     */
    @JvmStatic
    fun resolveCccdMode(requested: String?, preferred: String?, properties: Int): String {
      fun allows(mode: String) = when (mode) {
        DELIVERY_NOTIFICATION -> (properties and PROPERTY_NOTIFY) != 0
        DELIVERY_INDICATION -> (properties and PROPERTY_INDICATE) != 0
        else -> throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "delivery mode '$mode' is not a CCCD mode")
      }
      if (requested != null) {
        if (allows(requested)) return requested
        throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "characteristic does not support $requested")
      }
      val first = preferred ?: DELIVERY_NOTIFICATION
      val other = if (first == DELIVERY_NOTIFICATION) DELIVERY_INDICATION else DELIVERY_NOTIFICATION
      if (allows(first)) return first
      if (allows(other)) return other
      throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "characteristic supports neither notify nor indicate")
    }

    @JvmStatic
    internal fun encodeDiscovery(services: List<GattServiceNode>): DiscoveryTree {
      val levels = ArrayList<Int>()
      val uuids = ArrayList<String>()
      val occurrences = ArrayList<Long>()
      val properties = ArrayList<Int>()
      fun emit(level: Int, uuid: String, occurrence: Long, bits: Int) {
        levels.add(level)
        uuids.add(uuid)
        occurrences.add(occurrence)
        properties.add(bits)
      }
      val serviceCounts = HashMap<String, Long>()
      for (service in services) {
        val facts = (when (service.primary) { true -> 1; false -> 2; null -> 0 }) or
          (if (service.includedServices == null) 0 else 4)
        emit(0, service.uuid, nextOccurrence(serviceCounts, service.uuid), facts)
        val characteristicCounts = HashMap<String, Long>()
        for (characteristic in service.characteristics) {
          emit(1, characteristic.uuid, nextOccurrence(characteristicCounts, characteristic.uuid), characteristic.properties)
          val descriptorCounts = HashMap<String, Long>()
          for (descriptor in characteristic.descriptors) {
            emit(2, descriptor.uuid, nextOccurrence(descriptorCounts, descriptor.uuid), 0)
          }
        }
        service.includedServices?.forEach { included -> emit(3, included.uuid, included.occurrence, 0) }
      }
      return DiscoveryTree(levels.toIntArray(), uuids.toTypedArray(), occurrences.toLongArray(), properties.toIntArray())
    }

    private fun nextOccurrence(counts: MutableMap<String, Long>, uuid: String): Long {
      val key = uuid.lowercase()
      val occurrence = counts[key] ?: 0L
      counts[key] = occurrence + 1
      return occurrence
    }

    private fun detailOf(error: Throwable): String = error.message ?: error.javaClass.simpleName

    /** Maps a driver failure onto the frozen failure kinds; Rust maps kinds to contract identities. */
    @JvmStatic
    fun classify(error: Throwable): RadioFailure = when (error) {
      is RadioPortFailure -> RadioFailure(error.kind, error.gattStatus, detailOf(error), nativeCode = error.nativeCode)
      is OrderedMarkerCancelled -> RadioFailure(RadioFailureKind.CANCELLED, null, detailOf(error), dispatched = false)
      is OrderedRegistrationStale -> RadioFailure(RadioFailureKind.PATH_STALE, null, detailOf(error), dispatched = false)
      // Legacy failed every command pending at a link loss with connectionLost (132).
      is AndroidGattLinkLost -> RadioFailure(RadioFailureKind.NOT_CONNECTED, error.gattStatus, detailOf(error), error.submitted)
      is AndroidGattOperationFailure -> when {
        error.isLinkLoss -> RadioFailure(RadioFailureKind.NOT_CONNECTED, error.gattStatus, detailOf(error), error.submitted)
        !error.submitted && error.gattStatus == ANDROID_ERROR_GATT_WRITE_REQUEST_BUSY ->
          RadioFailure(RadioFailureKind.BUSY, error.gattStatus, detailOf(error), dispatched = false)
        error.gattStatus != null -> RadioFailure(RadioFailureKind.GATT_STATUS, error.gattStatus, detailOf(error), error.submitted)
        else -> RadioFailure(RadioFailureKind.PLATFORM, null, detailOf(error), error.submitted)
      }
      is AndroidGattNotSubmitted -> RadioFailure(RadioFailureKind.PLATFORM, null, detailOf(error), dispatched = false)
      // A rejected argument (for example an oversize value) never leaves the stack.
      is IllegalArgumentException -> RadioFailure(RadioFailureKind.PLATFORM, null, detailOf(error), dispatched = false)
      is ForegroundServiceControlException -> classifyBackgroundFailure(error)
      is SecurityException -> RadioFailure(RadioFailureKind.PERMISSION_DENIED, null, detailOf(error))
      is UnsupportedOperationException -> RadioFailure(RadioFailureKind.UNSUPPORTED, null, detailOf(error))
      else -> classifiedCause(error) ?: RadioFailure(RadioFailureKind.PLATFORM, null, detailOf(error))
    }

    /**
     * Cleanup combines its primary failure with later teardown failures.  The
     * wrapper itself is only transport structure: retain the first typed
     * cause so a permission denial is not silently degraded to platform.
     * Ordinary wrapper causes remain platform failures, and a cycle cannot
     * recurse forever.
     */
    private fun classifiedCause(error: Throwable): RadioFailure? {
      val seen = HashSet<Throwable>()
      var cause = error.cause
      while (cause != null && seen.add(cause)) {
        when (cause) {
          is RadioPortFailure,
          is OrderedMarkerCancelled,
          is OrderedRegistrationStale,
          is AndroidGattLinkLost,
          is AndroidGattOperationFailure,
          is AndroidGattNotSubmitted,
          is ForegroundServiceControlException,
          is SecurityException,
          is UnsupportedOperationException -> return classify(cause)
        }
        cause = cause.cause
      }
      return null
    }
  }
}
