import {
  BLE_ERROR_CODES,
  BLE_ERROR_DOMAINS,
  contractError,
  optionalPlatformErrorDetail,
  type NormalizedBleError
} from '../backend-contract/errors'
import type { StreamTerminalNotice } from '../backend-contract/streams'
import type { TerminalEvidenceCommit } from '../backend-contract/terminal-evidence'

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })

/** Versioned, canonical and reversible terminal payload; bytes use a tagged hex form. */
export function encodeTerminalEvidence(
  terminal: StreamTerminalNotice,
  scope: { readonly managerId: string; readonly operationId: string; readonly incarnation: string }
): Uint8Array {
  return encoder.encode(stable({ formatVersion: 1, scope, terminal: terminalSnapshot(terminal) }))
}

export function decodeTerminalEvidence(bytes: Readonly<Uint8Array>): {
  readonly formatVersion: 1
  readonly scope: { readonly managerId: string; readonly operationId: string; readonly incarnation: string }
  readonly terminal: Record<string, unknown>
} {
  let parsed: unknown
  try {
    parsed = JSON.parse(decoder.decode(bytes))
  } catch {
    throw contractError('protocol.malformed', 'stream', 'terminal-evidence.decode')
  }
  if (!isRecord(parsed) || parsed.formatVersion !== 1 || !isRecord(parsed.scope) || !isRecord(parsed.terminal))
    throw contractError('protocol.malformed', 'stream', 'terminal-evidence.decode')
  return Object.freeze({
    formatVersion: 1,
    scope: Object.freeze({
      managerId: requiredString(parsed.scope.managerId),
      operationId: requiredString(parsed.scope.operationId),
      incarnation: requiredString(parsed.scope.incarnation)
    }),
    terminal: reviveRecord(parsed.terminal)
  })
}

export function decodeTerminalEvidenceError(value: unknown): NormalizedBleError {
  if (
    !isRecord(value) ||
    typeof value.code !== 'string' ||
    typeof value.domain !== 'string' ||
    typeof value.operation !== 'string'
  )
    throw contractError('protocol.malformed', 'stream', 'terminal-evidence.error')
  const code = BLE_ERROR_CODES.find(candidate => candidate === value.code)
  const domain = BLE_ERROR_DOMAINS.find(candidate => candidate === value.domain)
  const retryability =
    value.retryability === 'never' || value.retryability === 'caller-decides' ? value.retryability : undefined
  if (code === undefined || domain === undefined || value.operation.length === 0 || retryability === undefined)
    throw contractError('protocol.malformed', 'stream', 'terminal-evidence.error')
  return Object.freeze({
    code,
    domain,
    operation: value.operation,
    retryability,
    platform: optionalPlatformErrorDetail(value.platform, 'terminal-evidence.error') ?? null
  })
}

/** Reads and validates a committed receipt before restoring the legacy error field. */
export async function hydrateTerminalEvidence(
  source: {
    readonly readTerminalEvidence?: (
      ref: string,
      maximumBytes: number
    ) => Promise<
      | { readonly kind: 'committed'; readonly canonicalBytes: Readonly<Uint8Array>; readonly digest: string }
      | { readonly kind: 'missing' | 'unresolved' }
    >
  },
  notice: StreamTerminalNotice,
  maximumBytes: number
): Promise<StreamTerminalNotice> {
  const evidence = notice.evidence
  if (evidence === undefined || source.readTerminalEvidence === undefined)
    throw contractError('capability.unsupported', 'stream', 'terminal-evidence.read')
  const read = await source.readTerminalEvidence(evidence.ref, maximumBytes)
  if (
    read.kind !== 'committed' ||
    read.canonicalBytes.byteLength !== evidence.byteLength ||
    read.digest !== evidence.digest
  )
    throw contractError('protocol.violation', 'stream', 'terminal-evidence.receipt')
  const decoded = decodeTerminalEvidence(read.canonicalBytes)
  if (!sameScope(decoded.scope, evidence.scope))
    throw contractError('protocol.violation', 'stream', 'terminal-evidence.scope')
  const error = decoded.terminal.error === undefined ? null : decodeTerminalEvidenceError(decoded.terminal.error)
  return Object.freeze({ ...notice, ...(error === null ? {} : { error }) })
}

function sameScope(
  left: { readonly managerId: string; readonly operationId: string; readonly incarnation: string },
  right: TerminalEvidenceCommit['scope']
): boolean {
  return (
    left.managerId === right.managerId &&
    left.operationId === right.operationId &&
    left.incarnation === right.incarnation
  )
}

function terminalSnapshot(terminal: StreamTerminalNotice): Record<string, unknown> {
  const result: Record<string, unknown> = Object.create(null)
  result.kind = terminal.kind
  result.reason = terminal.reason
  result.droppedItems = terminal.droppedItems
  result.droppedBytes = terminal.droppedBytes
  result.replacedItems = terminal.replacedItems
  if (terminal.error !== undefined && terminal.error !== null) result.error = terminal.error
  return result
}
function stable(value: unknown): string {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return JSON.stringify(value)
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw contractError('protocol.malformed', 'stream', 'terminal-evidence.number')
    return JSON.stringify(value)
  }
  if (value instanceof Uint8Array)
    return `{"$bytes":${JSON.stringify([...value].map(byte => byte.toString(16).padStart(2, '0')).join(''))}}`
  if (Array.isArray(value)) return `[${value.map(stable).join(',')}]`
  if (!isRecord(value)) throw contractError('protocol.malformed', 'stream', 'terminal-evidence.value')
  return `{${Object.keys(value)
    .sort()
    .map(key => `${JSON.stringify(key)}:${stable(value[key])}`)
    .join(',')}}`
}
function reviveRecord(value: Record<string, unknown>): Record<string, unknown> {
  const result: Record<string, unknown> = Object.create(null)
  for (const [key, entry] of Object.entries(value)) result[key] = revive(entry)
  return Object.freeze(result)
}
function revive(value: unknown): unknown {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) throw contractError('protocol.malformed', 'stream', 'terminal-evidence.number')
    return value
  }
  if (Array.isArray(value)) return Object.freeze(value.map(revive))
  if (!isRecord(value)) throw contractError('protocol.malformed', 'stream', 'terminal-evidence.value')
  if (Object.keys(value).length === 1 && typeof value.$bytes === 'string') return hex(value.$bytes)
  return reviveRecord(value)
}
function hex(value: string): Uint8Array {
  if (!/^(?:[0-9a-f]{2})*$/u.test(value)) throw contractError('protocol.malformed', 'stream', 'terminal-evidence.bytes')
  const result = new Uint8Array(value.length / 2)
  for (let index = 0; index < result.length; index++)
    result[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16)
  return result
}
function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}
function requiredString(value: unknown): string {
  if (typeof value !== 'string' || value.length === 0)
    throw contractError('protocol.malformed', 'stream', 'terminal-evidence.scope')
  return value
}
