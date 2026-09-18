// examples-shared/driver/scenarios/ble-scenario.ts
//
// Shared journey pieces for every BLE scenario: one host manager per run,
// host readiness, Polar H10 discovery (scan, or the system chooser behind a
// user gesture where the host requires one) of the strap the `device`
// argument names, connect (with one explicit retry when the library reports
// the failure as the caller's to retry) + discover, and a cleanup ledger that
// releases in reverse order and reports each record.
// Only the public unified-ble-manager API is used; the host adapter supplies
// the manager.

import type {
  BleConnection,
  BleConnectionEvent,
  BleManager,
  BlePeer,
  ChooseOptions,
  CleanupRecord,
  ConnectionIntent,
  GattDatabase,
  PublicBoundedAsyncStream,
  PublicStreamOverflowNotice,
  PublicStreamTerminalNotice,
  ScanQuery
} from 'unified-ble-manager'
import { BleError } from 'unified-ble-manager'
import { BATTERY_SERVICE } from 'unified-ble-manager/profiles/battery-service'
import { DEVICE_INFORMATION_SERVICE } from 'unified-ble-manager/profiles/device-information'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import { peerAcquisition, type DriverHost, type HostManager } from '../host.ts'
import { PMD_SERVICE } from '../polar-pmd.ts'
import type { DriverError, JsonObject, JsonValue } from '../protocol.ts'
import { describeError, isJsonObject, toJsonValue } from '../protocol.ts'
import {
  ScenarioController,
  ScenarioError,
  args,
  defineCommand,
  type CleanupStep,
  type ScenarioStopOutcome
} from '../scenario-core.ts'

export type { CleanupStep } from '../scenario-core.ts'

export const FIND_TIMEOUT_MS = 30_000
export const OPERATION_TIMEOUT_MS = 20_000
/** How long a run waits for a person to click before the chooser (Web Bluetooth). */
export const USER_GESTURE_TIMEOUT_MS = 120_000
const RECENT_LINES = 40
/** A single-shot connect is tried at most this many times (one retry), and only for a `caller-decides` failure. */
export const MAX_CONNECT_ATTEMPTS = 2

/** Which strap a run acquires: an exact advertised name, or a name prefix. */
export type DeviceSelector = {
  readonly match: 'exact' | 'prefix'
  readonly name: string
}

/** Without a `device` argument a run takes the first Polar H10 it finds. */
export const DEFAULT_DEVICE: DeviceSelector = { match: 'prefix', name: 'Polar H10' }

export const DEVICE_ARGUMENT_HELP = 'device?: string (exact advertised name, or a name prefix ending in "*"; default "Polar H10*")'

/** Reads the `device` argument: `"Polar H10 E997042F"` is an exact name, `"Polar H10 E99*"` a prefix. */
export function parseDevice(raw: JsonObject): DeviceSelector {
  const value = args.optionalString(raw, 'device')
  if (value === null) return DEFAULT_DEVICE
  const prefix = value.endsWith('*')
  const name = prefix ? value.slice(0, -1) : value
  if (name.trim().length === 0) {
    throw new ScenarioError('scenario.invalid-argument', `argument "device" must name a device (exact name, or a prefix ending in "*"); received ${JSON.stringify(value)}`)
  }
  return { match: prefix ? 'prefix' : 'exact', name }
}

export function deviceQuery(device: DeviceSelector): ScanQuery {
  const names = device.match === 'exact' ? { exact: [device.name] } : { prefixes: [device.name] }
  return { anyOf: [{ services: { any: [HEART_RATE_SERVICE] }, names }] }
}

/**
 * The chooser form of the same query. Web Bluetooth filters names by prefix
 * only, so an exact name is offered as a prefix and the pick is checked.
 * Every service any scenario touches must be listed for Web Bluetooth.
 */
