// src/desktop-core-addon.ts
//
// Loads the shared desktop Rust core (PR210-03) and binds its build identity
// (PR210-18) before any radio call.
//
// This file sits at the top of src/ on purpose: compiled into
// lib/{commonjs,module}/desktop-core-addon.js, the relative specifier
// `../../native/desktop-core/index.js` reaches the package's own
// native/desktop-core loader, which anchors the addon to its own location
// (`__dirname`): never the process cwd, never another platform's binary.
// Bundlers must keep native/desktop-core external (docs/ELECTRON.md).
//
// Identity: the addon's `nativeBuildIdentity()` must equal the identity this
// package was sealed with (`src/generated/native-build-identity.ts`):
// contract revision, source digest and binding schema exactly, a declared
// target, and a release profile (a debug build is accepted only from an
// explicit `UBM_NAPI_ADDON` source build). Any mismatch is
// `protocol.incompatible` / `core` / `<host>.native-identity`, naming the
// differing fields, and no central is ever opened.

import { contractError, BackendContractError } from './backend-contract/errors'
import { EXPECTED_NATIVE_BUILD_IDENTITY } from './generated/native-build-identity'
import type { ExpectedNativeBuildIdentity } from './generated/native-build-identity'
import {
  nativeBuildIdentityMismatches,
  parseNativeBuildIdentityText,
  type NativeBuildIdentityRecord
} from './native-build-identity-check'
import {
  desktopRustCoreOperation,
  throwDesktopRustCoreError,
  type DesktopRustCoreAdapterListing,
  type DesktopRustCoreBluezBus,
  type DesktopRustCoreBinding,
  type DesktopRustCoreCapabilityState,
  type DesktopRustCoreCentral,
  type DesktopRustCorePlatform
} from './backends/desktop/desktop-rust-core-binding'

/** The operation-name prefix and platform a host loads the core for. */
export interface DesktopCoreHost {
  readonly platform: DesktopRustCorePlatform
  readonly operationPrefix: string
}

/** What `native/desktop-core/index.js` returns. */
export interface LoadedDesktopCore {
  readonly module: unknown
  readonly path: string
  readonly mode: 'prebuilt' | 'source'
  readonly sidecar: { readonly identity: unknown } | null
}

const DESKTOP_CORE_LOADER = '../../native/desktop-core/index.js'

function hasFunction(value: unknown, name: string): value is object {
  return (
    (typeof value === 'object' || typeof value === 'function') &&
    value !== null &&
    typeof Reflect.get(value, name) === 'function'
  )
}

/** Read the packaged loader through a module-relative dynamic import (CJS and ESM alike). */
async function importPackagedLoader(): Promise<(environment?: NodeJS.ProcessEnv) => unknown> {
  const specifier = DESKTOP_CORE_LOADER
  const imported: unknown = await import(specifier)
  for (const candidate of [
    imported,
    typeof imported === 'object' && imported !== null ? Reflect.get(imported, 'default') : null
  ]) {
    if (hasFunction(candidate, 'loadDesktopCore')) {
      const load: unknown = Reflect.get(candidate, 'loadDesktopCore')
      if (typeof load === 'function') return environment => Reflect.apply(load, candidate, [environment])
    }
  }
  throw new Error(`${specifier} does not export loadDesktopCore`)
}

function stringField(record: object, field: string): string | null {
  const value: unknown = Reflect.get(record, field)
  return typeof value === 'string' ? value : null
}

/** The Windows deployment fact of one listing row: absent, or exactly `packaged` / `unpackaged`. */
function deploymentField(record: object, operation: string): 'packaged' | 'unpackaged' | null {
  const value = stringField(record, 'deployment')
  if (value === null || value === 'packaged' || value === 'unpackaged') return value
  throw contractError('protocol.malformed', 'core', operation)
}

function loaderErrorCode(error: unknown): string | null {
  return typeof error === 'object' && error !== null ? stringField(error, 'code') : null
}

function loaderMessage(error: unknown): string {
  if (typeof error === 'object' && error !== null) {
    const message = stringField(error, 'message')
    if (message !== null) return message
  }
  return String(error)
}

/**
 * Map a loader failure onto its contract identity, keeping the loader's own
 * code and the OS text (a `dlopen` error, a missing libc) verbatim.
 */
