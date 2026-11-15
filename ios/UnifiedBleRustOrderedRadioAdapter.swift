// ios/UnifiedBleRustOrderedRadioAdapter.swift
import Foundation

/// Typed UniFFI-to-radio mapping. It owns no GATT state; the queue-confined
/// bridge and the existing shared physical subscription owner retain claims.
enum UnifiedBleRustOrderedRadioAdapter {
  typealias Slots = UnifiedBleRustOrderedGroupBridge.AdmissionSlots

  static func identity(_ value: MobileOrderedOpenIdentity) throws -> Slots.Identity {
    guard let owner = UUID(uuidString: value.ownerIncarnation),
          owner.uuidString.lowercased() == value.ownerIncarnation else { throw Slots.Error.owner }
    let members = try value.spec.members.map { member -> UnifiedBleRustOrderedGroupBridge.Member in
      let instance = member.instance
      guard let service = Int(exactly: instance.serviceOccurrence),
            let characteristic = Int(exactly: instance.characteristicOccurrence) else { throw Slots.Error.identity }
      return .init(id: member.memberId, route: .init(peer: instance.peerId, serviceUUID: instance.serviceUuid,
        serviceOccurrence: service, characteristicUUID: instance.characteristicUuid, characteristicOccurrence: characteristic))
    }
    let snapshot = value.spec.snapshotIdentity
    return .init(version: value.version, owner: owner, slot: value.slot, generation: value.slotGeneration,
      requestID: value.originalRequestId, groupID: value.nativeGroupId, peer: value.peerId,
      spec: .init(connectionGeneration: value.spec.connectionGeneration, databaseGeneration: value.spec.databaseGeneration,
        snapshot: .init(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision),
        members: members, itemCapacity: value.spec.itemCapacity, byteCapacity: value.spec.byteCapacity))
  }

  static func result(_ identity: Slots.Identity, _ outcome: Slots.Outcome) -> MobileRadioCompletion {
    let slot = MobileOrderedSlotIdentity(ownerIncarnation: identity.owner.uuidString.lowercased(), slot: identity.slot, slotGeneration: identity.generation)
    func failure(_ value: Slots.FailureCause) -> MobileOrderedFailureV2 {
      .init(kind: value.kind, gattStatus: value.gattStatus, nativeDomain: value.domain, nativeCode: value.code,
        nativeName: nil, detail: value.detail, dispatched: value.dispatched)
    }
    func members(_ descriptor: UnifiedBleRustOrderedGroupBridge.Descriptor) -> [MobileOrderedRegistration] {
      descriptor.members.map(registration)
    }
    let mapped: MobileOrderedOpenOutcome
    switch outcome {
    case .pending: mapped = .pending
    case let .opened(descriptor):
      mapped = .opened(sourceGeneration: .opaque(value: descriptor.opaqueSourceGeneration), members: members(descriptor))
    case let .failed(cause, ownership):
      let mappedOwnership: MobileOrderedOwnershipV2
      switch ownership {
      case .notCreated: mappedOwnership = .notCreated
      case let .created(descriptor):
        mappedOwnership = .created(sourceGeneration: .opaque(value: descriptor.opaqueSourceGeneration), members: members(descriptor))
      }
      mapped = .failed(originalFailure: failure(cause), ownership: mappedOwnership)
    case let .indeterminate(cause): mapped = .indeterminate(originalFailureOrReason: .failure(failure: failure(cause)))
    }
    return .orderedOpenResultV2(slotIdentity: slot, outcome: mapped)
  }

