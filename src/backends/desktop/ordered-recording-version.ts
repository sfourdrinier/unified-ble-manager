import { contractError } from '../../backend-contract/errors'

/** A group transport version does not identify the independent receipt schema. */
export function assertDesktopOrderedReceiptVersion(
  owner: { orderedRecordingReceiptVersion?(): number },
  operation: string
): void {
  if (owner.orderedRecordingReceiptVersion?.call(owner) !== 2)
    throw contractError('protocol.incompatible', 'boundary', `${operation}.receipt-version`)
}
