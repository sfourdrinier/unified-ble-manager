// examples-shared/driver/scenarios/heart-rate.ts
//
// The Polar H10 heart-rate journey (formerly PolarH10Session): find, connect,
// discover and subscribe to 180D/2A37, either directly or under an
// application-owned `createConnectionSupervisor` that re-subscribes on every
// reconnect. link-loss and background extend it through the hooks below.

import type {
  BleConnection,
  BleConnectionEvent,
  BleManager,
  BlePeer,
  ConnectionIntent,
  ConnectionSupervisor,
  ConnectionSupervisorEvent,
  GattSubscription,
  GattValueEvent
} from 'unified-ble-manager'
import { createConnectionSupervisor } from 'unified-ble-manager'
import {
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  HEART_RATE_SERVICE,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'
import type { DriverHost, HostManager } from '../host.ts'
import type { JsonObject } from '../protocol.ts'
import { describeError, toJsonValue } from '../protocol.ts'
import { ScenarioError, args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import {
  BleScenario,
  DEVICE_ARGUMENT_HELP,
  FIND_TIMEOUT_MS,
  IDLE_BLE_STATE,
  OPERATION_TIMEOUT_MS,
  appendRecent,
  parseDevice,
  withTimeout,
  type BleScenarioState,
  type DeviceSelector
} from './ble-scenario.ts'

export type HeartRateState = BleScenarioState & {
  readonly mode: 'direct' | 'supervised' | null
  readonly intent: ConnectionIntent | null
  readonly connectionGeneration: string | null
  readonly bpm: number | null
  readonly contact: string | null
  readonly rrIntervalsMs: readonly number[]
  readonly valueCount: number
  readonly maxGapMs: number | null
  readonly requestedDelivery: string | null
  readonly effectiveDelivery: string | null
  readonly lastValueDelivery: string | null
  readonly supervisorState: string | null
  readonly supervisorAttempt: number
  readonly lifecycle: readonly string[]
  readonly streamNotices: readonly string[]
}

export const IDLE_HEART_RATE_STATE: HeartRateState = {
  ...IDLE_BLE_STATE,
  mode: null,
  intent: null,
  connectionGeneration: null,
  bpm: null,
  contact: null,
  rrIntervalsMs: [],
  valueCount: 0,
  maxGapMs: null,
  requestedDelivery: null,
  effectiveDelivery: null,
  lastValueDelivery: null,
  supervisorState: null,
  supervisorAttempt: 0,
  lifecycle: [],
  streamNotices: []
}

export interface HeartRateOptions {
  readonly autoReconnect: boolean
  readonly intent: ConnectionIntent
  readonly device: DeviceSelector
}

export interface HeartRateValueObservation {
  readonly atMs: number
  readonly gapMs: number | null
  readonly connectionGeneration: string
}

export type SupervisedLink = { readonly subscription: GattSubscription; readonly connectionGeneration: string }

const RECONNECTING_STATES: ReadonlySet<string> = new Set(['disconnecting', 'backoff', 'connecting', 'configuring', 'waiting-for-gate'])
const RETRY = { initialDelayMs: 500, maximumDelayMs: 5_000, multiplier: 2, jitter: 0.2 }

export function parseHeartRateOptions(raw: JsonObject, defaults: Omit<HeartRateOptions, 'device'>): HeartRateOptions {
  return {
    autoReconnect: args.boolean(raw, 'autoReconnect', defaults.autoReconnect),
    intent: args.oneOf<ConnectionIntent>(raw, 'intent', ['direct', 'when-available'], defaults.intent),
    device: parseDevice(raw)
  }
}

export abstract class HeartRateScenario<State extends HeartRateState> extends BleScenario<State> {
  private lastObservedAtMs: number | null = null
  protected currentConnection: BleConnection | null = null

  protected constructor(host: DriverHost, idle: State) {
    super(host, idle)
  }

  override headline(): string | null {
    const { bpm, phase } = this.snapshot()
    return bpm === null ? phase : `${bpm.toString()} bpm`
  }

  protected patchHeartRate(patch: Partial<HeartRateState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  /** Hook: every parsed measurement. */
  protected onHeartRateValue(_observation: HeartRateValueObservation): void {}
  /** Hook: every lifecycle transition of the current connection. */
  protected onLifecycle(_event: BleConnectionEvent): void {}
  /** Hook: the lifecycle stream of a connection ended (`error` null when expected). */
  protected onLifecycleEnded(_connectionGeneration: string, _error: unknown): void {}
  /** Hook: every supervisor state event. */
  protected onSupervisor(_event: ConnectionSupervisorEvent<SupervisedLink>): void {}
  /** Hook: the manager exists and Bluetooth is ready. */
  protected async afterManagerReady(_hosted: HostManager): Promise<void> {}

  protected startCommand(label: string, defaults: Omit<HeartRateOptions, 'device'>, presets: readonly { label: string; args: JsonObject }[]): ScenarioCommand {
    return defineCommand({
      label,
      description: `Find the Polar H10, connect, subscribe to heart rate. args: {autoReconnect?: boolean, intent?: "direct" | "when-available", ${DEVICE_ARGUMENT_HELP}}. A direct connect that fails with a caller-decides error is retried once (connect-retry event).`,
      presets,
      acceptsDevice: true,
      parse: raw => parseHeartRateOptions(raw, defaults),
      run: options => this.startHeartRate(options)
    })
  }

  protected startHeartRate(options: HeartRateOptions): Promise<JsonObject> {
    this.lastObservedAtMs = null
    return this.runJourney(
      async signal => {
        this.patchHeartRate({ mode: options.autoReconnect ? 'supervised' : 'direct', intent: options.intent })
        const hosted = await this.createManager(signal)
        await this.afterManagerReady(hosted)
        const manager = hosted.manager
        const peer = await this.findH10(manager, options.device, signal)
        if (options.autoReconnect) {
          await this.startSupervised(manager, peer, options.intent, signal)
        } else {
          const connection = await this.connect(manager, peer, signal, options.intent)
          await this.configureLink(connection, signal)
        }
        this.patchBase({ phase: 'streaming' })
        const snapshot = this.snapshot()
        return {
          device: snapshot.device,
          peer: this.peerReport(),
          mode: snapshot.mode,
          intent: snapshot.intent,
          connectionGeneration: snapshot.connectionGeneration,
          effectiveDelivery: snapshot.effectiveDelivery,
          reconnectTarget: options.autoReconnect ? (peer.reference === null ? 'peer-id' : 'peer-reference') : null
        }
      }
    )
  }

  /** Reconnects by peer reference when the platform minted one, so no rescan is needed. */
  private async startSupervised(manager: BleManager, peer: BlePeer, intent: ConnectionIntent, signal: AbortSignal): Promise<void> {
    const target = peer.reference ?? peer
    this.patchBase({ phase: 'connecting' })
    const supervisor = createConnectionSupervisor<SupervisedLink>(manager, target, {
      connection: { intent, timeoutMs: OPERATION_TIMEOUT_MS },
      retry: RETRY,
      configure: async connection => {
        this.emit('connected', { connectionGeneration: connection.connectionGeneration, intent, supervised: true })
        const subscription = await this.configureLink(connection, undefined)
        return { subscription, connectionGeneration: connection.connectionGeneration }
      },
      disposeSession: async link => {
        const record = await link.subscription.remove()
        this.emit('cleanup', { step: 'supervisor.subscription.remove', state: record.state, detail: toJsonValue(record.failures) })
      }
    })
    this.own('supervisor.stop', () => supervisor.stop())
    const firstConnection = this.watchSupervisor(supervisor)
    supervisor.start()
    await withTimeout(firstConnection, FIND_TIMEOUT_MS, signal, 'scenario.connect-timeout', 'supervisor did not reach "connected"')
  }

  /** Resolves on the first `connected`; rejects when the supervisor stops first. */
  private watchSupervisor(supervisor: ConnectionSupervisor<SupervisedLink>): Promise<void> {
    return new Promise((resolve, reject) => {
      let connectedOnce = false
      void (async () => {
        try {
          for await (const item of supervisor.events) {
            if (item.kind !== 'value') {
              this.emit(item.kind === 'overflow' ? 'stream-overflow' : 'stream-terminal', { stream: 'supervisor.events', notice: toJsonValue(item) })
              continue
            }
            const event = item.value
            this.emit('supervisor', {
              previous: event.previous,
              state: event.state,
              attempt: event.attempt,
              connectionGeneration: event.connectionGeneration,
              delayMs: event.delayMs,
              gateDecision: event.gateDecision,
              error: event.error === null ? null : describeError(event.error),
              cleanup: toJsonValue(event.cleanup ?? null)
            })
            this.patchHeartRate({ supervisorState: event.state, supervisorAttempt: event.attempt })
            if (connectedOnce && RECONNECTING_STATES.has(event.state)) this.patchBase({ phase: `reconnecting (${event.state})` })
            this.onSupervisor(event)
            if (event.state === 'connected' && !connectedOnce) {
              connectedOnce = true
              resolve()
            }
            if (event.state === 'stopped' && !connectedOnce) {
              reject(event.error ?? new ScenarioError('scenario.supervisor-stopped', 'supervisor stopped before the first connection'))
            }
          }
          this.emit('stream-ended', { stream: 'supervisor.events' })
        } catch (error) {
          this.emit('stream-threw', { stream: 'supervisor.events', error: describeError(error) })
          if (!connectedOnce) reject(error)
        }
      })()
    })
  }

  /** discover → subscribe 2A37 → consume; shared by the direct path and the supervisor's configure. */
  private async configureLink(connection: BleConnection, signal: AbortSignal | undefined): Promise<GattSubscription> {
    this.currentConnection = connection
    const generation = connection.connectionGeneration
    this.patchHeartRate({ connectionGeneration: generation })
    void this.watchLifecycle(connection, event => {
      this.patchHeartRate({
        lifecycle: appendRecent(
          this.snapshot().lifecycle,
          `#${event.sequence.toString()} ${event.previous} -> ${event.current} (${event.cause}) gen=${event.connectionGeneration}`
        )
      })
      this.onLifecycle(event)
    }).then(ended => this.onLifecycleEnded(generation, ended.expected ? null : ended.error))
    const gatt = await this.discover(connection, signal)
    this.patchBase({ phase: 'subscribing' })
    const subscription = await gatt.characteristic(HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC).subscribe({
      signal,
      timeoutMs: OPERATION_TIMEOUT_MS,
      stream: 'balanced'
    })
    if (this.snapshot().mode === 'direct') this.own('subscription.remove', () => subscription.remove())
    const requestedDelivery = subscription.requestedDelivery ?? 'default'
    this.emit('subscribed', { connectionGeneration: generation, requestedDelivery, effectiveDelivery: subscription.effectiveDelivery })
    this.patchHeartRate({ requestedDelivery, effectiveDelivery: subscription.effectiveDelivery })
    if (this.snapshot().phase === 'subscribing' && this.snapshot().supervisorState !== null) this.patchBase({ phase: 'streaming' })
    void this.consume('heart-rate', subscription.values, {
      value: value => this.recordValue(value, generation),
      overflow: notice =>
        this.patchHeartRate({ streamNotices: appendRecent(this.snapshot().streamNotices, `overflow dropped=${notice.droppedItems.toString()}`) }),
      terminal: notice => {
        this.patchHeartRate({ streamNotices: appendRecent(this.snapshot().streamNotices, `terminal ${notice.reason} gen=${generation}`) })
        if (this.snapshot().mode === 'direct' && this.snapshot().phase === 'streaming') this.patchBase({ phase: 'stream-ended' })
      }
    })
    return subscription
  }

  private recordValue(value: GattValueEvent, connectionGeneration: string): void {
    const atMs = this.runtime.now()
    const gapMs = this.lastObservedAtMs === null ? null : value.observedAtMonotonicMs - this.lastObservedAtMs
    this.lastObservedAtMs = value.observedAtMonotonicMs
    const previous = this.snapshot()
    const maxGapMs = gapMs === null ? previous.maxGapMs : Math.max(gapMs, previous.maxGapMs ?? Number.NEGATIVE_INFINITY)
    const valueCount = previous.valueCount + 1
    try {
      const measurement = parseHeartRateMeasurement(value.value)
      const rrIntervalsMs = measurement.rrIntervalsSeconds.map(seconds => Math.round(seconds * 1000))
      this.emit('value', {
        sequence: value.sequence,
        bpm: measurement.beatsPerMinute,
        rrIntervalsMs,
        contact: measurement.contact,
        delivery: value.delivery,
        gapMs,
        connectionGeneration
      })
      this.patchHeartRate({
        bpm: measurement.beatsPerMinute,
        contact: measurement.contact,
        rrIntervalsMs,
        valueCount,
        maxGapMs,
        lastValueDelivery: value.delivery
      })
    } catch (error) {
      this.emit('parse-failed', { sequence: value.sequence, bytes: toJsonValue(value.value), error: describeError(error) })
      this.patchHeartRate({ valueCount, maxGapMs, lastValueDelivery: value.delivery })
    }
    this.onHeartRateValue({ atMs, gapMs, connectionGeneration })
  }
}

/** The owner's original H10 journey, now one registry entry. */
export class H10StreamScenario extends HeartRateScenario<HeartRateState> {
  readonly id = 'h10-stream'
  readonly title = 'Polar H10 heart rate'
  readonly description =
    'Find + connect + subscribe 180D/2A37. autoReconnect uses createConnectionSupervisor (configure re-subscribes). intent "when-available" is answered by the backend connect (capability.unsupported where the platform has no such mode).'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: this.startCommand('Start', { autoReconnect: false, intent: 'direct' }, [
      { label: 'Find + connect + stream', args: {} },
      { label: 'Stream with auto-reconnect', args: { autoReconnect: true } },
      { label: 'Stream, intent when-available', args: { intent: 'when-available' } }
    ]),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, IDLE_HEART_RATE_STATE)
  }
}
