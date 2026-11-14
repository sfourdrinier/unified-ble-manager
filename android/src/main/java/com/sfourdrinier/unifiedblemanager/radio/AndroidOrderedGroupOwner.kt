// android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/AndroidOrderedGroupOwner.kt
package com.sfourdrinier.unifiedblemanager.radio

import android.bluetooth.BluetoothGattCharacteristic

/** Retains source routes and destructive drain batches until the Rust owner acknowledges them. */
internal class AndroidOrderedGroupOwner(private val radio: OwnedAndroidGattRadio) {
  internal data class Registration(val memberId: ULong, val sourceMember: ULong, val epoch: ULong)
  internal data class Opened(val generation: ULong, val members: List<Registration>)
  internal sealed interface OpenRecovery {
    data object NotCreated : OpenRecovery
    data object Indeterminate : OpenRecovery
    data class Created(val opened: Opened) : OpenRecovery
    data class Failed(val error: Throwable, val ownership: OpenRecovery) : OpenRecovery
  }
  internal data class Batch(val id: ULong?, val events: List<OrderedNotificationSource.Event<BluetoothGattCharacteristic>>)
  private class Route(
    val opened: OrderedAndroidSourceOpen,
    val deviceId: String,
    val graph: AndroidNativeGraphIdentity?,
    val members: Map<ULong, BluetoothGattCharacteristic>,
    val registrations: List<Registration>
  ) {
    val markers = OrderedMarkerTransactions()
    var batch: Batch? = null
    var lastAcknowledged: ULong? = null
    var nextBatch = 0uL
    var batchAvailable = true
  }
  // The table and its incarnation share the source owner's lifetime.
  internal val orderedOpenSlots = AndroidOrderedOpenSlots()
  private val lock = Any()
  internal class OpenAdmission internal constructor(internal val namespace: Any, val group: ULong, val request: ULong, val peer: String) {
    internal var fenced = false
    internal var started = false
    internal var settled = false
    internal var outcome: OpenRecovery? = null
  }
  private val openNamespace = Any()
  private val openAdmissions = mutableMapOf<ULong, OpenAdmission>()
  private data class OpenIdentity(val group: ULong, val request: ULong, val peer: String)
  private val acknowledgedOpens = mutableSetOf<OpenIdentity>()
  /** Exact recoveries which won before their queued legacy adapter work entered this owner. */
  private val fencedQueuedOpens = mutableSetOf<OpenIdentity>()
  private val routes = mutableMapOf<Pair<ULong, ULong>, Route>()
  private data class RemovalReceipt(val generation: ULong, var removed: Boolean = false)
  private val receipts = mutableMapOf<ULong, RemovalReceipt?>()
  private data class SavedOpen(val source: OrderedNotificationSource<BluetoothGattCharacteristic>,
                               val group: OrderedNotificationSource<BluetoothGattCharacteristic>.Group,
                               val resolved: Map<OwnedAndroidNotificationPath, BluetoothGattCharacteristic>,
                               val memberships: Map<BluetoothGattCharacteristic, OrderedNotificationSource.Membership>,
                               val deviceId: String, val members: Map<ULong, OwnedAndroidNotificationPath>,
                               val graph: AndroidNativeGraphIdentity?) {
    fun opened() = OrderedAndroidSourceOpen(source, group,
      resolved.mapValues { (_, characteristic) -> characteristic to memberships.getValue(characteristic) })
  }
  private val installed = mutableMapOf<ULong, SavedOpen>()
  private val receiptCapacity = 64
  internal var onInstalledForTest: (() -> Unit)? = null

  /** This store's accounting only; it does not certify native or adapter quiescence. */
  internal data class StoreSnapshot(
    val owner: java.util.UUID,
    val openAdmissions: Int,
    val acknowledgedOpens: Int,
    val routes: Int,
    val installed: Int,
    val batches: Int,
    val markerReceipts: Int,
    val removalReceipts: Int,
    val uncertainRemovals: Int
  )

  internal fun storeSnapshot(): StoreSnapshot = synchronized(lock) {
    StoreSnapshot(orderedOpenSlots.incarnation, openAdmissions.size, acknowledgedOpens.size,
      routes.size, installed.size, routes.values.count { it.batch != null },
      routes.values.sumOf { it.markers.retainedCount() }, receipts.size,
      receipts.values.count { it == null })
  }

