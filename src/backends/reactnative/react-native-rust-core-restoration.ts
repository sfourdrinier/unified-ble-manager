// src/backends/reactnative/react-native-rust-core-restoration.ts
//
// The restoration journal of the Rust route: the peers CoreBluetooth handed
// back through state restoration, adopted under the app-declared restoration
// authority. It answers the ReactNativeRestorationCoordinator exactly as the
// legacy native journal did (native/protocol/src/NativeProtocolV2Registry.cpp
// `NativeRestorationJournal` and the Apple execution's
// `appendRestorationRecords`): the same record layout, the same outcome
// order, the same rejections. As in legacy, the restored peers themselves are
// consumed once per process, not per manager: legacy cleared the process
// radio's restoration identifiers on the first adoption
// (`consumeRestorationPeerIdentifiers`), so a later manager's adoption
// replayed the adapter record only. The Rust owner keeps that fact
// (`peers.claim-restored`); this journal keeps the per-manager one
// (`alreadyConsumed` for a second adoption on the same attachment).

import { contractError } from '../../backend-contract/errors'
import type { AttachmentRecord } from '../../backend-contract/identity'
import type {
  ReactNativeRestorationAdoptionRecord,
  ReactNativeRestorationAdoptionRequestRecord,
  ReactNativeRestorationJournal,
  ReactNativeRestorationPlatform,
  ReactNativeRestorationReplayRecord
} from './react-native-restoration'
import type { WirePeerRecord } from './rust-core-wire'

/** The one native protocol version the restoration transport speaks. */
const RESTORATION_PROTOCOL_VERSION = 2
/** Journal record capacity (legacy `NativeRestorationJournal` capacity). */
const RESTORATION_RECORD_CAPACITY = 1024

/**
 * The app-declared restoration authority (Info.plist / manifest values the
 * native host reported through `restorationIdentity`).
 */
export interface ReactNativeRestorationAuthority {
  readonly namespaceValue: string
  readonly adoptionEpoch: string
  readonly clientId: string
  readonly hostSessionScope: string
}

export interface RustCoreRestorationJournalOptions {
  /** Target mobile platform: Android has no OS restoration journal. */
  readonly platform: ReactNativeRestorationPlatform
  /** The authority, or `null` when the app configured none. */
  readonly authority: () => ReactNativeRestorationAuthority | null
  /** The attachment the journal belongs to (the open backend). */
  readonly attachment: () => AttachmentRecord<string> | null
  /**
   * `peers.claim-restored` on the backend's session: the restored peers no
   * other adopter in the process claimed, now claimed by this one. Refused
   * (`bytes.too-large`, nothing claimed) when more than `maxPeers` remain.
   */
  readonly claimRestoredPeers: (maxPeers: number) => Promise<readonly WirePeerRecord[]>
}

let nextReceipt = 1

function validInteger(value: number): boolean {
  return Number.isSafeInteger(value) && value >= 1
}

function refuse(safeMessage: string): never {
  throw contractError('platform.failure', 'restoration', 'react-native-rust-core.restoration.adopt', {
    domain: 'react-native-rust-core',
    code: 'nativeRestorationAdoption',
    safeMessage,
    metadata: Object.freeze({})
  })
}

function rejection(
  outcome: ReactNativeRestorationAdoptionRecord['outcome'],
  boundClientId: string,
  adoptionEpoch: string
): ReactNativeRestorationAdoptionRecord {
  return Object.freeze({
    receiptId: '',
    outcome,
    boundClientId,
    adoptionEpoch,
    replayRecordCount: 0,
    records: Object.freeze([])
  })
}

/**
 * Journal over `peers.claim-restored`. Peers are claimed at the first
 * authorized adoption (the legacy journal was filled once, at attachment),
 * which consumes the journal for this manager; a rejected adoption claims
 * nothing (legacy rejections were non-consuming).
 */
export class RustCoreRestorationJournal implements ReactNativeRestorationJournal {
  private consumed = false

  constructor(private readonly options: RustCoreRestorationJournalOptions) {}

