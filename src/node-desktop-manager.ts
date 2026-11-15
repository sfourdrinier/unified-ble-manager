// src/node-desktop-manager.ts
//
// The one-call desktop manager every Node desktop factory shares: the
// shared Rust core provider for one platform, admitted through the Node host
// manager. Platform guard first (before anything loads), then the provider.

import { createNodeBleManagerFromProvider, selectNodeAdapter, type NodeBleManagerAppOptions } from './node-host-manager'
import { randomUUID } from 'node:crypto'
import {
  createDesktopProcessHostFromBackend,
  assertDesktopProcessOptions,
  type DesktopProcessHost
} from './desktop-process-host'
import { createPublicBleManager } from './public/ble-manager'
import type { BleManager } from './public/ble-manager'
import type { AdapterSelection, BackendProvider, HostNeutralBackendIdentity } from './backend-contract/identity'
import { contractError } from './backend-contract/errors'
import { desktopRustCoreOperation } from './backends/desktop/desktop-rust-core-binding'
import {
  DESKTOP_RUST_CORE_PROFILES,
  DesktopRustCoreBackend,
  createDesktopRustCoreBackendProvider
} from './backends/desktop/desktop-rust-core-provider'
import type {
  DesktopRustCoreBinding,
  DesktopRustCoreBluezBus,
  DesktopRustCoreGenerationController,
  DesktopRustCorePlatform
} from './backends/desktop/desktop-rust-core-binding'

/** One caller-bound opaque durable terminal-evidence recovery record. */
export interface NodeTerminalEvidenceRecoveryRecord {
  readonly handle: string
  readonly producer: 'scan' | 'subscription'
  readonly byteLength: number
  readonly digest: string
  readonly formatVersion: 1
}

export interface NodeTerminalEvidenceRecoveryValue {
  readonly canonicalBytes: Uint8Array
  readonly digest: string
}

/** Desktop recovery contract shared with Electron and Tauri. */
export interface NodeTerminalEvidenceRecovery {
  list(): Promise<readonly NodeTerminalEvidenceRecoveryRecord[]>
  /** Lists cold records while naming producer lanes currently held elsewhere. */
  listWithStatus(): Promise<
    Readonly<{
      records: readonly NodeTerminalEvidenceRecoveryRecord[]
      busyProducers: readonly ('scan' | 'subscription')[]
    }>
  >
  read(handle: string, maximumBytes: number): Promise<NodeTerminalEvidenceRecoveryValue>
  acknowledge(handle: string): Promise<void>
}

interface NodeTerminalEvidenceRecoveryController extends NodeTerminalEvidenceRecovery {
  close(): Promise<void>
}

export type DesktopCoreBleManager = BleManager & {
  readonly terminalEvidenceRecovery: NodeTerminalEvidenceRecovery
}

let nextNodeTerminalEvidenceRecoveryController = 1

/** Options every desktop factory accepts beyond the Node manager options. */
export interface DesktopCoreManagerOptions extends NodeBleManagerAppOptions {
  /** Owner label for admitted core centrals (host identity). */
  readonly owner?: string
  /**
   * Injected core entry (tests and embedding hosts). Absent, the factory
   * loads the packaged addon and verifies its build identity before any
   * radio call; there is no TypeScript fallback.
   */
  readonly binding?: DesktopRustCoreBinding
  /**
   * Trusted host-selected continuation recording for durable retained
   * peer-source failure recovery. Renderer code never supplies this value.
   */
  readonly peerFailureRecoveryRecordingId?: string
  /**
   * Trusted Node or Electron-main storage configuration. It is forwarded
   * to native code before the radio opens; never pass it across renderer IPC.
   */
  readonly failureEvidenceStore?: DesktopFailureEvidenceStoreOptions
  /**
   * Trusted Node or Electron-main storage for source-terminal evidence. It is
   * opened before scan or subscription activation; never pass it over renderer IPC.
   */
  readonly terminalEvidenceStore?: DesktopTerminalEvidenceStoreOptions
}

/** Host-owned private evidence storage; decimal text preserves native integer precision. */
export interface DesktopFailureEvidenceStoreOptions {
  readonly directory: string
  readonly maxBytes: string
}