  /** An exact open identity cannot be reused during this owner's lifetime. */
  fun admitOpen(group: ULong, request: ULong, peer: String): OpenAdmission {
    fun existing(): OpenAdmission? = openAdmissions[group]?.also {
      check(it.request == request && it.peer == peer.uppercase()) { "ordered open identity does not match" }
    }
    synchronized(lock) { existing()?.let { return it } }
    // Count creation before reacquiring the group monitor; fencing never waits
    // on a monitor that can be held across native work or a foreign callback.
    val work = orderedOpenSlots.reserveEffectWork()
    if (work == null) return synchronized(lock) { existing() ?: error("ordered owner is retiring") }
    try {
      return synchronized(lock) {
        existing()?.let { return@synchronized it }
        check(!receipts.containsKey(group)) { "ordered group ID is already in use" }
        val identity = OpenIdentity(group, request, peer.uppercase())
        check(identity !in acknowledgedOpens) { "ordered open identity was already acknowledged" }
        if (identity in fencedQueuedOpens) throw OrderedOpenNotCreated(IllegalStateException("ordered open was recovered before native admission"))
        check(openAdmissions.size < receiptCapacity) { "ordered open receipt capacity exhausted" }
        OpenAdmission(openNamespace, group, request, peer.uppercase()).also { openAdmissions[group] = it }
      }
    } finally {
      orderedOpenSlots.finishEffectWork(work)
    }
  }

  fun acknowledgeOpen(group: ULong, request: ULong, peer: String) = synchronized(lock) {
    val identity = OpenIdentity(group, request, peer.uppercase())
    if (identity in acknowledgedOpens) return
    if (identity in fencedQueuedOpens) {
      fencedQueuedOpens.remove(identity)
      acknowledgedOpens.add(identity)
      return
    }
    val admission = checkNotNull(openAdmissions[group]) { "ordered open identity is unknown" }
    check(admission.request == request && admission.peer == peer.uppercase()) { "ordered open identity does not match" }
    check(admission.fenced || admission.settled) { "ordered open is still pending" }
    val outcome = recoverOpen(group)
    check(outcome != OpenRecovery.Indeterminate && (outcome !is OpenRecovery.Failed || outcome.ownership != OpenRecovery.Indeterminate)) { "ordered open effect is unresolved" }
    acknowledgedOpens.add(identity)
    openAdmissions.remove(group)
    Unit
  }

  fun recoverOpenExact(group: ULong, request: ULong, peer: String): OpenRecovery = synchronized(lock) {
    if (OpenIdentity(group, request, peer.uppercase()) in fencedQueuedOpens) return OpenRecovery.NotCreated
    val admission = openAdmissions[group] ?: return OpenRecovery.Indeterminate
    check(admission.request == request && admission.peer == peer.uppercase()) { "ordered open identity does not match" }
    recoverOpen(group)
  }

  /**
   * Records a recovery that beats legacy work before it reached the native
   * owner.  This is deliberately an exact, acknowledgement-backed receipt:
   * it neither guesses about another request nor permits group reuse.
   */
  fun fenceQueuedOpen(group: ULong, request: ULong, peer: String): OpenRecovery = synchronized(lock) {
    val identity = OpenIdentity(group, request, peer.uppercase())
    val admission = openAdmissions[group]
    if (admission != null) {
      check(admission.request == request && admission.peer == peer.uppercase()) { "ordered open identity does not match" }
      return recoverOpen(group)
    }
    if (identity in acknowledgedOpens) return OpenRecovery.NotCreated
    if (fencedQueuedOpens.none { it.group == group } && fencedQueuedOpens.size < receiptCapacity) {
      fencedQueuedOpens.add(identity)
      return OpenRecovery.NotCreated
    }
    if (identity in fencedQueuedOpens) return OpenRecovery.NotCreated
    OpenRecovery.Indeterminate
  }
  fun recoverAdmission(admission: OpenAdmission): OpenRecovery = synchronized(lock) {
    check(admission.namespace === openNamespace) { "ordered open belongs to another host" }
    admission.outcome?.let { return it }
    if (openAdmissions[admission.group] !== admission) return OpenRecovery.Indeterminate
    recoverOpen(admission.group)
  }

