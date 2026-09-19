// examples-shared/driver/scenarios/live-dashboard.ts
//
// Live dashboard: one tile per Polar H10 in range. A tile appears when a
// strap is observed, streams heart rate (180D/2A37), PMD ECG at 130 Hz,
// battery (180F/2A19, notifications where the library allows them, a periodic
// read where it does not) and Device Information (180A firmware, model,
// serial, manufacturer), and reconnects through an application-owned
// `createConnectionSupervisor` when the strap drops out and returns.
//
// Only the public unified-ble-manager API is used. Peer acquisition reuses
// the shared journey pieces (`peerAcquisition`, `deviceQuery`,
// `deviceChooser`, `matchesDevice`, `chooseH10`); the tile vocabulary is the
// library's own (supervisor states, lifecycle causes, typed error codes).

import type {
  BleConnection,
  BleManager,
  BlePeer,
  ConnectionSupervisor,
  ConnectionSupervisorEvent,
  ConnectionSupervisorState,
  DiscoveryEvent,
  GattCharacteristic,
  GattDatabase,
  GattSubscription,
  GattValueEvent,
  PublicBoundedAsyncStream,
  PublicScanObservation,
  ScanQuery
} from 'unified-ble-manager'
import { createConnectionSupervisor } from 'unified-ble-manager'
import { BATTERY_LEVEL_CHARACTERISTIC, BATTERY_SERVICE, parseBatteryLevel } from 'unified-ble-manager/profiles/battery-service'
import {
  DEVICE_INFORMATION_SERVICE,
  FIRMWARE_REVISION_CHARACTERISTIC,
  MANUFACTURER_NAME_CHARACTERISTIC,
  MODEL_NUMBER_CHARACTERISTIC,
  SERIAL_NUMBER_CHARACTERISTIC,
  decodeDeviceInformationString
} from 'unified-ble-manager/profiles/device-information'
import {
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  HEART_RATE_SERVICE,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'
import { peerAcquisition, type DriverHost } from '../host.ts'
import type { DriverError, JsonObject } from '../protocol.ts'
import { bytesToHex, describeError, toJsonValue } from '../protocol.ts'
import {
  H10_ECG_SAMPLE_RATE_HZ,
  PMD_CONTROL_POINT,
  PMD_DATA,
  PMD_SERVICE,
  POLAR_PREFERRED_MTU,
  buildGetEcgSettingsCommand,
  buildStartEcgCommand,
  buildStopEcgCommand,
  byteAt,
  parseControlPointMessage,
  parseEcgFrame,
  parsePmdFeatures,
  parsePmdSettings,
  type ControlPointMessage
} from '../polar-pmd.ts'
import { ScenarioError, args, defineCommand, type ScenarioCommand, type ScenarioStopOutcome } from '../scenario-core.ts'
import {
  BleScenario,
  DEFAULT_DEVICE,
  IDLE_BLE_STATE,
  OPERATION_TIMEOUT_MS,
  appendRecent,
  deviceQuery,
  matchesDevice,
  outcomeOf,
  type BleScenarioState,
  type DeviceSelector
} from './ble-scenario.ts'

/** Samples kept per tile: a 10 s ring at 130 Hz, so the ~5 s display window survives reconnects. */
export const ECG_BUFFER_CAP_SAMPLES = H10_ECG_SAMPLE_RATE_HZ * 10
/** Display window: the newest ~5 s of the tile buffer. */
export const ECG_WINDOW_SAMPLES = H10_ECG_SAMPLE_RATE_HZ * 5
/** Display budget: the window decimated to this many points, so a tile render stays cheap. */
export const ECG_DISPLAY_MAX_POINTS = 130
/** Battery re-read interval where the library refuses notifications. */
export const BATTERY_POLL_MS = 30_000
const CONTROL_POINT_TIMEOUT_MS = 5_000
const RETRY = { initialDelayMs: 500, maximumDelayMs: 5_000, multiplier: 2, jitter: 0.2 }

/** Supervisor states that mean the tile is on its way back, not newly connecting. */
const RECONNECTING_SUPERVISOR_STATES: ReadonlySet<ConnectionSupervisorState> = new Set([
  'disconnecting',
  'backoff',
  'connecting',
  'configuring',
  'waiting-for-gate'
])

export type LiveDashboardTileStatus = 'discovered' | 'connecting' | 'streaming' | 'reconnecting' | 'lost' | 'off'

export type LiveDashboardTile = {
  readonly peerId: string
  readonly name: string | null
  readonly status: LiveDashboardTileStatus
  /** The library's own supervisor word (`connected`, `backoff`, …), never a paraphrase. */
  readonly supervisorState: string | null
  readonly supervisorAttempt: number
  /** The library's own lifecycle cause word (for example `connection.lost`). */
  readonly lifecycleCause: string | null
  readonly lifecycle: readonly string[]
  readonly connectionGeneration: string | null
  readonly rssi: number | null
  readonly lastSeenAtMs: number | null
  readonly bpm: number | null
  readonly contact: string | null
  readonly rrIntervalsMs: readonly number[]
  readonly valueCount: number
  readonly batteryPercent: number | null
  /** `notification`/`indication` where the library streams battery, `poll` where it polls. */
  readonly batteryDelivery: string | null
  readonly firmwareRevision: string | null
  readonly modelNumber: string | null
  readonly serialNumber: string | null
  readonly manufacturerName: string | null
  /** Per-field read failures; one missing characteristic never hides the others. */
  readonly infoErrors: JsonObject
  readonly ecgSamples: number
  readonly ecgBuffered: number
  /** The newest ~5 s, decimated to `ECG_DISPLAY_MAX_POINTS` for the tile trace. */
  readonly ecgDisplay: readonly number[]
  readonly lastSampleMicroVolts: number | null
  readonly parseFailures: number
  readonly error: DriverError | null
}

export type LiveDashboardState = BleScenarioState & {
  readonly devices: readonly string[] | 'all-polar'
  readonly ecgEnabled: boolean
  readonly tiles: { readonly [peerId: string]: LiveDashboardTile }
  readonly tileOrder: readonly string[]
}

export interface LiveDashboardStartOptions {
  readonly devices: readonly string[] | 'all-polar'
  readonly ecg: boolean
}

function parseDevicesArgument(raw: JsonObject): readonly string[] | 'all-polar' {
  const value = raw['devices']
  if (value === undefined) return 'all-polar'
  if (value === 'all-polar') return 'all-polar'
  if (Array.isArray(value) && value.length > 0) {
    const names: string[] = []
    for (const entry of value) {
      if (typeof entry !== 'string' || entry.trim().length === 0) {
        throw new ScenarioError(
          'scenario.invalid-argument',
          `argument "devices" must list non-empty advertised names; received ${JSON.stringify(entry)}`
        )
      }
      names.push(entry)
    }
    return names
  }
  throw new ScenarioError(
    'scenario.invalid-argument',
    `argument "devices" must be "all-polar" or a non-empty array of advertised names; received ${JSON.stringify(value ?? null)}`
  )
}

export function parseLiveDashboardStartOptions(raw: JsonObject): LiveDashboardStartOptions {
  return { devices: parseDevicesArgument(raw), ecg: args.boolean(raw, 'ecg', true) }
}

function selectorsFor(devices: readonly string[] | 'all-polar'): readonly DeviceSelector[] {
  if (devices === 'all-polar') return [DEFAULT_DEVICE]
  return devices.map(name => ({ match: 'exact', name }) satisfies DeviceSelector)
}

/** The scan form of the dashboard request: every named strap, or the Polar H10 prefix. */
export function dashboardQuery(devices: readonly string[] | 'all-polar'): ScanQuery {
  if (devices === 'all-polar') return deviceQuery(DEFAULT_DEVICE)
  return { anyOf: devices.map(name => ({ services: { any: [HEART_RATE_SERVICE] }, names: { exact: [name] } })) }
}

/** Newest `windowSamples` decimated so the first point is the window start and the last is the newest sample. */
export function downsampleEcg(buffer: readonly number[], windowSamples: number, maxPoints: number): readonly number[] {
  if (buffer.length === 0 || maxPoints <= 0) return []
  const window = buffer.slice(Math.max(0, buffer.length - windowSamples))
  if (window.length <= maxPoints) return [...window]
  if (maxPoints === 1) {
    const newest = window[window.length - 1]
    return newest === undefined ? [] : [newest]
  }
  const points: number[] = []
  for (let index = 0; index < maxPoints; index += 1) {
    const sample = window[Math.floor((index * (window.length - 1)) / (maxPoints - 1))]
    if (sample !== undefined) points.push(sample)
  }
  return points
}

type ControlPointResponse = Extract<ControlPointMessage, { kind: 'response' }>
type PmdWaiter = { readonly opCode: number; readonly resolve: (response: ControlPointResponse) => void }

/** One tile's live session: what `configure` built and `disposeSession` tears down. */
interface TileSession {
  readonly hrSubscription: GattSubscription
  readonly batterySubscription: GattSubscription | null
  readonly controlPointSubscription: GattSubscription | null
  readonly dataSubscription: GattSubscription | null
  readonly controlPoint: GattCharacteristic | null
  readonly ecgStarted: boolean
}

/** One tile's non-serializable runtime: supervisor, ECG ring buffer, PMD waiters, poll timer. */
interface TileRuntime {
  readonly peer: BlePeer
  supervisor: ConnectionSupervisor<TileSession> | null
  everStreamed: boolean
  ecgBuffer: number[]
  ecgWaiters: PmdWaiter[]
  controlPoint: GattCharacteristic | null
  cancelBatteryPoll: (() => void) | null
}

type InfoField = 'firmwareRevision' | 'modelNumber' | 'serialNumber' | 'manufacturerName'

const INFO_READS: readonly { readonly field: InfoField; readonly characteristic: string }[] = [
  { field: 'firmwareRevision', characteristic: FIRMWARE_REVISION_CHARACTERISTIC },
  { field: 'modelNumber', characteristic: MODEL_NUMBER_CHARACTERISTIC },
  { field: 'serialNumber', characteristic: SERIAL_NUMBER_CHARACTERISTIC },
  { field: 'manufacturerName', characteristic: MANUFACTURER_NAME_CHARACTERISTIC }
]

const IDLE_LIVE_DASHBOARD_STATE: LiveDashboardState = {
  ...IDLE_BLE_STATE,
  devices: 'all-polar',
  ecgEnabled: true,
  tiles: {},
  tileOrder: []
}

export class LiveDashboardScenario extends BleScenario<LiveDashboardState> {
  readonly id = 'live-dashboard'
  readonly title = 'Live dashboard: Polar H10 tiles'
  readonly description =
    'One tile per Polar H10 in range: strap name, live heart rate + RR + skin contact, PMD ECG trace, battery and Device Information. Tiles reconnect through createConnectionSupervisor when a strap returns.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: defineCommand({
      label: 'Start dashboard',
      description:
        'Scan for every Polar H10 in range (or the named straps) and keep one supervised tile per strap. args: {devices?: "all-polar" | string[] (exact advertised names), ecg?: boolean}.',
      presets: [{ label: 'Start dashboard (all Polar H10, ECG on)', args: {} }, { label: 'Start dashboard, no ECG', args: { ecg: false } }],
      acceptsDevice: false,
      parse: parseLiveDashboardStartOptions,
      run: options => this.start(options)
    }),
    stop: this.stopCommand,
    snapshot: defineCommand({
      label: 'Snapshot',
      description: 'Report the current tiles without changing anything.',
      parse: args.none,
      run: async () => this.snapshot()
    })
  }

  private readonly runtimes = new Map<string, TileRuntime>()

  constructor(host: DriverHost) {
    super(host, IDLE_LIVE_DASHBOARD_STATE)
  }

  override headline(): string | null {
    const { phase, tileOrder, tiles } = this.snapshot()
    if (tileOrder.length === 0) return phase
    const streaming = tileOrder.filter(id => tiles[id]?.status === 'streaming').length
    return `${streaming.toString()}/${tileOrder.length.toString()} streaming`
  }

  override async stop(): Promise<ScenarioStopOutcome> {
    const outcome = await super.stop()
    if (outcome.wasRunning) {
      const snapshot = this.snapshot()
      const tiles: Record<string, LiveDashboardTile> = {}
      for (const [peerId, tile] of Object.entries(snapshot.tiles)) tiles[peerId] = { ...tile, status: 'off' }
      this.replace({ ...snapshot, tiles })
      this.flushSnapshot()
    }
    this.runtimes.clear()
    return outcome
  }

  private start(options: LiveDashboardStartOptions): Promise<JsonObject> {
    return this.runJourney(async signal => {
      this.runtimes.clear()
      this.patch({ devices: options.devices, ecgEnabled: options.ecg })
      this.patchBase({ phase: 'preparing' })
      const hosted = await this.createManager(signal)
      const manager = hosted.manager
      const acquisition = peerAcquisition(manager)
      this.emit('peer-acquisition', { via: acquisition, devices: toJsonValue(options.devices) })
      if (acquisition === 'choose') {
        this.patchBase({ phase: 'choosing' })
        for (const selector of selectorsFor(options.devices)) {
          const peer = await this.chooseH10(manager, selector, signal)
          this.observePeer(manager, peer, peer.name, peer.rssi, options, signal)
        }
        this.patchBase({ phase: 'streaming' })
      } else {
        const query = dashboardQuery(options.devices)
        this.patchBase({ phase: 'scanning' })
        const session = await manager.scan({ query, duplicates: 'all', delivery: 'balanced', signal })
        this.own('scan.stop', () => session.stop())
        this.emit('scan-started', { query: toJsonValue(query) })
        const selectors = selectorsFor(options.devices)
        void this.watchObservations(manager, session.observations, selectors, options, signal)
        if (session.events !== undefined) void this.watchDiscoveryEvents(session.events)
      }
      const snapshot = this.snapshot()
      return { devices: snapshot.devices, ecg: snapshot.ecgEnabled, tiles: snapshot.tileOrder }
    })
  }

  /** A scan hit (or chooser pick): new straps gain a tile and a supervisor, known ones refresh last-seen. */
  private observePeer(
    manager: BleManager,
    peer: BlePeer,
    name: string | null,
    rssi: number | null,
    options: LiveDashboardStartOptions,
    signal: AbortSignal
  ): void {
    if (signal.aborted) return
    const atMs = this.runtime.now()
    if (this.snapshot().tiles[peer.id] === undefined) {
      this.emit('tile-discovered', { tile: peer.id, name, rssi })
      this.addTile(peer, name, rssi, atMs)
      this.startTileSupervisor(manager, peer, options, signal)
    } else {
      const current = this.snapshot().tiles[peer.id]
      this.patchTile(peer.id, { rssi, lastSeenAtMs: atMs, name: name ?? current?.name ?? null })
    }
  }

  private addTile(peer: BlePeer, name: string | null, rssi: number | null, atMs: number): void {
    const snapshot = this.snapshot()
    this.runtimes.set(peer.id, {
      peer,
      supervisor: null,
      everStreamed: false,
      ecgBuffer: [],
      ecgWaiters: [],
      controlPoint: null,
      cancelBatteryPoll: null
    })
    this.replace({
      ...snapshot,
      tiles: { ...snapshot.tiles, [peer.id]: initialTile(peer.id, name, rssi, atMs) },
      tileOrder: [...snapshot.tileOrder, peer.id]
    })
  }

  /** Patches one tile; a patch for a tile that is gone is reported, never dropped silently. */
  private patchTile(peerId: string, patch: Partial<LiveDashboardTile>): void {
    const snapshot = this.snapshot()
    const current = snapshot.tiles[peerId]
    if (current === undefined) {
      this.emit('tile-missing', { tile: peerId, patch: toJsonValue(patch) })
      return
    }
    this.replace({ ...snapshot, tiles: { ...snapshot.tiles, [peerId]: { ...current, ...patch } } })
  }

  private async watchObservations(
    manager: BleManager,
    stream: PublicBoundedAsyncStream<PublicScanObservation>,
    selectors: readonly DeviceSelector[],
    options: LiveDashboardStartOptions,
    signal: AbortSignal
  ): Promise<void> {
    try {
      for await (const item of stream) {
        if (item.kind === 'overflow') {
          this.emit('stream-overflow', {
            stream: 'dashboard.scan',
            policy: item.policy,
            droppedItems: item.droppedItems,
            droppedBytes: item.droppedBytes,
            replacedItems: item.replacedItems
          })
          continue
        }
        if (item.kind === 'terminal') {
          this.emit('stream-terminal', {
            stream: 'dashboard.scan',
            reason: item.reason,
            droppedItems: item.droppedItems,
            droppedBytes: item.droppedBytes,
            replacedItems: item.replacedItems,
            error: toJsonValue(item.error ?? null)
          })
          continue
        }
        const observation = item.value
        if (!selectors.some(selector => matchesDevice(observation, selector))) continue
        this.observePeer(manager, observation.peer, observation.localName ?? observation.peer.name, observation.rssi, options, signal)
      }
      this.emit('stream-ended', { stream: 'dashboard.scan' })
    } catch (error) {
      this.emit('stream-threw', { stream: 'dashboard.scan', error: describeError(error) })
    }
  }

  private async watchDiscoveryEvents(events: AsyncIterable<DiscoveryEvent>): Promise<void> {
    try {
      for await (const event of events) {
        if (event.kind === 'lost') {
          const tile = this.snapshot().tiles[event.peer.id]
          this.emit('tile-lost', { tile: event.peer.id, lastObservedAt: event.lastObservedAt, reason: event.reason })
          if (tile !== undefined && tile.status === 'discovered') {
            this.patchTile(event.peer.id, { status: 'lost', lastSeenAtMs: event.lastObservedAt })
          }
        } else if (event.kind === 'observed') {
          const tile = this.snapshot().tiles[event.peer.id]
          if (tile !== undefined) this.patchTile(event.peer.id, { lastSeenAtMs: this.runtime.now(), rssi: event.peer.rssi })
        } else {
          this.emit('tile-presence-overflow', {
            guarantee: event.guarantee,
            droppedEntries: event.droppedEntries,
            droppedBytes: event.droppedBytes
          })
        }
      }
      this.emit('stream-ended', { stream: 'dashboard.presence' })
    } catch (error) {
      this.emit('stream-threw', { stream: 'dashboard.presence', error: describeError(error) })
    }
  }

  private startTileSupervisor(manager: BleManager, peer: BlePeer, options: LiveDashboardStartOptions, signal: AbortSignal): void {
    const runtime = this.runtimes.get(peer.id)
    if (runtime === undefined || runtime.supervisor !== null || signal.aborted) return
    this.patchTile(peer.id, { status: 'connecting', supervisorState: 'idle' })
    const supervisor = createConnectionSupervisor<TileSession>(manager, peer.reference ?? peer, {
      connection: { intent: 'direct', timeoutMs: OPERATION_TIMEOUT_MS },
      retry: RETRY,
      configure: connection => this.configureTile(peer.id, connection, options),
      disposeSession: session => this.disposeTileSession(peer.id, session)
    })
    runtime.supervisor = supervisor
    this.own(`supervisor.stop ${peer.id}`, () => supervisor.stop())
    supervisor.start()
    void this.watchTileSupervisor(peer.id, supervisor)
  }

  private async watchTileSupervisor(peerId: string, supervisor: ConnectionSupervisor<TileSession>): Promise<void> {
    let connectedOnce = false
    try {
      for await (const item of supervisor.events) {
        if (item.kind !== 'value') {
          this.emit(item.kind === 'overflow' ? 'stream-overflow' : 'stream-terminal', {
            stream: `dashboard.supervisor ${peerId}`,
            notice: toJsonValue(item)
          })
          continue
        }
        const event = item.value
        this.emit('tile-supervisor', {
          tile: peerId,
          previous: event.previous,
          state: event.state,
          attempt: event.attempt,
          connectionGeneration: event.connectionGeneration,
          delayMs: event.delayMs,
          gateDecision: event.gateDecision,
          error: event.error === null ? null : describeError(event.error)
        })
        this.onSupervisorEvent(peerId, event)
        if (event.state === 'connected' && !connectedOnce) {
          connectedOnce = true
          this.emit('tile-online', { tile: peerId, connectionGeneration: event.connectionGeneration })
        }
      }
      this.emit('stream-ended', { stream: `dashboard.supervisor ${peerId}` })
    } catch (error) {
      this.emit('stream-threw', { stream: `dashboard.supervisor ${peerId}`, error: describeError(error) })
    }
  }

  private onSupervisorEvent(peerId: string, event: ConnectionSupervisorEvent<TileSession>): void {
    const runtime = this.runtimes.get(peerId)
    if (event.state === 'connected') {
      if (runtime !== undefined) runtime.everStreamed = true
      this.patchTile(peerId, {
        status: 'streaming',
        supervisorState: event.state,
        supervisorAttempt: event.attempt,
        connectionGeneration: event.connectionGeneration ?? this.snapshot().tiles[peerId]?.connectionGeneration ?? null
      })
    } else if (event.state === 'stopped') {
      this.patchTile(peerId, { status: 'off', supervisorState: event.state, supervisorAttempt: event.attempt })
    } else if (RECONNECTING_SUPERVISOR_STATES.has(event.state)) {
      this.patchTile(peerId, {
        status: runtime?.everStreamed === true ? 'reconnecting' : 'connecting',
        supervisorState: event.state,
        supervisorAttempt: event.attempt
      })
    } else {
      this.patchTile(peerId, { supervisorState: event.state, supervisorAttempt: event.attempt })
    }
  }

  /**
   * Builds one tile's live session on every (re)connect: HR subscribe, battery
   * subscribe-or-poll, Device Information reads and the PMD ECG start. A
   * failure unwinds what this attempt created and rethrows, so the supervisor
   * backs off and configures again; every outcome is emitted.
   */
  private async configureTile(peerId: string, connection: BleConnection, options: LiveDashboardStartOptions): Promise<TileSession> {
    const runtime = this.runtimes.get(peerId)
    if (runtime === undefined) {
      throw new ScenarioError('scenario.not-running', `live-dashboard tile ${peerId} is gone; refusing configure`)
    }
    const generation = connection.connectionGeneration
    this.patchTile(peerId, { connectionGeneration: generation })
    this.emit('tile-connected', { tile: peerId, connectionGeneration: generation })
    void this.watchLifecycle(connection, event => {
      const tile = this.snapshot().tiles[peerId]
      if (tile === undefined) return
      this.emit('tile-lifecycle', {
        tile: peerId,
        sequence: event.sequence,
        previous: event.previous,
        current: event.current,
        cause: String(event.cause),
        connectionGeneration: event.connectionGeneration
      })
      this.patchTile(peerId, {
        lifecycleCause: String(event.cause),
        lifecycle: appendRecent(tile.lifecycle, `#${event.sequence.toString()} ${event.previous} -> ${event.current} (${String(event.cause)})`)
      })
    })
    const created: GattSubscription[] = []
    const unwind = async (): Promise<void> => {
      runtime.cancelBatteryPoll?.()
      runtime.cancelBatteryPoll = null
      runtime.controlPoint = null
      runtime.ecgWaiters = []
      for (const subscription of created.reverse()) {
        const outcome = await outcomeOf(() => subscription.remove())
        this.emit(
          'tile-cleanup',
          outcome.ok
            ? { tile: peerId, step: 'configure-unwind', state: outcome.value.state }
            : { tile: peerId, step: 'configure-unwind', state: 'threw', error: outcome.error }
        )
      }
    }
    try {
      const gatt = await connection.discover({ timeoutMs: OPERATION_TIMEOUT_MS })
      this.emit('tile-discovered', { tile: peerId, generation: gatt.generation, services: gatt.services.map(service => service.uuid) })
      const mtu = await outcomeOf(() => connection.controls.requestMtu(POLAR_PREFERRED_MTU, { timeoutMs: OPERATION_TIMEOUT_MS }))
      this.emit('tile-mtu', {
        tile: peerId,
        requested: POLAR_PREFERRED_MTU,
        outcome: mtu.ok ? toJsonValue(mtu.value) : mtu.error
      })

      const hrSubscription = await gatt
        .characteristic(HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC)
        .subscribe({ timeoutMs: OPERATION_TIMEOUT_MS, stream: 'balanced' })
      created.push(hrSubscription)
      this.emit('tile-subscribed', {
        tile: peerId,
        characteristic: 'heart-rate',
        requestedDelivery: hrSubscription.requestedDelivery ?? null,
        effectiveDelivery: hrSubscription.effectiveDelivery
      })
      void this.consume(`dashboard.hr ${peerId}`, hrSubscription.values, {
        value: value => this.recordHeartRate(peerId, value, generation)
      })

      const batteryCharacteristic = gatt.characteristic(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC)
      const initialBattery = await outcomeOf(async () => parseBatteryLevel(await batteryCharacteristic.read({ timeoutMs: OPERATION_TIMEOUT_MS })))
      if (initialBattery.ok) this.patchTile(peerId, { batteryPercent: initialBattery.value })
      else this.emit('tile-battery-read-failed', { tile: peerId, error: initialBattery.error })
      let batterySubscription: GattSubscription | null = null
      try {
        const subscription = await batteryCharacteristic.subscribe({ timeoutMs: OPERATION_TIMEOUT_MS, stream: 'balanced' })
        batterySubscription = subscription
        created.push(subscription)
        this.patchTile(peerId, { batteryDelivery: subscription.effectiveDelivery })
        this.emit('tile-subscribed', {
          tile: peerId,
          characteristic: 'battery',
          requestedDelivery: subscription.requestedDelivery ?? null,
          effectiveDelivery: subscription.effectiveDelivery
        })
        void this.consume(`dashboard.battery ${peerId}`, subscription.values, { value: value => this.recordBattery(peerId, value) })
      } catch (error) {
        const described = describeError(error)
        this.emit('tile-battery-poll', { tile: peerId, reason: described.code, error: described })
        this.patchTile(peerId, { batteryDelivery: 'poll' })
        runtime.cancelBatteryPoll = this.pollBattery(peerId, batteryCharacteristic)
      }

      for (const spec of INFO_READS) {
        const outcome = await outcomeOf(async () =>
          decodeDeviceInformationString(
            await gatt.characteristic(DEVICE_INFORMATION_SERVICE, spec.characteristic).read({ timeoutMs: OPERATION_TIMEOUT_MS })
          )
        )
        if (outcome.ok) {
          if (spec.field === 'firmwareRevision') this.patchTile(peerId, { firmwareRevision: outcome.value })
          else if (spec.field === 'modelNumber') this.patchTile(peerId, { modelNumber: outcome.value })
          else if (spec.field === 'serialNumber') this.patchTile(peerId, { serialNumber: outcome.value })
          else this.patchTile(peerId, { manufacturerName: outcome.value })
          this.emit('tile-device-info', { tile: peerId, field: spec.field, ok: true, value: outcome.value })
        } else {
          const tile = this.snapshot().tiles[peerId]
          this.patchTile(peerId, { infoErrors: { ...(tile?.infoErrors ?? {}), [spec.field]: outcome.error } })
          this.emit('tile-device-info', { tile: peerId, field: spec.field, ok: false, error: outcome.error })
        }
      }

      let controlPointSubscription: GattSubscription | null = null
      let dataSubscription: GattSubscription | null = null
      let controlPoint: GattCharacteristic | null = null
      let ecgStarted = false
      if (options.ecg) {
        const outcome = await this.configureEcg(peerId, runtime, gatt, created)
        controlPointSubscription = outcome.controlPointSubscription
        dataSubscription = outcome.dataSubscription
        controlPoint = outcome.controlPoint
        ecgStarted = outcome.ecgStarted
      }
      return { hrSubscription, batterySubscription, controlPointSubscription, dataSubscription, controlPoint, ecgStarted }
    } catch (error) {
      await unwind()
      this.emit('tile-configure-failed', { tile: peerId, connectionGeneration: generation, error: describeError(error) })
      throw error
    }
  }

  private async configureEcg(
    peerId: string,
    runtime: TileRuntime,
    gatt: GattDatabase,
    created: GattSubscription[]
  ): Promise<{ readonly controlPointSubscription: GattSubscription | null; readonly dataSubscription: GattSubscription | null; readonly controlPoint: GattCharacteristic | null; readonly ecgStarted: boolean }> {
    const controlPointCharacteristic = gatt.characteristic(PMD_SERVICE, PMD_CONTROL_POINT)
    const dataCharacteristic = gatt.characteristic(PMD_SERVICE, PMD_DATA)
    const controlPointSubscription = await controlPointCharacteristic.subscribe({
      timeoutMs: OPERATION_TIMEOUT_MS,
      delivery: 'prefer-indication',
      stream: 'lossless-bounded'
    })
    created.push(controlPointSubscription)
    this.emit('tile-subscribed', {
      tile: peerId,
      characteristic: 'pmd-control-point',
      requestedDelivery: controlPointSubscription.requestedDelivery ?? null,
      effectiveDelivery: controlPointSubscription.effectiveDelivery
    })
    void this.consume(`dashboard.pmd-control-point ${peerId}`, controlPointSubscription.values, {
      value: value => this.onControlPoint(peerId, value)
    })
    const dataSubscription = await dataCharacteristic.subscribe({
      timeoutMs: OPERATION_TIMEOUT_MS,
      delivery: 'prefer-notification',
      stream: 'balanced'
    })
    created.push(dataSubscription)
    this.emit('tile-subscribed', {
      tile: peerId,
      characteristic: 'pmd-data',
      requestedDelivery: dataSubscription.requestedDelivery ?? null,
      effectiveDelivery: dataSubscription.effectiveDelivery
    })
    void this.consume(`dashboard.pmd-data ${peerId}`, dataSubscription.values, {
      value: value => this.onEcgFrame(peerId, value)
    })
    runtime.controlPoint = controlPointCharacteristic

    const featureBytes = await outcomeOf(async () => controlPointCharacteristic.read({ timeoutMs: OPERATION_TIMEOUT_MS }))
    if (!featureBytes.ok) {
      this.emit('tile-pmd-features', { tile: peerId, ok: false, error: featureBytes.error })
      return { controlPointSubscription, dataSubscription, controlPoint: controlPointCharacteristic, ecgStarted: false }
    }
    const features = await outcomeOf(async () => parsePmdFeatures(featureBytes.value))
    if (!features.ok) {
      this.emit('tile-pmd-features', { tile: peerId, ok: false, error: features.error })
      return { controlPointSubscription, dataSubscription, controlPoint: controlPointCharacteristic, ecgStarted: false }
    }
    this.emit('tile-pmd-features', { tile: peerId, ok: true, ...features.value, raw: bytesToHex(featureBytes.value) })
    if (!features.value.ecg) {
      this.emit('tile-ecg-unsupported', { tile: peerId, features: toJsonValue(features.value) })
      return { controlPointSubscription, dataSubscription, controlPoint: controlPointCharacteristic, ecgStarted: false }
    }
    const settings = await outcomeOf(
      async () => parsePmdSettings((await this.pmdCommand(peerId, controlPointCharacteristic, buildGetEcgSettingsCommand())).parameters)
    )
    this.emit(
      'tile-pmd-settings',
      settings.ok ? { tile: peerId, ok: true, settings: toJsonValue(settings.value) } : { tile: peerId, ok: false, error: settings.error }
    )
    const settleStop = await outcomeOf(() => this.pmdCommand(peerId, controlPointCharacteristic, buildStopEcgCommand()))
    this.emit(
      'tile-pmd-stop',
      settleStop.ok ? { tile: peerId, phase: 'pre-start', ok: true } : { tile: peerId, phase: 'pre-start', ok: false, error: settleStop.error }
    )
    await this.pmdCommand(peerId, controlPointCharacteristic, buildStartEcgCommand())
    this.emit('tile-ecg-started', { tile: peerId, sampleRateHz: H10_ECG_SAMPLE_RATE_HZ })
    return { controlPointSubscription, dataSubscription, controlPoint: controlPointCharacteristic, ecgStarted: true }
  }

  /** Writes a PMD control-point request and resolves with its response; a non-SUCCESS status throws. */
  private async pmdCommand(peerId: string, controlPoint: GattCharacteristic, bytes: Uint8Array): Promise<ControlPointResponse> {
    const runtime = this.runtimes.get(peerId)
    if (runtime === undefined || runtime.controlPoint !== controlPoint) {
      throw new ScenarioError('pmd.not-ready', `live-dashboard tile ${peerId} has no PMD control point`)
    }
    const opCode = byteAt(bytes, 0)
    let settle: ((response: ControlPointResponse) => void) | null = null
    const response = new Promise<ControlPointResponse>(resolve => {
      settle = resolve
    })
    const waiter: PmdWaiter = { opCode, resolve: answer => settle?.(answer) }
    runtime.ecgWaiters.push(waiter)
    try {
      const receipt = await controlPoint.write(bytes, { response: 'required', timeoutMs: OPERATION_TIMEOUT_MS })
      this.emit('tile-pmd-write', { tile: peerId, bytes: bytesToHex(bytes), receipt: toJsonValue(receipt) })
    } catch (error) {
      runtime.ecgWaiters = runtime.ecgWaiters.filter(entry => entry !== waiter)
      throw error
    }
    const timeout = new Promise<null>(resolve => {
      const cancel = this.runtime.schedule(() => resolve(null), CONTROL_POINT_TIMEOUT_MS)
      void response.then(() => cancel())
    })
    const settled = await Promise.race([response, timeout])
    if (settled === null) {
      runtime.ecgWaiters = runtime.ecgWaiters.filter(entry => entry !== waiter)
      throw new ScenarioError('pmd.control-point-timeout', `no PMD response to op 0x${opCode.toString(16)} within ${CONTROL_POINT_TIMEOUT_MS.toString()} ms`)
    }
    if (settled.statusName !== 'SUCCESS') {
      throw new ScenarioError('pmd.request-rejected', `PMD op 0x${opCode.toString(16)} answered ${settled.statusName} (${settled.status.toString()})`)
    }
    return settled
  }

  private onControlPoint(peerId: string, value: GattValueEvent): void {
    let message: ControlPointMessage
    try {
      message = parseControlPointMessage(value.value)
    } catch (error) {
      this.emit('tile-pmd-control-point-unparsed', { tile: peerId, bytes: bytesToHex(value.value), error: describeError(error) })
      return
    }
    const line =
      message.kind === 'response'
        ? `op 0x${message.opCode.toString(16)} ${message.statusName}${message.more ? ' (more)' : ''} params=${bytesToHex(message.parameters)}`
        : `device stopped measurement(s) ${message.measurementTypes.join(',')}`
    this.emit('tile-pmd-control-point', { tile: peerId, bytes: bytesToHex(value.value), message: line })
    if (message.kind !== 'response') return
    const runtime = this.runtimes.get(peerId)
    const waiter = runtime?.ecgWaiters.find(entry => entry.opCode === message.opCode)
    if (waiter === undefined) {
      this.emit('tile-pmd-control-point-unsolicited', { tile: peerId, message: line })
      return
    }
    if (runtime !== undefined) runtime.ecgWaiters = runtime.ecgWaiters.filter(entry => entry !== waiter)
    waiter.resolve(message)
  }

  private onEcgFrame(peerId: string, value: GattValueEvent): void {
    try {
      const frame = parseEcgFrame(value.value)
      this.pushEcgSamples(peerId, frame.samplesMicroVolts)
    } catch (error) {
      this.emit('tile-ecg-parse-failed', { tile: peerId, sequence: value.sequence, length: value.value.length, error: describeError(error) })
      const tile = this.snapshot().tiles[peerId]
      if (tile !== undefined) this.patchTile(peerId, { parseFailures: tile.parseFailures + 1 })
    }
  }

  /** Appends parsed ECG samples to the tile ring buffer and refreshes the downsampled display window. */
  private pushEcgSamples(peerId: string, samples: readonly number[]): void {
    const runtime = this.runtimes.get(peerId)
    const tile = this.snapshot().tiles[peerId]
    if (runtime === undefined || tile === undefined || samples.length === 0) return
    runtime.ecgBuffer.push(...samples)
    if (runtime.ecgBuffer.length > ECG_BUFFER_CAP_SAMPLES) {
      runtime.ecgBuffer.splice(0, runtime.ecgBuffer.length - ECG_BUFFER_CAP_SAMPLES)
    }
    const newest = samples[samples.length - 1]
    this.patchTile(peerId, {
      ecgSamples: tile.ecgSamples + samples.length,
      ecgBuffered: runtime.ecgBuffer.length,
      ecgDisplay: downsampleEcg(runtime.ecgBuffer, ECG_WINDOW_SAMPLES, ECG_DISPLAY_MAX_POINTS),
      lastSampleMicroVolts: newest ?? tile.lastSampleMicroVolts
    })
  }

  private recordHeartRate(peerId: string, value: GattValueEvent, generation: string): void {
    const tile = this.snapshot().tiles[peerId]
    if (tile === undefined) return
    const valueCount = tile.valueCount + 1
    try {
      const measurement = parseHeartRateMeasurement(value.value)
      const rrIntervalsMs = measurement.rrIntervalsSeconds.map(seconds => Math.round(seconds * 1000))
      this.emit('tile-value', {
        tile: peerId,
        sequence: value.sequence,
        bpm: measurement.beatsPerMinute,
        rrIntervalsMs,
        contact: measurement.contact,
        delivery: value.delivery,
        connectionGeneration: generation
      })
      this.patchTile(peerId, {
        bpm: measurement.beatsPerMinute,
        contact: measurement.contact,
        rrIntervalsMs,
        valueCount,
        lastSeenAtMs: this.runtime.now()
      })
    } catch (error) {
      this.emit('tile-parse-failed', { tile: peerId, sequence: value.sequence, bytes: bytesToHex(value.value), error: describeError(error) })
      this.patchTile(peerId, { valueCount, parseFailures: tile.parseFailures + 1 })
    }
  }

  private recordBattery(peerId: string, value: GattValueEvent): void {
    const tile = this.snapshot().tiles[peerId]
    if (tile === undefined) return
    try {
      const percent = parseBatteryLevel(value.value)
      this.emit('tile-battery', { tile: peerId, sequence: value.sequence, batteryPercent: percent, delivery: value.delivery })
      this.patchTile(peerId, { batteryPercent: percent })
    } catch (error) {
      this.emit('tile-parse-failed', { tile: peerId, sequence: value.sequence, bytes: bytesToHex(value.value), error: describeError(error) })
      this.patchTile(peerId, { parseFailures: tile.parseFailures + 1 })
    }
  }

  /** Re-reads battery on a timer where the library refused notifications; cancelled in `disposeTileSession`. */
  private pollBattery(peerId: string, characteristic: GattCharacteristic): () => void {
    let cancelled = false
    let cancelTimer: (() => void) | null = null
    const tick = (): void => {
      cancelTimer = null
      if (cancelled) return
      void outcomeOf(async () => parseBatteryLevel(await characteristic.read({ timeoutMs: OPERATION_TIMEOUT_MS }))).then(outcome => {
        if (cancelled) return
        if (outcome.ok) this.patchTile(peerId, { batteryPercent: outcome.value })
        else this.emit('tile-battery-read-failed', { tile: peerId, error: outcome.error })
        if (!cancelled) cancelTimer = this.runtime.schedule(tick, BATTERY_POLL_MS)
      })
    }
    cancelTimer = this.runtime.schedule(tick, BATTERY_POLL_MS)
    return () => {
      cancelled = true
      cancelTimer?.()
    }
  }

  private async disposeTileSession(peerId: string, session: TileSession): Promise<void> {
    const runtime = this.runtimes.get(peerId)
    if (runtime !== undefined) {
      runtime.cancelBatteryPoll?.()
      runtime.cancelBatteryPoll = null
      runtime.controlPoint = null
      runtime.ecgWaiters = []
    }
    if (session.controlPoint !== null && session.ecgStarted) {
      const stop = await outcomeOf(
        async () => session.controlPoint?.write(buildStopEcgCommand(), { response: 'required', timeoutMs: OPERATION_TIMEOUT_MS })
      )
      this.emit(
        'tile-pmd-stop',
        stop.ok ? { tile: peerId, phase: 'dispose', ok: true } : { tile: peerId, phase: 'dispose', ok: false, error: stop.error }
      )
    }
    const removals: { readonly step: string; readonly subscription: GattSubscription }[] = [
      { step: `dashboard.hr.remove ${peerId}`, subscription: session.hrSubscription },
      ...(session.batterySubscription === null
        ? []
        : [{ step: `dashboard.battery.remove ${peerId}`, subscription: session.batterySubscription }]),
      ...(session.controlPointSubscription === null
        ? []
        : [{ step: `dashboard.pmd-control-point.remove ${peerId}`, subscription: session.controlPointSubscription }]),
      ...(session.dataSubscription === null ? [] : [{ step: `dashboard.pmd-data.remove ${peerId}`, subscription: session.dataSubscription }])
    ]
    for (const { step, subscription } of removals) {
      const outcome = await outcomeOf(() => subscription.remove())
      this.emit(
        'tile-cleanup',
        outcome.ok
          ? { tile: peerId, step, state: outcome.value.state }
          : { tile: peerId, step, state: 'threw', error: outcome.error }
      )
    }
  }
}

function initialTile(peerId: string, name: string | null, rssi: number | null, atMs: number): LiveDashboardTile {
  return {
    peerId,
    name,
    status: 'discovered',
    supervisorState: null,
    supervisorAttempt: 0,
    lifecycleCause: null,
    lifecycle: [],
    connectionGeneration: null,
    rssi,
    lastSeenAtMs: atMs,
    bpm: null,
    contact: null,
    rrIntervalsMs: [],
    valueCount: 0,
    batteryPercent: null,
    batteryDelivery: null,
    firmwareRevision: null,
    modelNumber: null,
    serialNumber: null,
    manufacturerName: null,
    infoErrors: {},
    ecgSamples: 0,
    ecgBuffered: 0,
    ecgDisplay: [],
    lastSampleMicroVolts: null,
    parseFailures: 0,
    error: null
  }
}
