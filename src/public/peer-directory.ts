import { contractError } from '../backend-contract/errors'
import type { OperationOptions } from './operation-options'
import type { BlePeer } from './ble-manager'
import type { PeerReference } from './peer-reference'

export type PeerSource =
  | 'scan-observed'
  | 'app-reference'
  | 'system-connected'
  | 'system-bonded'
  | 'origin-authorized'
  | 'restored'
  | 'backend-cache'

export interface KnownPeerQuery extends OperationOptions {
  readonly sources?: readonly PeerSource[]
  readonly services?: readonly (string | number)[]
  readonly references?: readonly PeerReference[]
  readonly includeUnavailable?: boolean
}

export interface BlePeerState {
  readonly reachability: 'reachable' | 'unreachable' | 'unknown'
  readonly connection: 'connected' | 'disconnected' | 'unknown'
  readonly bond: 'bonded' | 'not-bonded' | 'unknown' | 'unsupported'
  readonly lastSeenAtMonotonicMs: number | null
}

export interface BlePeerDirectory {
  resolve(reference: PeerReference, options?: OperationOptions): Promise<BlePeer | null>
  known(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  connected(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  bonded(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  authorized(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
  restored(options?: KnownPeerQuery): Promise<readonly BlePeer[]>
}

export function unsupportedPeerDirectory(): BlePeerDirectory {
  const unsupported = async (): Promise<readonly BlePeer[]> => {
    throw contractError('capability.unsupported', 'connection', 'peer-directory')
  }
  return {
    resolve: async () => null,
    known: unsupported,
    connected: unsupported,
    bonded: unsupported,
    authorized: unsupported,
    restored: unsupported
  }
}