export function deviceChooser(device: DeviceSelector): ChooseOptions {
  return {
    filters: [{ serviceUuids: [HEART_RATE_SERVICE], localNamePrefix: device.name }],
    optionalServices: [BATTERY_SERVICE, DEVICE_INFORMATION_SERVICE, PMD_SERVICE.toLowerCase()]
  }
}

export const POLAR_H10_QUERY: ScanQuery = deviceQuery(DEFAULT_DEVICE)
export const POLAR_H10_CHOOSER: ChooseOptions = deviceChooser(DEFAULT_DEVICE)

/** The peer a run acquired and the selector that found it; reported in every snapshot and result. */
export type PeerReport = {
  readonly id: string
  readonly name: string | null
  readonly query: DeviceSelector
}

export type BleScenarioState = {
  readonly phase: string
  readonly device: string | null
  readonly peer: PeerReport | null
  readonly error: DriverError | null
  readonly cleanup: readonly CleanupStep[]
}

export const IDLE_BLE_STATE: BleScenarioState = { phase: 'idle', device: null, peer: null, error: null, cleanup: [] }

export interface ConnectedH10 {
  readonly manager: BleManager
  readonly peer: BlePeer
  readonly connection: BleConnection
  readonly gatt: GattDatabase
}

export interface StreamHandlers<Value> {
  readonly value: (value: Value) => void
  readonly overflow?: (notice: PublicStreamOverflowNotice) => void
  readonly terminal?: (notice: PublicStreamTerminalNotice) => void
}

type LedgerEntry = { readonly step: string; readonly release: () => Promise<CleanupRecord | JsonValue> }

let nextManagerOrdinal = 1

export abstract class BleScenario<State extends BleScenarioState> extends ScenarioController<State> {
  protected readonly host: DriverHost
  private readonly idleState: State
  private ledger: LedgerEntry[] = []
  private runAbort: AbortController | null = null

  protected constructor(host: DriverHost, idle: State) {
    super(host.runtime, idle)
    this.host = host
    this.idleState = idle
  }

  /** Shared by every BLE scenario: releases whatever the current run owns. */
  protected readonly stopCommand = defineCommand({
    label: 'Stop',
    description: 'Abort the run and release subscription/connection/manager, reporting each cleanup record.',
    parse: args.none,
    run: async () => ({ cleanup: await this.teardown('stopped') })
  })

