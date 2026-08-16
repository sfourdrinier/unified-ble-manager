// src/ipc/client.ts

/**
 * The desktop webview client is shared by Electron preload bridges and Tauri
 * plugin transports. The compatibility implementation remains in the Electron
 * module during the 4.0 migration, while this is the framework-neutral name.
 */
export { ElectronRendererBleClient as IpcBleClient } from '../electron/renderer'
export type {
  ElectronConnectionEventCleanupReceipt as IpcConnectionEventCleanupReceipt,
  ElectronConnectionEventSubscription as IpcConnectionEventSubscription
} from '../electron/renderer'
