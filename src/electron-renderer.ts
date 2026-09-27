// src/electron-renderer.ts

export { createNativeContinuationControl } from './backends/desktop/native-continuation-controller'
export type {
  NativeContinuationControl,
  NativeContinuationControlAccess,
  NativeContinuationCompleted,
  NativeContinuationStatus
} from './backends/desktop/native-continuation-controller'
export { createNativeContinuationRecordingController } from './core/continuation-recording'
export type { ContinuationRecordingAccess, ContinuationRecordingController } from './core/continuation-recording'

export * from './electron/protocol'
export { ElectronRendererBleClient } from './electron/renderer'
export type { ElectronConnectionEventCleanupReceipt, ElectronConnectionEventSubscription } from './electron/renderer'
export {
  createElectronRendererBleManager,
  createElectronRendererBleManagerWithEnvironment
} from './electron/public-manager'
export type { ElectronRendererBleManagerEnvironment } from './electron/public-manager'
export { assertAdvertisementObservation as assertElectronAdvertisementObservation } from './electron/advertisement-observation'
