import { contractError } from '../../backend-contract/errors'
import type { BlePeer, ChooseOptions } from '../../public/ble-manager'
import { normalizeOperationOptions } from '../../public/operation-options'
import type { ReactNativeRustCoreHostServices } from './react-native-rust-core-provider'
import { nativeChooserFilters } from './react-native-native-chooser-filters'
import { awaitWithOperationAdmission } from '../../core/unified-ble-core-helpers'

/** Reuses the existing Rust/CDM association owner; selection is not a bond,
 * native connection, background wake arm, or an invented scan observation. */
export function createReactNativeCompanionChooser(
  services: ReactNativeRustCoreHostServices,
  peerId: (id: string) => string,
  now: () => number
): (options: ChooseOptions) => Promise<BlePeer> {
  return async options => {
    const operation = normalizeOperationOptions({ ...options, timeoutMs: options.timeoutMs ?? 60000 }, now)
    const filters = nativeChooserFilters(options, 'android')
    const result = await awaitWithOperationAdmission(
      services.associateCompanion({ filtersJson: JSON.stringify(filters), ...operation }),
      operation,
      now,
      'react-native.companion-chooser'
    )
    if (operation.signal?.aborted === true)
      throw contractError('operation.aborted', 'chooser', 'react-native.companion-chooser')
    if (operation.deadline !== null && operation.deadline <= now())
      throw contractError('operation.timed-out', 'chooser', 'react-native.companion-chooser')
    if (result.peerId === null)
      throw contractError('chooser.permitted-device-unavailable', 'chooser', 'react-native.companion-chooser')
    return Object.freeze({
      id: peerId(result.peerId),
      name: null,
      rssi: null,
      reference: null,
      sources: Object.freeze(['origin-authorized'] as const),
      lastAdvertisement: null
    })
  }
}
