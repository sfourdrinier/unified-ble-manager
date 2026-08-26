// src/backends/bluez/bluez-backend.ts

import type { BackendAttachment, BackendAttachmentRequest, BleCentralBackend } from '../../backend-contract/backend'
import { contractError, type CleanupRecord } from '../../backend-contract/errors'
import {
  BUILT_IN_FEATURE_IDS,
  createBackendOperationCapabilityRegistration,
  createFeatureRegistry
} from '../../backend-contract/capabilities'
import type { AdapterDescriptor, HostNeutralBackendIdentity } from '../../backend-contract/identity'
import { negotiateCoreVersions, opaqueId, type BackendInstanceId } from '../../backend-contract/primitives'
import type { BluezBusKind, BluezDbusBoundary } from './bluez-dbus-contract'
import { BluezObjectStore } from './bluez-object-store'
import {
  BLUEZ_BACKEND_ID,
  BLUEZ_IMPLEMENTATION_VERSION,
  BLUEZ_PLATFORM_ID,
  bluezCompatibility
} from './bluez-backend-provider'
import {
  BluezBackendRuntime,
  inspectBluezRuntimeStreamOwnershipForTests,
  type BackendStreamOwnershipSnapshot
} from './bluez-backend-runtime'
import { createBluezConnectionControlRegistrations } from './bluez-connection-capabilities'
import { createBluezPairingGenerationRegistration } from './bluez-pairing-generation-capability'
import { type BluezPairingGenerationController } from './bluez-pairing-generation'

const bluezBackendStreamOwnershipInspectors = new WeakMap<BluezBackend, () => BackendStreamOwnershipSnapshot>()

export function inspectBluezStreamOwnershipForTests(backend: BluezBackend): BackendStreamOwnershipSnapshot {
  const inspect = bluezBackendStreamOwnershipInspectors.get(backend)
  if (inspect === undefined) {
    throw new Error('bluez stream ownership inspector is missing')
  }
  return inspect()
}

export interface BluezBackendConstruction {
  readonly boundary: BluezDbusBoundary
  readonly store: BluezObjectStore
  readonly adapter: AdapterDescriptor<string>
  readonly now: () => number
  readonly busKind: BluezBusKind
  /** Host-supplied privileged pairing-generation operation; absent by default. */
  readonly pairingGeneration?: BluezPairingGenerationController
}

let nextBluezBackendInstance = 1

const bluezSecurityFeatureIds = Object.freeze([
  BUILT_IN_FEATURE_IDS.securityState,
  BUILT_IN_FEATURE_IDS.securityPair,
  BUILT_IN_FEATURE_IDS.securityCancelPairing,
  BUILT_IN_FEATURE_IDS.securityUnpair
])

function allocateBluezBackendInstance(): number {
  const value = nextBluezBackendInstance
  nextBluezBackendInstance += 1
  return value
}

/** Contract-v1 BlueZ backend bound to one explicitly selected D-Bus adapter. */
export class BluezBackend implements BleCentralBackend<string, HostNeutralBackendIdentity<string>> {
  readonly features: ReturnType<typeof createFeatureRegistry>
  readonly adapter
  readonly scanner
  readonly connections
  readonly gatt
  readonly security
  private readonly backendInstanceId: BackendInstanceId<string>
  private readonly runtime: BluezBackendRuntime
  private destroyedIdentity: HostNeutralBackendIdentity<string> | null = null
  private attached = false