  /** Updates the shared BLE fields of any scenario state (spreading keeps the subclass fields). */
  protected patchBase(patch: Partial<BleScenarioState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  protected isRunning(): boolean {
    return this.runAbort !== null
  }

  /** Releases the current run, if any, exactly like the `stop` command. */
  override async stop(): Promise<ScenarioStopOutcome> {
    if (!this.isRunning()) return { wasRunning: false, cleanup: [] }
    return { wasRunning: true, cleanup: await this.teardown('stopped') }
  }

  /** The acquired peer, for command results (the snapshot carries the same value). */
  protected peerReport(): PeerReport | null {
    return this.snapshot().peer
  }

  /** Starts a run from a fresh state; a second start while one is active is refused, not ignored. */
  private beginRun(): AbortSignal {
    if (this.runAbort !== null) {
      throw new ScenarioError('scenario.busy', `${this.id} is already running (phase ${this.snapshot().phase}); run "stop" first`)
    }
    const abort = new AbortController()
    this.runAbort = abort
    this.ledger = []
    this.replace(this.idleState)
    this.patchBase({ phase: 'preparing' })
    return abort.signal
  }

  /**
   * Runs one journey: a failure releases everything and is rethrown; an abort
   * caused by `stop` is reported as `run-aborted` (stop already released).
   */
  protected async runJourney<Result>(body: (signal: AbortSignal) => Promise<Result>): Promise<Result> {
    const signal = this.beginRun()
    try {
      return await body(signal)
    } catch (error) {
      if (signal.aborted) this.emit('run-aborted', { phase: this.snapshot().phase, error: describeError(error) })
      else await this.failRun(error)
      throw error
    }
  }

  /**
   * Registers a resource release; releases run in reverse registration order.
   * A resource that arrives after the run was torn down is released at once.
   */
  protected own(step: string, release: () => Promise<CleanupRecord | JsonValue>): void {
    if (this.runAbort !== null) {
      this.ledger.push({ step, release })
      return
    }
    void release().then(
      outcome => this.emit('cleanup', cleanupStep(`${step} (arrived after teardown)`, outcome)),
      error => this.emit('cleanup', { step: `${step} (arrived after teardown)`, state: 'threw', detail: describeError(error) })
    )
  }

  protected async failRun(error: unknown): Promise<void> {
    const described = describeError(error)
    this.patchBase({ phase: 'failed', error: described })
    this.emit('failed', { error: described })
    await this.teardown('failed')
  }

  /** Aborts in-flight work and releases every owned resource, recording each outcome in `cleanup`. */
  protected async teardown(finalPhase: string): Promise<readonly CleanupStep[]> {
    this.runAbort?.abort()
    this.runAbort = null
    const entries = this.ledger.reverse()
    this.ledger = []
    if (finalPhase !== 'failed') this.patchBase({ phase: 'stopping' })
    const steps: CleanupStep[] = []
    for (const entry of entries) {
      let step: CleanupStep
      try {
        step = cleanupStep(entry.step, await entry.release())
      } catch (error) {
        step = { step: entry.step, state: 'threw', detail: describeError(error) }
      }
      steps.push(step)
      this.emit('cleanup', step)
    }
    this.patchBase({ phase: finalPhase, cleanup: steps })
    return steps
  }

  /** The host constructs the manager; the run owns it and the host makes Bluetooth ready. */
  protected async createManager(signal: AbortSignal): Promise<HostManager> {
    const instanceId = `driver-${this.id}-${(nextManagerOrdinal++).toString()}`
    const hosted = await this.host.createManager(instanceId)
    const manager = hosted.manager
    this.own('manager.destroy', () => manager.destroy())
    this.emit('manager-created', { instanceId, backend: this.host.identity.backend, discovery: manager.discovery.kind })
    await hosted.prepare((kind, data) => this.emit(kind, data), signal)
    return hosted
  }

  /**
   * Finds the H10 by query wherever the backend reports continuous scan; asks
   * the system chooser only where it reports scan unsupported and the chooser
   * supported (see `peerAcquisition`), first waiting in an explicit
   * `awaiting-user-gesture` phase when the host needs a person's click.
   */
  protected async findH10(manager: BleManager, device: DeviceSelector, signal: AbortSignal): Promise<BlePeer> {
    const acquisition = peerAcquisition(manager)
    this.emit('peer-acquisition', { via: acquisition, device })
    const peer = acquisition === 'choose' ? await this.chooseH10(manager, device, signal) : await this.scanForH10(manager, device, signal)
    this.emit('found', {
      id: peer.id,
      name: peer.name,
      rssi: peer.rssi,
      hasReference: peer.reference !== null,
      sources: toJsonValue(peer.sources),
      query: device
    })
    this.patchBase({ device: peer.name ?? peer.id, peer: { id: peer.id, name: peer.name, query: device } })
    return peer
  }

  /**
   * A single-shot connect. A failure the library reports as a `BleError` with
   * `retryability: 'caller-decides'` (a transient link-establishment failure
   * such as Android GATT 133) is retried once, announced by a `connect-retry`
   * event; any other failure, a second failure, or a stopped run ends here.
   * The supervised journeys never come through this path: the supervisor owns
   * their retry policy.
   */
  protected async connect(
    manager: BleManager,
    target: BlePeer,
    signal: AbortSignal,
    intent: ConnectionIntent = 'direct'
  ): Promise<BleConnection> {
    for (let attempt = 1; ; attempt += 1) {
      this.patchBase({ phase: 'connecting' })
      const startedAt = this.runtime.now()
      let connection: BleConnection
      try {
        connection = await manager.connect(target, { signal, timeoutMs: OPERATION_TIMEOUT_MS, intent })
      } catch (error) {
        if (attempt >= MAX_CONNECT_ATTEMPTS || signal.aborted || !callerDecidesRetry(error)) throw error
        this.emit('connect-retry', {
          attempt: attempt + 1,
          maxAttempts: MAX_CONNECT_ATTEMPTS,
          failedAfterMs: this.runtime.now() - startedAt,
          error: describeError(error)
        })
        continue
      }
      this.own('connection.release', () => connection.release())
      this.emit('connected', {
        connectionGeneration: connection.connectionGeneration,
        intent,
        attempt,
        connectMs: this.runtime.now() - startedAt
      })
      return connection
    }
  }

  protected async discover(connection: BleConnection, signal: AbortSignal | undefined): Promise<GattDatabase> {
    this.patchBase({ phase: 'discovering' })
    const startedAt = this.runtime.now()
    const gatt = await connection.discover({ signal, timeoutMs: OPERATION_TIMEOUT_MS })
    this.emit('discovered', {
      generation: gatt.generation,
      services: gatt.services.map(service => service.uuid),
      discoverMs: this.runtime.now() - startedAt
    })
    return gatt
  }

  private async scanForH10(manager: BleManager, device: DeviceSelector, signal: AbortSignal): Promise<BlePeer> {
    this.patchBase({ phase: 'finding' })
    return manager.find({ query: deviceQuery(device), signal, timeoutMs: FIND_TIMEOUT_MS })
  }

  private async chooseH10(manager: BleManager, device: DeviceSelector, signal: AbortSignal): Promise<BlePeer> {
    const gate = this.host.userGesture
    if (gate !== null) {
      const reason = 'Web Bluetooth opens the device chooser only from a user gesture: click "Open chooser" in the page'
      this.patchBase({ phase: 'awaiting-user-gesture' })
      this.emit('user-gesture-required', { reason, timeoutMs: USER_GESTURE_TIMEOUT_MS })
      const startedAt = this.runtime.now()
      await withTimeout(gate.request(this.id, reason, signal), USER_GESTURE_TIMEOUT_MS, signal, 'host.user-gesture-timeout', 'no user gesture')
      this.emit('user-gesture-received', { waitedMs: this.runtime.now() - startedAt })
    }
    this.patchBase({ phase: 'choosing' })
    const peer = await manager.choose({ ...deviceChooser(device), signal, timeoutMs: USER_GESTURE_TIMEOUT_MS })
    if (device.match === 'exact' && peer.name !== device.name) {
      throw new ScenarioError(
        'scenario.device-mismatch',
        `the chooser returned ${JSON.stringify(peer.name ?? peer.id)}, not the requested device ${JSON.stringify(device.name)}`
      )
    }
    return peer
  }

  /** Manager → readiness → find → connect → discover, every resource owned by the ledger. */
  protected async connectH10(device: DeviceSelector, signal: AbortSignal): Promise<ConnectedH10> {
    const { manager } = await this.createManager(signal)
    const peer = await this.findH10(manager, device, signal)
    const connection = await this.connect(manager, peer, signal)
    const gatt = await this.discover(connection, signal)
    return { manager, peer, connection, gatt }
  }

  /**
   * Reports lifecycle transitions until the stream ends. An expected end
   * (release, destroy) completes the iteration; any other end throws its typed
   * cause (for example `connection.lost`), which is reported, not dropped.
   */
  protected async watchLifecycle(connection: BleConnection, onEvent?: (event: BleConnectionEvent) => void): Promise<{ readonly expected: boolean; readonly error: unknown }> {
    try {
      for await (const event of connection.lifecycleEvents) {
        this.emit('lifecycle', {
          sequence: event.sequence,
          previous: event.previous,
          current: event.current,
          cause: event.cause,
          connectionGeneration: event.connectionGeneration
        })
        onEvent?.(event)
      }
      this.emit('lifecycle-ended', { expected: true, connectionGeneration: connection.connectionGeneration })
      return { expected: true, error: null }
    } catch (error) {
      this.emit('lifecycle-ended', { expected: false, connectionGeneration: connection.connectionGeneration, error: describeError(error) })
      return { expected: false, error }
    }
  }

  /** Iterates a bounded stream; overflow and terminal notices become events, a throw becomes `stream-threw`. */
  protected async consume<Value>(name: string, stream: PublicBoundedAsyncStream<Value>, handlers: StreamHandlers<Value>): Promise<void> {
    try {
      for await (const item of stream) {
        if (item.kind === 'value') {
          handlers.value(item.value)
        } else if (item.kind === 'overflow') {
          this.emit('stream-overflow', {
            stream: name,
            policy: item.policy,
            droppedItems: item.droppedItems,
            droppedBytes: item.droppedBytes,
            replacedItems: item.replacedItems
          })
          handlers.overflow?.(item)
        } else {
          this.emit('stream-terminal', {
            stream: name,
            reason: item.reason,
            droppedItems: item.droppedItems,
            droppedBytes: item.droppedBytes,
            replacedItems: item.replacedItems,
            error: toJsonValue(item.error ?? null)
          })
          handlers.terminal?.(item)
        }
      }
      this.emit('stream-ended', { stream: name })
    } catch (error) {
      this.emit('stream-threw', { stream: name, error: describeError(error) })
    }
  }
}

export function cleanupStep(step: string, outcome: CleanupRecord | JsonValue): CleanupStep {
  if (isCleanupRecord(outcome)) {
    return { step, state: outcome.state, detail: outcome.failures.length === 0 ? null : toJsonValue(outcome.failures) }
  }
  return { step, state: 'released', detail: toJsonValue(outcome) }
}

function isCleanupRecord(value: CleanupRecord | JsonValue): value is CleanupRecord {
  return typeof value === 'object' && value !== null && 'state' in value && 'failures' in value && Array.isArray(value.failures)
}

/** The library's own answer about repeating the failed operation; never inferred from the code or platform detail. */
function callerDecidesRetry(error: unknown): boolean {
  return error instanceof BleError && error.retryability === 'caller-decides'
}

export function toJsonObject(value: unknown): JsonObject {
  const json = toJsonValue(value)
  return isJsonObject(json) ? json : { value: json }
}

/** Keeps the newest `RECENT_LINES` entries of a rolling list shown in snapshots. */
export function appendRecent<Item>(list: readonly Item[], item: Item): readonly Item[] {
  return [...list.slice(-(RECENT_LINES - 1)), item]
}

export function outcomeOf<Value>(run: () => Promise<Value>): Promise<{ ok: true; value: Value } | { ok: false; error: DriverError }> {
  return run().then(
    value => ({ ok: true as const, value }),
    error => ({ ok: false as const, error: describeError(error) })
  )
}

/** Races `promise` against a deadline and the run's abort; both end as typed ScenarioErrors. */
export function withTimeout<Value>(
  promise: Promise<Value>,
  timeoutMs: number,
  signal: AbortSignal,
  code: string,
  message: string
): Promise<Value> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new ScenarioError(code, `${message} within ${timeoutMs.toString()} ms`)), timeoutMs)
    const onAbort = () => {
      clearTimeout(timer)
      reject(new ScenarioError('operation.aborted', `${message}: run aborted`))
    }
    signal.addEventListener('abort', onAbort, { once: true })
    promise.then(
      value => {
        clearTimeout(timer)
        signal.removeEventListener('abort', onAbort)
        resolve(value)
      },
      error => {
        clearTimeout(timer)
        signal.removeEventListener('abort', onAbort)
        reject(error)
      }
    )
  })
}
