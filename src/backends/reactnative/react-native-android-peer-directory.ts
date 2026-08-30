import type { BackendPeerRecord, BackendPeerQuery, PeerDirectoryBackend } from '../../backend-contract/backend'
import { contractError } from '../../backend-contract/errors'
import { assertPeerReference, encodePeerReference, type PeerReference } from '../../backend-contract/peer-reference'
import { opaqueId } from '../../backend-contract/primitives'
import type { CoreBluetoothBackend } from '../corebluetooth/corebluetooth-backend'
import type {
  ReactNativeAndroidProtocolBoundary,
  AndroidBondedPeerSnapshot
} from '../../native-protocol/rn-android-boundary'

const ANDROID_BACKEND_ID = 'unified-ble:react-native-android'

/**
 * Projects Android's current system bond table into the public peer directory.
 * Native identifiers are retained only as an in-process lookup input; the
 * reference opaqueId is a stable backend token, not a durable MAC address.
 */
export class ReactNativeAndroidPeerDirectory implements PeerDirectoryBackend<string> {
  readonly resolve = (reference: PeerReference, options: BackendPeerQuery): Promise<BackendPeerRecord<string> | null> =>
    this.resolveBonded(reference, options)
  readonly known = (_options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> =>
    this.unsupported('known')
  readonly connected = (_options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> =>
    this.unsupported('connected')
  readonly bonded = (options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> =>
    this.bondedPeers(options)
  readonly authorized = (_options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> =>
    this.unsupported('authorized')
  readonly restored = (_options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> =>
    this.unsupported('restored')

  constructor(
    private readonly boundary: ReactNativeAndroidProtocolBoundary,
    private readonly delegate: CoreBluetoothBackend
  ) {}

  private async bondedPeers(options: BackendPeerQuery): Promise<readonly BackendPeerRecord<string>[]> {
    assertSupportedQuery(options, 'bonded')
    if (options.sources !== undefined && !options.sources.includes('system-bonded')) {
      return Object.freeze([])
    }
    const snapshots = await this.enumerate(options)
    const records = deduplicateSnapshots(snapshots).map(snapshot => this.toRecord(snapshot))
    if (options.references === undefined) return Object.freeze(records)
    const references = new Set(
      options.references.map((reference, index) => this.referenceKey(reference, `bonded.references[${index}]`))
    )
    return Object.freeze(records.filter(record => references.has(encodePeerReference(record.reference))))
  }

  private async resolveBonded(
    reference: PeerReference,
    options: BackendPeerQuery
  ): Promise<BackendPeerRecord<string> | null> {
    assertPeerReference(reference, 'android-peer-directory.resolve')
    if (reference.backendId !== ANDROID_BACKEND_ID || reference.scope !== 'system') {
      throw contractError('peer.scope-mismatch', 'connection', 'android-peer-directory.resolve')
    }
    assertSupportedQuery(options, 'resolve')
    if (options.sources !== undefined && !options.sources.includes('system-bonded')) return null
    const snapshots = await this.enumerate(options)
    const snapshot = deduplicateSnapshots(snapshots).find(
      candidate => this.referenceForNativeId(candidate.nativePeerId).opaqueId === reference.opaqueId
    )
    return snapshot === undefined ? null : this.toRecord(snapshot)
  }

  private async enumerate(options: BackendPeerQuery): Promise<readonly AndroidBondedPeerSnapshot[]> {
    return this.delegate.operationLifecycle.awaitBoundaryOperation(
      options,
      'android-peer-directory.enumerate-bonded-peers',
      () => this.boundary.enumerateBondedPeers(),
      undefined,
      undefined,
      null
    )
  }

  private toRecord(snapshot: AndroidBondedPeerSnapshot): BackendPeerRecord<string> {
    const nativePeerId = snapshot.nativePeerId
    const connection = this.boundary.connectionState(nativePeerId)
    return Object.freeze({
      reference: this.referenceForNativeId(nativePeerId),
      peerId: this.delegate.peerIdForNativeId(nativePeerId),
      name: snapshot.displayName,
      rssi: null,
      source: 'system-bonded',
      state: Object.freeze({
        bond: 'bonded',
        reachability: 'unknown',
        connection: connection === 'connected' ? 'connected' : 'unknown',
        lastSeenAtMonotonicMs: null
      })
    })
  }

  private referenceForNativeId(nativePeerId: string): PeerReference {
    return Object.freeze({
      version: 1,
      backendId: ANDROID_BACKEND_ID,
      scope: 'system',
      opaqueId: opaqueId(stablePeerToken(nativePeerId), 'peer-reference', ANDROID_BACKEND_ID)
    })
  }

  private referenceKey(reference: PeerReference, operation: string): string {
    assertPeerReference(reference, `android-peer-directory.${operation}`)
    if (reference.backendId !== ANDROID_BACKEND_ID || reference.scope !== 'system') {
      throw contractError('peer.scope-mismatch', 'connection', `android-peer-directory.${operation}`)
    }
    return encodePeerReference(reference)
  }

  private unsupported(operation: string): Promise<readonly BackendPeerRecord<string>[]> {
    return Promise.reject(contractError('capability.unsupported', 'connection', `android-peer-directory.${operation}`))
  }
}

function assertSupportedQuery(options: BackendPeerQuery, operation: string): void {
  if (options.services !== undefined && options.services.length > 0) {
    throw contractError('capability.unsupported', 'connection', `android-peer-directory.${operation}.services`)
  }
}

function deduplicateSnapshots(snapshots: readonly AndroidBondedPeerSnapshot[]): readonly AndroidBondedPeerSnapshot[] {
  const byNativeId = new Map<string, AndroidBondedPeerSnapshot>()
  for (const snapshot of snapshots) {
    const current = byNativeId.get(snapshot.nativePeerId)
    if (
      current === undefined ||
      (current.displayName === null && snapshot.displayName !== null) ||
      (current.displayName !== null && snapshot.displayName !== null && snapshot.displayName < current.displayName)
    ) {
      byNativeId.set(snapshot.nativePeerId, snapshot)
    }
  }
  return Object.freeze(
    [...byNativeId.values()].sort((left, right) =>
      left.nativePeerId < right.nativePeerId ? -1 : left.nativePeerId > right.nativePeerId ? 1 : 0
    )
  )
}

function stablePeerToken(nativePeerId: string): string {
  let first = 0x811c9dc5
  let second = 0x9e3779b9
  for (let index = 0; index < nativePeerId.length; index += 1) {
    const code = nativePeerId.charCodeAt(index)
    first = Math.imul(first ^ code, 0x01000193)
    second = Math.imul(second ^ (code + index), 0x01000193)
  }
  return `android-bonded-${(first >>> 0).toString(16).padStart(8, '0')}-${(second >>> 0).toString(16).padStart(8, '0')}`
}
