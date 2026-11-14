// android/src/main/java/com/sfourdrinier/unifiedblemanager/rustcore/OrderedGroupBridgeMapper.kt
package com.sfourdrinier.unifiedblemanager.rustcore

import com.ubm.core.MobileCoreBridge

internal object OrderedGroupBridgeMapper {
  fun generation(kind: String, value: String): ULong {
    require(kind == "counter") { "Android ordered source requires a counter generation" }
    val parsed = value.toULong()
    require(parsed.toString() == value) { "ordered source generation is not canonical" }
    return parsed
  }
  fun registration(value: OrderedPortRegistration) = MobileCoreBridge.OrderedRegistration().apply {
    member = value.memberId.toLong(); sourceMember = value.sourceMember.toLong(); epoch = value.epoch.toLong()
  }
  fun opened(value: OrderedPortOpen) = MobileCoreBridge.OrderedResult().apply {
    kind = "opened"; generationKind = "counter"; generationValue = value.generation.toString()
    members = value.members.map(::registration).toTypedArray()
  }
  fun registered(value: OrderedPortRegistration) = MobileCoreBridge.OrderedResult().apply {
    kind = "registered"; member = registration(value)
  }
  fun claim(generation: ULong, value: OrderedPortRegistration) = MobileCoreBridge.OrderedResult().apply {
    kind = "claim"; generationKind = "counter"; generationValue = generation.toString(); member = registration(value)
  }
  private fun MobileCoreBridge.OrderedRecord.setLoss(loss: OrderedPortLoss) {
    scope = loss.scope; scopeMember = loss.memberId?.toLong() ?: 0L; unit = loss.unit
    hasCount = loss.count != null; count = loss.count?.toLong() ?: 0L
    hasByteCount = loss.bytes != null; byteCount = loss.bytes?.toLong() ?: 0L
  }
  fun records(batchValue: OrderedPortBatch) = MobileCoreBridge.OrderedResult().apply {
    kind = "records"; hasBatch = batchValue.id != null; batch = batchValue.id?.toLong() ?: 0L
    records = batchValue.records.map { record ->
      MobileCoreBridge.OrderedRecord().apply {
        when (record) {
          is OrderedPortRecord.Value -> {
            kind = "value"; member = registration(record.member); ordinal = record.ordinal.toLong()
            value = record.bytes; provenance = "notification"; delivery = "unknown"
          }
          is OrderedPortRecord.Fence -> { kind = "fence"; member = registration(record.member); uncertainTail = record.uncertainTail }
          is OrderedPortRecord.Barrier -> { kind = "barrier"; barrierId = record.barrierId.toLong(); uncertainTail = record.uncertainTail }
          is OrderedPortRecord.Marker -> { kind = "marker"; markerId = record.markerId.toLong(); ordinal = record.ordinal.toLong() }
          is OrderedPortRecord.Loss -> { kind = "loss"; setLoss(record.loss) }
          is OrderedPortRecord.SubsequentSourceFailure -> {
            kind = "subsequent-source-failure"; failureKind = record.failure.kind.wire
            gattStatus = record.failure.gattStatus ?: MobileCoreBridge.ABSENT_INT
            nativeName = record.failure.nativeCode; detail = record.failure.detail; dispatched = record.failure.dispatched
          }
          OrderedPortRecord.FailureObservationsOmitted -> { kind = "failure-observations-omitted" }
          is OrderedPortRecord.BridgeFailure -> { kind = "bridge-failure"; bridgeCode = record.code; detail = record.detail }
          is OrderedPortRecord.Terminal -> {
            kind = "terminal"; reason = record.reason; uncertainTail = record.uncertainTail
            nativeDomain = record.nativeDomain; nativeCode = record.nativeCode
            record.failure?.let { failure ->
              failureKind = failure.kind.wire; gattStatus = failure.gattStatus ?: MobileCoreBridge.ABSENT_INT
              nativeName = failure.nativeCode; detail = failure.detail; dispatched = failure.dispatched
            }
            failureMember = record.failureMember?.let(::registration)
            when (val rejected = record.rejected) {
              is OrderedPortRejected.Value -> { rejectedKind = "value"; rejectedMember = registration(rejected.member); rejectedByteCount = rejected.bytes.toLong() }
              is OrderedPortRejected.Fence -> { rejectedKind = "fence"; rejectedMember = registration(rejected.member) }
              is OrderedPortRejected.Barrier -> { rejectedKind = "barrier"; rejectedBarrierId = rejected.barrierId.toLong() }
              is OrderedPortRejected.Marker -> { rejectedKind = "marker"; rejectedMarkerId = rejected.markerId.toLong() }
              is OrderedPortRejected.Loss -> { rejectedKind = "loss"; setLoss(rejected.loss) }
              OrderedPortRejected.Unmapped -> rejectedKind = "unmapped"
              null -> Unit
            }
          }
        }
      }
    }.toTypedArray()
  }
}