  /** Settle an adapter-side failure against the captured admission, never a later group reuse. */
  fun failAdmission(admission: OpenAdmission, error: Throwable): OpenRecovery = synchronized(lock) {
    check(admission.namespace === openNamespace) { "ordered open belongs to another host" }
    admission.outcome?.let { if (it is OpenRecovery.Failed || it is OpenRecovery.Created) return it }
    if (openAdmissions[admission.group] !== admission) return admission.outcome ?: OpenRecovery.Indeterminate
    val ownership = recoverOpen(admission.group)
    OpenRecovery.Failed(error, ownership).also { admission.outcome = it }
  }

  fun openAdmitted(admission: OpenAdmission, deviceId: String, members: Map<ULong, OwnedAndroidNotificationPath>,
                   capacity: Int, byteCapacity: Int, onWakeFailure: ((ULong, ULong, Throwable) -> Unit)? = null,
                   expectedGraph: AndroidNativeGraphIdentity? = null, wake: (ULong, ULong) -> Unit): Opened = synchronized(lock) {
    check(admission.namespace === openNamespace) { "ordered open belongs to another host" }
    check(admission.peer == deviceId.uppercase()) { "ordered open peer does not match" }
    if (admission.fenced) throw OrderedOpenNotCreated(IllegalStateException("ordered open was fenced"))
    check(openAdmissions[admission.group] === admission && !admission.started) { "ordered open admission is not current" }
    admission.started = true
    try {
      openInternal(admission.group, deviceId, members, capacity, byteCapacity, onWakeFailure, expectedGraph, wake).also {
        admission.outcome = OpenRecovery.Created(it)
      }
    } catch (error: Throwable) {
      admission.settled = true
      val ownership = recoverOpen(admission.group)
      admission.outcome = OpenRecovery.Failed(error, ownership)
      throw error
    } finally {
      admission.settled = true
    }
  }

  fun open(groupId: ULong, deviceId: String, members: Map<ULong, OwnedAndroidNotificationPath>,
           capacity: Int, byteCapacity: Int, onWakeFailure: ((ULong, ULong, Throwable) -> Unit)? = null,
           expectedGraph: AndroidNativeGraphIdentity? = null,
           wake: (ULong, ULong) -> Unit): Opened = synchronized(lock) {
    check(groupId !in openAdmissions) { "ordered group admission is pending" }
    openInternal(groupId, deviceId, members, capacity, byteCapacity, onWakeFailure, expectedGraph, wake)
  }

  /** Installs under the source-owner lock, independently of the slot control lock. */
  fun openSlot(groupId: ULong, deviceId: String, members: Map<ULong, OwnedAndroidNotificationPath>,
               capacity: Int, byteCapacity: Int, onWakeFailure: ((ULong, ULong, Throwable) -> Unit)?,
               graph: AndroidNativeGraphIdentity, wake: (ULong, ULong) -> Unit): AndroidOrderedOpenSlots.Result = synchronized(lock) {
    if (groupId in openAdmissions || receipts.containsKey(groupId)) {
      return AndroidOrderedOpenSlots.Result.Failed(IllegalStateException("ordered group ID is already in use"), AndroidOrderedOpenSlots.Ownership.NotCreated)
    }
    try {
      AndroidOrderedOpenSlots.Result.Opened(openInternal(groupId, deviceId, members, capacity, byteCapacity, onWakeFailure, graph, wake))
    } catch (error: Throwable) {
      val ownership = recoverOpen(groupId)
      when (ownership) {
        is OpenRecovery.Created -> AndroidOrderedOpenSlots.Result.Failed(error, AndroidOrderedOpenSlots.Ownership.Created(ownership.opened))
        else -> if (error is OrderedRegistrationStale || error is OrderedOpenNotCreated || !receipts.containsKey(groupId))
          AndroidOrderedOpenSlots.Result.Failed(error, AndroidOrderedOpenSlots.Ownership.NotCreated)
        else AndroidOrderedOpenSlots.Result.Indeterminate(error)
      }
    }
  }

