// examples-shared/driver/scenarios/w6-generation-fence.ts
//
// W6 acceptance §4, physical-runner form: a rapid disconnect/reconnect must
// never let old-generation events affect the new connection. A supervised
// heart-rate stream reconnects by peer reference (no rescan); every value is
// checked against the live connection generation, and any value from a
// retired generation is recorded as a violation instead of being applied.
//
// Only the public unified-ble-manager API is used, so this runs on every
// host. A later adversarial simulator mode may delay old callbacks or reorder
// link events; this scenario declares no compile-time dependency on it — a
// delayed old value surfaces here as a generation violation, never as data.

import type { BleConnectionEvent, ConnectionSupervisorEvent } from 'unified-ble-manager'
import type { DriverHost } from '../host.ts'
import type { JsonValue } from '../protocol.ts'
import { toJsonValue } from '../protocol.ts'
import { ScenarioError, args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import { appendRecent } from './ble-scenario.ts'
import {
  HeartRateScenario,
  IDLE_HEART_RATE_STATE,
  type HeartRateState,
  type HeartRateValueObservation,
  type SupervisedLink
} from './heart-rate.ts'

export type GenerationFenceState = HeartRateState & {
  readonly generations: readonly string[]
  readonly violations: readonly string[]
  readonly outages: number
  readonly outageOpen: boolean
  readonly generationBeforeOutage: string | null
}

const IDLE_GENERATION_FENCE_STATE: GenerationFenceState = {
  ...IDLE_HEART_RATE_STATE,
  generations: [],
  violations: [],
  outages: 0,
  outageOpen: false,
  generationBeforeOutage: null
}

export class W6GenerationFenceScenario extends HeartRateScenario<GenerationFenceState> {
  readonly id = 'w6-generation-fence'
  readonly title = 'W6 generation fence (rapid reconnect)'
  readonly description =
    'Supervised H10 stream that records every connection generation. "rapid-outage" disconnects and the ' +
    'supervisor reconnects by peer reference; values from a retired generation are violations, never data.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: this.startCommand('Start', { autoReconnect: true, intent: 'direct' }, [
      { label: 'Start supervised stream', args: {} },
      { label: 'Start, intent when-available', args: { intent: 'when-available' } }
    ]),
    'rapid-outage': defineCommand({
      label: 'Rapid outage',
      description: 'Disconnect now; the supervisor reconnects without rescanning.',
      parse: args.none,
      run: async () => this.rapidOutage()
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, IDLE_GENERATION_FENCE_STATE)
  }

  private patchFence(patch: Partial<GenerationFenceState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  override headline(): string | null {
    const snapshot = this.snapshot()
    const generations = snapshot.generations.length
    const violations = snapshot.violations.length
    return `gens ${generations.toString()} · outages ${snapshot.outages.toString()} · violations ${violations.toString()}`
  }

  protected override onHeartRateValue(observation: HeartRateValueObservation): void {
    const snapshot = this.snapshot()
    const live = snapshot.connectionGeneration
    if (live !== null && observation.connectionGeneration !== live) {
      this.patchFence({
        violations: appendRecent(
          snapshot.violations,
          `value from retired generation ${observation.connectionGeneration} (live ${live})`
        )
      })
    }
    if (snapshot.outageOpen && observation.connectionGeneration !== snapshot.generationBeforeOutage) {
      this.patchFence({ outageOpen: false })
      this.emit('reconnected', {
        generationBefore: snapshot.generationBeforeOutage,
        generationAfter: observation.connectionGeneration,
        outages: snapshot.outages
      })
    }
  }

  protected override onLifecycle(event: BleConnectionEvent): void {
    const snapshot = this.snapshot()
    if (!snapshot.generations.includes(event.connectionGeneration)) {
      this.patchFence({ generations: [...snapshot.generations, event.connectionGeneration] })
    }
  }

  protected override onSupervisor(event: ConnectionSupervisorEvent<SupervisedLink>): void {
    const snapshot = this.snapshot()
    if (event.connectionGeneration !== null && !snapshot.generations.includes(event.connectionGeneration)) {
      this.patchFence({ generations: [...snapshot.generations, event.connectionGeneration] })
    }
  }

  private async rapidOutage(): Promise<JsonValue> {
    const connection = this.currentConnection
    if (connection === null || !this.isRunning()) {
      throw new ScenarioError('scenario.not-connected', 'no current connection to disconnect; run "start" first')
    }
    const snapshot = this.snapshot()
    const generationBefore = snapshot.connectionGeneration
    this.patchFence({ outageOpen: true, outages: snapshot.outages + 1, generationBeforeOutage: generationBefore })
    this.emit('outage-opened', { generationBefore, outages: snapshot.outages + 1 })
    const record = await connection.disconnect()
    return toJsonValue({
      generationBefore,
      outages: snapshot.outages + 1,
      disconnect: record.state
    })
  }
}