  async adoptRestoration(
    request: ReactNativeRestorationAdoptionRequestRecord
  ): Promise<ReactNativeRestorationAdoptionRecord> {
    const attachment = this.options.attachment()
    const authority = this.options.authority()
    if (authority === null && this.options.platform === 'android') {
      // Legacy rule preserved: Android has no OS restoration journal, so
      // without a configured restoration source the platform cannot answer.
      // Known peers are restored through Companion Device Manager presence
      // (API 31+) for an armed associated peer and claimed with the same
      // once-per-process semantics as iOS; that wake-fed path authenticates
      // against its own source, never against this refusal.
      throw contractError('capability.unsupported', 'restoration', 'react-native-rust-core.restoration.adopt', {
        domain: 'react-native-rust-core',
        code: 'androidRestorationNeedsPresenceWake',
        safeMessage:
          'Android restores known peers only through Companion Device Manager presence for an armed associated peer; no restoration source is configured.',
        metadata: Object.freeze({})
      })
    }
    if (
      attachment === null ||
      !validInteger(request.nativeProtocolMinimum) ||
      !validInteger(request.nativeProtocolMaximum) ||
      request.nativeProtocolMinimum > request.nativeProtocolMaximum ||
      request.nativeProtocolMinimum > RESTORATION_PROTOCOL_VERSION ||
      request.nativeProtocolMaximum < RESTORATION_PROTOCOL_VERSION ||
      request.namespaceValue.length === 0 ||
      request.expectedEpoch.length === 0 ||
      request.clientId.length === 0 ||
      request.hostSessionScope.length === 0 ||
      authority === null
    ) {
      refuse('The restoration request is malformed')
    }
    if (request.clientId !== authority.clientId || request.hostSessionScope !== authority.hostSessionScope) {
      refuse('Restoration adoption client or host session is unauthorized')
    }
    if (this.consumed) return rejection('alreadyConsumed', authority.clientId, authority.adoptionEpoch)
    if (request.namespaceValue !== authority.namespaceValue) {
      return rejection('namespaceMismatch', '', authority.adoptionEpoch)
    }
    if (request.attachmentId !== String(attachment.attachmentId)) {
      return rejection('attachmentMismatch', '', authority.adoptionEpoch)
    }
    if (request.expectedBackendInstanceId !== String(attachment.backendInstanceId)) {
      return rejection('backendMismatch', '', authority.adoptionEpoch)
    }
    if (request.expectedEpoch !== authority.adoptionEpoch) {
      return rejection('epochMismatch', '', authority.adoptionEpoch)
    }
    const records = journalRecords(
      attachment,
      authority,
      await this.options.claimRestoredPeers(RESTORATION_RECORD_CAPACITY - 1)
    )
    if (this.consumed) return rejection('alreadyConsumed', authority.clientId, authority.adoptionEpoch)
    this.consumed = true
    const receipt = nextReceipt
    nextReceipt += 1
    return Object.freeze({
      receiptId: `restoration-receipt-${receipt}`,
      outcome: 'adopted',
      boundClientId: authority.clientId,
      adoptionEpoch: authority.adoptionEpoch,
      replayRecordCount: records.length,
      records
    })
  }
}

/** One adapter record, then one connection record per restored peer (legacy layout). */
function journalRecords(
  attachment: AttachmentRecord<string>,
  authority: ReactNativeRestorationAuthority,
  peers: readonly WirePeerRecord[]
): readonly ReactNativeRestorationReplayRecord[] {
  if (peers.length + 1 > RESTORATION_RECORD_CAPACITY) {
    throw contractError('bytes.too-large', 'restoration', 'react-native-rust-core.restoration.journal-capacity')
  }
  const base = {
    recordVersion: RESTORATION_PROTOCOL_VERSION,
    namespaceValue: authority.namespaceValue,
    attachmentId: String(attachment.attachmentId),
    backendInstanceId: String(attachment.backendInstanceId),
    backendGeneration: String(attachment.backendGeneration),
    adapterId: String(attachment.adapter.adapterId),
    adapterGeneration: String(attachment.adapter.adapterGeneration),
    adoptionEpoch: authority.adoptionEpoch
  }
  const records: ReactNativeRestorationReplayRecord[] = [
    Object.freeze({
      ...base,
      ordinal: 1,
      kind: 'adapter',
      peerId: null,
      connectionId: null,
      ownerLeaseId: null,
      connectionGeneration: null
    })
  ]
  peers.forEach((peer, index) => {
    const ordinal = index + 2
    records.push(
      Object.freeze({
        ...base,
        ordinal,
        kind: 'connection',
        peerId: peer.peerId,
        connectionId: `restoration-connection-${ordinal}`,
        ownerLeaseId: `restoration-owner-${ordinal}`,
        connectionGeneration: `restoration-generation-${ordinal}`
      })
    )
  })
  return Object.freeze(records)
}