function loaderFailure(host: DesktopCoreHost, error: unknown): BackendContractError {
  const code = loaderErrorCode(error) ?? 'load-failed'
  const detail = {
    domain: 'desktop-rust-core',
    code,
    safeMessage: loaderMessage(error).slice(0, 1024),
    metadata: Object.freeze({})
  }
  if (code === 'argument-invalid') {
    return contractError(
      'argument.invalid',
      'core',
      desktopRustCoreOperation(host.operationPrefix, 'rust-core-addon-path'),
      detail
    )
  }
  if (
    code === 'prebuild-sidecar-missing' ||
    code === 'prebuild-sidecar-malformed' ||
    code === 'prebuild-digest-mismatch'
  ) {
    return contractError(
      'protocol.incompatible',
      'core',
      desktopRustCoreOperation(host.operationPrefix, 'native-identity'),
      detail
    )
  }
  return contractError(
    'capability.unavailable',
    'platform',
    desktopRustCoreOperation(host.operationPrefix, 'rust-core-missing'),
    detail
  )
}

function parseLoaded(value: unknown): LoadedDesktopCore {
  if (typeof value !== 'object' || value === null) throw new Error('desktop core loader returned no record')
  const path = stringField(value, 'path')
  const mode = stringField(value, 'mode')
  if (path === null || (mode !== 'prebuilt' && mode !== 'source')) {
    throw new Error('desktop core loader returned a malformed record')
  }
  const sidecar: unknown = Reflect.get(value, 'sidecar')
  return Object.freeze({
    module: Reflect.get(value, 'module'),
    path,
    mode,
    sidecar:
      typeof sidecar === 'object' && sidecar !== null
        ? Object.freeze({ identity: Reflect.get(sidecar, 'identity') })
        : null
  })
}

function identityFailure(host: DesktopCoreHost, safeMessage: string, fields: readonly string[]): BackendContractError {
  return contractError(
    'protocol.incompatible',
    'core',
    desktopRustCoreOperation(host.operationPrefix, 'native-identity'),
    {
      domain: 'desktop-rust-core',
      code: 'native-identity',
      safeMessage,
      metadata: Object.freeze({ fields: Object.freeze([...fields]) })
    }
  )
}

/**
 * Verify the loaded binary before anything opens a radio: its own
 * `nativeBuildIdentity()` against the sealed expectation, and (prebuilt mode)
 * against the identity the builder recorded in the staged sidecar.
 */
export function verifyDesktopCoreIdentity(
  host: DesktopCoreHost,
  loaded: LoadedDesktopCore,
  expected: ExpectedNativeBuildIdentity = EXPECTED_NATIVE_BUILD_IDENTITY
): NativeBuildIdentityRecord {
  const module = loaded.module
  if (!hasFunction(module, 'nativeBuildIdentity')) {
    throw identityFailure(host, `${loaded.path} exports no nativeBuildIdentity()`, ['nativeBuildIdentity'])
  }
  let reported: unknown
  try {
    reported = Reflect.apply(Reflect.get(module, 'nativeBuildIdentity'), module, [])
  } catch (error) {
    throw identityFailure(host, `nativeBuildIdentity() threw: ${loaderMessage(error)}`, ['nativeBuildIdentity'])
  }
  const identity = parseNativeBuildIdentityText(reported)
  if (identity === null) {
    throw identityFailure(host, `${loaded.path} reported a malformed build identity`, ['nativeBuildIdentity'])
  }
  const mismatches = [...nativeBuildIdentityMismatches(identity, 'napi', expected, loaded.mode === 'prebuilt')]
  if (loaded.mode === 'prebuilt') {
    const recorded = loaded.sidecar?.identity
    if (typeof recorded !== 'string' || recorded !== reported) mismatches.push('sidecar')
  }
  if (mismatches.length > 0) {
    throw identityFailure(
      host,
      `${loaded.path} (${loaded.mode}) differs from the packaged identity in: ${mismatches.join(', ')}`,
      mismatches
    )
  }
  return identity
}

interface CentralEntry {
  open(options: object): Promise<unknown>
  openSynthetic(owner: string, options: object | undefined): Promise<unknown>
  listAdapters(bluezBus: string | undefined): Promise<unknown>
  capabilityStates(platform: string, pairingGeneration: boolean): unknown
  vendoredBtleplugPatches(): unknown
}

