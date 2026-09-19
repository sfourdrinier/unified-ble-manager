// examples-shared/driver/scenarios/link-loss.ts
//
// Link loss and reconnect without rescan: the heart-rate journey under a
// connection supervisor that targets the peer reference from the first find.
// Each outage is measured from the app's own observations (performance.now):
// last value → loss detected → reconnected → first value.

import type { BleConnectionEvent, ConnectionSupervisorEvent } from 'unified-ble-manager'
import type { DriverHost } from '../host.ts'
import type { JsonObject } from '../protocol.ts'
import { describeError, toJsonValue } from '../protocol.ts'
import { ScenarioError, args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import { DEVICE_ARGUMENT_HELP, appendRecent, cleanupStep, parseDevice } from './ble-scenario.ts'
import {
  HeartRateScenario,
  IDLE_HEART_RATE_STATE,
  type HeartRateState,
  type HeartRateValueObservation,
  type SupervisedLink
} from './heart-rate.ts'

export type Outage = {
  readonly index: number
  readonly generationBefore: string
  readonly detectedVia: string
  readonly lostAtMs: number
  readonly lastValueBeforeLossAtMs: number | null
  /** Upper bound of detection latency: nothing arrived between the last value and the detection. */
  readonly detectLatencyUpperBoundMs: number | null
  readonly reconnectedAtMs: number | null
  readonly generationAfter: string | null
  readonly reconnectMs: number | null
  readonly firstValueAtMs: number | null
  readonly firstValueAfterReconnectMs: number | null
  readonly outageMs: number | null
  readonly supervisorAttempt: number
}

export type LinkLossState = HeartRateState & {
  readonly reconnectTarget: string | null
  /** The app only scans for the initial find; reconnects go through the supervisor's peer target. */
  readonly scansAfterInitialFind: number
  readonly lastValueAtMs: number | null
  readonly openOutage: Outage | null
  readonly outages: readonly Outage[]
}

const IDLE_LINK_LOSS_STATE: LinkLossState = {
  ...IDLE_HEART_RATE_STATE,
  reconnectTarget: null,
  scansAfterInitialFind: 0,
  lastValueAtMs: null,
  openOutage: null,
  outages: []
}

export class LinkLossScenario extends HeartRateScenario<LinkLossState> {
  readonly id = 'link-loss'
  readonly title = 'Link loss / reconnect'
  readonly description =
    'Supervised H10 stream reconnecting by peer reference (no rescan). Walk out of range or take the strap off: each outage reports time-to-detect, reconnect time and time-to-first-value.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: defineCommand({
      label: 'Start',
      description: `Supervised stream. args: {intent?: "direct" | "when-available", ${DEVICE_ARGUMENT_HELP}}`,
      presets: [
        { label: 'Start supervised stream', args: {} },
        { label: 'Start, intent when-available', args: { intent: 'when-available' } }
      ],
      acceptsDevice: true,
      parse: raw => ({ intent: args.oneOf(raw, 'intent', ['direct', 'when-available'], 'direct'), device: parseDevice(raw) }),
      run: async ({ intent, device }) => {
        const result = await this.startHeartRate({ autoReconnect: true, intent, device })
        this.patchLinkLoss({ reconnectTarget: typeof result.reconnectTarget === 'string' ? result.reconnectTarget : null })
        return result
      }
    }),
    'force-disconnect': defineCommand({
      label: 'Force disconnect',
      description: 'Local connection.disconnect() (a requested disconnect, not a radio loss) to exercise the reconnect path.',
      parse: args.none,
      run: async () => {
        const connection = this.currentConnection
        if (connection === null || !this.isRunning()) throw new ScenarioError('scenario.not-connected', 'no current connection to disconnect')
        this.openOutage('force-disconnect')
        const record = await connection.disconnect()
        return toJsonValue(cleanupStep('connection.disconnect', record))
      }
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, IDLE_LINK_LOSS_STATE)
  }

  override headline(): string | null {
    const { outages, openOutage, bpm } = this.snapshot()
    const status = openOutage === null ? (bpm === null ? this.snapshot().phase : `${bpm.toString()} bpm`) : 'OUTAGE'
    return `${status} · ${outages.length.toString()} outage(s)`
  }

  private patchLinkLoss(patch: Partial<LinkLossState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  protected override onLifecycle(event: BleConnectionEvent): void {
    if (event.current === 'connected') {
      this.noteReconnect(event.connectionGeneration, this.snapshot().supervisorAttempt, this.runtime.now())
    } else if (event.current === 'disconnected' || event.current === 'lost') {
      this.openOutage(`lifecycle ${event.current} (${event.cause})`, event.connectionGeneration)
    }
  }

  protected override onLifecycleEnded(connectionGeneration: string, error: unknown): void {
    if (error !== null && connectionGeneration === this.snapshot().connectionGeneration) {
      this.openOutage(`lifecycle stream ended: ${describeError(error).code}`, connectionGeneration)
    }
  }

  protected override onSupervisor(event: ConnectionSupervisorEvent<SupervisedLink>): void {
    if ((event.state === 'disconnecting' || event.state === 'backoff') && this.snapshot().valueCount > 0) {
      this.openOutage(`supervisor ${event.state}`, event.connectionGeneration)
    }
    if (event.state === 'connected' && event.connectionGeneration !== null) {
      this.noteReconnect(event.connectionGeneration, event.attempt, this.runtime.now())
    }
  }

  protected override onHeartRateValue(observation: HeartRateValueObservation): void {
    const outage = this.snapshot().openOutage
    if (outage !== null && observation.connectionGeneration !== outage.generationBefore) {
      // Finding 162: the first value with the new generation can arrive
      // before any `connected` observation does. The value itself is then
      // the reconnect observation, so the record fills from it instead of
      // staying null forever.
      if (outage.reconnectedAtMs === null) {
        this.noteReconnect(observation.connectionGeneration, outage.supervisorAttempt, observation.atMs)
      }
      const filled = this.snapshot().openOutage
      if (filled !== null) {
        const closed: Outage = {
          ...filled,
          generationAfter: filled.generationAfter ?? observation.connectionGeneration,
          firstValueAtMs: observation.atMs,
          firstValueAfterReconnectMs:
            filled.reconnectedAtMs === null ? null : observation.atMs - filled.reconnectedAtMs,
          outageMs: observation.atMs - filled.lostAtMs
        }
        this.patchLinkLoss({ openOutage: null, outages: appendRecent(this.snapshot().outages, closed) })
        this.emit('outage-recovered', outageJson(closed))
      }
    }
    this.patchLinkLoss({ lastValueAtMs: observation.atMs })
  }

  /**
   * Finding 162: record the reconnect the first time the new generation is
   * observed — on the supervisor `connected` event, the lifecycle
   * `connected` transition, or the first value with the new generation,
   * whichever arrives first. A reconnect observed only via its first value
   * fills `reconnectMs` from that value (`firstValueAfterReconnectMs` 0):
   * the link is provably back, and the value time is the tightest known
   * upper bound of the reconnect instant.
   */
  private noteReconnect(generationAfter: string, attempt: number, reconnectedAtMs: number): boolean {
    const outage = this.snapshot().openOutage
    if (outage === null || outage.reconnectedAtMs !== null || generationAfter === outage.generationBefore) return false
    const reconnectMs = reconnectedAtMs - outage.lostAtMs
    this.patchLinkLoss({
      openOutage: { ...outage, reconnectedAtMs, generationAfter, reconnectMs, supervisorAttempt: attempt }
    })
    this.emit('reconnected', { index: outage.index, reconnectMs, attempt })
    return true
  }

  /**
   * Opens at most one outage per connection generation, from whichever signal
   * arrives first. The losing generation comes from the detecting event: a
   * late or duplicate signal for a superseded generation matches the
   * completed outage for that generation instead of opening a ghost outage
   * under the current one (finding 162).
   */
  private openOutage(detectedVia: string, generation: string | null = null): void {
    const state = this.snapshot()
    const losing = generation ?? state.connectionGeneration
    if (!this.isRunning() || losing === null || state.openOutage !== null) return
    if (state.outages.some(outage => outage.generationBefore === losing)) return
    const lostAtMs = this.runtime.now()
    const outage: Outage = {
      index: state.outages.length + 1,
      generationBefore: losing,
      detectedVia,
      lostAtMs,
      lastValueBeforeLossAtMs: state.lastValueAtMs,
      detectLatencyUpperBoundMs: state.lastValueAtMs === null ? null : lostAtMs - state.lastValueAtMs,
      reconnectedAtMs: null,
      generationAfter: null,
      reconnectMs: null,
      firstValueAtMs: null,
      firstValueAfterReconnectMs: null,
      outageMs: null,
      supervisorAttempt: state.supervisorAttempt
    }
    this.patchLinkLoss({ openOutage: outage })
    this.emit('outage-detected', outageJson(outage))
  }
}

function outageJson(outage: Outage): JsonObject {
  return { ...outage }
}
