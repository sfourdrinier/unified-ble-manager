import { contractError } from '../../backend-contract/errors'
import { canonicalUuidInput } from '../../backend-contract/primitives'
import { assertPublicChooseOptions, type ChooseOptions } from '../../public/ble-manager'

/** Shared OS selector lowering. Both platforms expose one service/company
 * condition per native filter, while alternative filters remain OR. */
export function nativeChooserFilters(options: ChooseOptions, platform: 'apple' | 'android') {
  assertPublicChooseOptions(options)
  const unsupported = (): never => {
    throw contractError('capability.unsupported', 'chooser', `react-native.${platform}.chooser-filter`)
  }
  const acceptAllDevices = options.acceptAllDevices ?? (options.filters?.length ?? 0) === 0
  if (acceptAllDevices) return platform === 'android' ? [{}] : unsupported()
  if (options.filters === undefined || options.filters.length === 0 || options.filters.length > 16) return unsupported()
  return options.filters.map(filter => {
    if ((filter.serviceUuids?.length ?? 0) > 1 || (filter.manufacturerData?.length ?? 0) > 1) return unsupported()
    const manufacturer = filter.manufacturerData?.[0]
    const namePrefix = filter.localNamePrefix
    if (
      platform === 'apple' &&
      (((filter.serviceUuids?.length ?? 0) === 0 && manufacturer === undefined) ||
        (namePrefix === undefined && (manufacturer?.dataPrefix?.length ?? 0) === 0))
    )
      return unsupported()
    return {
      ...(filter.serviceUuids?.[0] === undefined ? {} : { serviceUuid: canonicalUuidInput(filter.serviceUuids[0]) }),
      ...(namePrefix === undefined ? {} : { namePrefix }),
      ...(manufacturer === undefined
        ? {}
        : {
            companyIdentifier: manufacturer.companyIdentifier,
            manufacturerPrefix: Array.from(manufacturer.dataPrefix ?? [])
          })
    }
  })
}