/** Host-owned terminal-evidence ledger; decimal text preserves native integer precision. */
export interface DesktopTerminalEvidenceStoreOptions {
  readonly directory: string
  readonly maxBytes: string
}

export interface DesktopCoreProviderOptions {
  readonly now: () => number
  readonly owner?: string
  readonly binding?: DesktopRustCoreBinding
  /**
   * An existing, host-authorized continuation recording that owns durable
   * peer-source failure recovery. This is an opaque recording id selected by
   * trusted Node, Electron-main, or Tauri host code; it is never a renderer
   * supplied filesystem path.
   */
  readonly peerFailureRecoveryRecordingId?: string
  readonly failureEvidenceStore?: DesktopFailureEvidenceStoreOptions
  readonly terminalEvidenceStore?: DesktopTerminalEvidenceStoreOptions
}

/** BlueZ-only provider facts (the legacy dbus-next options). */
export interface BluezCoreProviderExtras {
  readonly connectionPolicy?: import('./backends/desktop/bluez-connection-policy').BluezConnectionPolicy | undefined
  readonly bluezBus?: DesktopRustCoreBluezBus
  readonly pairingGeneration?: DesktopRustCoreGenerationController
}

function assertNoForeignConnectionPolicy(platform: DesktopRustCorePlatform, options: object): void {
  if (platform !== 'bluez' && Reflect.get(options, 'connectionPolicy') !== undefined) {
    throw contractError('argument.invalid', 'core', 'desktop.bluez-only-option')
  }
}

/** Admit the BlueZ bus before anything loads: `system` or `session`; anything else is invalid. */
export function admitBluezBusKind(busKind: unknown): DesktopRustCoreBluezBus {
  if (busKind === 'system' || busKind === 'session') return busKind
  throw contractError(
    'argument.invalid',
    'core',
    desktopRustCoreOperation(DESKTOP_RUST_CORE_PROFILES.bluez.operationPrefix, 'bus-kind')
  )
}

export function createDesktopCoreProvider(
  platform: DesktopRustCorePlatform,
  options: DesktopCoreProviderOptions,
  hostKind: 'node' | 'desktop-native',
  bluez: BluezCoreProviderExtras = {}
): BackendProvider<string, HostNeutralBackendIdentity<string>> {
  assertNoForeignConnectionPolicy(platform, options)
  const profile = DESKTOP_RUST_CORE_PROFILES[platform]
  if (options.owner !== undefined && options.owner.length === 0) {
    throw contractError('argument.invalid', 'core', desktopRustCoreOperation(profile.operationPrefix, 'owner'))
  }
  return createDesktopRustCoreBackendProvider({
    platform,
    owner: options.owner ?? profile.defaultOwner,
    now: options.now,
    hostKind,
    ...(options.binding === undefined ? {} : { binding: options.binding }),
    ...(options.peerFailureRecoveryRecordingId === undefined
      ? {}
      : { peerFailureRecoveryRecordingId: options.peerFailureRecoveryRecordingId }),
    ...(options.failureEvidenceStore === undefined ? {} : { failureEvidenceStore: options.failureEvidenceStore }),
    ...(options.terminalEvidenceStore === undefined ? {} : { terminalEvidenceStore: options.terminalEvidenceStore }),
    ...(bluez.bluezBus === undefined ? {} : { bluezBus: bluez.bluezBus }),
    ...(bluez.connectionPolicy === undefined ? {} : { connectionPolicy: bluez.connectionPolicy }),
    ...(bluez.pairingGeneration === undefined ? {} : { pairingGeneration: bluez.pairingGeneration })
  })
}

