// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/OwnedOrderedGroupPort.kt
package com.sfourdrinier.unifiedblemanager.rustcore

import android.bluetooth.BluetoothGattCharacteristic
import com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner
import com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedOpenSlots
import com.sfourdrinier.unifiedblemanager.radio.AndroidNativeGraphIdentity
import com.sfourdrinier.unifiedblemanager.radio.OrderedNotificationSource
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidNotificationPath
import com.sfourdrinier.unifiedblemanager.radio.OrderedLocalCleanupStatus
import java.util.UUID

internal class OwnedOrderedGroupPort(private val radio: OwnedAndroidGattRadio) : OrderedGroupPort, OwnerOrderedStoreSnapshotPort {
  private val owner = AndroidOrderedGroupOwner(radio)
  private val quiescence = AndroidOwnerQuiescenceCoordinator()
  init {
    // Native GATT callbacks mutate the source before the adapter relay. Gate
    // that earlier edge so a post-fence callback cannot race a quiescent read.
    radio.setOrderedCallbackAdmission { quiescence.admit("native-notification") }
  }
  private inline fun <T> admitOwnerEffect(domain: String, action: () -> T): T {
    val lease = quiescence.admit(domain) ?: throw IllegalStateException("ordered owner is retiring")
    try { return action() } finally { lease.close() }
  }
  override fun orderedStoreSnapshot() = owner.storeSnapshot()
  private val graphOwner = radio.newGraphOwner(owner.orderedOpenSlots.incarnation)
  internal fun <T> withGraphOwner(action: () -> T): T = radio.withGraphOwner(graphOwner, action)
  internal fun fenceGraphOwner() = radio.withGraphOwner(graphOwner) { radio.fenceGraphOwner() }
  internal fun graphOwnerStatus() = radio.graphOwnerStatus(graphOwner)
  private fun sourceFailure(error: Throwable): RadioFailure {
    val failure = RustRadioHostAdapter.classify(error)
    // Exception messages can contain app or device data; only emit fixed explanations.
    val safeDetail = when {
      error is SecurityException -> "Bluetooth permission denied while maintaining notification source"
      error is IllegalStateException && error.message == "notification delivery may have been interrupted by an abandoned disable" ->
        "notification delivery may have been interrupted by an abandoned disable"
      failure.gattStatus != null -> "Android notification source GATT failure"
      else -> "Android notification source failure"
    }
    val submission = error as? com.sfourdrinier.unifiedblemanager.radio.AndroidCccdSubmissionFailure
    return failure.copy(detail = safeDetail, nativeCode = failure.nativeCode ?: submission?.platformStatus?.toString())
  }
  override fun orderedAdmissionOwner(version: Int): String = when (version) {
    // v3 identifies ordered-open slots. v4 identifies the broader native
    // retirement coordinator; it intentionally has its own incarnation so a
    // control request cannot mistake a v3 slot retirement for v4 teardown.
    3 -> owner.orderedOpenSlots.incarnation.toString()
    4 -> quiescence.owner().toString()
    else -> ""
  }
  override fun ownerQuiescenceCoordinator() = quiescence
  override fun orderedSlots() = owner.orderedOpenSlots
  override fun orderedOpenSlot(identity: AndroidOrderedOpenSlots.Identity, onWakeFailure: (ULong, ULong, Throwable) -> Unit,
                               wake: (ULong, ULong) -> Unit): AndroidOrderedOpenSlots.Result {
    val spec = identity.spec
    val paths = try {
      require(spec.itemCapacity in 1u..Int.MAX_VALUE.toUInt() && spec.byteCapacity in 1u..Int.MAX_VALUE.toUInt())
      spec.members.associate { member ->
        require(member.serviceOccurrence <= Int.MAX_VALUE.toULong() && member.characteristicOccurrence <= Int.MAX_VALUE.toULong())
        member.memberId to OwnedAndroidNotificationPath(UUID.fromString(member.serviceUuid), member.serviceOccurrence.toInt(),
          UUID.fromString(member.characteristicUuid), member.characteristicOccurrence.toInt())
      }
    } catch (error: Throwable) {
      return AndroidOrderedOpenSlots.Result.Failed(error, AndroidOrderedOpenSlots.Ownership.NotCreated)
    }
    return admitOwnerEffect("ordered-open-slot") {
      owner.openSlot(identity.nativeGroupId, identity.peerId, paths, spec.itemCapacity.toInt(), spec.byteCapacity.toInt(),
        onWakeFailure, AndroidNativeGraphIdentity(spec.snapshot.owner, spec.snapshot.attachment.toLong(), spec.snapshot.revision), wake)
    }
  }
  override fun orderedGraphPermitReservation(operation: String, request: OwnedAndroidGattRadio.GraphPermitRequest) =
    if (operation == "reserve") admitOwnerEffect("graph-permit-reservation") {
      radio.withGraphOwner(graphOwner) { radio.reserveGraphPermit(request) }
    } else radio.withGraphOwner(graphOwner) { when (operation) {
    "reserve" -> radio.reserveGraphPermit(request)
    "status" -> radio.statusGraphPermit(request)
    "fence" -> radio.fenceGraphPermit(request)
    "forget" -> { radio.forgetGraphPermitReservation(request); null }
    else -> throw IllegalArgumentException("invalid graph reservation operation")
  } }
  override fun orderedGraphAuthority(operation: String, value: ULong): GraphAuthorityPortResponse =
    if (operation == "reserve" || operation == "open") admitOwnerEffect("graph-authority-$operation") {
      radio.withGraphOwner(graphOwner) { graphAuthority(operation, value) }
    } else radio.withGraphOwner(graphOwner) { graphAuthority(operation, value) }
  private fun graphAuthority(operation: String, value: ULong): GraphAuthorityPortResponse = when (operation) {
    "reserve" -> GraphAuthorityPortResponse.Reservation(radio.reserveGraphAuthority(value))
    "reserve-status" -> GraphAuthorityPortResponse.Reservation(radio.statusGraphAuthorityReservation(value))
    "reserve-fence" -> GraphAuthorityPortResponse.Reservation(radio.fenceGraphAuthorityReservation(value))
    "reserve-forget" -> { radio.forgetGraphAuthorityReservation(value); GraphAuthorityPortResponse.Unit }
    "open" -> GraphAuthorityPortResponse.Open(radio.openGraphAuthority(value))
    "status" -> GraphAuthorityPortResponse.Open(radio.statusGraphAuthorityOpen(value))
    "fence-open" -> GraphAuthorityPortResponse.Open(radio.fenceGraphAuthorityOpen(value))
    "forget-open" -> { radio.forgetGraphAuthorityOpen(value); GraphAuthorityPortResponse.Unit }
    "close" -> { radio.closeGraphAuthority(value); GraphAuthorityPortResponse.Unit }
    "forget" -> { radio.forgetGraphAuthority(value); GraphAuthorityPortResponse.Unit }
    else -> throw IllegalArgumentException("invalid graph authority operation")
  }
  override fun orderedGraphPermit(operation: String, key: OwnedAndroidGattRadio.GraphPermitKey, validateConnection: () -> Boolean) =
    if (operation == "acquire" || operation == "commit") admitOwnerEffect("graph-permit-$operation") {
      radio.withGraphOwner(graphOwner) { graphPermit(operation, key, validateConnection) }
    } else radio.withGraphOwner(graphOwner) { graphPermit(operation, key, validateConnection) }
  private fun graphPermit(operation: String, key: OwnedAndroidGattRadio.GraphPermitKey, validateConnection: () -> Boolean) = when (operation) {
    "acquire" -> radio.acquireGraphPermit(key, validateConnection)
    "commit" -> radio.commitGraphPermit(key)
    "release" -> radio.releaseGraphPermit(key)
    "forget" -> radio.forgetGraphPermit(key)
    else -> throw IllegalArgumentException("invalid graph permit operation")
  }