function centralEntry(module: unknown): CentralEntry | null {
  const central: unknown = typeof module === 'object' && module !== null ? Reflect.get(module, 'UbmCentral') : null
  if (
    !hasFunction(central, 'open') ||
    !hasFunction(central, 'openSynthetic') ||
    !hasFunction(central, 'listAdapters') ||
    !hasFunction(central, 'capabilityStates') ||
    !hasFunction(central, 'vendoredBtleplugPatches')
  ) {
    return null
  }
  return {
    open: (options: object) => Reflect.apply(Reflect.get(central, 'open'), central, [options]),
    openSynthetic: (owner: string, options: object | undefined) =>
      Reflect.apply(Reflect.get(central, 'openSynthetic'), central, [owner, options]),
    listAdapters: (bluezBus: string | undefined) =>
      Reflect.apply(Reflect.get(central, 'listAdapters'), central, [bluezBus]),
    capabilityStates: (platform: string, pairingGeneration: boolean) =>
      Reflect.apply(Reflect.get(central, 'capabilityStates'), central, [platform, pairingGeneration]),
    vendoredBtleplugPatches: () => Reflect.apply(Reflect.get(central, 'vendoredBtleplugPatches'), central, [])
  }
}

const CENTRAL_METHODS: readonly (keyof DesktopRustCoreCentral)[] = Object.freeze([
  'createTicket',
  'cancelTicket',
  'releaseTicket',
  'adapterName',
  'adapterState',
  'takeAdapterEvent',
  'takeLifecycleEvent',
  'dispatchCounters',
  'startScan',
  'stopScan',
  'takeScanObservation',
  'connect',
  'disconnect',
  'readRssi',
  'discover',
  'discoveredPaths',
  'read',
  'write',
  'readDescriptor',
  'writeDescriptor',
  'subscribe',
  'pollNotification',
  'consumerCounters',
  'unsubscribe',
  'cancelOperation',
  'close',
  'adapterAuthorization',
  'securityState',
  'pair',
  'cancelPairing',
  'unpair',
  'takeSecurityEvent',
  'resolveAddress',
  'addressType',
  'maximumWriteLength',
  'installPairingGenerationController',
  'writeReadiness',
  'takeWriteReadinessEvent',
  'takeScanTerminalEvent',
  'peerRecords',
  'setEventWaker',
  'eventWakeFailures',
  'activeScanId',
  'takeAdapterResetEvent',
  'adapterStatus',
  'awaitUsableAdapter',
  'connectionMaximumWriteLength'
])

/**
 * The vendored btleplug patches the loaded binary links, as one
 * comma-separated diagnostic: the evidence that the parity patches
 * (repeated attribute instances, WinRT uncached discovery, scan policy,
 * adapter-by-id, ...) are in this build. A malformed answer is refused.
 */
function vendoredPatches(host: DesktopCoreHost, entry: CentralEntry): string {
  const patches = entry.vendoredBtleplugPatches()
  if (!Array.isArray(patches) || !patches.every(patch => typeof patch === 'string' && patch.length > 0)) {
    throw contractError(
      'protocol.malformed',
      'core',
      desktopRustCoreOperation(host.operationPrefix, 'rust-core-patches')
    )
  }
  return patches.join(',')
}

/** Guard: the opened native object carries every method the binding interface names. */
function isDesktopRustCoreCentral(value: unknown): value is DesktopRustCoreCentral {
  return CENTRAL_METHODS.every(method => hasFunction(value, method))
}

function asCentral(value: unknown, operation: string): DesktopRustCoreCentral {
  if (!isDesktopRustCoreCentral(value)) {
    const missing = CENTRAL_METHODS.filter(method => !hasFunction(value, method))
    throw contractError('protocol.incompatible', 'core', operation, {
      domain: 'desktop-rust-core',
      code: 'central-surface',
      safeMessage: `the opened UbmCentral lacks ${missing.join(', ')}`,
      metadata: Object.freeze({ missing: Object.freeze(missing) })
    })
  }
  return value
}

/**
 * Bind a loaded, identity-verified addon as the provider's core entry.
 * Exported for the acceptance tooling; production code calls
 * {@link loadDesktopCoreBinding}.
 */
