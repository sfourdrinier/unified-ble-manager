// src/backends/bluez/bluez-pairing-generation-capability.ts
//
// How this backend reports whether it can select an LE pairing generation.
// Kept out of the connection-capability module because this is a security
// capability, and out of the backend module because what it reports depends on
// what the host supplied rather than on which backend is running.

import { BUILT_IN_FEATURE_IDS, createBackendOperationCapabilityRegistration } from '../../backend-contract/capabilities'
import type { FeatureRegistry, Limitation } from '../../backend-contract/capabilities'
import { version, versionRange } from '../../backend-contract/primitives'

const capabilitySchemaRange = versionRange(version('capability-schema', 1), version('capability-schema', 1))
const pairingGenerationScenarioIds = Object.freeze(['security.state-pair-cancel-unpair'])

/** Why a pairing generation cannot be selected when no host controller is supplied. */
export const BLUEZ_PAIRING_GENERATION_LIMITATIONS: readonly Limitation[] = Object.freeze([
  Object.freeze({
    code: 'bluez-pairing-generation-needs-a-host-supplied-privileged-operation',
    explanation:
      'BlueZ exposes no D-Bus API to select the LE pairing generation: org.bluez.Adapter1 (BlueZ 5.85) has no ' +
      'Secure Connections property and org.bluez.Device1.Pair takes no parameters. The setting lives behind the ' +
      'kernel management socket Set Secure Connections command, which requires CAP_NET_ADMIN. This package never ' +
      'acquires that privilege itself - a library that silently escalates would give an application capabilities ' +
      'its author did not choose and cannot audit. Supply BluezBackendProviderOptions.pairingGeneration to enable ' +
      'it. The setting is adapter-wide and outlives the process, so read docs/BONDING.md before implementing one.',
    affectedGuarantee: 'caller-selected LE pairing generation'
  })
])

/** What a host accepts by supplying the privileged operation. */
export const BLUEZ_PAIRING_GENERATION_ENABLED_LIMITATIONS: readonly Limitation[] = Object.freeze([
  Object.freeze({
    code: 'bluez-pairing-generation-is-adapter-wide',
    explanation:
      'The kernel Secure Connections setting is per-adapter, not per-pairing. While a directed pairing holds it, ' +
      'every pairing on that controller uses the selected generation - including pairings this package did not ' +
      'initiate. It also outlives this process: the kernel keeps the value until something sets it back, so a ' +
      'crash between set and restore leaves the adapter changed. This backend restores it after every pairing and ' +
      'reports a failed restore, but cannot undo a change it did not survive to reverse.',
    affectedGuarantee: 'adapter-wide pairing generation while a directed pairing is in flight'
  })
])

/**
 * Report `security:pairing-generation` according to what the host actually
 * supplied.
 *
 * This is the capability contract working as intended: a capability is reported
 * by the instantiated backend at runtime, never by a static platform matrix.
 * Two BlueZ backends on the same machine legitimately answer differently,
 * because one was given the privileged operation and the other was not.
 */
export function createBluezPairingGenerationRegistration(
  implementationVersion: string,
  controllerSupplied: boolean
): FeatureRegistry['registrations'][number] {
  if (controllerSupplied) {
    return createBackendOperationCapabilityRegistration({
      id: BUILT_IN_FEATURE_IDS.securityPairingGeneration,
      implementationVersion,
      sourceDigest: 'bluez-pairing-generation-host-supplied-v1',
      tckSuiteId: 'tck.feature.security.bluez',
      requiredScenarioIds: ['security.state-pair-cancel-unpair'],
      operation: 'security:pairing-generation.invoke-without-security-backend',
      // The branch that actually mutates adapter-wide security state must
      // advertise at least what the inert one does. Reporting the blast radius
      // only when the capability is unavailable is exactly backwards.
      limitations: BLUEZ_PAIRING_GENERATION_ENABLED_LIMITATIONS
    })
  }
  return Object.freeze({
    id: BUILT_IN_FEATURE_IDS.securityPairingGeneration,
    state: 'unsupported' as const,
    selectedSchemaRange: capabilitySchemaRange,
    implementationOrigin: 'backend-native' as const,
    implementation: Object.freeze({
      featureId: BUILT_IN_FEATURE_IDS.securityPairingGeneration,
      invoke: async () => {
        throw new Error('security:pairing-generation is metadata-only on this backend')
      }
    }),
    tck: Object.freeze({
      suiteId: 'tck.feature.security.bluez',
      requiredScenarioIds: pairingGenerationScenarioIds,
      contractRange: capabilitySchemaRange
    }),
    evidence: Object.freeze({
      receiptId: 'bluez-pairing-generation-unsupported-v1:blocked',
      evidenceLevel: 'blocked' as const,
      implementationVersion,
      sourceDigest: 'bluez-pairing-generation-unsupported-v1',
      scenarioIds: pairingGenerationScenarioIds,
      limitations: BLUEZ_PAIRING_GENERATION_LIMITATIONS
    }),
    limitations: BLUEZ_PAIRING_GENERATION_LIMITATIONS,
    limits: Object.freeze({
      generationSelections: Object.freeze({ maximum: 0, minimum: null, unit: 'selections' })
    })
  })
}
