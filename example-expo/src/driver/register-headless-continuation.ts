import { AppRegistry, Platform } from 'react-native'
import AsyncStorage from '@react-native-async-storage/async-storage'
import { createExpoBleManager } from 'unified-ble-manager/expo'
import { installHeadlessContinuation } from './headless-continuation-job.ts'
import { createHeadlessHistory } from './headless-continuation-history.ts'

/** App-private, bounded diagnostic history. No automatic log/network export. */
export { HEADLESS_EVIDENCE_KEY } from './headless-continuation-history.ts'
export const headlessHistory = createHeadlessHistory(AsyncStorage)
installHeadlessContinuation(AppRegistry, Platform.OS, {
  createManager: () => createExpoBleManager({ instanceId: 'reference-headless-battery' }),
  save: evidence => headlessHistory.save(evidence)
})
