// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/OrderedGroupPort.kt
package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.radio.AndroidNativeGraphIdentity
import com.sfourdrinier.unifiedblemanager.radio.OrderedLocalCleanupStatus
import com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedAdmission
import com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedOpenSlots

internal data class OrderedPortRegistration(val memberId: ULong, val sourceMember: ULong, val epoch: ULong)
internal data class OrderedPortOpen(val generation: ULong, val members: List<OrderedPortRegistration>)
internal sealed interface OrderedPortOpenRecovery {
  data object NotCreated : OrderedPortOpenRecovery
  data object Indeterminate : OrderedPortOpenRecovery
  data class Created(val opened: OrderedPortOpen) : OrderedPortOpenRecovery
  data class Failed(val error: Throwable, val ownership: OrderedPortOpenRecovery) : OrderedPortOpenRecovery
}
internal data class OrderedPortLoss(val scope: String, val memberId: ULong?, val unit: String, val count: ULong?, val bytes: ULong?)
internal sealed interface OrderedPortRejected {
  data class Value(val member: OrderedPortRegistration, val bytes: ULong) : OrderedPortRejected
  data class Loss(val loss: OrderedPortLoss) : OrderedPortRejected
  data class Fence(val member: OrderedPortRegistration) : OrderedPortRejected
  data class Barrier(val barrierId: ULong) : OrderedPortRejected
  data class Marker(val markerId: ULong) : OrderedPortRejected
  data object Unmapped : OrderedPortRejected
}
internal sealed interface OrderedPortRecord {
  data class Value(val member: OrderedPortRegistration, val ordinal: ULong, val bytes: ByteArray) : OrderedPortRecord
  data class Loss(val loss: OrderedPortLoss) : OrderedPortRecord
  data class Fence(val member: OrderedPortRegistration, val uncertainTail: Boolean) : OrderedPortRecord
  data class Barrier(val barrierId: ULong, val uncertainTail: Boolean) : OrderedPortRecord
  data class Marker(val markerId: ULong, val ordinal: ULong) : OrderedPortRecord
  data class Terminal(val reason: String, val uncertainTail: Boolean, val rejected: OrderedPortRejected?, val wakeFailed: Boolean,
                      val nativeDomain: String? = null, val nativeCode: String? = null,
                      val failureMember: OrderedPortRegistration? = null, val failure: RadioFailure? = null) : OrderedPortRecord
  data class SubsequentSourceFailure(val failure: RadioFailure) : OrderedPortRecord
  data object FailureObservationsOmitted : OrderedPortRecord
  data class BridgeFailure(val code: String, val detail: String) : OrderedPortRecord
}
internal sealed interface GraphAuthorityPortResponse {
  data object Unit : GraphAuthorityPortResponse
  data class Reservation(val status: com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus) : GraphAuthorityPortResponse
  data class Open(val status: com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphAuthorityOpenStatus) : GraphAuthorityPortResponse
}
internal data class OrderedPortBatch(val id: ULong?, val records: List<OrderedPortRecord>)