export async function createDesktopCoreBleManager(
  platform: DesktopRustCorePlatform,
  options: DesktopCoreManagerOptions,
  bluez: BluezCoreProviderExtras = {}
): Promise<DesktopCoreBleManager> {
  assertNoForeignConnectionPolicy(platform, options)
  const now = options.now ?? (() => performance.now())
  const {
    owner,
    binding,
    peerFailureRecoveryRecordingId,
    failureEvidenceStore,
    terminalEvidenceStore,
    ...managerOptions
  } = options
  const provider = createDesktopCoreProvider(
    platform,
    {
      now,
      ...(owner === undefined ? {} : { owner }),
      ...(binding === undefined ? {} : { binding }),
      ...(peerFailureRecoveryRecordingId === undefined ? {} : { peerFailureRecoveryRecordingId }),
      ...(failureEvidenceStore === undefined ? {} : { failureEvidenceStore }),
      ...(terminalEvidenceStore === undefined ? {} : { terminalEvidenceStore })
    },
    'node',
    bluez
  )
  let desktopBackend: DesktopRustCoreBackend | null = null
  const recoveringProvider: BackendProvider<string, HostNeutralBackendIdentity<string>> = Object.freeze({
    descriptor: provider.descriptor,
    listAdapters: () => provider.listAdapters(),
    create: async (selection: AdapterSelection<string>) => {
      const backend = await provider.create(selection)
      if (!(backend instanceof DesktopRustCoreBackend)) {
        await backend.destroy()
        throw contractError('protocol.incompatible', 'core', 'desktop.terminal-evidence-recovery.backend')
      }
      desktopBackend = backend
      return backend
    }
  })
  const internal = await createNodeBleManagerFromProvider(
    recoveringProvider,
    DESKTOP_RUST_CORE_PROFILES[platform].compatibility,
    managerOptions
  )
  if (desktopBackend === null) {
    await internal.destroy()
    throw contractError('protocol.incompatible', 'core', 'desktop.terminal-evidence-recovery.backend')
  }
  const manager = await createPublicBleManager(internal, now)
  const recovery = createNodeTerminalEvidenceRecovery(desktopBackend)
  const destroy = manager.destroy.bind(manager)
  return Object.assign(manager, {
    terminalEvidenceRecovery: recovery,
    destroy: async () => {
      await recovery.close()
      return destroy()
    }
  })
}

