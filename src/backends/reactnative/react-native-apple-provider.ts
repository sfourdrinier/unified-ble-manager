// src/backends/reactnative/react-native-apple-provider.ts
//
// LEGACY REFERENCE ONLY (FIX-PLAN decision 12). `main`'s TypeScript Apple
// provider — the Swift radio driven through the JSI protocol boundary — kept
// solely as the parity reference the legacy-vs-Rust capability tests open,
// until Phase 4 deletes it. No public factory reaches it: every public React
// Native and Expo factory runs the Rust mobile owner
// (react-native-rust-core-provider.ts).

import type {
  AdapterBackend,
  BackendAttachment,
  BackendAttachmentRequest,
  BackendEvent,
  BleCentralBackend,
  ConnectionBackend,
  GattBackend,
  ResourceCounters,
  ScannerBackend
} from '../../backend-contract/backend'
import type { OwnerScanOptions } from '../../backend-contract/advertisement'
import { contractError, type CleanupRecord } from '../../backend-contract/errors'
import type { AdapterSelection, AttachmentRecord, NativeBackendIdentity } from '../../backend-contract/identity'
import {
  negotiateVersion,
  type ClientId,
  type CoreVersionAxes,
  type NativeVersionAxes
} from '../../backend-contract/primitives'
import {
  REACT_NATIVE_APPLE_BACKEND_ID,
  REACT_NATIVE_APPLE_PLATFORM_ID,
  REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION,
  REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID,
  reactNativeAppleCompatibility,
  reactNativeAppleDefaultAdapterId
} from './react-native-platform-identity'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import type { NativeAttachmentIdentity, Spec as NativeProtocolControl } from '../../NativeUnifiedBleProtocolControl'
import { CoreBluetoothBackend, type DirectGattBackendIdentityOptions } from '../corebluetooth/corebluetooth-backend'
import { ReactNativeAppleProtocolBoundary } from '../../native-protocol/rn-apple-boundary'
import { trustedServiceUuidFilter } from '../scan-planning/service-uuid-scan-planner'
import { createReactNativeConnectionControlFeatureRegistry } from './react-native-connection-control-features'
import { createReactNativeDescriptorFeatureRegistry } from './react-native-descriptor-features'
import { diagnosticReactNativeAppleScanPlan, planReactNativeAppleScan } from './react-native-scan-planner'
import { withReactNativeProviderCleanup } from './react-native-provider-cleanup'
import {
  combineReactNativeFeatureRegistries,
  createReactNativeRestorationFeatureRegistry,
  ReactNativeRestorationCoordinator,
  type ReactNativeRestorationActivation,
  type ReactNativeRestorationBackendProvider
} from './react-native-restoration'

export {
  REACT_NATIVE_APPLE_BACKEND_ID,
  REACT_NATIVE_APPLE_PLATFORM_ID,
  REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION,
  REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID,
  reactNativeAppleCompatibility,
  reactNativeAppleDefaultAdapterId
}

const reactNativeAppleProviderDescriptor: ReactNativeRestorationBackendProvider['descriptor'] = Object.freeze({
  providerId: 'unified-ble:react-native-apple-provider',
  hostKind: 'native-mobile',
  loadability: 'loadable',
  compatibility: reactNativeAppleCompatibility
})

let nextBoundaryOwner = 1

export interface ReactNativeAppleLegacyBackendProviderOptions {
  /** Generated legacy protocol control (reference route only). */
  readonly control: NativeProtocolControl
  /** Monotonic clock supplied by the React Native host application. */
  readonly now: () => number
  /** Optional deterministic owner identity factory for controlled tests. */
  readonly createOwnerId?: () => string
}

/**
 * The legacy TypeScript reference route: `main`'s Apple provider, which drives
 * the Swift `OwnedCoreBluetoothProtocolRadio` through the canonical JSI
 * protocol boundary with no Rust core. Kept reachable, never a default and
 * never a fallback: the manager selects it only for
 * `legacyTypeScriptCore: 'isolated-test-reference'`, and it is not part of the
 * public React Native entrypoint.
 */
export function createReactNativeAppleLegacyBackendProvider(
  options: ReactNativeAppleLegacyBackendProviderOptions
): ReactNativeRestorationBackendProvider {
  const createOwnerId = options.createOwnerId ?? allocateBoundaryOwnerId
  const restoration = new ReactNativeRestorationCoordinator(options.control)
  return Object.freeze({
    descriptor: reactNativeAppleProviderDescriptor,
    restoration,
    listAdapters: async () => {
      const backend = await createOpenedBackend(options.control, options.now, createOwnerId(), restoration, false)
      return withReactNativeProviderCleanup(backend, 'apple', 'react-native-apple.provider.list-adapters.cleanup', () =>
        Object.freeze([backend.identity.attachment.adapter])
      )
    },
    create: async (selection: AdapterSelection<string>) => {
      if (String(selection.selectedAdapterId) !== REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID) {
        throw contractError('adapter.unavailable', 'adapter', 'react-native-apple.provider.select-adapter')
      }
      return createOpenedBackend(options.control, options.now, createOwnerId(), restoration, true)
    }
  })
}

class ReactNativeAppleBackend implements BleCentralBackend<string, NativeBackendIdentity<string>> {
  readonly adapter: AdapterBackend<string>
  readonly scanner: ScannerBackend<string>
  readonly connections: ConnectionBackend<string>
  readonly gatt: GattBackend<string>
  readonly features: CoreBluetoothBackend['features']

  private destroyResult: Promise<CleanupRecord> | null = null

