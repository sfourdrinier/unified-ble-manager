import { utf8ByteLength } from '../backend-contract/serializable'
import { webRecordingRecord } from './web-recording-journal-bridge'
import type { WebIngressTerminalReason } from './web-notification-ingress'

/** Exact upper bounds for this Web ingress's fixed terminal vocabulary and a
 * marker envelope. Delivery/source IDs are unsigned 64-bit decimal strings. */
export function webRecordingControlBudget(row: Record<string, unknown>): { marker: number; terminal: number } {
  const context = row.context
  if (
    typeof context !== 'object' ||
    context === null ||
    !('version' in context) ||
    !('peerId' in context) ||
    (context.version !== 1 && context.version !== 2) ||
    typeof context.peerId !== 'string' ||
    typeof row.id !== 'string' ||
    typeof row.collectionId !== 'string' ||
    typeof row.sourceGeneration !== 'string'
  )
    throw new Error('Invalid Web recording control budget declaration')
  const maxId = '18446744073709551615'
  const version = context.version
  const metadata = {
    kind: 'ordered',
    version: context.version,
    recordingId: row.id,
    peerId: context.peerId,
    collectionId: row.collectionId,
    sourceGeneration: { kind: 'opaque', value: row.sourceGeneration },
    deliveryId: maxId
  }
  const size = (record: unknown): number =>
    utf8ByteLength(JSON.stringify({ ordinal: Number.MAX_SAFE_INTEGER, metadata, record }))
  const reasons: WebIngressTerminalReason[] = [
    'closed',
    'owner-released',
    'connection-lost',
    'source-failed',
    'service-changed',
    'host-reset',
    'operation-aborted',
    'operation-timed-out',
    'overflow',
    'ordinal-exhausted'
  ]
  return {
    marker: size({ kind: 'marker', markerId: maxId, ordinal: maxId }),
    terminal: Math.max(
      ...reasons.map(reason => size(webRecordingRecord({ kind: 'terminal', reason, uncertainTail: false }, version)))
    )
  }
}
