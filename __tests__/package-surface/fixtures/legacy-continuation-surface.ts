// A 5.2.1 consumer can still exhaustively handle its complete-claim terminal
// vocabulary. Rich terminal recovery is selected through a versioned route.
import type { ContinuationBacklogStreamEnd } from 'unified-ble-manager/node/corebluetooth'

function assertNever(value: never): never {
  throw new Error(`unexpected legacy continuation reason: ${value}`)
}

export function handleLegacyContinuationEnd(end: ContinuationBacklogStreamEnd): number {
  switch (end.reason) {
    case 'overflow':
      return end.droppedItems
    case 'invalidated':
      return end.droppedBytes
    case 'closed':
      return 0
    default:
      return assertNever(end.reason)
  }
}
