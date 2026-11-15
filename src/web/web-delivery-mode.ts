import { contractError } from '../backend-contract/errors'
import type { SubscriptionOptions } from '../backend-contract/operations'
import type { WebBluetoothCharacteristicBoundary } from './web-bluetooth-boundary'

/** A browser characteristic event does not reveal its source or delivery mode. */
export function assertWebDeliveryMode(
  characteristic: WebBluetoothCharacteristicBoundary,
  mode: SubscriptionOptions['deliveryMode'],
  operation: string
): void {
  const { notify, indicate } = characteristic.properties
  if (
    (!notify && !indicate) ||
    (mode === 'require-notification' && !notify) ||
    (mode === 'require-indication' && !indicate)
  ) {
    throw contractError('gatt.property-not-supported', 'gatt', operation)
  }
  if (mode === 'require-notification' || mode === 'require-indication') {
    throw contractError('capability.limited', 'gatt', operation)
  }
}
