// example-node/host.ts
//
// The Node desktop host adapter of the shared test driver: the explicit
// desktop entrypoint (`unified-ble-manager/node/{corebluetooth,winrt,bluez}`),
// Node's WebSocket, no app lifecycle and no user-gesture requirement. The
// backend is chosen explicitly by the caller; nothing here falls back to
// another one.

import os from 'node:os'
import path from 'node:path'
import { createRequire } from 'node:module'
import { isConfirmedNodeRelease } from './cleanup.ts'
import { trustedDesktopOptions, type TrustedDesktopOptions } from './trusted-options.cjs'
import type { CleanupRecord, ContinuationRecordingController } from 'unified-ble-manager'
import type { DesktopProcessHost } from 'unified-ble-manager/node/corebluetooth'
import {
  adapterHostManager,
  createConsoleRuntime,
  hostLabel,
  type DriverHost,
  type DriverSocket,
  type DriverSocketHandlers,
  type RemoteDriverHost
} from '../examples-shared/driver/index.ts'

export const NODE_BACKENDS = ['corebluetooth', 'winrt', 'bluez'] as const
export type NodeBackend = (typeof NODE_BACKENDS)[number]

const PLATFORM_OF: Readonly<Record<string, string>> = { darwin: 'macos', win32: 'windows', linux: 'linux' }
const DEFAULT_BACKEND_OF: Readonly<Record<string, NodeBackend>> = {
  darwin: 'corebluetooth',
  win32: 'winrt',
  linux: 'bluez'
}

export function parseBackend(value: string | undefined, platform: NodeJS.Platform): NodeBackend {
  if (value === undefined) {
    const fallback = DEFAULT_BACKEND_OF[platform]
    if (fallback === undefined)
      throw new Error(`no desktop backend for ${platform}; pass --backend ${NODE_BACKENDS.join('|')}`)
    return fallback
  }
  const backend = NODE_BACKENDS.find(candidate => candidate === value)
  if (backend === undefined) throw new Error(`--backend must be one of ${NODE_BACKENDS.join(' | ')}; received ${value}`)
  return backend
}

/** Each backend through its own explicit entrypoint, loaded only when selected. */
export function nodeManagerOptions(adapterId: string | undefined): { readonly adapterId?: string } {
  return trustedDesktopOptions('bluez', adapterId, undefined)
}

async function createBackendProcessHost(
  backend: NodeBackend,
  options: TrustedDesktopOptions
): Promise<DesktopProcessHost> {
  switch (backend) {
    case 'corebluetooth':
      return (await import('unified-ble-manager/node/corebluetooth')).createCoreBluetoothProcessHost(options)
    case 'winrt':
      return (await import('unified-ble-manager/node/winrt')).createWinRtProcessHost(options)
    case 'bluez':
      return (await import('unified-ble-manager/node/bluez')).createBluezProcessHost(options)
  }
}

async function openRecordings(backend: NodeBackend, directory: string): Promise<ContinuationRecordingController> {
  const entry = await (backend === 'corebluetooth'
    ? import('unified-ble-manager/node/corebluetooth')
    : backend === 'winrt'
      ? import('unified-ble-manager/node/winrt')
      : import('unified-ble-manager/node/bluez'))
  const profile = entry.DESKTOP_RUST_CORE_PROFILES[backend]
  const binding = await entry.loadDesktopCoreBinding({ platform: backend, operationPrefix: profile.operationPrefix })
  return entry.openNativeContinuationRecordings(binding, directory)
}

export interface NodeDriverHostOptions {
  /** Trusted daemon attestation, never inferred from a scan or supplied by a scenario. */
  readonly bluezDaemonOwner?: string
  /** Trusted application configuration, never scenario/remote input. */
  readonly recordingsDirectory?: string
  readonly dependencies?: {
    /** Ordinary rejection owns nothing; retained cleanup must expose retryCleanup. */
    createProcessHost(backend: NodeBackend, options: TrustedDesktopOptions): Promise<DesktopProcessHost>
    openRecordings(backend: NodeBackend, directory: string): Promise<ContinuationRecordingController>
  }
}

export interface NodeDriverHost extends DriverHost {
  destroy(): Promise<CleanupRecord>
}

function hasCleanupRetry(error: unknown): error is { retryCleanup(): Promise<CleanupRecord> } {
  return (
    typeof error === 'object' && error !== null && 'retryCleanup' in error && typeof error.retryCleanup === 'function'
  )
}

function defaultRecordingsDirectory(backend: NodeBackend): string {
  const root =
    process.platform === 'darwin'
      ? path.join(os.homedir(), 'Library', 'Application Support')
      : process.platform === 'win32'
        ? (process.env.LOCALAPPDATA ?? path.join(os.homedir(), 'AppData', 'Local'))
        : (process.env.XDG_DATA_HOME ?? path.join(os.homedir(), '.local', 'share'))
  return process.env.UBM_RECORDINGS_DIRECTORY ?? path.join(root, 'unified-ble-manager', 'example-node', backend)
}

