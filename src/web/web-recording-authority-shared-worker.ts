import { installWebRecordingAuthorityWorker } from './web-recording-authority-worker'
import type { WebRecordingCoordinatorScope } from './web-recording-authority-worker'

// Bundle this module as the same fixed URL for every participant, and construct
// it as a module SharedWorker in the same browser storage partition.
const scope: WebRecordingCoordinatorScope = { onconnect: null }
installWebRecordingAuthorityWorker(scope)
Object.defineProperty(globalThis, 'onconnect', { value: scope.onconnect, configurable: true, writable: true })