/** Android's tagged counter source; IDs and ordinals retain every unsigned bit. */
internal interface OrderedGroupPort {
  fun orderedAdmissionOwner(version: Int): String
  /** Version-4 owner-wide admission fence, when this port can prove it. */
  fun ownerQuiescenceCoordinator(): AndroidOwnerQuiescenceCoordinator? = null
  fun orderedSlots(): AndroidOrderedOpenSlots = throw UnsupportedOperationException("ordered slots unavailable")
  fun orderedOpenSlot(identity: AndroidOrderedOpenSlots.Identity, onWakeFailure: (ULong, ULong, Throwable) -> Unit,
                      wake: (ULong, ULong) -> Unit): AndroidOrderedOpenSlots.Result = throw UnsupportedOperationException("ordered slots unavailable")
  /** Admit synchronously; an exact group/request/peer identity cannot be reused by this owner. */
  fun orderedAdmitOpen(group: ULong, request: ULong, peer: String): com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner.OpenAdmission
  /** Retires a terminal receipt after its exact outcome has been accepted; retries are idempotent. */
  fun orderedAcknowledgeOpen(group: ULong, request: ULong, peer: String)
  /** Fences deferred work or waits for a started installation, without guessing for an unknown identity. */
  fun orderedRecoverOpenExact(group: ULong, request: ULong, peer: String): OrderedPortOpenRecovery
  /** Persists an exact pre-admission legacy recovery so direct retries see the same outcome. */
  fun orderedFenceQueuedOpen(group: ULong, request: ULong, peer: String): OrderedPortOpenRecovery = OrderedPortOpenRecovery.Indeterminate
  /** Uses the original admission even if its receipt has since been acknowledged. */
  fun orderedRecoverAdmission(admission: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner.OpenAdmission): OrderedPortOpenRecovery
  fun orderedFailAdmission(admission: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner.OpenAdmission, error: Throwable): OrderedPortOpenRecovery
  fun orderedOpenAdmitted(admission: com.sfourdrinier.unifiedblemanager.radio.AndroidOrderedGroupOwner.OpenAdmission, graph: AndroidNativeGraphIdentity,
                          members: Map<ULong, CharacteristicInstance>, itemCapacity: Int, byteCapacity: Int,
                          onWakeFailure: ((ULong, ULong, Throwable) -> Unit)? = null, wake: (ULong, ULong) -> Unit): OrderedPortOpen
  fun orderedGraphPermitReservation(operation: String, request: com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitRequest): com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphReservationStatus? = throw UnsupportedOperationException("native graph reservation unavailable")
  fun orderedGraphAuthority(operation: String, value: ULong): GraphAuthorityPortResponse = throw UnsupportedOperationException("native graph authority unavailable")
  fun orderedGraphPermit(operation: String, key: com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitKey, validateConnection: () -> Boolean): com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitResult = throw UnsupportedOperationException("native graph permit unavailable")
  fun orderedGraphPermitInvalidation(key: com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitKey): com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GraphPermitInvalidation? = null
  fun orderedOpen(group: ULong, graph: AndroidNativeGraphIdentity, members: Map<ULong, CharacteristicInstance>, itemCapacity: Int, byteCapacity: Int,
                  onWakeFailure: ((ULong, ULong, Throwable) -> Unit)? = null, wake: (ULong, ULong) -> Unit): OrderedPortOpen
  fun orderedRegister(generation: ULong, group: ULong, member: OrderedPortRegistration): OrderedPortRegistration
  fun orderedRecoverOpen(group: ULong): OrderedPortOpenRecovery
  fun orderedRetire(generation: ULong, group: ULong, member: OrderedPortRegistration)
  fun orderedReserveMarkerWrite(generation: ULong, group: ULong, markerId: ULong, connectionGeneration: String,
                                graph: com.sfourdrinier.unifiedblemanager.radio.AndroidNativeGraphIdentity,
                                instance: CharacteristicInstance, value: ByteArray, withResponse: Boolean,
                                cancellation: com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation): com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.Reservation
  fun orderedCancelMarkerWrite(generation: ULong, group: ULong, markerId: ULong, error: Throwable): com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State
  fun orderedMarkerWrite(generation: ULong, group: ULong, markerId: ULong, connectionGeneration: String,
                         graph: com.sfourdrinier.unifiedblemanager.radio.AndroidNativeGraphIdentity,
                         instance: CharacteristicInstance, value: ByteArray, withResponse: Boolean, validate: () -> Unit,
                         cancellation: com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerCancellation, result: (com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State) -> Unit): Long?
  fun orderedCancelMarkerWriteExact(generation: ULong, group: ULong, markerId: ULong): com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State
  fun orderedMarkerWriteSettlement(generation: ULong, group: ULong, markerId: ULong): com.sfourdrinier.unifiedblemanager.radio.OrderedMarkerTransactions.State
  fun orderedForgetMarkerWriteReceipt(generation: ULong, group: ULong, markerId: ULong)
  fun orderedBarrier(generation: ULong, group: ULong, barrierId: ULong)
  fun orderedDrain(generation: ULong, group: ULong): OrderedPortBatch
  fun orderedAcknowledge(generation: ULong, group: ULong, batch: ULong)
  fun orderedClose(generation: ULong, group: ULong)
  fun orderedSeal(generation: ULong, group: ULong)
  fun orderedRetainClaim(generation: ULong, group: ULong, member: OrderedPortRegistration): OrderedPortRegistration
  fun orderedHasClaim(generation: ULong, group: ULong, member: OrderedPortRegistration, instance: CharacteristicInstance): Boolean
  fun orderedAdmission(generation: ULong, group: ULong, member: OrderedPortRegistration, instance: CharacteristicInstance): AndroidOrderedAdmission
  fun orderedRetryLocalCleanup(generation: ULong, group: ULong, instance: CharacteristicInstance, epoch: Long): OrderedLocalCleanupStatus
  fun orderedConfirmCleanup(generation: ULong, group: ULong, member: OrderedPortRegistration)
  fun orderedRemove(generation: ULong, group: ULong)
  fun orderedForgetRemovalReceipt(generation: ULong, group: ULong)
}