  private fun openInternal(groupId: ULong, deviceId: String, members: Map<ULong, OwnedAndroidNotificationPath>,
                           capacity: Int, byteCapacity: Int, onWakeFailure: ((ULong, ULong, Throwable) -> Unit)?,
                           expectedGraph: AndroidNativeGraphIdentity?, wake: (ULong, ULong) -> Unit): Opened {
    require(members.isNotEmpty() && members.values.toSet().size == members.size)
    check(!receipts.containsKey(groupId)) { "ordered group ID is already in use" }
    check(receipts.size < receiptCapacity) { "ordered removal receipt capacity exhausted" }
    receipts[groupId] = null
    val opened = try {
      radio.openOrderedSourceForPeer(deviceId, members.values.toSet(), capacity, byteCapacity, groupId, onWakeFailure, expectedGraph,
        onInstalled = { source, group, resolved, memberships ->
          installed[groupId] = SavedOpen(source, group, resolved, memberships, deviceId, members, expectedGraph)
          onInstalledForTest?.invoke()
        }) { generation ->
        wake(generation, groupId)
      }
    } catch (error: Throwable) {
      if (!installed.containsKey(groupId) && (error is OrderedRegistrationStale || error is OrderedOpenNotCreated)) receipts.remove(groupId)
      throw error
    }
    val key = opened.source.sourceGeneration to groupId
    check(key !in routes)
    val resolved = members.mapValues { (_, path) -> opened.members.getValue(path).first }
    val registrations = members.map { (id, path) ->
      val membership = opened.members.getValue(path).second
      Registration(id, membership.sourceMember, membership.epoch)
    }
    routes[key] = Route(opened, deviceId.uppercase(), expectedGraph, resolved, registrations)
    receipts[groupId] = RemovalReceipt(key.first)
    return Opened(key.first, registrations)
  }

  fun recoverOpen(groupId: ULong): OpenRecovery = synchronized(lock) {
    val admission = openAdmissions[groupId]
    admission?.let {
      it.outcome?.let { outcome -> return outcome }
      if (!admission.started) admission.fenced = true
      if (admission.fenced || (admission.settled && !receipts.containsKey(groupId))) {
        admission.outcome = OpenRecovery.NotCreated
        return OpenRecovery.NotCreated
      }
    }
    // The adapter may have accepted an open which has not entered this
    // owner yet. A missing local receipt cannot fence that queued work.
    if (!receipts.containsKey(groupId)) return OpenRecovery.Indeterminate
    var receipt = receipts[groupId]
    if (receipt == null) {
      val saved = installed[groupId] ?: return OpenRecovery.Indeterminate
      val source = saved.opened()
      val deviceId = saved.deviceId
      val members = saved.members
      val key = source.source.sourceGeneration to groupId
      val resolved = members.mapValues { (_, path) -> source.members.getValue(path).first }
      val registrations = members.map { (id, path) ->
        val membership = source.members.getValue(path).second
        Registration(id, membership.sourceMember, membership.epoch)
      }
      routes[key] = Route(source, deviceId.uppercase(), saved.graph, resolved, registrations)
      receipt = RemovalReceipt(key.first)
      receipts[groupId] = receipt
    }
    if (receipt.removed) return OpenRecovery.NotCreated
    val route = route(receipt.generation, groupId)
    OpenRecovery.Created(Opened(receipt.generation, route.registrations)).also { admission?.outcome = it }
  }

  private fun route(generation: ULong, groupId: ULong): Route =
    checkNotNull(routes[generation to groupId]) { "ordered group identity is not current" }

  fun memberId(generation: ULong, groupId: ULong, characteristic: BluetoothGattCharacteristic): ULong = synchronized(lock) {
    route(generation, groupId).members.entries.single { it.value === characteristic }.key
  }

  fun register(generation: ULong, groupId: ULong, expected: Registration): Registration = synchronized(lock) {
    val route = route(generation, groupId)
    val membership = route.opened.source.register(route.opened.group, route.members.getValue(expected.memberId),
      OrderedNotificationSource.Membership(expected.sourceMember, expected.epoch))
    Registration(expected.memberId, membership.sourceMember, membership.epoch)
  }

  fun retire(generation: ULong, groupId: ULong, expected: Registration): Boolean {
    val route = synchronized(lock) { route(generation, groupId) }
    // Source membership validation serializes with removal; wake runs without
    // the group owner monitor so it may safely re-enter the host.
    return route.opened.source.retire(route.opened.group, route.members.getValue(expected.memberId),
      OrderedNotificationSource.Membership(expected.sourceMember, expected.epoch))
  }

  fun barrier(generation: ULong, groupId: ULong, barrierId: ULong): Boolean {
    val opened = synchronized(lock) { route(generation, groupId).opened }
    return opened.source.barrier(opened.group, barrierId)
  }

