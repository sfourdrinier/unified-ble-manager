// src/backends/reactnative/native-terminal-evidence.ts
// Decode the RN owner's authenticated native terminal record without rewriting its bytes.
import { contractError, type NormalizedBleError } from '../../backend-contract/errors'
import { resourceCount } from '../../backend-contract/primitives'
import { decodeTerminalEvidenceError } from '../../core/terminal-evidence-codec'
import type { StreamTerminalNotice } from '../../backend-contract/streams'
import type { TerminalEvidenceCommit } from '../../backend-contract/terminal-evidence'
import { parseDrainText, remoteFailureError, type WireScanEndV2 } from './rust-core-wire'

/** Decode only the RN native terminal format selected by this trusted backend. */
export function decodeNativeTerminalEvidence(
  bytes: Readonly<Uint8Array>,
  receipt: TerminalEvidenceCommit,
  scanFailure: (record: WireScanEndV2) => NormalizedBleError,
  notice: StreamTerminalNotice
): { readonly scope: TerminalEvidenceCommit['scope']; readonly terminal: StreamTerminalNotice } {
  let input: unknown
  try {
    input = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
  } catch {
    throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.decode')
  }
  if (typeof input !== 'object' || input === null || Array.isArray(input))
    throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.record')
  const fields = new Map<string, unknown>(Object.entries(input))
  const type = fields.get('t')
  if (
    (receipt.producer === 'scan' && type !== 'scan-end') ||
    (receipt.producer === 'subscription' && type !== 'stream-end')
  )
    throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.producer')
  const sourceLoss = fields.get('sourceLoss')
  if (sourceLoss !== undefined) {
    if (type !== 'scan-end' || typeof sourceLoss !== 'object' || sourceLoss === null || Array.isArray(sourceLoss))
      throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.source-loss')
    const loss = new Map(Object.entries(sourceLoss))
    const count = loss.get('lostRecords')
    if (
      loss.size !== 3 ||
      loss.get('scope') !== 'host-global' ||
      loss.get('source') !== 'advertisement-ingress' ||
      typeof count !== 'number' ||
      !Number.isSafeInteger(count) ||
      count < 0
    )
      throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.source-loss')
    fields.delete('sourceLoss')
  }
  let localError: NormalizedBleError | null = null
  if (type === 'scan-end' && sourceLoss !== undefined && fields.get('reason') === 'source-failed') {
    const rawError = fields.get('error')
    if (typeof rawError !== 'object' || rawError === null || Array.isArray(rawError))
      throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.local-error')
    const errorFields = new Map(Object.entries(rawError))
    if (
      errorFields.size !== 5 ||
      !['code', 'domain', 'operation', 'retryability', 'platform'].every(key => errorFields.has(key)) ||
      errorFields.get('platform') !== null
    )
      throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.local-error')
    localError = decodeTerminalEvidenceError(rawError)
    if (
      localError.code !== 'stream.quota' ||
      localError.domain !== 'stream' ||
      localError.operation !== 'bounded-stream.source-loss-control' ||
      localError.retryability !== 'never' ||
      localError.platform !== null
    )
      throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.local-error')
    fields.delete('error')
  }
  const hasScanCounts =
    type === 'scan-end' && ['droppedItems', 'droppedBytes', 'replacedItems'].some(key => fields.has(key))
  if (sourceLoss !== undefined && (!hasScanCounts || (fields.get('reason') !== 'overflow' && localError === null)))
    throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.source-loss')
  let droppedItems = Number(notice.droppedItems)
  let droppedBytes = Number(notice.droppedBytes)
  let replacedItems = Number(notice.replacedItems)
  if (hasScanCounts) {
    if (fields.get('reason') !== 'overflow' && localError === null)
      throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.counts')
    const readCount = (key: string): number => {
      const value = fields.get(key)
      if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0)
        throw contractError('protocol.malformed', 'stream', `react-native.terminal-evidence.${key}`)
      fields.delete(key)
      return value
    }
    droppedItems = readCount('droppedItems')
    droppedBytes = readCount('droppedBytes')
    replacedItems = readCount('replacedItems')
    if (
      Number(notice.droppedItems) < droppedItems ||
      Number(notice.droppedBytes) < droppedBytes ||
      Number(notice.replacedItems) < replacedItems
    )
      throw contractError('protocol.violation', 'stream', 'react-native.terminal-evidence.counts')
  }
  if (fields.has('ordinal') || fields.has('evidence'))
    throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.record')
  fields.set('ordinal', 0)
  const decoded = parseDrainText(
    JSON.stringify({ more: false, records: [Object.fromEntries(fields)], controlLost: 0 }),
    null,
    fields.get('version') === 2 ? 2 : 1
  )
  if (!decoded.ok) throw decoded.error
  const record = decoded.value.records[0]
  if (record === undefined || (record.t !== 'scan-end' && record.t !== 'stream-end'))
    throw contractError('protocol.malformed', 'stream', 'react-native.terminal-evidence.record')
  const error =
    localError ??
    (record.t === 'scan-end' && 'version' in record
      ? scanFailure(record)
      : record.t === 'stream-end' && record.error !== undefined
        ? remoteFailureError(record.error).normalized
        : null)
  return Object.freeze({
    scope: receipt.scope,
    terminal: Object.freeze({
      kind: 'terminal',
      reason: record.t === 'scan-end' ? record.reason : notice.reason,
      droppedItems: resourceCount(droppedItems),
      droppedBytes: resourceCount(droppedBytes),
      replacedItems: resourceCount(replacedItems),
      ...(error === null ? {} : { error })
    })
  })
}
