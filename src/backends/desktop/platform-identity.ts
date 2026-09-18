// src/backends/desktop/platform-identity.ts
//
// Public identities of the three desktop platform backends. The Rust-core
// desktop provider registers under these ids, so they belong to the shipped
// path rather than to the legacy backends that first defined them.

import { UNIFIED_BLE_IMPLEMENTATION_VERSION } from '../../implementation-version'
import { version, versionRange, type BackendCompatibilityOffer } from '../../backend-contract/primitives'

export const COREBLUETOOTH_BACKEND_ID = 'unified-ble:corebluetooth'
export const COREBLUETOOTH_PLATFORM_ID = 'unified-ble:macos-corebluetooth'
export const COREBLUETOOTH_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION

export const WINRT_BACKEND_ID = 'unified-ble:winrt'
export const WINRT_PLATFORM_ID = 'unified-ble:windows-winrt'
export const WINRT_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION

export const BLUEZ_BACKEND_ID = 'unified-ble:bluez-dbus'
export const BLUEZ_PLATFORM_ID = 'unified-ble:linux-bluez'
export const BLUEZ_IMPLEMENTATION_VERSION = UNIFIED_BLE_IMPLEMENTATION_VERSION

/**
 * Disclosed whenever a BlueZ adapter state reports `authorization: 'unknown'`.
 * BlueZ has no per-application Bluetooth authorization concept, so there is
 * nothing for this backend to measure and nothing it may substitute.
 */
export const BLUEZ_NO_AUTHORIZATION_CONCEPT_REASON = 'BlueZ exposes no per-application Bluetooth authorization concept'

export type BluezBusKind = 'system' | 'session'

function schemaVersionOneCompatibility(): BackendCompatibilityOffer {
  return Object.freeze({
    backendContract: versionRange(version('backend-contract', 1), version('backend-contract', 1)),
    capabilitySchema: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
    eventSchema: versionRange(version('event-schema', 1), version('event-schema', 1)),
    traceFormat: versionRange(version('trace-format', 1), version('trace-format', 1))
  })
}

export const coreBluetoothCompatibility: BackendCompatibilityOffer = schemaVersionOneCompatibility()
export const winRtCompatibility: BackendCompatibilityOffer = schemaVersionOneCompatibility()
export const bluezCompatibility: BackendCompatibilityOffer = schemaVersionOneCompatibility()