  constructor(
    private readonly delegate: CoreBluetoothBackend,
    readonly restoration: ReactNativeRestorationCoordinator,
    private readonly restorationActivation: ReactNativeRestorationActivation | null
  ) {
    this.adapter = delegate.adapter
    this.scanner = Object.freeze({
      plan: diagnosticReactNativeAppleScanPlan,
      start: (options: OwnerScanOptions<string, string>, clientId: ClientId<string, string>) =>
        delegate.scanner.start(
          {
            ...options,
            filter: trustedServiceUuidFilter(options, planReactNativeAppleScan, 'rn-apple.scan')
          },
          clientId
        ),
      join: delegate.scanner.join
    })
    this.connections = delegate.connections
    this.gatt = delegate.gatt
    this.features = delegate.features
  }

  get identity(): NativeBackendIdentity<string> {
    const delegateIdentity = this.delegate.identity
    return Object.freeze({
      registeredBackendId: REACT_NATIVE_APPLE_BACKEND_ID,
      registeredPlatformId: REACT_NATIVE_APPLE_PLATFORM_ID,
      attachment: delegateIdentity.attachment,
      versions: nativeVersions(delegateIdentity.versions),
      runtime: Object.freeze({
        hostKind: 'native-mobile',
        implementationVersion: REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION,
        diagnostics: Object.freeze({
          boundary: 'react-native-apple-jsi-v1',
          transport: 'native-protocol-v2'
        })
      })
    })
  }

  attach(request: BackendAttachmentRequest): Promise<BackendAttachment<string, NativeBackendIdentity<string>>> {
    return this.delegate.attach(request).then(() =>
      Object.freeze({
        attachment: this.identity.attachment,
        identity: this.identity
      })
    )
  }

  events(): BoundedAsyncStream<BackendEvent<string>> {
    return this.delegate.events()
  }

  resourceCounters(): ResourceCounters {
    return this.delegate.resourceCounters()
  }

  destroy(): Promise<CleanupRecord> {
    if (this.destroyResult === null) {
      const destruction = this.destroyInternal()
      this.destroyResult = destruction.then(
        cleanup => {
          if (cleanup.state !== 'released' || cleanup.failures.length !== 0) {
            this.destroyResult = null
          }
          return cleanup
        },
        error => {
          this.destroyResult = null
          throw error
        }
      )
    }
    return this.destroyResult
  }

  private async destroyInternal(): Promise<CleanupRecord> {
    if (this.restorationActivation !== null) {
      await this.restoration.deactivate(this.restorationActivation)
    }
    return this.delegate.destroy()
  }
}

async function createOpenedBackend(
  control: NativeProtocolControl,
  now: () => number,
  ownerId: string,
  restoration: ReactNativeRestorationCoordinator,
  activateRestoration: boolean
): Promise<ReactNativeAppleBackend> {
  if (ownerId.length === 0) {
    throw contractError('argument.invalid', 'core', 'react-native-apple.provider.owner-id')
  }
  const boundary = new ReactNativeAppleProtocolBoundary(control, ownerId)
  const directBackend = new CoreBluetoothBackend(boundary, now, 'native-mobile', appleDirectGattIdentity())
  boundary.bindAttachment(nativeAttachmentIdentity(directBackend.attachment()))
  try {
    await boundary.open()
    directBackend.refreshAttachmentState()
    const activation = activateRestoration
      ? restoration.activate(directBackend.identity.attachment, nativeVersions(directBackend.identity.versions))
      : null
    return new ReactNativeAppleBackend(directBackend, restoration, activation)
  } catch (error) {
    return withReactNativeProviderCleanup(directBackend, 'apple', 'react-native-apple.provider.open.cleanup', () => {
      throw error
    })
  }
}

function appleDirectGattIdentity(): DirectGattBackendIdentityOptions {
  return Object.freeze({
    registeredBackendId: REACT_NATIVE_APPLE_BACKEND_ID,
    registeredPlatformId: REACT_NATIVE_APPLE_PLATFORM_ID,
    implementationVersion: REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION,
    attachmentScope: 'react-native-apple',
    backendInstancePrefix: 'react-native-apple-backend',
    adapterNativeId: REACT_NATIVE_APPLE_DEFAULT_ADAPTER_NATIVE_ID,
    adapterDisplayName: 'Apple CoreBluetooth central adapter',
    limitations: Object.freeze([
      'Apple exposes the process-owned CoreBluetooth central through the canonical JSI protocol boundary'
    ]),
    features: combineReactNativeFeatureRegistries(
      createReactNativeConnectionControlFeatureRegistry('apple', REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION),
      createReactNativeDescriptorFeatureRegistry('apple', REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION),
      createReactNativeRestorationFeatureRegistry('apple', REACT_NATIVE_APPLE_IMPLEMENTATION_VERSION)
    )
  })
}

function nativeAttachmentIdentity(attachment: AttachmentRecord<string>): NativeAttachmentIdentity {
  return {
    attachmentId: String(attachment.attachmentId),
    backendInstanceId: String(attachment.backendInstanceId),
    backendGeneration: String(attachment.backendGeneration),
    adapterId: String(attachment.adapter.adapterId),
    adapterGeneration: String(attachment.adapter.adapterGeneration)
  }
}

function nativeVersions(coreVersions: CoreVersionAxes): NativeVersionAxes {
  return Object.freeze({
    ...coreVersions,
    nativeProtocol: negotiateVersion(
      reactNativeAppleCompatibility.nativeProtocol,
      reactNativeAppleCompatibility.nativeProtocol
    )
  })
}

function allocateBoundaryOwnerId(): string {
  const ordinal = nextBoundaryOwner
  nextBoundaryOwner += 1
  return `react-native-apple-owner-${ordinal}`
}
