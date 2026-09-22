// examples-shared/driver/scenarios/w6-slow-drain.ts
//
// W6 acceptance §5, physical-runner form: sustained notifications with a slow
// JS drain stay bounded, loss is explicitly accounted for, and lifecycle
// events are still delivered promptly.
//
// Beside the continuously drained heart-rate consumer, `stall` opens a second
// subscription on the same characteristic (the shared CCCD fanout) and leaves
// it undrained for `stallMs` while values arrive. Draining it afterwards
// reports retained values, overflow notices with exact dropped counts, and
// whether the lifecycle stream stayed responsive throughout.
//
// Only the public unified-ble-manager API is used, so this runs on every
// host. A later adversarial simulator mode may flood values or stall the link;
// this scenario declares no compile-time dependency on it — a flood surfaces
// here as overflow notices, never as silent loss or unbounded memory.

import type {
  BleConnection,
  GattSubscription,
  GattValueEvent,
  PublicBoundedAsyncStreamIterator,
  PublicStreamItem
} from 'unified-ble-manager'
import { HEART_RATE_MEASUREMENT_CHARACTERISTIC, HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import type { DriverHost } from '../host.ts'
import type { JsonValue } from '../protocol.ts'
import { describeError, toJsonValue } from '../protocol.ts'
import { ScenarioError, args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import { OPERATION_TIMEOUT_MS, appendRecent } from './ble-scenario.ts'
import {
  HeartRateScenario,
  IDLE_HEART_RATE_STATE,
  type HeartRateState
} from './heart-rate.ts'

export type SlowDrainState = HeartRateState & {
  readonly stallOpen: boolean
  readonly stallValues: number
  readonly stallOverflows: number
  readonly stallDroppedItems: number
  readonly stallDroppedBytes: number
  readonly stallTerminal: string | null
  readonly recent: readonly string[]
}

const IDLE_SLOW_DRAIN_STATE: SlowDrainState = {
  ...IDLE_HEART_RATE_STATE,
  stallOpen: false,
  stallValues: 0,
  stallOverflows: 0,
  stallDroppedItems: 0,
  stallDroppedBytes: 0,
  stallTerminal: null,
  recent: []
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms))
}

async function pullNext(
  iterator: PublicBoundedAsyncStreamIterator<GattValueEvent>,
  waitMs: number
): Promise<IteratorResult<PublicStreamItem<GattValueEvent>, undefined> | null> {
  let timer: ReturnType<typeof setTimeout> | null = null
  const timeout = new Promise<null>(resolve => {
    timer = setTimeout(() => resolve(null), waitMs)
  })
  const raced = await Promise.race([iterator.next(), timeout])
  if (timer !== null) clearTimeout(timer)
  return raced
}

export class W6SlowDrainScenario extends HeartRateScenario<SlowDrainState> {
  readonly id = 'w6-slow-drain'
  readonly title = 'W6 slow drain (bounded backlog)'
  readonly description =
    'Direct H10 stream plus a "stall" command that leaves a second subscription undrained for stallMs. ' +
    'Draining it reports retained values, exact overflow loss accounting, and lifecycle responsiveness.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: this.startCommand('Start', { autoReconnect: false, intent: 'direct' }, [
      { label: 'Find + connect + stream', args: {} }
    ]),
    stall: defineCommand({
      label: 'Stall drain',
      description: 'Open a second subscription, leave it undrained for stallMs, then drain and account. args: {stallMs?: number (50..10000, default 2000)}',
      presets: [
        { label: 'Stall 2s', args: {} },
        { label: 'Stall 5s', args: { stallMs: 5000 } }
      ],
      parse: raw => ({ stallMs: args.number(raw, 'stallMs', 2000, { min: 50, max: 10000 }) }),
      run: ({ stallMs }) => this.stallDrain(stallMs)
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, IDLE_SLOW_DRAIN_STATE)
  }

  override headline(): string | null {
    const snapshot = this.snapshot()
    return `values ${snapshot.valueCount.toString()} · stall values ${snapshot.stallValues.toString()} · dropped ${snapshot.stallDroppedItems.toString()}`
  }

  private patchSlowDrain(patch: Partial<SlowDrainState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  private note(line: string): void {
    this.patchSlowDrain({ recent: appendRecent(this.snapshot().recent, line) })
  }

  private async stallDrain(stallMs: number): Promise<JsonValue> {
    const connection: BleConnection | null = this.currentConnection
    if (connection === null || !this.isRunning()) {
      throw new ScenarioError('scenario.not-connected', 'no current connection; run "start" first')
    }
    if (this.snapshot().stallOpen) {
      throw new ScenarioError('scenario.busy', 'a stall is already open')
    }
    this.patchSlowDrain({
      stallOpen: true,
      stallValues: 0,
      stallOverflows: 0,
      stallDroppedItems: 0,
      stallDroppedBytes: 0,
      stallTerminal: null
    })
    const generation = connection.connectionGeneration
    let subscription: GattSubscription | null = null
    try {
      const gatt = await connection.discover({ timeoutMs: OPERATION_TIMEOUT_MS })
      subscription = await gatt.characteristic(HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC).subscribe({
        timeoutMs: OPERATION_TIMEOUT_MS,
        stream: 'balanced'
      })
      this.emit('stall-opened', { stallMs, connectionGeneration: generation })
      await sleep(stallMs)
      const iterator = subscription.values[Symbol.asyncIterator]()
      let values = 0
      let overflows = 0
      let droppedItems = 0
      let droppedBytes = 0
      let terminal: string | null = null
      for (;;) {
        const pulled = await pullNext(iterator, 500)
        if (pulled === null || pulled.done === true) break
        const item = pulled.value
        if (item.kind === 'value') {
          values += 1
        } else if (item.kind === 'overflow') {
          overflows += 1
          droppedItems += item.droppedItems
          droppedBytes += item.droppedBytes
        } else {
          terminal = item.reason
          break
        }
      }
      this.patchSlowDrain({
        stallOpen: false,
        stallValues: values,
        stallOverflows: overflows,
        stallDroppedItems: droppedItems,
        stallDroppedBytes: droppedBytes,
        stallTerminal: terminal
      })
      this.emit('stall-drained', { values, overflows, droppedItems, droppedBytes, terminal })
      const record = await subscription.remove()
      subscription = null
      return toJsonValue({
        values,
        overflows,
        droppedItems,
        droppedBytes,
        terminal,
        removeState: record.state
      })
    } catch (error) {
      this.note(`stall failed ${JSON.stringify(describeError(error))}`)
      throw error
    } finally {
      if (subscription !== null) {
        try {
          await subscription.remove()
        } catch (error) {
          this.note(`stall cleanup failed ${JSON.stringify(describeError(error))}`)
        }
      }
      if (this.snapshot().stallOpen) this.patchSlowDrain({ stallOpen: false })
    }
  }
}
