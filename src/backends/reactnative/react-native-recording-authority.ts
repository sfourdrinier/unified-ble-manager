import { contractError } from '../../backend-contract/errors'

// This is an integration boundary, not a wire schema. Native owns the exact
// version and encoding; no implementation is registered until that contract is
// agreed and wired through the provider and manager.
export interface ReactNativeRecordingAuthorityRequest {
  readonly processHostIncarnation: string
  readonly runtimeIncarnation: string
  readonly session: string
  readonly lease: string
  readonly peerId: string
  readonly collectionId: string
  readonly recordingId: string
  readonly capability: string
  readonly requestId: string
  readonly callerKey: string
  readonly connectionGeneration: string
  readonly databaseGeneration: string
  readonly authorityRevision: string
}

export type ReactNativeRecordingAuthorityOutcome =
  | { readonly state: 'unknown' }
  | { readonly state: 'no-effect'; readonly reason: import('../../backend-contract/errors').NormalizedBleError }
  | { readonly state: 'committed'; readonly authorityRevision: string; readonly graphRevision: string }

/** All calls authenticate the exact recorder and request. Fence covers late dispatch. */
export interface ReactNativeRecordingAuthorityAdapter {
  readonly version: string
  commit(request: ReactNativeRecordingAuthorityRequest): Promise<ReactNativeRecordingAuthorityOutcome>
  status(request: ReactNativeRecordingAuthorityRequest): Promise<ReactNativeRecordingAuthorityOutcome>
  fence(request: ReactNativeRecordingAuthorityRequest): Promise<ReactNativeRecordingAuthorityOutcome>
  snapshot(request: ReactNativeRecordingAuthorityRequest): Promise<{
    readonly state: 'active' | 'stopping' | 'terminal' | 'unknown'
    readonly authorityRevision: string
    readonly graphRevision: string
  }>
}

/** Select only an exact authority offer; never infer it from the outer wire. */
export function negotiateReactNativeRecordingAuthority(
  offer: string | null,
  adapters: readonly ReactNativeRecordingAuthorityAdapter[]
): ReactNativeRecordingAuthorityAdapter | null {
  if (offer === null || offer.length === 0) return null
  const matches = adapters.filter(adapter => adapter.version === offer)
  if (matches.length > 1)
    throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.authority-version')
  const match = matches[0]
  if (match === undefined) return null
  if (
    typeof match.commit !== 'function' ||
    typeof match.status !== 'function' ||
    typeof match.fence !== 'function' ||
    typeof match.snapshot !== 'function'
  )
    throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.authority-adapter')
  return match
}

/** Reclaim wire v2 predates atomic graph and terminal authority. */
export function supportsReactNativeRecordingAuthority(_reclaimVersion: number): boolean {
  return false
}