  constructor(construction: BluezBackendConstruction) {
    this.backendInstanceId = opaqueId(`bluez-backend-${allocateBluezBackendInstance()}`, 'backend-instance', 'bluez')
    // Built here rather than as a field initialiser because what this backend
    // reports for `security:pairing-generation` depends on whether the HOST
    // supplied the privileged operation - a capability is reported by the
    // instantiated backend at runtime, never by a static platform matrix.
    this.features = createFeatureRegistry([
      createBackendOperationCapabilityRegistration({
        implementationVersion: BLUEZ_IMPLEMENTATION_VERSION,
        sourceDigest: 'bluez-direct-connection-v1',
        tckSuiteId: 'capability.catalog-v2',
        requiredScenarioIds: ['scenario.scan-connect-discover-read-notify-destroy']
      }),
      createBackendOperationCapabilityRegistration({
        id: BUILT_IN_FEATURE_IDS.peerAddressTargeting,
        implementationVersion: BLUEZ_IMPLEMENTATION_VERSION,
        sourceDigest: 'bluez-address-targeting-v1',
        tckSuiteId: 'capability.catalog-v2',
        requiredScenarioIds: ['scenario.scan-connect-discover-read-notify-destroy'],
        operation: 'peer:address-targeting.invoke-without-connection'
      }),
      ...bluezSecurityFeatureIds.map(id =>
        createBackendOperationCapabilityRegistration({
          id,
          implementationVersion: BLUEZ_IMPLEMENTATION_VERSION,
          sourceDigest: `bluez-${id.replace(':', '-')}-v1`,
          tckSuiteId: 'tck.feature.security.bluez',
          requiredScenarioIds: ['security.state-pair-cancel-unpair'],
          operation: `${id}.invoke-without-security-backend`
        })
      ),
      ...createBluezConnectionControlRegistrations(BLUEZ_IMPLEMENTATION_VERSION),
      createBluezPairingGenerationRegistration(
        BLUEZ_IMPLEMENTATION_VERSION,
        // `!= null`, not `!== undefined`: the runtime coerces with `?? null`, so a
        // host passing an explicit null - trivially produced by JSON config -
        // would otherwise be told the capability is available while every
        // directed pairing rejects. A descriptor must not state a fact the
        // backend does not have.
        construction.pairingGeneration != null
      )
    ])
    this.runtime = new BluezBackendRuntime({
      ...construction,
      backendInstanceId: this.backendInstanceId
    })
    this.adapter = this.runtime.adapter
    this.scanner = this.runtime.scanner
    this.connections = this.runtime.connections
    this.gatt = this.runtime.gatt
    this.security = this.runtime.security
    bluezBackendStreamOwnershipInspectors.set(this, () => inspectBluezRuntimeStreamOwnershipForTests(this.runtime))
  }

  get identity(): HostNeutralBackendIdentity<string> {
    if (this.destroyedIdentity !== null) {
      return this.destroyedIdentity
    }
    const attachment = this.runtime.attachment()
    return Object.freeze({
      registeredBackendId: BLUEZ_BACKEND_ID,
      registeredPlatformId: BLUEZ_PLATFORM_ID,
      attachment,
      versions: negotiateCoreVersions(bluezCompatibility, bluezCompatibility),
      runtime: Object.freeze({
        hostKind: 'node',
        implementationVersion: BLUEZ_IMPLEMENTATION_VERSION,
        diagnostics: Object.freeze({
          busKind: this.runtime.busKind,
          selectedAdapterPath: String(attachment.adapter.adapterId)
        })
      })
    })
  }

  async attach(
    request: BackendAttachmentRequest
  ): Promise<BackendAttachment<string, HostNeutralBackendIdentity<string>>> {
    this.runtime.assertUsable('bluez.attach')
    if (this.attached) {
      throw contractError('lifecycle.invalid-state', 'core', 'bluez.attach')
    }
    negotiateCoreVersions(bluezCompatibility, request.coreCompatibility)
    this.attached = true
    const identity = this.identity
    return Object.freeze({ attachment: identity.attachment, identity })
  }

  events() {
    return this.runtime.events()
  }

  resourceCounters() {
    return this.runtime.resourceCounters()
  }

  destroy(): Promise<CleanupRecord> {
    if (this.destroyedIdentity === null) {
      this.destroyedIdentity = this.identity
    }
    return this.runtime.destroy()
  }
}