  private fun registration(member: AndroidOrderedGroupOwner.Registration) = OrderedPortRegistration(member.memberId, member.sourceMember, member.epoch)
  private fun sourceRegistration(member: OrderedPortRegistration) = AndroidOrderedGroupOwner.Registration(member.memberId, member.sourceMember, member.epoch)
  override fun orderedGraphPermitInvalidation(key: OwnedAndroidGattRadio.GraphPermitKey) = radio.withGraphOwner(graphOwner) { radio.graphPermitInvalidation(key) }
  override fun orderedAdmitOpen(group: ULong, request: ULong, peer: String) = admitOwnerEffect("ordered-admit-open") { owner.admitOpen(group, request, peer) }
  override fun orderedAcknowledgeOpen(group: ULong, request: ULong, peer: String) = owner.acknowledgeOpen(group, request, peer)
  override fun orderedRecoverOpenExact(group: ULong, request: ULong, peer: String) = recovery(owner.recoverOpenExact(group, request, peer))
  override fun orderedFenceQueuedOpen(group: ULong, request: ULong, peer: String) = recovery(owner.fenceQueuedOpen(group, request, peer))
  override fun orderedRecoverAdmission(admission: AndroidOrderedGroupOwner.OpenAdmission) = recovery(owner.recoverAdmission(admission))
  override fun orderedFailAdmission(admission: AndroidOrderedGroupOwner.OpenAdmission, error: Throwable) = recovery(owner.failAdmission(admission, error))
  override fun orderedOpenAdmitted(admission: AndroidOrderedGroupOwner.OpenAdmission, graph: AndroidNativeGraphIdentity,
                                   members: Map<ULong, CharacteristicInstance>, itemCapacity: Int, byteCapacity: Int,
                                   onWakeFailure: ((ULong, ULong, Throwable) -> Unit)?, wake: (ULong, ULong) -> Unit): OrderedPortOpen =
    admitOwnerEffect("ordered-open-admitted") { open(admission.group, graph, members, itemCapacity, byteCapacity, onWakeFailure, wake, admission) }
  override fun orderedOpen(group: ULong, graph: AndroidNativeGraphIdentity, members: Map<ULong, CharacteristicInstance>, itemCapacity: Int, byteCapacity: Int,
                           onWakeFailure: ((ULong, ULong, Throwable) -> Unit)?, wake: (ULong, ULong) -> Unit): OrderedPortOpen =
    admitOwnerEffect("ordered-open") { open(group, graph, members, itemCapacity, byteCapacity, onWakeFailure, wake, null) }
  private fun open(group: ULong, graph: AndroidNativeGraphIdentity, members: Map<ULong, CharacteristicInstance>, itemCapacity: Int, byteCapacity: Int,
                   onWakeFailure: ((ULong, ULong, Throwable) -> Unit)?, wake: (ULong, ULong) -> Unit,
                   admission: AndroidOrderedGroupOwner.OpenAdmission?): OrderedPortOpen {
    require(members.isNotEmpty())
    val peer = members.values.first().peerId
    require(members.values.all { it.peerId == peer })
    val paths = members.mapValues { (_, instance) ->
      require(instance.serviceOccurrence in 0..Int.MAX_VALUE.toLong() && instance.characteristicOccurrence in 0..Int.MAX_VALUE.toLong())
      OwnedAndroidNotificationPath(UUID.fromString(instance.serviceUuid), instance.serviceOccurrence.toInt(), UUID.fromString(instance.characteristicUuid), instance.characteristicOccurrence.toInt())
    }
    val opened = if (admission == null) owner.open(group, peer, paths, itemCapacity, byteCapacity, onWakeFailure, graph, wake)
      else owner.openAdmitted(admission, peer, paths, itemCapacity, byteCapacity, onWakeFailure, graph, wake)
    return OrderedPortOpen(opened.generation, opened.members.map(::registration))
  }
  override fun orderedRegister(generation: ULong, group: ULong, member: OrderedPortRegistration) = admitOwnerEffect("ordered-register") { registration(owner.register(generation, group, sourceRegistration(member))) }
  override fun orderedRecoverOpen(group: ULong): OrderedPortOpenRecovery = recovery(owner.recoverOpen(group))
  private fun recovery(recovery: AndroidOrderedGroupOwner.OpenRecovery): OrderedPortOpenRecovery = when (recovery) {
    AndroidOrderedGroupOwner.OpenRecovery.NotCreated -> OrderedPortOpenRecovery.NotCreated
    AndroidOrderedGroupOwner.OpenRecovery.Indeterminate -> OrderedPortOpenRecovery.Indeterminate
    is AndroidOrderedGroupOwner.OpenRecovery.Created -> OrderedPortOpenRecovery.Created(
      OrderedPortOpen(recovery.opened.generation, recovery.opened.members.map(::registration)))
    is AndroidOrderedGroupOwner.OpenRecovery.Failed -> OrderedPortOpenRecovery.Failed(recovery.error, recovery(recovery.ownership))
  }
  override fun orderedRetire(generation: ULong, group: ULong, member: OrderedPortRegistration) { check(owner.retire(generation, group, sourceRegistration(member))) { "ordered fence could not be retained" } }
  override fun orderedBarrier(generation: ULong, group: ULong, barrierId: ULong) = admitOwnerEffect("ordered-barrier") { check(owner.barrier(generation, group, barrierId)) { "ordered barrier could not be retained" } }
  override fun orderedRetainClaim(generation: ULong, group: ULong, member: OrderedPortRegistration) = admitOwnerEffect("ordered-retain-claim") {
    registration(owner.retainClaim(generation, group, sourceRegistration(member)))
  }
  override fun orderedHasClaim(generation: ULong, group: ULong, member: OrderedPortRegistration, instance: CharacteristicInstance): Boolean {
    require(instance.serviceOccurrence in 0..Int.MAX_VALUE.toLong() && instance.characteristicOccurrence in 0..Int.MAX_VALUE.toLong())
    val path = OwnedAndroidNotificationPath(UUID.fromString(instance.serviceUuid), instance.serviceOccurrence.toInt(),
      UUID.fromString(instance.characteristicUuid), instance.characteristicOccurrence.toInt())
    return owner.hasClaim(generation, group, sourceRegistration(member), instance.peerId, path)
  }
  override fun orderedAdmission(generation: ULong, group: ULong, member: OrderedPortRegistration, instance: CharacteristicInstance): com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedAdmission {
    require(instance.serviceOccurrence in 0..Int.MAX_VALUE.toLong() && instance.characteristicOccurrence in 0..Int.MAX_VALUE.toLong())
    val path = OwnedAndroidNotificationPath(UUID.fromString(instance.serviceUuid), instance.serviceOccurrence.toInt(),
      UUID.fromString(instance.characteristicUuid), instance.characteristicOccurrence.toInt())
    return owner.admission(generation, group, sourceRegistration(member), instance.peerId, path)
  }
  override fun orderedRetryLocalCleanup(generation: ULong, group: ULong, instance: CharacteristicInstance, epoch: Long): OrderedLocalCleanupStatus {
    require(instance.serviceOccurrence in 0..Int.MAX_VALUE.toLong() && instance.characteristicOccurrence in 0..Int.MAX_VALUE.toLong())
    val path = OwnedAndroidNotificationPath(UUID.fromString(instance.serviceUuid), instance.serviceOccurrence.toInt(),
      UUID.fromString(instance.characteristicUuid), instance.characteristicOccurrence.toInt())
    return radio.retryOrderedLocalCleanup(instance.peerId, path, owner.attachment(generation, group), epoch)
  }
  override fun orderedReserveMarkerWrite(generation: ULong, group: ULong, markerId: ULong, connectionGeneration: String,
                                         graph: AndroidNativeGraphIdentity, instance: CharacteristicInstance, value: ByteArray,
                                         withResponse: Boolean, cancellation: com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation): com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.Reservation {
    require(instance.serviceOccurrence in 0..Int.MAX_VALUE.toLong() && instance.characteristicOccurrence in 0..Int.MAX_VALUE.toLong())
    val path = OwnedAndroidNotificationPath(UUID.fromString(instance.serviceUuid), instance.serviceOccurrence.toInt(),
      UUID.fromString(instance.characteristicUuid), instance.characteristicOccurrence.toInt())
    return admitOwnerEffect("marker-reservation") {
      owner.reserveMarkerWrite(generation, group, markerId, instance.peerId, graph, connectionGeneration, path, value, withResponse, cancellation)
    }
  }
  override fun orderedCancelMarkerWrite(generation: ULong, group: ULong, markerId: ULong, error: Throwable) = owner.cancelMarkerWrite(generation, group, markerId, error)
  override fun orderedMarkerWrite(generation: ULong, group: ULong, markerId: ULong, connectionGeneration: String,
                                  graph: AndroidNativeGraphIdentity, instance: CharacteristicInstance, value: ByteArray,
                                  withResponse: Boolean, validate: () -> Unit, cancellation: com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation, result: (com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State) -> Unit): Long? {
    require(instance.serviceOccurrence in 0..Int.MAX_VALUE.toLong() && instance.characteristicOccurrence in 0..Int.MAX_VALUE.toLong())
    val path = OwnedAndroidNotificationPath(UUID.fromString(instance.serviceUuid), instance.serviceOccurrence.toInt(),
      UUID.fromString(instance.characteristicUuid), instance.characteristicOccurrence.toInt())
    return admitOwnerEffect("marker-write") {
      owner.markerWrite(generation, group, markerId, instance.peerId, graph, connectionGeneration, path, value, withResponse, validate, cancellation, preReserved = true, result = result)
    }
  }
  override fun orderedCancelMarkerWriteExact(generation: ULong, group: ULong, markerId: ULong) = owner.cancelMarkerWriteExact(generation, group, markerId)
  override fun orderedMarkerWriteSettlement(generation: ULong, group: ULong, markerId: ULong) = owner.markerWriteSettlement(generation, group, markerId)
  override fun orderedForgetMarkerWriteReceipt(generation: ULong, group: ULong, markerId: ULong) {
    check(owner.forgetMarkerWriteReceipt(generation, group, markerId)) { "marker write receipt still has pending outcome or source evidence" }
  }
  override fun orderedConfirmCleanup(generation: ULong, group: ULong, member: OrderedPortRegistration) = owner.confirmCleanup(generation, group, sourceRegistration(member))
  override fun orderedAcknowledge(generation: ULong, group: ULong, batch: ULong) = owner.acknowledge(generation, group, batch)
  override fun orderedClose(generation: ULong, group: ULong) = owner.close(generation, group)
  override fun orderedSeal(generation: ULong, group: ULong) = owner.seal(generation, group)
  override fun orderedRemove(generation: ULong, group: ULong) { check(owner.remove(generation, group)) { "ordered group still has retained ownership" } }
  override fun orderedForgetRemovalReceipt(generation: ULong, group: ULong) = owner.forgetRemovalReceipt(generation, group)
  override fun orderedDrain(generation: ULong, group: ULong): OrderedPortBatch {
    val batch = owner.drain(generation, group)
    fun member(characteristic: BluetoothGattCharacteristic, source: ULong, epoch: ULong) =
      OrderedPortRegistration(owner.memberId(generation, group, characteristic), source, epoch)
    fun loss(event: OrderedNotificationSource.Event.Loss<BluetoothGattCharacteristic>): OrderedPortLoss {
      val (scope, id) = when (val value = event.scope) {
        is OrderedNotificationSource.LossScope.Member -> "member" to owner.memberId(generation, group, value.member)
        is OrderedNotificationSource.LossScope.Group -> "group" to null
        is OrderedNotificationSource.LossScope.Unknown -> "unknown" to null
      }
      return OrderedPortLoss(scope, id, when (event.unit) {
        OrderedNotificationSource.LossUnit.VALUES -> "values"
        OrderedNotificationSource.LossUnit.SOURCE_RECORDS -> "source-records"
      }, event.count, event.bytes)
    }
    val records = batch.events.flatMap { event ->
      when (event) {
        is OrderedNotificationSource.Event.Value -> listOf(OrderedPortRecord.Value(member(event.member, event.sourceMember, event.epoch), event.ordinal, event.bytes))
        is OrderedNotificationSource.Event.Fence -> listOf(OrderedPortRecord.Fence(member(event.member, event.sourceMember, event.epoch), true))
        is OrderedNotificationSource.Event.Barrier -> listOf(OrderedPortRecord.Barrier(event.barrierId, true))
        is OrderedNotificationSource.Event.Marker -> listOf(OrderedPortRecord.Marker(event.markerId, event.ordinal))
        is OrderedNotificationSource.Event.Loss -> listOf(OrderedPortRecord.Loss(loss(event)))
        is OrderedNotificationSource.Event.SubsequentFailure -> {
          val retained = event.failure
          val records = mutableListOf<OrderedPortRecord>()
          if (retained != null) {
            records.add(OrderedPortRecord.SubsequentSourceFailure(sourceFailure(retained.cause)))
          }
          if (event.overflow) records.add(OrderedPortRecord.FailureObservationsOmitted)
          check(records.isNotEmpty()) { "empty subsequent source failure observation" }
          records
        }
        is OrderedNotificationSource.Event.Terminal -> {
          val rejected = when {
            event.rejectedValue != null -> event.rejectedValue.let { OrderedPortRejected.Value(member(it.member, it.sourceMember, it.epoch), it.bytes) }
            event.rejectedLoss != null -> OrderedPortRejected.Loss(loss(event.rejectedLoss))
            event.rejectedFence != null -> event.rejectedFence.let { OrderedPortRejected.Fence(member(it.member, it.sourceMember, it.epoch)) }
            event.rejectedMarker != null -> OrderedPortRejected.Marker(event.rejectedMarker)
            event.rejectedBarrier != null -> OrderedPortRejected.Barrier(event.rejectedBarrier)
            else -> null
          }
          val cause = event.sourceFailure?.cause
          val classified = cause?.let(::sourceFailure)
          val nativeCode = classified?.nativeCode ?: classified?.gattStatus?.toString()
          val terminal = OrderedPortRecord.Terminal(when (event.reason) {
            OrderedNotificationSource.Reason.CLOSED -> "closed"
            OrderedNotificationSource.Reason.CONNECTION_LOST -> "connection-lost"
            OrderedNotificationSource.Reason.DATABASE_CHANGED -> "service-changed"
            OrderedNotificationSource.Reason.OVERFLOW -> "overflow"
            OrderedNotificationSource.Reason.ORDINAL_EXHAUSTED -> "ordinal-exhausted"
            OrderedNotificationSource.Reason.WAKE_FAILED -> "wake-failed"
            OrderedNotificationSource.Reason.SOURCE_FAILED -> "source-failed"
          }, event.uncertainTail, rejected, event.wakeFailed,
            nativeDomain = nativeCode?.let { "android" },
            nativeCode = nativeCode,
            failure = classified,
            failureMember = event.sourceFailure?.let { failure ->
              member(failure.member, failure.membership.sourceMember, failure.membership.epoch)
            })
          if (event.wakeFailed && event.reason != OrderedNotificationSource.Reason.WAKE_FAILED)
            listOf(terminal, OrderedPortRecord.BridgeFailure("wake-failed", "ordered source wake failed")) else listOf(terminal)
        }
      }
    }
    return OrderedPortBatch(batch.id, records)
  }
}
