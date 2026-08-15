// src/backend-contract/electron.ts

/**
 * Electron compatibility surface. The authority itself is framework-neutral so
 * Tauri and future desktop webview hosts share the same replay, quota, version,
 * attachment, and ownership enforcement.
 */
export * from './ipc'
export { IpcArbiterContext as ElectronMainArbiterContext } from './ipc'