  /** Reserves exact identity before an executor can cancel or reject dispatch. */
  fun reserveMarkerWrite(generation: ULong, groupId: ULong, markerId: ULong, deviceId: String,
                         graph: AndroidNativeGraphIdentity, connectionGeneration: String,
                         path: OwnedAndroidNotificationPath, value: ByteArray, withResponse: Boolean,
                         cancellation: OrderedMarkerCancellation = OrderedMarkerCancellation()): OrderedMarkerTransactions.Reservation = synchronized(lock) {
    val route = route(generation, groupId)
    check(route.deviceId == deviceId.uppercase() && route.graph == graph)
    require(connectionGeneration.isNotEmpty())
    route.markers.reserve(markerId, OrderedMarkerTransactions.Identity(graph, connectionGeneration, path, value.copyOf(), withResponse), cancellation)
  }
  fun cancelMarkerWrite(generation: ULong, groupId: ULong, markerId: ULong, error: Throwable): OrderedMarkerTransactions.State = synchronized(lock) {
    val markers = route(generation, groupId).markers
    val state = markers.cancel(markerId, error)
    markers.adapterSettledBeforeSchedule(markerId)
    state
  }
  fun markerWrite(generation: ULong, groupId: ULong, markerId: ULong, deviceId: String,
                  graph: AndroidNativeGraphIdentity, connectionGeneration: String,
                  path: OwnedAndroidNotificationPath, value: ByteArray, withResponse: Boolean,
                  validate: () -> Unit = {}, cancellation: OrderedMarkerCancellation = OrderedMarkerCancellation(),
                  preReserved: Boolean = false, result: (OrderedMarkerTransactions.State) -> Unit): Long? {
    val ownedValue = value.copyOf()
    val reservation = reserveMarkerWrite(generation, groupId, markerId, deviceId, graph, connectionGeneration, path, ownedValue, withResponse, cancellation)
    val route = synchronized(lock) { route(generation, groupId) }
    if ((!reservation.created && !preReserved) || !route.markers.schedule(markerId)) {
      if (preReserved) route.markers.adapterSettledBeforeSchedule(markerId)
      result(route.markers.state(markerId) ?: reservation.state)
      return null
    }
    val admission = AndroidOrderedMarkerAdmission(graph, route.opened.source, route.opened.group)
    return try {
      validate()
      val operation = radio.writeAfterOrderedMarker(deviceId, path, ownedValue, withResponse, markerId, admission, route.markers, reservation.cancellation, result)
      if (route.markers.bindOperation(markerId, operation)) radio.cancelOperation(operation)
      operation
    } catch (error: Throwable) {
      route.markers.driverSettled(markerId)
      result(route.markers.cancel(markerId, error))
      null
    }
  }
  fun cancelMarkerWriteExact(generation: ULong, groupId: ULong, markerId: ULong): OrderedMarkerTransactions.State {
    val markers = synchronized(lock) { route(generation, groupId).markers }
    val (state, operation) = markers.cancelExact(markerId, OrderedMarkerCancelled())
    if (operation != null) radio.cancelOperation(operation)
    return markers.state(markerId) ?: state
  }
  fun markerWriteSettlement(generation: ULong, groupId: ULong, markerId: ULong): OrderedMarkerTransactions.State = synchronized(lock) {
    checkNotNull(route(generation, groupId).markers.state(markerId)) { "marker write receipt is unavailable" }
  }
  fun forgetMarkerWriteReceipt(generation: ULong, groupId: ULong, markerId: ULong): Boolean = synchronized(lock) {
    route(generation, groupId).markers.forget(markerId)
  }