export function nodeSocket(url: string, handlers: DriverSocketHandlers): DriverSocket {
  const socket = new WebSocket(url)
  socket.addEventListener('open', () => handlers.onOpen())
  socket.addEventListener('message', event => handlers.onMessage(event.data))
  socket.addEventListener('error', event =>
    handlers.onError(
      'message' in event && typeof event.message === 'string' ? event.message : `WebSocket error on ${url}`
    )
  )
  socket.addEventListener('close', event => handlers.onClose(event.code, event.reason))
  return { send: text => socket.send(text), close: () => socket.close() }
}

function ubmVersion(): string {
  const manifest: unknown = createRequire(import.meta.url)('unified-ble-manager/package.json')
  return typeof manifest === 'object' &&
    manifest !== null &&
    'version' in manifest &&
    typeof manifest.version === 'string'
    ? manifest.version
    : 'unknown'
}

export function nodeIdentity(backend: NodeBackend): RemoteDriverHost {
  return {
    host: 'node',
    platform: PLATFORM_OF[process.platform] ?? process.platform,
    backend: `node/${backend}`,
    model: `${os.hostname()} ${process.arch}`,
    osVersion: os.release(),
    appBuild: { ubmVersion: ubmVersion(), node: process.version }
  }
}

/**
 * A CLI has no app lifecycle (`appState: null`) and no user activation
 * (`userGesture: null`). Its background answer is the library's report for
 * `background:desktop-maintain-connection`.
 */
export function createNodeDriverHost(
  backend: NodeBackend,
  adapterId?: string,
  configuration: NodeDriverHostOptions = {}
): NodeDriverHost {
  const identity = nodeIdentity(backend)
  const options = trustedDesktopOptions(backend, adapterId, configuration.bluezDaemonOwner)
  const directory = configuration.recordingsDirectory ?? defaultRecordingsDirectory(backend)
  if (!path.isAbsolute(directory)) throw new Error('recordingsDirectory must be a trusted absolute path')
  const dependencies = configuration.dependencies ?? { createProcessHost: createBackendProcessHost, openRecordings }
  let pending: Promise<DesktopProcessHost> | null = null
  let closing = false
  let destruction: Promise<CleanupRecord> | null = null
  const admit = () => {
    if (closing) throw new Error('Node driver process host is closed')
  }
  const owner = async () => {
    admit()
    pending ??= Promise.resolve().then(() => dependencies.createProcessHost(backend, options))
    const opening = pending
    let processHost
    try {
      processHost = await opening
    } catch (error) {
      // The factory compensates every allocated central before ordinary rejection.
      // A retained cleanup handle is the exception and must remain reachable.
      if (!hasCleanupRetry(error) && pending === opening) pending = null
      throw error
    }
    admit()
    return processHost
  }
  return {
    identity,
    runtime: createConsoleRuntime(hostLabel(identity), line => {
      process.stderr.write(`${line}\n`)
    }),
    createManager: async instanceId =>
      adapterHostManager(await (await owner()).createManager({ instanceId }), 'background:desktop-maintain-connection'),
    nativeContinuation: {
      execute: async declaration => {
        const processHost = await owner()
        if (declaration.recording !== undefined) await processHost.continuation.recordings(directory)
        admit()
        return processHost.continuation.execute(declaration)
      },
      status: async () => (pending === null ? null : (await pending).continuation.status()),
      claim: async options => {
        if (pending === null)
          return {
            selectors: [],
            values: [],
            streamEnds: [],
            control: [],
            controlLost: 0,
            afterCutoffLoss: { items: 0, bytes: 0 },
            disposed: false,
            disposeFailure: null
          }
        return (await pending).continuation.claim(options)
      },
      recordings: async () => dependencies.openRecordings(backend, directory)
    },
    destroy: () => {
      closing = true
      if (destruction === null) {
        destruction = (async () => {
          if (pending === null) return { state: 'released', failures: [] } satisfies CleanupRecord
          let processHost
          try {
            processHost = await pending
          } catch (error) {
            if (hasCleanupRetry(error)) {
              const receipt = await error.retryCleanup()
              if (isConfirmedNodeRelease(receipt)) pending = null
              return receipt
            }
            pending = null
            return { state: 'released', failures: [] } satisfies CleanupRecord
          }
          return processHost.destroy()
        })().then(
          result => {
            if (!isConfirmedNodeRelease(result)) destruction = null
            return result
          },
          error => {
            destruction = null
            throw error
          }
        )
      }
      return destruction
    },
    appState: null,
    userGesture: null
  }
}