function createNodeTerminalEvidenceRecovery(backend: DesktopRustCoreBackend): NodeTerminalEvidenceRecoveryController {
  const entries = new Map<
    string,
    Readonly<{ reference: string; producer: 'scan' | 'subscription'; byteLength: number; digest: string }>
  >()
  const controllerId = nextNodeTerminalEvidenceRecoveryController++
  const leaseId = `node-terminal-evidence-recovery-${randomUUID()}`
  let nextHandle = 1
  let closed = false
  const operation = 'node.terminal-evidence-recovery'
  const unavailable = (): never => {
    throw contractError('capability.unsupported', 'restoration', `${operation}.unconfigured`)
  }
  const assertOpen = (): void => {
    if (closed) throw contractError('lifecycle.destroyed', 'restoration', operation)
  }
  const entryFor = (handle: string, suffix: string) => {
    if (typeof handle !== 'string' || handle.length === 0)
      throw contractError('argument.invalid', 'restoration', `${operation}.${suffix}.handle`)
    const entry = entries.get(handle)
    if (entry === undefined) throw contractError('ownership.denied', 'restoration', `${operation}.${suffix}`)
    return entry
  }
  const maximum = (value: number): number => {
    if (!Number.isSafeInteger(value) || value < 1)
      throw contractError('argument.invalid', 'restoration', `${operation}.read.maximum-bytes`)
    return value
  }
  return Object.freeze({
    async list() {
      assertOpen()
      if (!backend.hasTerminalEvidenceRecovery()) unavailable()
      const recovered = await backend.recoverTerminalEvidenceForHost(leaseId)
      const records: NodeTerminalEvidenceRecoveryRecord[] = []
      for (const item of recovered) {
        let handle: string | undefined
        for (const [candidate, entry] of entries) {
          if (entry.reference === item.reference) {
            handle = candidate
            break
          }
        }
        if (handle === undefined) {
          handle = `node-terminal-evidence-${controllerId}-${nextHandle++}`
          entries.set(
            handle,
            Object.freeze({
              reference: item.reference,
              producer: item.producer,
              byteLength: item.byteLength,
              digest: item.digest
            })
          )
        }
        records.push(
          Object.freeze({
            handle,
            producer: item.producer,
            byteLength: item.byteLength,
            digest: item.digest,
            formatVersion: 1
          })
        )
      }
      return Object.freeze(records)
    },
    async listWithStatus() {
      assertOpen()
      if (!backend.hasTerminalEvidenceRecovery()) unavailable()
      const recovered = await backend.recoverTerminalEvidenceForHostWithStatus(leaseId)
      const records: NodeTerminalEvidenceRecoveryRecord[] = []
      for (const item of recovered.records) {
        let handle: string | undefined
        for (const [candidate, entry] of entries) {
          if (entry.reference === item.reference) {
            handle = candidate
            break
          }
        }
        if (handle === undefined) {
          handle = `node-terminal-evidence-${controllerId}-${nextHandle++}`
          entries.set(
            handle,
            Object.freeze({
              reference: item.reference,
              producer: item.producer,
              byteLength: item.byteLength,
              digest: item.digest
            })
          )
        }
        records.push(
          Object.freeze({
            handle,
            producer: item.producer,
            byteLength: item.byteLength,
            digest: item.digest,
            formatVersion: 1
          })
        )
      }
      return Object.freeze({ records: Object.freeze(records), busyProducers: recovered.busyProducers })
    },
    async read(handle: string, maximumBytes: number) {
      assertOpen()
      const entry = entryFor(handle, 'read')
      const maximumValue = maximum(maximumBytes)
      const result = await backend.readRecoveredTerminalEvidence(entry.reference, maximumValue)
      if (
        result.kind !== 'committed' ||
        result.canonicalBytes.byteLength !== entry.byteLength ||
        result.digest !== entry.digest
      ) {
        throw contractError('protocol.malformed', 'restoration', `${operation}.read`)
      }
      return Object.freeze({ canonicalBytes: new Uint8Array(result.canonicalBytes), digest: result.digest })
    },
    async acknowledge(handle: string) {
      assertOpen()
      const entry = entryFor(handle, 'acknowledge')
      await backend.acknowledgeRecoveredTerminalEvidence(entry.reference)
      entries.delete(handle)
    },
    async close() {
      if (closed) return
      if (!backend.hasTerminalEvidenceRecovery()) {
        closed = true
        entries.clear()
        return
      }
      await backend.releaseTerminalEvidenceRecoveryLease(leaseId)
      closed = true
      entries.clear()
    }
  })
}

/** Trusted process owner over one selected desktop backend; children borrow its lifetime. */
export async function createDesktopCoreProcessHost(
  platform: DesktopRustCorePlatform,
  options: DesktopCoreManagerOptions = {},
  bluez: BluezCoreProviderExtras = {},
  hostKind: 'node' | 'desktop-native' = 'node'
): Promise<DesktopProcessHost> {
  assertNoForeignConnectionPolicy(platform, options)
  const {
    owner,
    binding,
    peerFailureRecoveryRecordingId,
    failureEvidenceStore,
    terminalEvidenceStore,
    now = () => performance.now(),
    ...createOptions
  } = options
  assertDesktopProcessOptions(createOptions)
  const provider = createDesktopCoreProvider(
    platform,
    {
      now,
      ...(owner === undefined ? {} : { owner }),
      ...(binding === undefined ? {} : { binding }),
      ...(peerFailureRecoveryRecordingId === undefined ? {} : { peerFailureRecoveryRecordingId }),
      ...(failureEvidenceStore === undefined ? {} : { failureEvidenceStore }),
      ...(terminalEvidenceStore === undefined ? {} : { terminalEvidenceStore })
    },
    hostKind,
    bluez
  )
  const adapter = selectNodeAdapter(await provider.listAdapters(), options.adapterId)
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  if (!(backend instanceof DesktopRustCoreBackend))
    throw contractError('protocol.incompatible', 'core', 'desktop-process-host.backend')
  return createDesktopProcessHostFromBackend(backend, {
    now,
    ...(options.instanceId === undefined ? {} : { instanceId: options.instanceId }),
    ...(options.diagnostics === undefined ? {} : { diagnostics: options.diagnostics }),
    ...(options.randomBytes === undefined ? {} : { randomBytes: options.randomBytes })
  })
}
