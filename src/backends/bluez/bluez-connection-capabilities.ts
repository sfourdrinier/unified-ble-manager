// src/backends/bluez/bluez-connection-capabilities.ts
//
// BlueZ connection-control capability truth (#149).
//
// BlueZ's D-Bus API exposes no LE connection-parameter surface to any client,
// privileged or not: `org.bluez.Device1` (BlueZ 5.85, doc/org.bluez.Device.rst)
// carries no connection interval, peripheral latency, supervision timeout, or
// parameter-update method — its `RSSI`/`TxPower` are inquiry/advertising-time
// values — and `org.bluez.Adapter1` (doc/org.bluez.Adapter.rst) adds nothing.
// The Linux channels that do reach connection parameters are privileged AND are
// not live per-connection updates (doc/mgmt.rst): the kernel management socket
// requires CAP_NET_ADMIN, its `Load Connection Parameters` command only stores
// per-device preferences for future connections, and `Get Connection
// Information` returns RSSI/TX power only; the debugfs knobs under
// /sys/kernel/debug/bluetooth/hci*/conn_{min,max}_interval require root and are
// adapter-global defaults for future connections. Raw HCI `LE Connection
// Update` would race bluetoothd for the device.
//
// A capability-detected privileged path would therefore still fabricate
// Android `requestConnectionPriority` semantics that the platform cannot
// honour on a live connection. The honest surface is an explicit `unsupported`
// registration whose limitations name the platform gap, the privilege
// requirement, and the consequence — so a caller learns *before* a slow link
// manifests as a peer disconnect, and the failure carries the reason instead
// of a bare `capability.unsupported`.

import {
  BUILT_IN_FEATURE_IDS,
  type FeatureId,
  type FeatureImplementation,
  type FeatureRegistry,
  type CapabilityLimits,
  type Limitation
} from '../../backend-contract/capabilities'
import { contractError } from '../../backend-contract/errors'
import { version, versionRange, type SerializableRecord } from '../../backend-contract/primitives'

const capabilitySchemaRange = versionRange(version('capability-schema', 1), version('capability-schema', 1))
const connectionControlScenarioIds = Object.freeze(['connection.rssi-and-att-mtu-capability-contract'])

/** Why `connection:priority` cannot be honoured on BlueZ, in caller-actionable terms. */
export const BLUEZ_CONNECTION_PRIORITY_LIMITATIONS: readonly Limitation[] = Object.freeze([
  Object.freeze({
    code: 'bluez-dbus-exposes-no-connection-priority',
    explanation:
      'BlueZ exposes no D-Bus API to request an LE connection priority or connection-parameter update ' +
      '(org.bluez.Device1 and org.bluez.Adapter1, BlueZ 5.85). The privileged Linux channels do not help a ' +
      'live connection either: the kernel management socket requires CAP_NET_ADMIN and its Load Connection ' +
      'Parameters command only stores per-device preferences for future connections, and the root-only ' +
      'debugfs conn_min_interval/conn_max_interval knobs are adapter-global defaults for future connections. ' +
      'This backend deliberately attempts no privileged path.',
    affectedGuarantee: 'caller-directed LE connection priority'
  }),
  Object.freeze({
    code: 'bluez-connection-interval-is-peer-negotiated',
    explanation:
      'GATT traffic runs at whatever LE connection interval the link negotiated; on a slow link each ' +
      'write-with-response round trip can take hundreds of milliseconds and a latency-sensitive peripheral ' +
      'may disconnect. If the peripheral needs a fast link, lower the adapter-wide kernel connection-interval ' +
      'defaults with administrative privilege before connecting, or drive the peripheral from a host stack ' +
      'that accepts connection-priority requests.',
    affectedGuarantee: 'GATT operation latency'
  })
])

/** Why `connection:parameters` cannot be observed on BlueZ, in caller-actionable terms. */
export const BLUEZ_CONNECTION_PARAMETERS_LIMITATIONS: readonly Limitation[] = Object.freeze([
  Object.freeze({
    code: 'bluez-dbus-exposes-no-connection-parameters',
    explanation:
      'BlueZ exposes no D-Bus read of the live LE connection interval, peripheral latency, or supervision ' +
      'timeout (org.bluez.Device1, BlueZ 5.85, documents only inquiry/advertising-time RSSI and TxPower), and ' +
      'even the CAP_NET_ADMIN kernel management Get Connection Information command returns only RSSI and TX ' +
      'power. The active connection interval is not observable from this process, so a slow link cannot be ' +
      'diagnosed through connection.controls.parameters() on this backend.',
    affectedGuarantee: 'observed LE connection parameters'
  })
])

/**
 * Registers `connection:priority` and `connection:parameters` as explicitly
 * `unsupported` so the BlueZ capability surface states the platform truth
 * instead of omitting the concept, and so the fail-closed public errors carry
 * these limitations as their reason.
 */
export function createBluezConnectionControlRegistrations(
  implementationVersion: string
): readonly FeatureRegistry['registrations'][number][] {
  return Object.freeze([
    unsupportedMetadataRegistration(
      BUILT_IN_FEATURE_IDS.connectionPriority,
      implementationVersion,
      'bluez-connection-priority-unsupported-v1',
      BLUEZ_CONNECTION_PRIORITY_LIMITATIONS,
      Object.freeze({ priorityRequests: Object.freeze({ maximum: 0, minimum: null, unit: 'requests' }) })
    ),
    unsupportedMetadataRegistration(
      BUILT_IN_FEATURE_IDS.connectionParameters,
      implementationVersion,
      'bluez-connection-parameters-unsupported-v1',
      BLUEZ_CONNECTION_PARAMETERS_LIMITATIONS,
      Object.freeze({ parameterObservations: Object.freeze({ maximum: 0, minimum: null, unit: 'observations' }) })
    )
  ])
}

function unsupportedMetadataRegistration(
  id: typeof BUILT_IN_FEATURE_IDS.connectionPriority | typeof BUILT_IN_FEATURE_IDS.connectionParameters,
  implementationVersion: string,
  sourceDigest: string,
  limitations: readonly Limitation[],
  limits: CapabilityLimits
): FeatureRegistry['registrations'][number] {
  return Object.freeze({
    id,
    state: 'unsupported' as const,
    selectedSchemaRange: capabilitySchemaRange,
    implementationOrigin: 'backend-native' as const,
    implementation: metadataImplementation(id),
    tck: Object.freeze({
      suiteId: 'connection-controls',
      requiredScenarioIds: connectionControlScenarioIds,
      contractRange: capabilitySchemaRange
    }),
    evidence: Object.freeze({
      receiptId: `${sourceDigest}:blocked`,
      evidenceLevel: 'blocked' as const,
      implementationVersion,
      sourceDigest,
      scenarioIds: connectionControlScenarioIds,
      limitations
    }),
    limitations,
    limits
  })
}

function metadataImplementation(featureId: FeatureId): FeatureImplementation<SerializableRecord, SerializableRecord> {
  return Object.freeze({
    async invoke(_input: SerializableRecord): Promise<SerializableRecord> {
      throw contractError('lifecycle.invalid-state', 'capability', `${featureId}.invoke-without-connection`)
    }
  })
}