  fun retainClaim(generation: ULong, groupId: ULong, expected: Registration): Registration = synchronized(lock) {
    val route = route(generation, groupId)
    val claim = route.opened.source.retainPhysicalClaim(route.opened.group, route.members.getValue(expected.memberId),
      OrderedNotificationSource.Membership(expected.sourceMember, expected.epoch))
    Registration(expected.memberId, claim.membership.sourceMember, claim.membership.epoch)
  }
  fun hasClaim(generation: ULong, groupId: ULong, expected: Registration, deviceId: String, path: OwnedAndroidNotificationPath): Boolean = synchronized(lock) {
    val route = route(generation, groupId)
    if (route.deviceId != deviceId.uppercase()) return false
    val characteristic = route.members.getValue(expected.memberId)
    if (route.opened.members[path]?.first !== characteristic) return false
    route.opened.source.hasPhysicalClaim(route.opened.group, characteristic,
      OrderedNotificationSource.Membership(expected.sourceMember, expected.epoch))
  }
  fun admission(generation: ULong, groupId: ULong, expected: Registration, deviceId: String, path: OwnedAndroidNotificationPath): AndroidOrderedAdmission = synchronized(lock) {
    val route = route(generation, groupId)
    check(route.deviceId == deviceId.uppercase())
    val characteristic = route.members.getValue(expected.memberId)
    check(route.opened.members[path]?.first === characteristic)
    val graph = checkNotNull(route.graph) { "ordered graph identity is unavailable" }
    AndroidOrderedAdmission(graph, route.opened.source, route.opened.group, characteristic,
      OrderedNotificationSource.Membership(expected.sourceMember, expected.epoch))
  }
  fun attachment(generation: ULong, groupId: ULong): Long = synchronized(lock) { route(generation, groupId).opened.source.generation }

  fun confirmCleanup(generation: ULong, groupId: ULong, member: Registration) = synchronized(lock) {
    val route = route(generation, groupId)
    val claim = OrderedNotificationSource.PhysicalClaim(generation, route.opened.group.id,
      route.members.getValue(member.memberId), OrderedNotificationSource.Membership(member.sourceMember, member.epoch))
    route.opened.source.confirmPhysicalCleanup(route.opened.group, claim)
  }

  fun drain(generation: ULong, groupId: ULong): Batch = synchronized(lock) {
    val route = route(generation, groupId)
    route.batch?.let { return it }
    check(route.batchAvailable) { "ordered batch identity exhausted" }
    val events = route.opened.source.drain(route.opened.group)
    if (events.isEmpty()) return Batch(null, emptyList())
    val id = route.nextBatch
    if (id == ULong.MAX_VALUE) route.batchAvailable = false else route.nextBatch++
    Batch(id, events).also { route.batch = it }
  }

  fun acknowledge(generation: ULong, groupId: ULong, batchId: ULong) {
    val opened = synchronized(lock) {
      val route = route(generation, groupId)
      if (route.lastAcknowledged != batchId) {
        check(route.batch?.id == batchId) { "ordered batch identity does not match" }
        route.batch?.events?.forEach { event ->
          if (event is OrderedNotificationSource.Event.Barrier) route.opened.source.acknowledgeBarrier(route.opened.group, event.barrierId)
          if (event is OrderedNotificationSource.Event.Marker) route.markers.acknowledgeEvidence(event.markerId)
          if (event is OrderedNotificationSource.Event.Terminal) event.rejectedMarker?.let { route.markers.acknowledgeEvidence(it) }
        }
        route.batch = null
        route.lastAcknowledged = batchId
      }
      route.opened
    }
    opened.source.wakeIfPending(opened.group)
  }

  fun close(generation: ULong, groupId: ULong) {
    val opened = synchronized(lock) { route(generation, groupId).opened }
    // A concurrent removal can only follow source termination and drain;
    // close is then already satisfied for this never-reused group identity.
    opened.source.close(opened.group)
  }
  fun seal(generation: ULong, groupId: ULong) = synchronized(lock) {
    val route = route(generation, groupId)
    route.opened.source.seal(route.opened.group)
  }

  fun remove(generation: ULong, groupId: ULong): Boolean = synchronized(lock) {
    val key = generation to groupId
    val receipt = checkNotNull(receipts[groupId]) { "ordered group receipt is not current" }
    check(receipt.generation == generation) { "ordered group generation does not match" }
    if (receipt.removed) return true
    val route = route(generation, groupId)
    if (route.batch != null || !route.markers.isEmpty() || !route.opened.source.remove(route.opened.group)) return false
    routes.remove(key)
    installed.remove(groupId)
    receipt.removed = true
    true
  }

  fun forgetRemovalReceipt(generation: ULong, groupId: ULong) = synchronized(lock) {
    if (!receipts.containsKey(groupId)) return
    val receipt = checkNotNull(receipts[groupId]) { "ordered group opening is in progress" }
    check(receipt.generation == generation) { "ordered group generation does not match" }
    check(receipt.removed) { "ordered group is still live" }
    receipts.remove(groupId)
    Unit
  }
}
