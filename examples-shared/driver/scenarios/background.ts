// examples-shared/driver/scenarios/background.ts
//
// Observes whether heart-rate values keep arriving while the host is not in
// front (app backgrounded, phone locked, page hidden). It records the host's
// app-state transitions and per-state value counts; it adds no background
// mode of its own. An optional `backgroundLease` asks the host's background
// API (or, without one, the library's capability registry) and reports
// whatever it answers. A host with no app lifecycle (a CLI) says so.

import type { AppStateReading, DriverHost, HostManager } from '../host.ts'
import { describeError } from '../protocol.ts'
import { args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import { DEVICE_ARGUMENT_HELP, appendRecent, parseDevice } from './ble-scenario.ts'
import { HeartRateScenario, IDLE_HEART_RATE_STATE, type HeartRateState, type HeartRateValueObservation } from './heart-rate.ts'

export const APP_STATE_UNTRACKED = 'untracked'

export type AppStatePeriod = {
  readonly appState: string
  readonly foreground: boolean
  readonly startedAtMs: number
  readonly endedAtMs: number | null
  readonly values: number
  readonly maxGapMs: number | null
  readonly firstValueAtMs: number | null
  readonly lastValueAtMs: number | null
}

export type BackgroundState = HeartRateState & {
  readonly appState: string
  readonly leaseRequested: boolean
  readonly leaseState: string | null
  readonly currentPeriod: AppStatePeriod | null
  readonly periods: readonly AppStatePeriod[]
  readonly valuesWhileNotActive: number
}

export class BackgroundScenario extends HeartRateScenario<BackgroundState> {
  readonly id = 'background'
  readonly title = 'Background / locked'
  readonly description =
    'H10 stream plus host app-state tracking: counts values and the largest gap per foreground/background period. Start, then background the app, lock the phone or hide the page, then come back.'
  private leaseRequested = false
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: defineCommand({
      label: 'Start',
      description: `args: {autoReconnect?: boolean (default true), backgroundLease?: boolean (default false), ${DEVICE_ARGUMENT_HELP}}`,
      presets: [
        { label: 'Start (supervised)', args: {} },
        { label: 'Start with background lease', args: { backgroundLease: true } }
      ],
      acceptsDevice: true,
      parse: raw => ({
        autoReconnect: args.boolean(raw, 'autoReconnect', true),
        backgroundLease: args.boolean(raw, 'backgroundLease', false),
        device: parseDevice(raw)
      }),
      run: async ({ autoReconnect, backgroundLease, device }) => {
        this.leaseRequested = backgroundLease
        return this.startHeartRate({ autoReconnect, intent: 'direct', device })
      }
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, {
      ...IDLE_HEART_RATE_STATE,
      appState: host.appState?.current().state ?? APP_STATE_UNTRACKED,
      leaseRequested: false,
      leaseState: null,
      currentPeriod: null,
      periods: [],
      valuesWhileNotActive: 0
    })
  }

  override headline(): string | null {
    const { appState, valuesWhileNotActive, valueCount } = this.snapshot()
    return `${appState} · ${valueCount.toString()} values · ${valuesWhileNotActive.toString()} while not active`
  }

  private patchBackground(patch: Partial<BackgroundState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  protected override async afterManagerReady(hosted: HostManager): Promise<void> {
    this.patchBackground({ leaseRequested: this.leaseRequested })
    this.trackAppState()
    if (!this.leaseRequested) return
    try {
      const lease = await hosted.acquireBackgroundLease('Polar H10 background streaming test')
      this.own('background.lease.release', () => lease.release())
      this.patchBackground({ leaseState: lease.state })
      this.emit('background-lease', { state: lease.state, detail: lease.detail })
    } catch (error) {
      const described = describeError(error)
      this.patchBackground({ leaseState: `failed: ${described.code}` })
      this.emit('background-lease', { state: 'failed', error: described })
    }
  }

  private trackAppState(): void {
    const source = this.host.appState
    if (source === null) {
      this.patchBackground({ appState: APP_STATE_UNTRACKED })
      this.emit('app-state-untracked', { reason: `${this.host.identity.host} has no app lifecycle; every value counts as foreground` })
      return
    }
    const now = this.runtime.now()
    const reading = source.current()
    this.patchBackground({ appState: reading.state, currentPeriod: openPeriod(reading, now) })
    const unsubscribe = source.subscribe(next => this.onAppState(next))
    this.own('app-state.listener.remove', async () => {
      unsubscribe()
      this.closePeriod(this.runtime.now())
      return null
    })
  }

  private onAppState(next: AppStateReading): void {
    const now = this.runtime.now()
    const previous = this.snapshot().appState
    this.emit('app-state', { from: previous, to: next.state, foreground: next.foreground })
    this.closePeriod(now)
    this.patchBackground({ appState: next.state, currentPeriod: openPeriod(next, now) })
  }

  private closePeriod(now: number): void {
    const period = this.snapshot().currentPeriod
    if (period === null) return
    const closed = { ...period, endedAtMs: now }
    this.emit('app-state-period', closed)
    this.patchBackground({ currentPeriod: null, periods: appendRecent(this.snapshot().periods, closed) })
  }

  protected override onHeartRateValue(observation: HeartRateValueObservation): void {
    const state = this.snapshot()
    const period = state.currentPeriod
    if (period === null) return
    const gapMs = period.lastValueAtMs === null ? null : observation.atMs - period.lastValueAtMs
    this.patchBackground({
      valuesWhileNotActive: state.valuesWhileNotActive + (period.foreground ? 0 : 1),
      currentPeriod: {
        ...period,
        values: period.values + 1,
        maxGapMs: gapMs === null ? period.maxGapMs : Math.max(gapMs, period.maxGapMs ?? 0),
        firstValueAtMs: period.firstValueAtMs ?? observation.atMs,
        lastValueAtMs: observation.atMs
      }
    })
  }
}

function openPeriod(reading: AppStateReading, startedAtMs: number): AppStatePeriod {
  return {
    appState: reading.state,
    foreground: reading.foreground,
    startedAtMs,
    endedAtMs: null,
    values: 0,
    maxGapMs: null,
    firstValueAtMs: null,
    lastValueAtMs: null
  }
}
