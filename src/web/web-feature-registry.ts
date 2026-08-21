// src/web/web-feature-registry.ts

import {
  BUILT_IN_FEATURE_IDS,
  createBackendOperationCapabilityRegistration,
  createFeatureRegistry,
  type CapabilityLimits,
  type FeatureId,
  type FeatureImplementation,
  type FeatureRegistry,
  type Limitation
} from '../backend-contract/capabilities'
import { contractError } from '../backend-contract/errors'
import { version, versionRange, type SerializableRecord } from '../backend-contract/primitives'
import {
  WEB_CHOOSER_TCK_FEATURE_SUITE,
  WEB_CHOOSER_TCK_SCENARIO_ID,
  WEB_CHOOSER_TCK_SUITE_ID,
  WEB_UNSUPPORTED_CAPABILITIES_TCK_SCENARIO_ID
} from '../tck/contracts'

const capabilityVersion = version('capability-schema', 1)
const capabilityRange = versionRange(capabilityVersion, capabilityVersion)
const chooserLimitations: readonly Limitation[] = Object.freeze([
  Object.freeze({
    code: 'web-secure-context-required',
    explanation: 'Web Bluetooth chooser discovery is available only from a secure browser context.',
    affectedGuarantee: 'discovery availability from insecure origins'
  }),
  Object.freeze({
    code: 'web-transient-user-activation-required',
    explanation: 'Each Web Bluetooth chooser request must begin during a transient user activation.',
    affectedGuarantee: 'programmatic or background discovery initiation'
  }),
  Object.freeze({
    code: 'web-single-outstanding-chooser',
    explanation: 'The backend serializes chooser requests and permits only one outstanding browser chooser at a time.',
    affectedGuarantee: 'concurrent discovery sessions or chooser sharing'
  }),
  Object.freeze({
    code: 'web-authorized-services-only',
    explanation:
      'GATT access after chooser selection is limited to services requested by the chooser filters or optional services and granted by the browser.',
    affectedGuarantee: 'access to services not authorized by the chooser request'
  }),
  Object.freeze({
    code: 'web-live-browser-proof-pending',
    explanation:
      'This registration has deterministic boundary conformance only; it does not establish L4 live-browser or physical-radio interoperability.',
    affectedGuarantee: 'L4 live-browser physical-radio proof'
  })
])

const backgroundLimitation: Limitation = {
  code: 'web-page-lifecycle-only',
  explanation: 'Web Bluetooth does not provide a reliable background execution guarantee after page suspension.',
  affectedGuarantee: 'background BLE operation'
}

const restorationLimitation: Limitation = {
  code: 'web-restoration-unavailable',
  explanation: 'Web Bluetooth does not expose process-level state restoration or adoption records.',
  affectedGuarantee: 'state restoration'
}

const continuousScanLimitation: Limitation = {
  code: 'web-chooser-is-not-continuous-scan',
  explanation:
    'Web Bluetooth exposes a user-activated chooser and cannot provide a continuous scan session or scan sharing.',
  affectedGuarantee: 'continuous discovery and scan-session ownership'
}

function unsupportedRegistration(
  id: FeatureId,
  limitation: Limitation,
  implementationVersion: string,
  limits: CapabilityLimits
): FeatureRegistry['registrations'][number] {
  return {
    id,
    state: 'unsupported',
    selectedSchemaRange: capabilityRange,
    implementationOrigin: 'backend-native',
    implementation: {
      invoke: async () => {
        return { supported: false }
      }
    },
    tck: {
      suiteId: `unsupported:${id}`,
      requiredScenarioIds: [WEB_UNSUPPORTED_CAPABILITIES_TCK_SCENARIO_ID],
      contractRange: capabilityRange
    },
    evidence: {
      receiptId: `${id}:${implementationVersion}:blocked`,
      evidenceLevel: 'blocked',
      implementationVersion,
      sourceDigest: 'web-bluetooth-platform-contract-v1',
      scenarioIds: [WEB_UNSUPPORTED_CAPABILITIES_TCK_SCENARIO_ID],
      limitations: [limitation]
    },
    limitations: [limitation],
    limits
  }
}

