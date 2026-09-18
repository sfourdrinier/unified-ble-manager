// example-expo/src/driver/app-driver.ts
//
// The Expo host adapter of the shared test driver (examples-shared/driver):
// the Expo manager factory with its readiness/permission step and background
// lease, React Native AppState, the RN WebSocket, and the driver URL derived
// from the Metro bundle. The scenarios themselves are the shared ones, and in
// development builds the remote channel lets the control server drive them.

/// <reference types="expo/types" />

import { AppState, Platform, TurboModuleRegistry, type AppStateStatus, type TurboModule } from 'react-native'
import { createExpoBleManager, type ExpoBleManager } from 'unified-ble-manager/expo'
import ubmPackage from 'unified-ble-manager/package.json'
import {
  DRIVER_PORT,
  DRIVER_HOST_PATH,
  ScenarioError,
  createConsoleRuntime,
  createRemoteDriver,
  createScenarioRegistry,
  describeError,
  disposeDriver,
  driverUrlFromScriptUrl,
  explicitDriverUrl,
  hostLabel,
  toJsonObject,
  toJsonValue,
  type AppStateReading,
  type AppStateSource,
  type DriverHost,
  type DriverSocket,
  type DriverSocketHandlers,
  type DriverUrlResolution,
  type HostManager,
  type HostReport,
  type JsonObject,
  type RemoteDriverChannel
} from './shared.ts'

function deviceFacts(): { platform: string; model: string; osVersion: string } {
  if (Platform.OS === 'android') {
    const { Brand, Model, Release } = Platform.constants
    return { platform: 'android', model: `${Brand} ${Model}`, osVersion: Release }
  }
  if (Platform.OS === 'ios') {
    const { systemName, interfaceIdiom, osVersion } = Platform.constants
    return { platform: 'ios', model: `${systemName} ${interfaceIdiom}`, osVersion }
  }
  return { platform: Platform.OS, model: Platform.OS, osVersion: String(Platform.Version) }
}

function appBuild(): JsonObject {
  const { major, minor, patch } = Platform.constants.reactNativeVersion
  return {
    ubmVersion: ubmPackage.version,
    reactNative: `${major.toString()}.${minor.toString()}.${patch.toString()}`,
    dev: __DEV__
  }
}

/**
 * Android cannot tell a never-asked permission from a denied one, so both
 * read as `denied`. Ask first; the system prompt returns at once without
 * showing anything when the user already said "don't ask".
 */
async function prepareExpo(manager: ExpoBleManager, report: HostReport): Promise<void> {
  const readiness = await manager.readiness()
  report('readiness', { state: readiness.state, actions: toJsonValue(readiness.actions), adapter: toJsonValue(readiness.adapter) })
  if (readiness.state === 'ready') return
  if (readiness.adapter.authorization !== 'granted') {
    const result = await manager.permissions.request({ purpose: 'scan-and-connect' })
    report('permission-request', toJsonObject(result))
    if (result.denied.length > 0) {
      throw new ScenarioError(
        'scenario.permission-denied',
        `Bluetooth permission denied (${result.denied.join(', ')}); open ${result.recommendedSettingsTarget ?? 'app'} settings to grant it`
      )
    }
  }
  const after = await manager.readiness()
  report('readiness', { state: after.state, actions: toJsonValue(after.actions), adapter: toJsonValue(after.adapter) })
  if (after.state !== 'ready') {
    throw new ScenarioError('scenario.bluetooth-not-ready', `Bluetooth is not ready: ${after.state} (${after.actions.map(action => action.kind).join(', ')})`)
  }
}

async function createExpoHostManager(instanceId: string): Promise<HostManager> {
  const manager = await createExpoBleManager({ instanceId })
  return {
    manager,
    prepare: report => prepareExpo(manager, report),
    async acquireBackgroundLease(reason) {
      const lease = await manager.background.acquire({ kind: 'connected-device', reason })
      return {
        state: 'acquired',
        detail: null,
        release: async () => {
          await lease.release()
          return null
        }
      }
    }
  }
}

function readAppState(status: AppStateStatus): AppStateReading {
  return { state: status, foreground: status === 'active' }
}

const reactNativeAppState: AppStateSource = {
  current: () => readAppState(AppState.currentState),
  subscribe(listener) {
    const subscription = AppState.addEventListener('change', next => listener(readAppState(next)))
    return () => subscription.remove()
  }
}

const facts = deviceFacts()
const identity = { host: 'expo', platform: facts.platform, backend: `expo/${facts.platform}`, model: facts.model, osVersion: facts.osVersion, appBuild: appBuild() } as const

export const expoDriverHost: DriverHost = {
  identity,
  runtime: createConsoleRuntime(hostLabel(identity)),
  createManager: createExpoHostManager,
  appState: reactNativeAppState,
  userGesture: null
}

/** The app's one scenario registry: what the scenario screens render and the remote channel drives. */
export const scenarioRegistry = createScenarioRegistry(expoDriverHost)

interface SourceCodeModule extends TurboModule {
  getConstants(): { scriptURL: string | null }
}

function resolveDriverUrl(): DriverUrlResolution {
  const explicit = explicitDriverUrl(process.env.EXPO_PUBLIC_UBM_DRIVER_URL, 'EXPO_PUBLIC_UBM_DRIVER_URL')
  if (explicit !== null) return explicit
  const scriptUrl = TurboModuleRegistry.get<SourceCodeModule>('SourceCode')?.getConstants().scriptURL ?? null
  const url = driverUrlFromScriptUrl(scriptUrl)
  return url === null
    ? {
        url: null,
        reason: `bundle was not served by Metro (${scriptUrl ?? 'no SourceCode.scriptURL'}); set EXPO_PUBLIC_UBM_DRIVER_URL=ws://<mac>:${DRIVER_PORT.toString()}${DRIVER_HOST_PATH}`
      }
    : { url, reason: 'derived from the Metro bundle URL' }
}

function reactNativeSocket(url: string, handlers: DriverSocketHandlers): DriverSocket {
  const socket = new WebSocket(url)
  socket.onopen = () => handlers.onOpen()
  socket.onmessage = event => handlers.onMessage(event.data)
  socket.onerror = event => handlers.onError('message' in event && typeof event.message === 'string' ? event.message : 'WebSocket error')
  socket.onclose = event => handlers.onClose(event.code, event.reason)
  return { send: text => socket.send(text), close: () => socket.close() }
}

function createAppRemoteDriver(): RemoteDriverChannel | null {
  if (!__DEV__) return null
  return createRemoteDriver(expoDriverHost, scenarioRegistry, { ...resolveDriverUrl(), createSocket: reactNativeSocket })
}

/** Null in release builds: the remote channel exists only in development builds. */
export const remoteDriver = createAppRemoteDriver()

declare global {
  namespace NodeJS {
    interface Module {
      /** Metro's hot-module API, present in development builds only. Metro does not await a dispose callback. */
      readonly hot?: { dispose(callback: () => void): void }
    }
  }
}

// Fast Refresh re-executes this module with a fresh registry. Before it does,
// the old one stops taking remote commands and stops every run, so an
// orphaned run never keeps holding the strap's connection. Metro does not
// await this callback, so a cleanup failure is reported here (disposeDriver
// has already logged its report), never left as a silent rejection.
module.hot?.dispose(() => {
  disposeDriver({ remote: remoteDriver, registry: scenarioRegistry, runtime: expoDriverHost.runtime }, 'fast-refresh-dispose').catch((error: unknown) => {
    console.error('[driver] Fast Refresh dispose left resources behind', JSON.stringify(describeError(error)))
  })
})
