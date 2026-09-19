// example-expo/src/services/BLEService/PolarH10Session.ts

import type {
  BleConnection,
  BleConnectionEvent,
  CleanupRecord,
  GattSubscription,
  GattValueEvent,
  PublicStreamTerminalNotice
} from 'unified-ble-manager'
import { Platform } from 'react-native'
import { createExpoBleManager, type ExpoBleManager } from 'unified-ble-manager/expo'
import {
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  HEART_RATE_SERVICE,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'

const LOG_PREFIX = '[H10]'
const FIND_TIMEOUT_MS = 30_000
const OPERATION_TIMEOUT_MS = 20_000

type GattDelivery = GattValueEvent['delivery']

export type PolarH10Phase =
  | 'idle'
  | 'preparing'
  | 'finding'
  | 'connecting'
  | 'discovering'
  | 'subscribing'
  | 'streaming'
  | 'stream-ended'
  | 'stopping'
  | 'stopped'
  | 'failed'

export interface PolarH10CleanupStep {
  readonly step: 'subscription.remove' | 'connection.release' | 'manager.destroy'
  readonly state: CleanupRecord['state'] | 'skipped' | 'threw'
  readonly detail: string
}

export interface PolarH10Snapshot {
  readonly phase: PolarH10Phase
  readonly deviceName: string | null
  readonly connectionGeneration: string | null
  readonly bpm: number | null
  readonly contact: string | null
  readonly rrIntervalsMs: readonly number[]
  readonly valueCount: number
  readonly maxGapMs: number | null
  readonly requestedDelivery: string | null
  readonly effectiveDelivery: GattDelivery | null
  readonly lastValueDelivery: GattDelivery | null
  readonly lifecycleEvents: readonly string[]
  readonly streamNotices: readonly string[]
  readonly cleanup: readonly PolarH10CleanupStep[]
  readonly error: string | null
}

export const INITIAL_POLAR_H10_SNAPSHOT: PolarH10Snapshot = Object.freeze({
  phase: 'idle',
  deviceName: null,
  connectionGeneration: null,
  bpm: null,
  contact: null,
  rrIntervalsMs: [],
  valueCount: 0,
  maxGapMs: null,
  requestedDelivery: null,
  effectiveDelivery: null,
  lastValueDelivery: null,
  lifecycleEvents: [],
  streamNotices: [],
  cleanup: [],
  error: null
})

let nextH10ManagerId = 1

/**
 * Owns one Polar H10 heart-rate journey end to end: its own Expo manager,
 * connection and 180D/2A37 subscription. Every observation is reported to the
 * listener and logged with the `[H10]` prefix; nothing is filtered out.
 */
export class PolarH10Session {
  private snapshot: PolarH10Snapshot = INITIAL_POLAR_H10_SNAPSHOT
  private manager: ExpoBleManager | null = null
  private connection: BleConnection | null = null
  private subscription: GattSubscription | null = null
  private startup: AbortController | null = null
  private lastValueAtMs: number | null = null

  constructor(private readonly onChange: (snapshot: PolarH10Snapshot) => void) {}

  current(): PolarH10Snapshot {
    return this.snapshot
  }

  async start(): Promise<void> {
    const abort = new AbortController()
    this.startup = abort
    this.lastValueAtMs = null
    this.replace({ ...INITIAL_POLAR_H10_SNAPSHOT, phase: 'preparing' })
    try {
      const managerId = nextH10ManagerId
      nextH10ManagerId += 1
      const manager = await createExpoBleManager({ instanceId: `expo-h10-${managerId.toString()}` })
      this.manager = manager
      log('manager created', { instanceId: `expo-h10-${managerId.toString()}` })
      await ensureBluetoothReady(manager)

      this.update({ phase: 'finding' })
      const peer = await manager.find({
        query: { anyOf: [{ services: { any: [HEART_RATE_SERVICE] }, names: { prefixes: ['Polar H10'] } }] },
        signal: abort.signal,
        timeoutMs: FIND_TIMEOUT_MS
      })
      log('found peer', { id: peer.id, name: peer.name, rssi: peer.rssi })
      this.update({ phase: 'connecting', deviceName: peer.name ?? peer.id })

      const connection = await manager.connect(peer, { signal: abort.signal, timeoutMs: OPERATION_TIMEOUT_MS })
      this.connection = connection
      log('connected', { connectionGeneration: connection.connectionGeneration })
      this.update({ phase: 'discovering', connectionGeneration: connection.connectionGeneration })
      void this.consumeLifecycle(connection)

      const gatt = await connection.discover({ signal: abort.signal, timeoutMs: OPERATION_TIMEOUT_MS })
      log('discovered', { generation: gatt.generation, services: gatt.services.map(service => service.uuid) })

      this.update({ phase: 'subscribing' })
      const subscription = await gatt.characteristic(HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC).subscribe({
        signal: abort.signal,
        timeoutMs: OPERATION_TIMEOUT_MS,
        stream: 'balanced'
      })
      this.subscription = subscription
      log('subscribed', {
        requestedDelivery: subscription.requestedDelivery ?? 'default',
        effectiveDelivery: subscription.effectiveDelivery
      })
      this.update({
        phase: 'streaming',
        requestedDelivery: subscription.requestedDelivery ?? 'default',
        effectiveDelivery: subscription.effectiveDelivery
      })
      void this.consumeValues(subscription)
    } catch (error) {
      console.error(`${LOG_PREFIX} start failed:`, error)
      this.update({ phase: 'failed', error: describeError(error) })
    } finally {
      if (this.startup === abort) this.startup = null
    }
  }

  /** Removes the subscription, releases the connection and destroys the manager, reporting each record. */
  async stop(): Promise<void> {
    this.startup?.abort()
    this.startup = null
    this.update({ phase: 'stopping', cleanup: [] })
    const subscription = this.subscription
    this.subscription = null
    await this.runCleanup('subscription.remove', subscription === null ? null : () => subscription.remove())
    const connection = this.connection
    this.connection = null
    await this.runCleanup('connection.release', connection === null ? null : () => connection.release())
    const manager = this.manager
    this.manager = null
    await this.runCleanup('manager.destroy', manager === null ? null : () => manager.destroy())
    this.update({ phase: 'stopped' })
  }

  private async runCleanup(
    step: PolarH10CleanupStep['step'],
    action: (() => Promise<CleanupRecord>) | null
  ): Promise<void> {
    let result: PolarH10CleanupStep
    if (action === null) {
      result = { step, state: 'skipped', detail: 'nothing to release' }
    } else {
      try {
        const record = await action()
        result = {
          step,
          state: record.state,
          detail: record.failures.length === 0 ? 'no failures' : JSON.stringify(record.failures)
        }
      } catch (error) {
        console.error(`${LOG_PREFIX} ${step} threw:`, error)
        result = { step, state: 'threw', detail: describeError(error) }
      }
    }
    log('cleanup', result)
    this.update({ cleanup: [...this.snapshot.cleanup, result] })
  }

  /**
   * An expected end (release, manager destroy) completes the iteration; any
   * other end throws its typed cause, e.g. `connection.lost` on link loss.
   */
  private async consumeLifecycle(connection: BleConnection): Promise<void> {
    try {
      for await (const event of connection.lifecycleEvents) {
        this.recordLifecycle(event)
      }
      const line = 'lifecycle stream ended (expected: released/closed)'
      log(line)
      this.appendLifecycle(line)
    } catch (error) {
      const line = `lifecycle stream ended by error: ${describeError(error)}`
      console.warn(`${LOG_PREFIX} ${line}`, error)
      this.appendLifecycle(line)
    }
  }

  private recordLifecycle(event: BleConnectionEvent): void {
    const line = `#${event.sequence.toString()} ${event.previous} -> ${event.current} (${event.cause}) gen=${event.connectionGeneration}`
    log('lifecycle', event)
    this.appendLifecycle(line)
  }

  private appendLifecycle(line: string): void {
    this.update({ lifecycleEvents: [...this.snapshot.lifecycleEvents, line] })
  }

  private appendStreamNotice(line: string): void {
    this.update({ streamNotices: [...this.snapshot.streamNotices, line] })
  }

  private async consumeValues(subscription: GattSubscription): Promise<void> {
    try {
      for await (const item of subscription.values) {
        if (item.kind === 'value') {
          this.recordValue(item.value.value, item.value.delivery, item.value.observedAtMonotonicMs, item.value.sequence)
          continue
        }
        if (item.kind === 'overflow') {
          const line = `overflow policy=${item.policy} dropped=${item.droppedItems.toString()} replaced=${item.replacedItems.toString()}`
          console.warn(`${LOG_PREFIX} ${line}`)
          this.appendStreamNotice(line)
          continue
        }
        this.recordTerminal(item)
      }
      log('value stream iteration finished')
    } catch (error) {
      console.error(`${LOG_PREFIX} value stream threw:`, error)
      this.appendStreamNotice(`stream threw: ${describeError(error)}`)
    } finally {
      if (this.snapshot.phase === 'streaming') this.update({ phase: 'stream-ended' })
    }
  }

  private recordTerminal(notice: PublicStreamTerminalNotice): void {
    const cause = notice.error === undefined || notice.error === null ? '' : ` error=${JSON.stringify(notice.error)}`
    const line = `terminal reason=${notice.reason} dropped=${notice.droppedItems.toString()}${cause}`
    log(line)
    this.appendStreamNotice(line)
  }

  private recordValue(bytes: Uint8Array, delivery: GattDelivery, observedAtMs: number, sequence: number): void {
    const gapMs = this.lastValueAtMs === null ? null : observedAtMs - this.lastValueAtMs
    this.lastValueAtMs = observedAtMs
    const maxGapMs =
      gapMs === null ? this.snapshot.maxGapMs : Math.max(gapMs, this.snapshot.maxGapMs ?? Number.NEGATIVE_INFINITY)
    const valueCount = this.snapshot.valueCount + 1
    try {
      const measurement = parseHeartRateMeasurement(bytes)
      const rrIntervalsMs = measurement.rrIntervalsSeconds.map(seconds => Math.round(seconds * 1000))
      log('value', {
        sequence,
        bpm: measurement.beatsPerMinute,
        rrIntervalsMs,
        contact: measurement.contact,
        delivery,
        gapMs
      })
      this.update({
        bpm: measurement.beatsPerMinute,
        contact: measurement.contact,
        rrIntervalsMs,
        valueCount,
        maxGapMs,
        lastValueDelivery: delivery
      })
    } catch (error) {
      console.error(`${LOG_PREFIX} measurement #${sequence.toString()} failed to parse:`, error, Array.from(bytes))
      this.update({ valueCount, maxGapMs, lastValueDelivery: delivery })
      this.appendStreamNotice(`parse failure #${sequence.toString()}: ${describeError(error)}`)
    }
  }

  private update(patch: Partial<PolarH10Snapshot>): void {
    this.replace({ ...this.snapshot, ...patch })
  }

  private replace(next: PolarH10Snapshot): void {
    if (next.phase !== this.snapshot.phase) log('phase', next.phase)
    this.snapshot = next
    this.onChange(next)
  }
}

async function ensureBluetoothReady(manager: ExpoBleManager): Promise<void> {
  const readiness = await manager.readiness()
  log('readiness', { state: readiness.state, actions: readiness.actions, adapter: readiness.adapter })
  if (readiness.state === 'ready') return
  // Android cannot tell a never-asked permission from a denied one, so both
  // read as `denied` (open-settings). Ask first; the system prompt returns
  // at once without showing anything when the user already said "don't ask".
  if (readiness.adapter.authorization !== 'granted') {
    const result = await manager.permissions.request({ purpose: 'scan-and-connect' })
    log('permission request', result)
    if (result.denied.length > 0) {
      throw new Error(
        `Bluetooth permission denied (${result.denied.join(', ')}); open ${result.recommendedSettingsTarget ?? 'app'} settings to grant it.`
      )
    }
  }
  const after = await manager.readiness()
  log('readiness after permission', { state: after.state, actions: after.actions, adapter: after.adapter })
  if (after.state !== 'ready') {
    throw new Error(`Bluetooth is not ready: ${after.state} (${after.actions.map(action => action.kind).join(', ')})`)
  }
}

function log(message: string, detail?: unknown): void {
  const stamp = `${LOG_PREFIX} +${Math.round(performance.now()).toString()}ms ${Platform.OS}`
  if (detail === undefined) console.log(`${stamp} ${message}`)
  else console.log(`${stamp} ${message}`, JSON.stringify(detail))
}

function describeError(error: unknown): string {
  if (error instanceof Error) {
    const code = 'code' in error && typeof error.code === 'string' ? `${error.code}: ` : ''
    return `${code}${error.message}`
  }
  return String(error)
}