function originAuthorizedPeerRegistration(
  implementationVersion: string,
  available: boolean
): FeatureRegistry['registrations'][number] {
  if (!available) {
    return unsupportedRegistration(
      BUILT_IN_FEATURE_IDS.peerOriginAuthorized,
      {
        code: 'web-get-devices-unavailable',
        explanation: 'This browser does not expose navigator.bluetooth.getDevices().',
        affectedGuarantee: 'origin-authorized peer enumeration'
      },
      implementationVersion,
      {
        authorizedPeers: { maximum: 0, minimum: null, unit: 'items' }
      }
    )
  }
  const limitation: Limitation = {
    code: 'web-origin-scoped-authorized-devices',
    explanation: 'Authorized peers are scoped to the current browser origin and may be out of range or disconnected.',
    affectedGuarantee: 'cross-origin or globally stable peer identity'
  }
  return {
    id: BUILT_IN_FEATURE_IDS.peerOriginAuthorized,
    state: 'limited',
    selectedSchemaRange: capabilityRange,
    implementationOrigin: 'backend-native',
    implementation: { invoke: async () => ({ supported: true }) },
    tck: {
      suiteId: 'web.peer-origin-authorized',
      requiredScenarioIds: ['web.peer-origin-authorized'],
      contractRange: capabilityRange
    },
    evidence: {
      receiptId: `web-peer-origin-authorized-v1:${implementationVersion}:deterministic`,
      evidenceLevel: 'deterministic',
      implementationVersion,
      sourceDigest: 'web-bluetooth-authorized-devices-v1',
      scenarioIds: ['web.peer-origin-authorized'],
      limitations: [limitation]
    },
    limitations: [limitation],
    limits: { authorizedPeers: { maximum: 128, minimum: 0, unit: 'items' } }
  }
}

function chooserDiscoveryRegistration(implementationVersion: string): FeatureRegistry['registrations'][number] {
  return {
    id: 'web:chooser-discovery',
    state: 'limited',
    selectedSchemaRange: capabilityRange,
    implementationOrigin: 'backend-native',
    implementation: chooserDiscoveryImplementation(),
    tck: {
      suiteId: WEB_CHOOSER_TCK_SUITE_ID,
      requiredScenarioIds: WEB_CHOOSER_TCK_FEATURE_SUITE.scenarioIds,
      contractRange: capabilityRange
    },
    evidence: {
      receiptId: `web-chooser-discovery-v1:${implementationVersion}:deterministic`,
      evidenceLevel: 'deterministic',
      implementationVersion,
      sourceDigest: 'web-bluetooth-chooser-discovery-v1',
      scenarioIds: WEB_CHOOSER_TCK_FEATURE_SUITE.scenarioIds,
      limitations: chooserLimitations
    },
    limitations: chooserLimitations,
    limits: {
      concurrentChooserSessions: { maximum: 1, minimum: null, unit: 'sessions' }
    }
  }
}

/** The concrete chooser is exposed through WebHost rather than a generic feature invocation. */
function chooserDiscoveryImplementation(): FeatureImplementation<SerializableRecord, SerializableRecord> {
  return {
    async invoke(_input: SerializableRecord): Promise<SerializableRecord> {
      throw contractError('lifecycle.invalid-state', 'chooser', 'web:chooser-discovery.invoke-without-web-chooser')
    }
  }
}

export function createWebBluetoothFeatureRegistry(
  implementationVersion: string,
  authorizedPeerSupport = false
): FeatureRegistry {
  return createFeatureRegistry([
    createBackendOperationCapabilityRegistration({
      implementationVersion,
      sourceDigest: 'web-direct-connection-v1',
      tckSuiteId: WEB_CHOOSER_TCK_SUITE_ID,
      requiredScenarioIds: [WEB_CHOOSER_TCK_SCENARIO_ID]
    }),
    chooserDiscoveryRegistration(implementationVersion),
    originAuthorizedPeerRegistration(implementationVersion, authorizedPeerSupport),
    unsupportedRegistration('web:background-operation', backgroundLimitation, implementationVersion, {
      backgroundDuration: { maximum: 0, minimum: null, unit: 'milliseconds' }
    }),
    unsupportedRegistration('web:continuous-scan', continuousScanLimitation, implementationVersion, {
      concurrentScanSessions: { maximum: 0, minimum: null, unit: 'sessions' }
    }),
    unsupportedRegistration('web:state-restoration', restorationLimitation, implementationVersion, {
      restorationRecords: { maximum: 0, minimum: null, unit: 'items' }
    })
  ])
}
