// examples-shared/driver/host.ts
//
// The seam between the shared scenarios and one host. A host adapter supplies
// only what differs per runtime: how a manager is constructed and made ready,
// what a background lease is, where app-lifecycle facts come from, and whether
// the radio chooser needs a user gesture. Everything else is shared, so every
// host runs the same scenario code against the same public API.

import type { BleManager, FeatureId } from 'unified-ble-manager'
import type { JsonObject, JsonValue } from './protocol.ts'
import { toJsonValue } from './protocol.ts'
import type { RemoteDriverHost } from './remote-channel.ts'
import type { ScenarioRuntime } from './scenario-core.ts'
import type { UserGestureGate } from './user-gesture.ts'

export const READINESS_TIMEOUT_MS = 15_000

export type HostReport = (kind: string, data: JsonObject) => void

export interface BackgroundLease {
  /** What the host's API answered, for example `acquired` or `capability-supported`. */
  readonly state: string
  readonly detail: JsonValue
  release(): Promise<JsonValue>
}

export interface HostManager {
  readonly manager: BleManager
  /** Brings Bluetooth to ready, reporting each observation, or throws the library's typed error. */
  prepare(report: HostReport, signal: AbortSignal): Promise<void>
  /** Asks the host's background API; a host without one answers through the library's capability registry. */
  acquireBackgroundLease(reason: string): Promise<BackgroundLease>
}

export interface AppStateReading {
  /** The host's own word: `active`/`background`/`inactive` (React Native), `visible`/`hidden` (documents). */
  readonly state: string
  readonly foreground: boolean
}

export interface AppStateSource {
  current(): AppStateReading
  subscribe(listener: (reading: AppStateReading) => void): () => void
}

export interface DriverHost {
  readonly identity: RemoteDriverHost
  readonly runtime: ScenarioRuntime
  createManager(instanceId: string): Promise<HostManager>
  /** Null when the runtime has no app lifecycle (a CLI process). */
  readonly appState: AppStateSource | null
  /** Non-null when the radio chooser must run inside a user activation (Web Bluetooth). */
  readonly userGesture: UserGestureGate | null
}

/** The runtime label stamped on events: `<host>/<platform>`. */
export function hostLabel(identity: Pick<RemoteDriverHost, 'host' | 'platform'>): string {
  return `${identity.host}/${identity.platform}`
}

export type PeerAcquisition = 'scan' | 'choose'

/**
 * How a scenario acquires a peer, read from the backend's own capability
 * report, never from the host's name: scan (`find`) wherever continuous scan
 * is supported; the system chooser only where scan is not supported and the
 * chooser is. A backend that supports neither is asked to `find`, so the
 * library answers with its own typed error.
 */
export function peerAcquisition(manager: BleManager): PeerAcquisition {
  const { capabilities } = manager
  if (capabilities.supports('discovery:continuous-scan')) return 'scan'
  return capabilities.supports('discovery:system-chooser') ? 'choose' : 'scan'
}

/**
 * The readiness step of every host whose manager is a plain `BleManager`:
 * report the adapter state, then wait for the operation the host acquires
 * peers with. `waitUntilReady` throws the library's typed error when it cannot.
 */
export async function prepareAdapter(manager: BleManager, report: HostReport, signal: AbortSignal): Promise<void> {
  const before = await manager.adapter.state()
  report('readiness', { adapter: toJsonValue(before) })
  const operation = peerAcquisition(manager)
  const after = await manager.adapter.waitUntilReady({ operation, signal, timeoutMs: READINESS_TIMEOUT_MS })
  report('readiness', { operation, adapter: toJsonValue(after) })
}

/**
 * A background "lease" for hosts without a lease API: the library's own
 * capability report for the host's background feature, passed on verbatim.
 * Nothing is held either way: a supported feature needs no lease, and an
 * unsupported, unavailable or unregistered one is reported as such (with the
 * backend's limitations), never replaced by a plausible substitute.
 */
export async function capabilityLease(manager: BleManager, feature: FeatureId): Promise<BackgroundLease> {
  const descriptor = manager.capabilities.get(feature)
  return {
    state: descriptor === undefined ? 'capability-unregistered' : `capability-${descriptor.state}`,
    detail: { feature, descriptor: descriptor === undefined ? null : toJsonValue(descriptor) },
    release: async () => ({ feature, held: false })
  }
}

/** Wraps a plain `BleManager` for the shared scenarios. */
export function adapterHostManager(manager: BleManager, backgroundFeature: FeatureId): HostManager {
  return {
    manager,
    prepare: (report, signal) => prepareAdapter(manager, report, signal),
    acquireBackgroundLease: () => capabilityLease(manager, backgroundFeature)
  }
}
