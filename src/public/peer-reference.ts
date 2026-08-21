import { contractError } from '../backend-contract/errors'

export type PeerReferenceScope = 'application' | 'origin' | 'system'

export interface PeerReference {
  readonly version: 1
  readonly backendId: string
  readonly scope: PeerReferenceScope
  readonly opaqueId: string
}

export function encodePeerReference(reference: PeerReference): string {
  assertPeerReference(reference, 'peer.reference.encode')
  return JSON.stringify({
    backendId: reference.backendId,
    opaqueId: reference.opaqueId,
    scope: reference.scope,
    version: reference.version
  })
}

export function decodePeerReference(value: string): PeerReference {
  let parsed: unknown
  try {
    parsed = JSON.parse(value)
  } catch {
    throw contractError('peer.reference-invalid', 'connection', 'peer.reference.decode')
  }
  if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
    throw contractError('peer.reference-invalid', 'connection', 'peer.reference.decode')
  }
  const version = Reflect.get(parsed, 'version')
  if (typeof version !== 'number') {
    throw contractError('peer.reference-invalid', 'connection', 'peer.reference.decode-version')
  }
  if (version > 1) {
    throw contractError('peer.reference-version-unsupported', 'connection', 'peer.reference.decode-version')
  }
  if (version !== 1) {
    throw contractError('peer.reference-invalid', 'connection', 'peer.reference.decode-version')
  }
  const backendId = Reflect.get(parsed, 'backendId')
  const scope = Reflect.get(parsed, 'scope')
  const opaqueId = Reflect.get(parsed, 'opaqueId')
  if (Object.keys(parsed).some(key => !['version', 'backendId', 'scope', 'opaqueId'].includes(key))) {
    throw contractError('peer.reference-invalid', 'connection', 'peer.reference.decode-keys')
  }
  if (typeof backendId !== 'string' || typeof scope !== 'string' || typeof opaqueId !== 'string') {
    throw contractError('peer.reference-invalid', 'connection', 'peer.reference.decode-fields')
  }
  const reference: PeerReference = { version: 1, backendId, scope: parseScope(scope), opaqueId }
  assertPeerReference(reference, 'peer.reference.decode')
  return Object.freeze(reference)
}

export function assertPeerReference(reference: PeerReference, operation: string): void {
  if (typeof reference !== 'object' || reference === null) {
    throw contractError('peer.reference-invalid', 'connection', operation)
  }
  const backendId = Reflect.get(reference, 'backendId')
  const opaqueId = Reflect.get(reference, 'opaqueId')
  const scope = Reflect.get(reference, 'scope')
  const version = Reflect.get(reference, 'version')
  if (
    version !== 1 ||
    typeof backendId !== 'string' ||
    backendId.length === 0 ||
    typeof opaqueId !== 'string' ||
    opaqueId.length === 0 ||
    typeof scope !== 'string' ||
    !['application', 'origin', 'system'].includes(scope)
  ) {
    throw contractError('peer.reference-invalid', 'connection', operation)
  }
}

function parseScope(value: string): PeerReferenceScope {
  if (value === 'application' || value === 'origin' || value === 'system') return value
  throw contractError('peer.reference-invalid', 'connection', 'peer.reference.scope')
}
