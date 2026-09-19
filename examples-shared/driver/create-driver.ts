// examples-shared/driver/create-driver.ts
//
// One call per host: the same scenario list, in the same order, on every host,
// plus the remote channel that lets the control server drive that registry.

import type { DriverHost } from './host.ts'
import { RemoteDriverChannel, type DriverSocketFactory } from './remote-channel.ts'
import { describeError, toJsonValue } from './protocol.ts'
import { ScenarioRegistry, type ScenarioRuntime, type StopAllReport } from './scenario-core.ts'
import { BackgroundScenario } from './scenarios/background.ts'
import { DeviceInfoScenario } from './scenarios/device-info.ts'
import { EcgScenario } from './scenarios/ecg.ts'
import { H10CaptureScenario } from './scenarios/h10-capture.ts'
import { H10StreamScenario } from './scenarios/heart-rate.ts'
import { LinkLossScenario } from './scenarios/link-loss.ts'
import { MtuScenario } from './scenarios/mtu.ts'
import { RestorationScenario } from './scenarios/restoration.ts'
import { ScanDetailsScenario } from './scenarios/scan-details.ts'

/** Scenario ids in registry order; identical on every host. */
export const SCENARIO_IDS = [
  'h10-stream',
  'link-loss',
  'device-info',
  'mtu',
  'scan-details',
  'ecg',
  'background',
  'restoration',
  'h10-capture'
] as const

export function createScenarioRegistry(host: DriverHost): ScenarioRegistry {
  return new ScenarioRegistry([
    new H10StreamScenario(host),
    new LinkLossScenario(host),
    new DeviceInfoScenario(host),
    new MtuScenario(host),
    new ScanDetailsScenario(host),
    new EcgScenario(host),
    new BackgroundScenario(host),
    new RestorationScenario(host),
    new H10CaptureScenario(host)
  ])
}

export interface RemoteDriverSetup {
  readonly url: string | null
  /** Why `url` is what it is (its source, or why there is none). */
  readonly reason: string
  readonly createSocket: DriverSocketFactory
}

export function createRemoteDriver(host: DriverHost, registry: ScenarioRegistry, setup: RemoteDriverSetup): RemoteDriverChannel {
  return new RemoteDriverChannel({
    url: setup.url,
    noHostReason: setup.reason,
    registry,
    runtime: host.runtime,
    identity: host.identity,
    createSocket: setup.createSocket
  })
}

export interface DisposableDriver {
  readonly remote: Pick<RemoteDriverChannel, 'stop'> | null
  readonly registry: ScenarioRegistry
  readonly runtime: ScenarioRuntime
}

/**
 * The teardown every host runs when its driver module is replaced (Fast
 * Refresh, Vite HMR) or the host shuts down: stop taking remote commands,
 * then stop every scenario and wait for each release, so an orphaned run
 * never keeps a connection. The report is logged; a cleanup failure is logged
 * and rethrown, never dropped.
 */
export async function disposeDriver(driver: DisposableDriver, reason: string): Promise<StopAllReport> {
  driver.remote?.stop()
  try {
    const report = await driver.registry.stopAll()
    driver.runtime.log('driver', `${reason}: every scenario stopped`, toJsonValue(report))
    return report
  } catch (error) {
    driver.runtime.log('driver', `${reason}: cleanup failed`, describeError(error))
    throw error
  }
}