export function bindDesktopCore(host: DesktopCoreHost, loaded: LoadedDesktopCore): DesktopRustCoreBinding {
  const identity = verifyDesktopCoreIdentity(host, loaded)
  const entry = centralEntry(loaded.module)
  if (entry === null) {
    throw contractError(
      'protocol.incompatible',
      'core',
      desktopRustCoreOperation(host.operationPrefix, 'rust-core-missing'),
      {
        domain: 'desktop-rust-core',
        code: 'entry-missing',
        safeMessage: `${loaded.path} exports no UbmCentral.open/openSynthetic/listAdapters/capabilityStates`,
        metadata: Object.freeze({})
      }
    )
  }
  return Object.freeze({
    diagnostics: Object.freeze({
      addonPath: loaded.path,
      addonMode: loaded.mode,
      buildTarget: identity.target,
      buildProfile: identity.profile,
      sourceDigest: identity.sourceDigest,
      bindingSchema: identity.bindingSchema,
      btleplugPatches: vendoredPatches(host, entry)
    }),
    openProduction: async (options: Parameters<DesktopRustCoreBinding['openProduction']>[0]) => {
      try {
        return asCentral(
          await entry.open({ ...options, adapterId: options.adapterId ?? undefined }),
          desktopRustCoreOperation(host.operationPrefix, 'rust-core-open')
        )
      } catch (error) {
        throwDesktopRustCoreError(error, desktopRustCoreOperation(host.operationPrefix, 'rust-core-open'))
      }
    },
    openSynthetic: async (
      owner: string,
      options?: { readonly pairingGeneration?: boolean; readonly platform?: DesktopRustCorePlatform }
    ) => {
      try {
        return asCentral(
          await entry.openSynthetic(owner, options),
          desktopRustCoreOperation(host.operationPrefix, 'rust-core-open-synthetic')
        )
      } catch (error) {
        throwDesktopRustCoreError(error, desktopRustCoreOperation(host.operationPrefix, 'rust-core-open-synthetic'))
      }
    },
    capabilityStates: (platform: DesktopRustCorePlatform, pairingGeneration = false) => {
      const rows: unknown = entry.capabilityStates(platform, pairingGeneration)
      if (!Array.isArray(rows)) {
        throw contractError(
          'protocol.malformed',
          'core',
          desktopRustCoreOperation(host.operationPrefix, 'capability-states.shape')
        )
      }
      return Object.freeze(
        rows.map((row: unknown): DesktopRustCoreCapabilityState => {
          if (typeof row !== 'object' || row === null) {
            throw contractError(
              'protocol.malformed',
              'core',
              desktopRustCoreOperation(host.operationPrefix, 'capability-states.row')
            )
          }
          const id = stringField(row, 'id')
          const state = stringField(row, 'state')
          if (id === null || (state !== 'limited' && state !== 'unsupported')) {
            throw contractError(
              'protocol.malformed',
              'core',
              desktopRustCoreOperation(host.operationPrefix, 'capability-states.row')
            )
          }
          return Object.freeze({ id, state, limitation: stringField(row, 'limitation') })
        })
      )
    },
    listAdapters: async (bluezBus?: DesktopRustCoreBluezBus) => {
      let listing: unknown
      try {
        listing = await entry.listAdapters(bluezBus)
      } catch (error) {
        throwDesktopRustCoreError(error, desktopRustCoreOperation(host.operationPrefix, 'list-adapters'))
      }
      if (!Array.isArray(listing)) {
        throw contractError(
          'protocol.malformed',
          'core',
          desktopRustCoreOperation(host.operationPrefix, 'list-adapters.shape')
        )
      }
      return Object.freeze(
        listing.map((entryValue: unknown): DesktopRustCoreAdapterListing => {
          if (typeof entryValue !== 'object' || entryValue === null) {
            throw contractError(
              'protocol.malformed',
              'core',
              desktopRustCoreOperation(host.operationPrefix, 'list-adapters.entry')
            )
          }
          const index: unknown = Reflect.get(entryValue, 'index')
          if (typeof index !== 'number') {
            throw contractError(
              'protocol.malformed',
              'core',
              desktopRustCoreOperation(host.operationPrefix, 'list-adapters.entry')
            )
          }
          const deployment = deploymentField(
            entryValue,
            desktopRustCoreOperation(host.operationPrefix, 'list-adapters.deployment')
          )
          return Object.freeze({
            index,
            label: stringField(entryValue, 'label'),
            error: stringField(entryValue, 'error'),
            displayName: stringField(entryValue, 'displayName'),
            default: Reflect.get(entryValue, 'default') === true,
            deployment
          })
        })
      )
    }
  })
}

/**
 * Load the packaged desktop core for `host`, verify its identity, and return
 * the provider's binding. Every failure is typed and names its cause; none
 * falls back to another binary or to TypeScript.
 */
export async function loadDesktopCoreBinding(
  host: DesktopCoreHost,
  loader: () => Promise<(environment?: NodeJS.ProcessEnv) => unknown> = importPackagedLoader
): Promise<DesktopRustCoreBinding> {
  let loaded: LoadedDesktopCore
  try {
    const load = await loader()
    loaded = parseLoaded(load(process.env))
  } catch (error) {
    throw loaderFailure(host, error)
  }
  return bindDesktopCore(host, loaded)
}