  static func execute(_ identity: Slots.Identity, bridge: UnifiedBleRustOrderedGroupBridge,
                      wake: ((UInt64, String) -> Void)? = nil) -> Slots.Outcome {
    do {
      return .opened(try bridge.open(groupID: identity.groupID, members: identity.spec.members,
        capacity: Int(identity.spec.itemCapacity), byteCapacity: Int(identity.spec.byteCapacity), snapshot: identity.spec.snapshot, wake: wake))
    } catch let error as UnifiedBleRustOrderedGroupBridge.FactoryNoEffect {
      let native = error.underlying as NSError
      return .failed(.init(domain: native.domain, code: Int64(native.code), detail: native.localizedDescription), .notCreated)
    } catch let error as UnifiedBleRustOrderedGroupBridge.Failure {
      switch error {
      case .captureCapacity:
        return .failed(.init(kind: "busy", detail: "ordered group capture capacity is exhausted"), .notCreated)
      case let .openIndeterminate(generation, group, members, cause):
        return .failed(.init(domain: cause.domain, code: cause.code, detail: cause.detail, dispatched: true),
          .created(.init(groupID: group, opaqueSourceGeneration: generation, members: members)))
      case let .factoryIndeterminate(cause):
        return .indeterminate(.init(domain: cause.domain, code: cause.code, detail: cause.detail, dispatched: true))
      default: return .failed(.init(detail: "ordered source open failed: \(error)"), .notCreated)
      }
    } catch {
      let native = error as NSError
      return .failed(.init(domain: native.domain, code: Int64(native.code), detail: native.localizedDescription), .notCreated)
    }
  }
  static func handle(_ request: MobileRadioRequest, bridge: UnifiedBleRustOrderedGroupBridge?,
                     wake: ((UInt64, String) -> Void)? = nil) -> MobileRadioCompletion? {
    switch request {
    case let .orderedOpen(_, groupID, _, _, snapshot, members, itemCapacity, byteCapacity):
      do {
        guard let bridge else { throw UnifiedBleRustOrderedGroupBridge.Failure.unknownGroup }
        let mapped = try members.map { member -> UnifiedBleRustOrderedGroupBridge.Member in
          let instance = member.instance
          guard let service = Int(exactly: instance.serviceOccurrence),
                let characteristic = Int(exactly: instance.characteristicOccurrence) else {
            throw UnifiedBleRustOrderedGroupBridge.Failure.invalidMembers
          }
          return .init(id: member.memberId, route: OrderedNotificationRoute(peer: instance.peerId,
            serviceUUID: instance.serviceUuid, serviceOccurrence: service,
            characteristicUUID: instance.characteristicUuid, characteristicOccurrence: characteristic))
        }
        let descriptor = try bridge.open(groupID: groupID, members: mapped,
          capacity: Int(itemCapacity), byteCapacity: Int(byteCapacity),
          snapshot: OwnedGattSnapshotIdentity(owner: snapshot.owner, attachment: snapshot.attachment, revision: snapshot.revision), wake: wake)
        return .orderedOpened(generation: .opaque(value: descriptor.opaqueSourceGeneration),
          members: descriptor.members.map(registration))
      } catch let error as UnifiedBleRustOrderedGroupBridge.FactoryNoEffect {
        let native = error.underlying as NSError
        let failure = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: native.domain, nativeCode: Int64(native.code), detail: "ordered source open failed: \(native.localizedDescription)", dispatched: false)
        return .orderedOpenFailed(failure: failure, ownership: .notCreated)
      } catch let error as UnifiedBleRustOrderedGroupBridge.Failure {
        if case .captureCapacity = error {
          let failure = MobilePlatformFailure(kind: "busy", gattStatus: nil, nativeDomain: nil, nativeCode: nil,
            detail: "ordered group capture capacity is exhausted", dispatched: false)
          return .orderedOpenFailed(failure: failure, ownership: .notCreated)
        }
        if case let .openIndeterminate(generation, _, members, cause) = error {
          let failure = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: cause.domain, nativeCode: cause.code,
            detail: "ordered source open failed: \(cause.detail)", dispatched: true)
          return .orderedOpenFailed(failure: failure,
            ownership: .created(generation: .opaque(value: generation), members: members.map(registration)))
        }
        if case let .factoryIndeterminate(cause) = error {
          let failure = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: cause.domain, nativeCode: cause.code,
            detail: "ordered source open failed: \(cause.detail)", dispatched: true)
          return .orderedOpenFailed(failure: failure, ownership: .indeterminate(recoveryKey: groupID))
        }
        let failure = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: nil, nativeCode: nil, detail: "ordered source open failed: \(error)", dispatched: false)
        return .orderedOpenFailed(failure: failure, ownership: .notCreated)
      } catch {
        let native = error as NSError
        let failure = MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: native.domain, nativeCode: Int64(native.code), detail: "ordered source open failed: \(native.localizedDescription)", dispatched: false)
        return .orderedOpenFailed(failure: failure, ownership: .notCreated)
      }
    case let .orderedRecoverOpen(_, groupID, requestID, peer):
      return perform(bridge) { bridge in
        switch try bridge.fenceOpen(groupID: groupID, requestID: requestID, peer: peer) {
        case .notCreated: return fencedOpen()
        case let .created(descriptor):
          return .orderedOpened(generation: .opaque(value: descriptor.opaqueSourceGeneration), members: descriptor.members.map(registration))
        case let .completed(value), let .unresolved(value):
          guard let completion = value.base as? MobileRadioCompletion else {
            return UnifiedBleRustRadioAdapter.failed("platform", "ordered open receipt has an invalid response", dispatched: true)
          }
          return completion
        case .pending, .indeterminate:
          return UnifiedBleRustRadioAdapter.failed("busy", "ordered open effect remains unresolved", dispatched: true)
        }
      }
    case let .orderedAcknowledgeOpen(_, version, groupID, requestID, peer):
      return perform(bridge) { bridge in
        try bridge.acknowledgeOpen(version: version, groupID: groupID, requestID: requestID, peer: peer)
        return .unit
      }
    case let .orderedRegister(_, generation, groupID, member):
      return perform(bridge) { bridge in
        try validate(generation, groupID: groupID, bridge: bridge)
        let expected = try bridge.expectedMember(groupID: groupID, memberID: member.memberId, sourceMember: member.sourceMember, epoch: member.epoch)
        return .orderedRegistered(member: registration(try bridge.register(groupID: groupID, expected: expected)))
      }
    case let .orderedRetire(_, generation, groupID, member):
      return perform(bridge) { bridge in
        try validate(generation, groupID: groupID, bridge: bridge)
        let expected = try bridge.expectedMember(groupID: groupID, memberID: member.memberId, sourceMember: member.sourceMember, epoch: member.epoch)
        try bridge.retire(groupID: groupID, expected: expected)
        return .unit
      }
    case let .orderedDrain(_, generation, groupID):
      return perform(bridge) { bridge in
        try validate(generation, groupID: groupID, bridge: bridge)
        guard let batch = try bridge.drain(groupID: groupID) else { return .orderedRecords(batchId: nil, records: []) }
        guard wirePreservesIdentity(batch) else { return unrepresentableFailure() }
        try bridge.markLegacyBatch(groupID: groupID, batchID: batch.id)
        return .orderedRecords(batchId: batch.id, records: batch.records.map(record))
      }
    case let .orderedDrainV2(_, version, generation, groupID):
      return perform(bridge) { bridge in
        guard version == 2 else { return unrepresentableFailure() }
        try validate(generation, groupID: groupID, bridge: bridge)
        guard let batch = try bridge.drain(groupID: groupID) else { return .orderedRecords(batchId: nil, records: []) }
        var records: [MobileOrderedRecord] = []
        for (index, item) in batch.records.enumerated() {
          if case let .subsequentSourceFailure(failure) = item {
            guard let origin = failure.origin, let position = UInt32(exactly: index),
                  let service = UInt64(exactly: origin.claim.route.serviceOccurrence),
                  let characteristic = UInt64(exactly: origin.claim.route.characteristicOccurrence) else { return unrepresentableFailure() }
            let route = origin.claim.route
            let instance = MobileInstance(peerId: route.peer, serviceUuid: route.serviceUUID, serviceOccurrence: service,
              characteristicUuid: route.characteristicUUID, characteristicOccurrence: characteristic)
            records.append(.subsequentSourceFailureV2(failure: sourceFailure(failure), instance: instance,
              sourceGeneration: origin.claim.generation, sourceMember: origin.claim.token, epoch: origin.epoch,
              batchId: batch.id, recordIndex: position))
          } else if case .terminal(.sourceFailureUnmapped, _, _) = item {
            return unrepresentableFailure()
          } else {
            records.append(record(item))
          }
        }
        try bridge.markVersionedBatch(groupID: groupID, batchID: batch.id)
        return .orderedRecords(batchId: batch.id, records: records)
      }
    case let .orderedAcknowledgeBatch(_, generation, groupID, batchID):
      return perform(bridge) { bridge in
        try validate(generation, groupID: groupID, bridge: bridge)
        if let batch = try bridge.pendingBatch(groupID: groupID, batchID: batchID), !wirePreservesIdentity(batch) {
          return unrepresentableFailure()
        }
        try bridge.acknowledgeLegacyBatch(groupID: groupID, batchID: batchID)
        return .unit
      }
    case let .orderedAcknowledgeBatchV2(_, version, generation, groupID, batchID):
      return perform(bridge) { bridge in
        guard version == 2 else { return unrepresentableFailure() }
        try validate(generation, groupID: groupID, bridge: bridge)
        try bridge.acknowledgeVersionedBatch(groupID: groupID, batchID: batchID)
        return .unit
      }
    case let .orderedClose(_, generation, groupID):
      return perform(bridge) { bridge in
        try validate(generation, groupID: groupID, bridge: bridge)
        try bridge.close(groupID: groupID)
        return .unit
      }
    case let .orderedSeal(_, generation, groupID):
      return perform(bridge) { bridge in
        try validate(generation, groupID: groupID, bridge: bridge)
        try bridge.seal(groupID: groupID)
        return .unit
      }
    case let .orderedBarrier(_, generation, groupID, barrierID):
      return perform(bridge) { bridge in
        try validate(generation, groupID: groupID, bridge: bridge)
        try bridge.barrier(groupID: groupID, id: barrierID)
        return .unit
      }
    case let .orderedRetainClaim(_, generation, groupID, member):
      return perform(bridge) { bridge in
        try validate(generation, groupID: groupID, bridge: bridge)
        let claim = try bridge.retainPhysicalClaim(groupID: groupID, memberID: member.memberId, sourceMember: member.sourceMember, epoch: member.epoch)
        return .orderedClaim(generation: .opaque(value: claim.sourceClaim.generation),
          member: MobileOrderedRegistration(memberId: member.memberId, sourceMember: claim.sourceClaim.token, epoch: claim.epoch))
      }
    case let .orderedConfirmCleanup(_, generation, groupID, member):
      return perform(bridge) { bridge in
        let opaque = try opaqueGeneration(generation)
        try bridge.validateSource(groupID: groupID, opaqueSourceGeneration: opaque)
        try bridge.confirmPhysicalCleanup(groupID: groupID, memberID: member.memberId,
          opaqueSourceGeneration: opaque, sourceMember: member.sourceMember, epoch: member.epoch)
        return .unit
      }
    case let .orderedRemove(_, generation, groupID):
      return perform(bridge) { bridge in
        try bridge.remove(groupID: groupID, opaqueSourceGeneration: opaqueGeneration(generation))
        return .unit
      }
    case let .orderedForgetRemovalReceipt(_, generation, groupID):
      return perform(bridge) { bridge in
        try bridge.acknowledgeRemoval(groupID: groupID, opaqueSourceGeneration: opaqueGeneration(generation))
        return .unit
      }
    default:
      return nil
    }
  }

  static func fencedOpen() -> MobileRadioCompletion {
    let failure = MobilePlatformFailure(kind: "cancelled", gattStatus: nil, nativeDomain: nil, nativeCode: nil,
      detail: "ordered open was fenced before admission", dispatched: false)
    return .orderedOpenFailed(failure: failure, ownership: .notCreated)
  }

  private static func perform(_ bridge: UnifiedBleRustOrderedGroupBridge?,
                              operation: (UnifiedBleRustOrderedGroupBridge) throws -> MobileRadioCompletion) -> MobileRadioCompletion {
    guard let bridge else { return UnifiedBleRustRadioAdapter.failed("unsupported", "ordered source is unavailable", dispatched: false) }
    do { return try operation(bridge) }
    catch let error as UnifiedBleRustOrderedGroupBridge.Failure {
      switch error {
      case .staleGeneration, .unknownGroup, .unknownMember, .inactiveMember, .unknownPhysicalClaim, .unknownOpenReceipt:
        return UnifiedBleRustRadioAdapter.failed("path-stale", "ordered source identity is stale", dispatched: false)
      case .cleanupPending, .captureCapacity, .capturePending:
        return UnifiedBleRustRadioAdapter.failed("busy", "ordered source cleanup remains pending", dispatched: false)
      case .duplicateGroup, .invalidMembers, .alreadyRetained, .unknownBatch, .markerBusy, .markerStale, .markerMismatch, .markerNotSettled, .openAdmissionUnavailable, .openAdmissionConflict:
        return UnifiedBleRustRadioAdapter.failed("platform", "invalid ordered source operation", dispatched: false)
      case .sourceClaimExhausted, .batchIDExhausted:
        return UnifiedBleRustRadioAdapter.failed("platform", "ordered source identity exhausted", dispatched: false)
      case .openIndeterminate, .factoryIndeterminate:
        return UnifiedBleRustRadioAdapter.failed("platform", "ordered source rollback remains owned", dispatched: true)
      case .openAdmissionUntracked:
        return UnifiedBleRustRadioAdapter.failed("busy", "ordered open receipt is unavailable", dispatched: true)
      case .source:
        return UnifiedBleRustRadioAdapter.failed("platform", "ordered source refused the operation", dispatched: false)
      }
    } catch {
      let native = error as NSError
      return UnifiedBleRustRadioAdapter.failed("platform", "ordered source operation failed: \(native.domain)#\(native.code)",
        nativeError: native, dispatched: false)
    }
  }

  private static func opaqueGeneration(_ generation: MobileOrderedGeneration) throws -> String {
    guard case let .opaque(value) = generation else { throw UnifiedBleRustOrderedGroupBridge.Failure.staleGeneration }
    return value
  }

  private static func validate(_ generation: MobileOrderedGeneration, groupID: UInt64,
                               bridge: UnifiedBleRustOrderedGroupBridge) throws {
    try bridge.validateSource(groupID: groupID, opaqueSourceGeneration: opaqueGeneration(generation))
  }

  // The current UniFFI record has no late-failure claim or epoch fields.
  // Keep the entire exact batch owned until a versioned wire can carry them.
  private static func wirePreservesIdentity(_ batch: UnifiedBleRustOrderedGroupBridge.Batch) -> Bool {
    !batch.records.contains { record in
      switch record {
      case .subsequentSourceFailure, .terminal(.sourceFailureUnmapped, _, _): return true
      default: return false
      }
    }
  }

  private static func unrepresentableFailure() -> MobileRadioCompletion {
    UnifiedBleRustRadioAdapter.failed("unsupported", "ordered failure identity cannot be represented by this UniFFI wire; batch retained", dispatched: false)
  }

  private static func registration(_ member: UnifiedBleRustOrderedGroupBridge.RegisteredMember) -> MobileOrderedRegistration {
    MobileOrderedRegistration(memberId: member.id, sourceMember: member.sourceMember, epoch: member.epoch)
  }

  private static func record(_ record: UnifiedBleRustOrderedGroupBridge.Record) -> MobileOrderedRecord {
    switch record {
    case let .value(memberID, sourceMember, epoch, ordinal, bytes, provenance):
      return .value(member: MobileOrderedRegistration(memberId: memberID, sourceMember: sourceMember, epoch: epoch),
        ordinal: ordinal, value: bytes, delivery: "unknown", provenance: provenance == .notification ? "notification" : "read-or-notification")
    case let .fence(memberID, sourceMember, epoch, _):
      return .fence(member: MobileOrderedRegistration(memberId: memberID, sourceMember: sourceMember, epoch: epoch), uncertainTail: true)
    case let .barrier(id, _): return .barrier(barrierId: id, uncertainTail: true)
    case let .marker(id, ordinal): return .marker(markerId: id, ordinal: ordinal)
    case let .loss(memberID, count):
      return .loss(scope: memberID == nil ? "unknown" : "member", memberId: memberID,
        unit: "source-records", count: count, byteCount: nil)
    case let .subsequentSourceFailure(failure):
      return .subsequentSourceFailure(failure: sourceFailure(failure))
    case .failureObservationsOmitted:
      return .failureObservationsOmitted
    case let .terminal(reason, _, rejected):
      switch reason {
      case let .sourceFailed(failure, memberID, sourceMember, epoch):
        return .sourceFailed(uncertainTail: true, failure: sourceFailure(failure), nativeDomain: failure.domain,
          nativeCode: String(failure.code), failureMember: MobileOrderedRegistration(memberId: memberID, sourceMember: sourceMember, epoch: epoch), rejected: rejected.map(rejection))
      case let .sourceFailureUnmapped(failure, _, _):
        return .sourceFailed(uncertainTail: true, failure: sourceFailure(failure), nativeDomain: failure.domain,
          nativeCode: String(failure.code), failureMember: nil, rejected: rejected.map(rejection))
      default: break
      }
      let (name, domain, code, member) = termination(reason)
      return .terminal(reason: name, uncertainTail: true, nativeDomain: domain, nativeCode: code,
        failureMember: member, rejected: rejected.map(rejection))
    case .bridgeFailure:
      return .bridgeFailure(code: "protocol-violation", detail: "ordered source membership could not be mapped")
    }
  }

  private static func sourceFailure(_ failure: OwnedOrderedNotificationIngress.NativeFailure) -> MobilePlatformFailure {
    MobilePlatformFailure(kind: "platform", gattStatus: nil, nativeDomain: failure.domain,
      nativeCode: Int64(failure.code), detail: failure.detail, dispatched: false)
  }

  private static func termination(_ reason: UnifiedBleRustOrderedGroupBridge.End) -> (String, String?, String?, MobileOrderedRegistration?) {
    switch reason {
    case .closed: return ("closed", nil, nil, nil)
    case .connectionLost: return ("connection-lost", nil, nil, nil)
    case .serviceChanged: return ("service-changed", nil, nil, nil)
    case .sourceReplaced: return ("source-replaced", nil, nil, nil)
    case .shutdown: return ("shutdown", nil, nil, nil)
    case .adapterLost: return ("adapter-lost", nil, nil, nil)
    case .overflow: return ("overflow", nil, nil, nil)
    case .ordinalExhausted: return ("ordinal-exhausted", nil, nil, nil)
    case .wakeFailed: return ("wake-failed", nil, nil, nil)
    case let .sourceFailed(failure, memberID, sourceMember, epoch):
      return ("source-failed", failure.domain, String(failure.code),
        MobileOrderedRegistration(memberId: memberID, sourceMember: sourceMember, epoch: epoch))
    case let .sourceFailureUnmapped(failure, _, _):
      return ("source-failed", failure.domain, String(failure.code), nil)
    }
  }

  private static func rejection(_ rejected: UnifiedBleRustOrderedGroupBridge.Rejected) -> MobileOrderedRejected {
    switch rejected {
    case let .value(memberID, sourceMember, epoch, bytes):
      return .value(member: MobileOrderedRegistration(memberId: memberID, sourceMember: sourceMember, epoch: epoch), byteCount: bytes)
    case let .sourceLoss(memberID, count):
      return .loss(scope: memberID == nil ? "unknown" : "member", memberId: memberID,
        unit: "source-records", count: count, byteCount: nil)
    case let .fence(memberID, sourceMember, epoch):
      return .fence(member: MobileOrderedRegistration(memberId: memberID, sourceMember: sourceMember, epoch: epoch))
    case let .barrier(id): return .barrier(barrierId: id)
    case let .marker(id): return .marker(markerId: id)
    case .unmapped: return .unmapped
    }
  }
}
