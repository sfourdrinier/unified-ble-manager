// example-node/host.ts
//
// The Node desktop host adapter of the shared test driver: the explicit
// desktop entrypoint (`unified-ble-manager/node/{corebluetooth,winrt,bluez}`),
// Node's WebSocket, no app lifecycle and no user-gesture requirement. The
// backend is chosen explicitly by the caller; nothing here falls back to
// another one.

import os from 'node:os'
import { createRequire } from 'node:module'
import type { BleManager } from 'unified-ble-manager'
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
const DEFAULT_BACKEND_OF: Readonly<Record<string, NodeBackend>> = { darwin: 'corebluetooth', win32: 'winrt', linux: 'bluez' }

export function parseBackend(value: string | undefined, platform: NodeJS.Platform): NodeBackend {
  if (value === undefined) {
    const fallback = DEFAULT_BACKEND_OF[platform]
    if (fallback === undefined) throw new Error(`no desktop backend for ${platform}; pass --backend ${NODE_BACKENDS.join('|')}`)
    return fallback
  }
  const backend = NODE_BACKENDS.find(candidate => candidate === value)
  if (backend === undefined) throw new Error(`--backend must be one of ${NODE_BACKENDS.join(' | ')}; received ${value}`)
  return backend
}

/** Each backend through its own explicit entrypoint, loaded only when selected. */
async function createBackendManager(backend: NodeBackend): Promise<BleManager> {
  switch (backend) {
    case 'corebluetooth':
      return (await import('unified-ble-manager/node/corebluetooth')).createCoreBluetoothBleManager()
    case 'winrt':
      return (await import('unified-ble-manager/node/winrt')).createWinRtBleManager()
    case 'bluez':
      return (await import('unified-ble-manager/node/bluez')).createBluezBleManager()
  }
}

export function nodeSocket(url: string, handlers: DriverSocketHandlers): DriverSocket {
  const socket = new WebSocket(url)
  socket.addEventListener('open', () => handlers.onOpen())
  socket.addEventListener('message', event => handlers.onMessage(event.data))
  socket.addEventListener('error', event => handlers.onError('message' in event && typeof event.message === 'string' ? event.message : `WebSocket error on ${url}`))
  socket.addEventListener('close', event => handlers.onClose(event.code, event.reason))
  return { send: text => socket.send(text), close: () => socket.close() }
}

function ubmVersion(): string {
  const manifest: unknown = createRequire(import.meta.url)('unified-ble-manager/package.json')
  return typeof manifest === 'object' && manifest !== null && 'version' in manifest && typeof manifest.version === 'string' ? manifest.version : 'unknown'
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
export function createNodeDriverHost(backend: NodeBackend): DriverHost {
  const identity = nodeIdentity(backend)
  return {
    identity,
    runtime: createConsoleRuntime(hostLabel(identity)),
    createManager: async () => adapterHostManager(await createBackendManager(backend), 'background:desktop-maintain-connection'),
    appState: null,
    userGesture: null
  }
}
