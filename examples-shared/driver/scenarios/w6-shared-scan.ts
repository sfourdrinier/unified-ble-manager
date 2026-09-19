// examples-shared/driver/scenarios/w6-shared-scan.ts
//
// W6 acceptance §1, physical-runner form: two observers share one scan
// session; one cancels (or its deadline expires) while joining, acquires
// nothing, and the other keeps receiving observations.
//
// Only the public unified-ble-manager API is used, so this runs on every
// host. A later adversarial simulator mode may inject advertisement bursts,
// loss, or stop the source mid-run; this scenario declares no compile-time
// dependency on it — bursts surface here as overflow notices, loss as gaps,
// a stopped source as stream terminals, all reported, never swallowed.

import type {
  PublicBoundedAsyncStreamIterator,
  PublicScanObservation,
  ScanSession
} from 'unified-ble-manager'
import type { DriverHost } from '../host.ts'
import type { JsonObject } from '../protocol.ts'
import { describeError, toJsonValue } from '../protocol.ts'
import { ScenarioError, args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import {
  BleScenario,
  DEVICE_ARGUMENT_HELP,
  FIND_TIMEOUT_MS,
  IDLE_BLE_STATE,
  appendRecent,
  deviceQuery,
  matchesDevice,
  parseDevice,
  type BleScenarioState,
  type DeviceSelector
} from './ble-scenario.ts'

export type SharedScanState = BleScenarioState & {
  readonly joinerCancelled: boolean
  readonly joinerReceived: number
  readonly ownerReceived: number
  readonly ownerValuesAfterCancel: number
  readonly recent: readonly string[]
}

const IDLE_SHARED_SCAN_STATE: SharedScanState = {
  ...IDLE_BLE_STATE,
  joinerCancelled: false,
  joinerReceived: 0,
  ownerReceived: 0,
  ownerValuesAfterCancel: 0,
  recent: []
}

export class W6SharedScanScenario extends BleScenario<SharedScanState> {
  readonly id = 'w6-shared-scan'
  readonly title = 'W6 shared scan (cancel while joining)'
  readonly description =
    'Two observers share one scan session for the Polar H10. Cancelling the joiner acquires nothing; ' +
    'the owner keeps receiving. A later adversarial simulator mode can burst, drop, or stop advertisements; ' +
    'every outcome is reported as values, overflow notices, or terminals.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: defineCommand({
      label: 'Start',
      description: `Open one shared scan and attach two observers. args: {${DEVICE_ARGUMENT_HELP}}`,
      presets: [{ label: 'Start shared scan', args: {} }],
      acceptsDevice: true,
      parse: raw => ({ device: parseDevice(raw) }),
      run: ({ device }) => this.startSharedScan(device)
    }),
    'cancel-joiner': defineCommand({
      label: 'Cancel joiner',
      description: 'Cancel the joining observer mid-run. The owner must keep receiving.',
      parse: args.none,
      run: async () => this.cancelJoiner()
    }),
    stop: this.stopCommand
  }

  private joiner: PublicBoundedAsyncStreamIterator<PublicScanObservation> | null = null
  private joinerTask: Promise<void> | null = null

  constructor(host: DriverHost) {
    super(host, IDLE_SHARED_SCAN_STATE)
  }

  override headline(): string | null {
    const snapshot = this.snapshot()
    return `owner ${snapshot.ownerReceived.toString()} · joiner ${snapshot.joinerReceived.toString()}${snapshot.joinerCancelled ? ' (cancelled)' : ''}`
  }

  private note(line: string): void {
    this.replace({ ...this.snapshot(), recent: appendRecent(this.snapshot().recent, line) })
  }

  private async startSharedScan(device: DeviceSelector): Promise<JsonObject> {
    return this.runJourney(async signal => {
      const { manager } = await this.createManager(signal)
      // Standard acquisition first (the device argument is honored exactly
      // like every peer-acquiring command); the shared session opens after.
      const peer = await this.findH10(manager, device, signal)
      this.patchBase({ phase: 'scanning' })
      const session: ScanSession = await manager.scan({
        signal,
        timeoutMs: FIND_TIMEOUT_MS,
        query: deviceQuery(device),
        duplicates: 'all',
        delivery: 'balanced'
      })
      this.own('scan.stop', () => session.stop())
      this.joiner = session.observations[Symbol.asyncIterator]()
      this.joinerTask = this.drain('joiner', this.joiner, device, signal)
      void this.drain('owner', session.observations[Symbol.asyncIterator](), device, signal)
      this.patchBase({ phase: 'streaming' })
      return { shared: true, observers: 2, peer: peer.id }
    })
  }

  private async drain(
    name: 'owner' | 'joiner',
    iterator: PublicBoundedAsyncStreamIterator<PublicScanObservation>,
    device: DeviceSelector,
    signal: AbortSignal
  ): Promise<void> {
    try {
      for (;;) {
        if (signal.aborted) return
        const next = await iterator.next()
        if (next.done === true) {
          this.note(`${name} ended`)
          return
        }
        const item = next.value
        if (item.kind === 'value' && matchesDevice(item.value, device)) {
          const snapshot = this.snapshot()
          if (name === 'owner') {
            this.replace({
              ...snapshot,
              ownerReceived: snapshot.ownerReceived + 1,
              ownerValuesAfterCancel: snapshot.ownerValuesAfterCancel + (snapshot.joinerCancelled ? 1 : 0)
            })
          } else {
            this.replace({ ...snapshot, joinerReceived: snapshot.joinerReceived + 1 })
          }
          if (this.snapshot().device === null) this.patchBase({ device: item.value.peer.name ?? device.name })
        } else if (item.kind === 'overflow' || item.kind === 'terminal') {
          this.note(`${name} ${item.kind} ${JSON.stringify(toJsonValue(item))}`)
          if (item.kind === 'terminal') return
        }
      }
    } catch (error) {
      this.note(`${name} threw ${JSON.stringify(describeError(error))}`)
    }
  }

  private async cancelJoiner(): Promise<JsonObject> {
    const joiner = this.joiner
    const task = this.joinerTask
    if (joiner === null || task === null || !this.isRunning()) {
      throw new ScenarioError('scenario.not-running', 'no shared scan is running; run "start" first')
    }
    await joiner.return()
    await task
    this.joiner = null
    this.joinerTask = null
    const snapshot = this.snapshot()
    this.replace({ ...snapshot, joinerCancelled: true })
    this.emit('joiner-cancelled', {
      joinerReceived: snapshot.joinerReceived,
      ownerReceived: snapshot.ownerReceived
    })
    return { joinerCancelled: true, ownerReceived: snapshot.ownerReceived }
  }
}
