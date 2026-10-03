import { invoke } from '@tauri-apps/api/core'
import { createTauriBleManager } from 'unified-ble-manager/tauri'

// Keep both public routes live in the bundle without requiring a Tauri process
// or pretending that a browser build exercises the radio.
globalThis.ubmPackedTauriProof = { invoke, createTauriBleManager }
