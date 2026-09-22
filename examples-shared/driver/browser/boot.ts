// examples-shared/driver/browser/boot.ts
//
// Boots the shared driver in a browser engine (Web page, Tauri webview,
// Electron renderer). The caller passes only its host facts: which host it
// is, how it builds a manager, and whether the chooser needs a user gesture.

import { createRemoteDriver, createScenarioRegistry, disposeDriver } from '../create-driver.ts'
import type { DriverUrlResolution } from '../driver-url.ts'
import { hostLabel, type DriverHost, type HostManager } from '../host.ts'
import type { HostKind, JsonObject } from '../protocol.ts'
import type { RemoteDriverChannel } from '../remote-channel.ts'
import { createConsoleRuntime, type ScenarioRegistry, type StopAllReport } from '../scenario-core.ts'
import { PendingUserGestureGate } from '../user-gesture.ts'
import { browserSocket, documentAppState, engineFromUserAgent, platformFromUserAgent } from './host-facts.ts'
import { mountDriverPanel } from './panel.ts'

export interface BrowserHostOptions {
  readonly host: Extract<HostKind, 'web' | 'tauri' | 'electron'>
  readonly backend: string
  readonly createManager: (instanceId: string) => Promise<HostManager>
  /** True only where the chooser needs a user activation (Web Bluetooth). */
  readonly requireUserGesture: boolean
  readonly driverUrl: DriverUrlResolution
  readonly mount: HTMLElement
  readonly appBuild: JsonObject
}

export interface BrowserDriver {
  readonly host: DriverHost
  readonly registry: ScenarioRegistry
  readonly remote: RemoteDriverChannel
  /**
   * Stops the remote channel, unmounts the panel and stops every scenario,
   * resolving once each release reported (see `disposeDriver`). Hosts call it
   * from `import.meta.hot.dispose` so a hot update never orphans a run.
   */
  dispose(reason: string): Promise<StopAllReport>
}

export function bootBrowserDriver(options: BrowserHostOptions): BrowserDriver {
  const { navigator } = globalThis
  const document = options.mount.ownerDocument
  const gate = options.requireUserGesture ? new PendingUserGestureGate() : null
  const identity = {
    host: options.host,
    platform: platformFromUserAgent(navigator.userAgent),
    backend: options.backend,
    model: engineFromUserAgent(navigator.userAgent),
    osVersion: 'not exposed to pages',
    appBuild: options.appBuild
  }
  const host: DriverHost = {
    identity,
    runtime: createConsoleRuntime(hostLabel(identity)),
    createManager: options.createManager,
    appState: documentAppState(document),
    userGesture: gate
  }
  const registry = createScenarioRegistry(host)
  const remote = createRemoteDriver(host, registry, { ...options.driverUrl, createSocket: browserSocket })
  const unmount = mountDriverPanel({
    mount: options.mount,
    title: `UBM test driver · ${hostLabel(identity)} · ${identity.backend}`,
    registry,
    remote,
    userGesture: gate
  })
  remote.start()
  return {
    host,
    registry,
    remote,
    dispose(reason) {
      unmount()
      return disposeDriver({ remote, registry, runtime: host.runtime }, reason)
    }
  }
}
