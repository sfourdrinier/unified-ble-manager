import { contractError } from '../../backend-contract/errors'
import {
  projectTerminalEvidenceScopeUtf16,
  terminalEvidenceScopeUtf16le,
  unpackTerminalEvidenceScopeUtf16le
} from '../../core/terminal-evidence-scope-utf16'
import { decodeBase64, encodeBase64, type WireResult } from './rust-core-wire'

type TerminalEvidenceScope = Readonly<{
  managerId: string
  operationId: string
  incarnation: string
}>

export type NativeTerminalEvidenceScope = Readonly<
  TerminalEvidenceScope & {
    scopeUtf16leBase64?: string
  }
>

/**
 * Builds the private mobile wire scope. The scalar form remains byte-for-byte
 * legacy-compatible; only an unpaired JavaScript surrogate adds the raw
 * sidecar that lets Rust retain the original identity.
 */
export function terminalEvidenceScopeForNative(
  producer: 'scan' | 'subscription',
  scope: TerminalEvidenceScope
): NativeTerminalEvidenceScope {
  const raw = terminalEvidenceScopeUtf16le(producer, scope)
  return Object.freeze({
    managerId: projectTerminalEvidenceScopeUtf16(scope.managerId),
    operationId: projectTerminalEvidenceScopeUtf16(scope.operationId),
    incarnation: projectTerminalEvidenceScopeUtf16(scope.incarnation),
    ...(raw === undefined ? {} : { scopeUtf16leBase64: unwrap(encodeBase64(raw)) })
  })
}

/** Reconstructs a JavaScript scope after native recovery without replacing an unpaired surrogate. */
export function terminalEvidenceScopeFromNative(
  producer: 'scan' | 'subscription',
  scope: NativeTerminalEvidenceScope,
  operation: string
): TerminalEvidenceScope {
  if (scope.scopeUtf16leBase64 === undefined)
    return Object.freeze({
      managerId: scope.managerId,
      operationId: scope.operationId,
      incarnation: scope.incarnation
    })
  const raw = unwrap(decodeBase64(scope.scopeUtf16leBase64, `${operation}.scope-utf16le`))
  const fields = unpackTerminalEvidenceScopeUtf16le(raw)
  if (fields === undefined) throw malformed(`${operation}.scope-utf16le`)
  const expected = [producer, scope.managerId, scope.operationId, scope.incarnation]
  if (
    fields[0] !== producer ||
    projectTerminalEvidenceScopeUtf16(fields[1]) !== expected[1] ||
    projectTerminalEvidenceScopeUtf16(fields[2]) !== expected[2] ||
    projectTerminalEvidenceScopeUtf16(fields[3]) !== expected[3]
  )
    throw malformed(operation)
  return Object.freeze({ managerId: fields[1], operationId: fields[2], incarnation: fields[3] })
}

function unwrap<Value>(result: WireResult<Value>): Value {
  if (result.ok) return result.value
  throw result.error
}

function malformed(operation: string): ReturnType<typeof contractError> {
  return contractError('protocol.malformed', 'stream', operation)
}
