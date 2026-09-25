import { contractError, type NormalizedBleError } from '../backend-contract/errors'
import type { StreamOverflowNotice, StreamTerminalNotice } from '../backend-contract/streams'

/** Aggregate event ingress has no child-stream attribution. Never copy its counts onto a child. */
export function aggregateEventLossError(
  operation: string,
  source: string,
  notice: Pick<StreamOverflowNotice | StreamTerminalNotice, 'droppedItems' | 'droppedBytes' | 'replacedItems'>
): NormalizedBleError {
  return contractError('stream.overflow', 'ipc', operation, {
    domain: source,
    code: 'aggregate-overflow',
    safeMessage: 'The shared IPC event stream lost events with unknown child stream attribution',
    metadata: Object.freeze({
      attribution: 'unknown',
      droppedItems: Number(notice.droppedItems),
      droppedBytes: Number(notice.droppedBytes),
      replacedItems: Number(notice.replacedItems)
    })
  }).normalized
}
