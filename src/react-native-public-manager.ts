import type { ReactNativeManagerHost } from './react-native-manager'
import { createPublicBleManager } from './public/ble-manager'

/** One public composition for RN and Expo: native capabilities and owned UI
 * must not disappear when a host adds readiness/permission conveniences. */
export function composeReactNativePublicManager(host: ReactNativeManagerHost, now: () => number) {
  return createPublicBleManager(
    host.manager,
    now,
    host.chooseAccessory === undefined ? {} : { choose: host.chooseAccessory, discoveryKind: 'hybrid' }
  )
}
