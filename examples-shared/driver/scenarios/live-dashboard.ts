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
import { BleError, createConnectionSupervisor } from 'unified-ble-manager'
import {
  BATTERY_LEVEL_CHARACTERISTIC,
  BATTERY_SERVICE,
  parseBatteryLevel
} from 'unified-ble-manager/profiles/battery-service'
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
import { PmdRecorder } from '../pmd-recording.ts'
import type { DriverError, JsonObject } from '../protocol.ts'
import { bytesToHex, describeError, isJsonObject, toJsonValue } from '../protocol.ts'
import {
  H10_ECG_SAMPLE_RATE_HZ,
  PMD_CONTROL_POINT,
  PMD_DATA,
  PMD_SERVICE,
  POLAR_PREFERRED_MTU,
  PmdControlPointResponseAssembler,
  buildGetEcgSettingsCommand,
  buildStartEcgCommand,
  buildStopEcgCommand,
  buildGetAccSettingsCommand,
  buildStartAccCommand,
  buildStopAccCommand,
  parseAccFrame,
  type AccSample,
  type H10AccSettings,
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
  USER_GESTURE_TIMEOUT_MS,
  appendRecent,
  deviceQuery,
  deviceChooser,
  withTimeout,
  matchesDevice,
  matchesDeviceName,
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

export type LiveDashboardTileStatus =
  | 'discovered'
  | 'connecting'
  | 'streaming'
  | 'reconnecting'
  | 'lost'
  | 'failed'
  | 'off'

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
  readonly accSamples: number
  readonly accBuffered: number
  readonly accDisplay: readonly { readonly x: number; readonly y: number; readonly z: number }[]
  readonly lastAccMilliG: { readonly x: number; readonly y: number; readonly z: number } | null
  readonly accSettings: {
    readonly sampleRateHz: H10AccSettings['sampleRateHz']
    readonly resolutionBits: 16
    readonly rangeG: H10AccSettings['rangeG']
  } | null
  readonly accError: DriverError | null
  readonly pmdDroppedItems: number
  readonly pmdDroppedBytes: number
  readonly pmdReplacedItems: number
  readonly parseFailures: number
  readonly error: DriverError | null
}

export type LiveDashboardState = BleScenarioState & {
  readonly devices: readonly string[] | 'all-polar'
  readonly ecgEnabled: boolean
  readonly accEnabled: boolean
  readonly recording: JsonObject
  readonly tiles: { readonly [peerId: string]: LiveDashboardTile }
  readonly tileOrder: readonly string[]
}

export interface LiveDashboardStartOptions {
  readonly devices: readonly string[] | 'all-polar'
  readonly ecg: boolean
  readonly acc: boolean
  readonly accSampleRateHz: H10AccSettings['sampleRateHz']
  readonly accRangeG: H10AccSettings['rangeG']
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
  const rate = raw['accSampleRateHz'] ?? 200
  const range = raw['accRangeG'] ?? 8
  if (rate !== 25 && rate !== 50 && rate !== 100 && rate !== 200)
    throw new ScenarioError('scenario.invalid-argument', 'accSampleRateHz must be 25, 50, 100 or 200')
  if (range !== 2 && range !== 4 && range !== 8)
    throw new ScenarioError('scenario.invalid-argument', 'accRangeG must be 2, 4 or 8')
  return {
    devices: parseDevicesArgument(raw),
    ecg: args.boolean(raw, 'ecg', true),
    acc: args.boolean(raw, 'acc', false),
    accSampleRateHz: rate,
    accRangeG: range
  }
}

const DASHBOARD_SELECTORS: readonly DeviceSelector[] = [DEFAULT_DEVICE, { match: 'prefix', name: 'SIM Polar H10' }]

function selectorsFor(devices: readonly string[] | 'all-polar'): readonly DeviceSelector[] {
  if (devices === 'all-polar') return DASHBOARD_SELECTORS
  return devices.map(name => ({ match: 'exact', name }) satisfies DeviceSelector)
}

/** The scan form of the dashboard request: every named strap, or the Polar H10 prefix. */
export function dashboardQuery(devices: readonly string[] | 'all-polar'): ScanQuery {
  return { anyOf: selectorsFor(devices).flatMap(selector => deviceQuery(selector).anyOf ?? []) }
}

/** Newest `windowSamples` decimated so the first point is the window start and the last is the newest sample. */
function downsample<Value>(buffer: readonly Value[], windowSamples: number, maxPoints: number): readonly Value[] {
  if (buffer.length === 0 || maxPoints <= 0) return []
  const window = buffer.slice(Math.max(0, buffer.length - windowSamples))
  if (window.length <= maxPoints) return [...window]
  if (maxPoints === 1) {
    const newest = window[window.length - 1]
    return newest === undefined ? [] : [newest]
  }
  const points: Value[] = []
  for (let index = 0; index < maxPoints; index += 1) {
    const sample = window[Math.floor((index * (window.length - 1)) / (maxPoints - 1))]
    if (sample !== undefined) points.push(sample)
  }
  return points
}

export function downsampleEcg(buffer: readonly number[], windowSamples: number, maxPoints: number): readonly number[] {
  return downsample(buffer, windowSamples, maxPoints)
}

type ControlPointResponse = Extract<ControlPointMessage, { kind: 'response' }>
type PmdWaiter = {
  readonly opCode: number
  readonly measurementType: number
  readonly generation: number
  readonly assembler: PmdControlPointResponseAssembler
  readonly resolve: (response: ControlPointResponse) => void
  readonly fail: (error: Error) => void
  readonly cancel: () => void
}

/** One tile's live session: what `configure` built and `disposeSession` tears down. */
interface TileSession {
  readonly hrSubscription: GattSubscription
  readonly batterySubscription: GattSubscription | null
  readonly controlPointSubscription: GattSubscription | null
  readonly dataSubscription: GattSubscription | null
  readonly controlPoint: GattCharacteristic | null
  readonly ecgStarted: boolean
  readonly attempted: readonly number[]
  readonly generation: number
}

/** One tile's non-serializable runtime: supervisor, ECG ring buffer, PMD waiters, poll timer. */
interface TileRuntime {
  readonly peer: BlePeer
  supervisor: ConnectionSupervisor<TileSession> | null
  everStreamed: boolean
  ecgBuffer: number[]
  accBuffer: AccSample[]
  pmdGeneration: number
  closing: boolean
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
  accEnabled: false,
  recording: new PmdRecorder().summary(),
  tiles: {},
  tileOrder: []
}

export class LiveDashboardScenario extends BleScenario<LiveDashboardState> {
  readonly id = 'live-dashboard'
  readonly title = 'Live dashboard: Polar H10 tiles'
  readonly description =
    'One tile per Polar H10: heart rate, ECG and optional ACC XYZ, battery and Device Information. Bounded raw PMD recording exports exact sensor timestamps and explicit loss. Tiles reconnect through createConnectionSupervisor.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: defineCommand({
      label: 'Start dashboard',
      description:
        'Scan for Polar H10 straps. args: {devices?: "all-polar" | string[], ecg?: boolean (true), acc?: boolean (false), accSampleRateHz?: 25|50|100|200 (200), accRangeG?: 2|4|8 (8)}. ACC is fixed 16-bit milli-g XYZ.',
      presets: [
        { label: 'Start dashboard (ECG)', args: {} },
        { label: 'Start ECG + ACC (200 Hz, ±8 g)', args: { acc: true } },
        { label: 'Start dashboard, HR only', args: { ecg: false } }
      ],
      acceptsDevice: false,
      parse: parseLiveDashboardStartOptions,
      run: options => this.start(options)
    }),
    stop: defineCommand({
      label: 'Stop',
      description: 'Release the dashboard and seal its recording.',
      parse: args.none,
      run: async () => ({ cleanup: (await this.stop()).cleanup })
    }),
    'record-start': defineCommand({
      label: 'Start PMD recording',
      description:
        'Start bounded raw ECG/ACC capture. args: {label?: string (120 characters), notes?: string (2000 characters)}. Clear the previous recording first.',
      parse: raw => {
        const label = args.optionalString(raw, 'label')
        const notes = args.optionalString(raw, 'notes')
        if ((label?.length ?? 0) > 120 || (notes?.length ?? 0) > 2000)
          throw new ScenarioError(
            'scenario.invalid-argument',
            'recording label exceeds 120 characters or notes exceeds 2000 characters'
          )
        return { label, notes }
      },
      run: async metadata => {
        this.recorder.start(
          {
            ...metadata,
            host: toJsonValue(this.host.identity),
            optionsAtRecordingStart: toJsonValue(this.activeOptions),
            wallClockStartedAt: new Date().toISOString(),
            peersAtRecordingStart: this.activeOptions === null ? {} : toJsonValue(this.snapshot().tiles)
          },
          this.runtime.now()
        )
        this.patch({ recording: this.recorder.summary() })
        return this.recorder.summary()
      }
    }),
    'record-stop': defineCommand({
      label: 'Stop recording',
      description: 'Seal recording without stopping live streams.',
      parse: args.none,
      run: async () => {
        this.recorder.stop(this.runtime.now())
        this.patch({ recording: this.recorder.summary() })
        return this.recorder.summary()
      }
    }),
    'record-clear': defineCommand({
      label: 'Clear recording',
      description: 'Explicitly discard the retained recording.',
      parse: args.none,
      run: async () => {
        this.recorder.clear()
        this.patch({ recording: this.recorder.summary() })
        return this.recorder.summary()
      }
    }),
    'record-export': defineCommand({
      label: 'Export PMD JSON',
      description: 'Return the sealed comparison capture; snapshots and event history contain only its summary.',
      parse: args.none,
      run: async () => this.recorder.export(),
      summarizeResult: () => this.recorder.summary()
    }),
    snapshot: defineCommand({
      label: 'Snapshot',
      description: 'Report the current tiles without changing anything.',
      parse: args.none,
      run: async () => this.snapshot()
    })
  }

  private readonly runtimes = new Map<string, TileRuntime>()
  private readonly recorder = new PmdRecorder()
  private activeOptions: LiveDashboardStartOptions | null = null

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
    for (const runtime of this.runtimes.values()) {
      runtime.closing = true
      for (const waiter of runtime.ecgWaiters) waiter.cancel()
    }
    const outcome = await super.stop()
    this.activeOptions = null
    this.recorder.stop(this.runtime.now())
    this.patch({ recording: this.recorder.summary() })
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
      this.activeOptions = options
      this.patch({
        devices: options.devices,
        ecgEnabled: options.ecg,
        accEnabled: options.acc,
        recording: this.recorder.summary()
      })
      this.patchBase({ phase: 'preparing' })
      const hosted = await this.createManager(signal)
      const manager = hosted.manager
      const acquisition = peerAcquisition(manager)
      this.emit('peer-acquisition', { via: acquisition, devices: toJsonValue(options.devices) })
      if (acquisition === 'choose') {
        this.patchBase({ phase: 'choosing' })
        if (options.devices === 'all-polar') {
          const peer = await this.chooseDashboardPeer(manager, signal)
          this.observePeer(manager, peer, peer.name, peer.rssi, options, signal)
        } else {
          for (const selector of selectorsFor(options.devices)) {
            const peer = await this.chooseH10(manager, selector, signal)
            this.observePeer(manager, peer, peer.name, peer.rssi, options, signal)
          }
        }
        this.patchBase({ phase: 'streaming' })
      } else {
        // Already-connected peers may have stopped advertising. Resolve real
        // directory peers before scanning; do not synthesize scan observations.
        const selectors = selectorsFor(options.devices)
        const directoryDeadline = this.runtime.now() + OPERATION_TIMEOUT_MS
        const remainingDirectoryBudget = () => {
          if (signal.aborted)
            throw new BleError('operation.aborted', 'connection', 'live-dashboard.connected-directory')
          // Public timeouts are whole milliseconds. Rounding down preserves
          // the original deadline; a sub-millisecond remainder cannot be sent.
          const remaining = Math.floor(directoryDeadline - this.runtime.now())
          if (remaining <= 0)
            throw new BleError('operation.timed-out', 'connection', 'live-dashboard.connected-directory')
          return remaining
        }
        const queryConnected = async () => {
          let peers: readonly BlePeer[]
          try {
            peers = await manager.peers.connected({ signal, timeoutMs: remainingDirectoryBudget() })
          } catch (error) {
            const diagnostic = describeError(error)
            if (
              diagnostic.code !== 'capability.unsupported' ||
              !isJsonObject(diagnostic.detail) ||
              diagnostic.detail.operation !== 'peers.connected.services-required'
            )
              throw error
            remainingDirectoryBudget()
            this.emit('connected-directory-query-limited', { error: toJsonValue(diagnostic), services: ['180d'] })
            peers = await manager.peers.connected({ signal, timeoutMs: remainingDirectoryBudget(), services: ['180d'] })
          }
          remainingDirectoryBudget()
          return peers
        }
        const connected = await withTimeout(
          Promise.resolve().then(queryConnected),
          OPERATION_TIMEOUT_MS,
          signal,
          'operation.timed-out',
          'connected peer directory did not settle'
        ).catch(error => {
          const diagnostic = describeError(error)
          if (diagnostic.code !== 'capability.unsupported') throw error
          this.emit('connected-directory-unavailable', { error: toJsonValue(diagnostic) })
          return []
        })
        if (signal.aborted)
          throw new ScenarioError('operation.aborted', 'dashboard stopped during connected peer lookup')
        for (const peer of connected) {
          if (selectors.some(selector => matchesDeviceName(peer.name, selector)))
            this.observePeer(manager, peer, peer.name, peer.rssi, options, signal)
        }
        this.emit('connected-directory', { peers: connected.length })
        const query = dashboardQuery(options.devices)
        this.patchBase({ phase: 'scanning' })
        const session = await manager.scan({ query, duplicates: 'all', delivery: 'balanced', signal })
        this.own('scan.stop', () => session.stop())
        this.emit('scan-started', { query: toJsonValue(query) })
        void this.watchObservations(manager, session.observations, selectors, options, signal)
        if (session.events !== undefined) void this.watchDiscoveryEvents(session.events)
      }
      const snapshot = this.snapshot()
      return {
        devices: snapshot.devices,
        ecg: snapshot.ecgEnabled,
        acc: snapshot.accEnabled,
        tiles: snapshot.tileOrder
      }
    })
  }

  /** One user gesture/chooser contains both real straps and explicit simulators. */
  private async chooseDashboardPeer(manager: BleManager, signal: AbortSignal): Promise<BlePeer> {
    const gate = this.host.userGesture
    if (gate !== null) {
      const reason = 'Choose a real Polar H10 or SIM Polar H10 for the dashboard'
      this.patchBase({ phase: 'awaiting-user-gesture' })
      this.emit('user-gesture-required', { reason, timeoutMs: USER_GESTURE_TIMEOUT_MS })
      await withTimeout(
        gate.request(this.id, reason, signal),
        USER_GESTURE_TIMEOUT_MS,
        signal,
        'host.user-gesture-timeout',
        'no user gesture'
      )
    }
    this.patchBase({ phase: 'choosing' })
    const peer = await manager.choose({
      ...deviceChooser(DEFAULT_DEVICE),
      filters: DASHBOARD_SELECTORS.flatMap(selector => deviceChooser(selector).filters ?? []),
      signal,
      timeoutMs: USER_GESTURE_TIMEOUT_MS
    })
    if (!DASHBOARD_SELECTORS.some(selector => (peer.name ?? '').startsWith(selector.name)))
      throw new ScenarioError('scenario.device-mismatch', 'chooser did not return a real or simulated Polar H10')
    return peer
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
      accBuffer: [],
      pmdGeneration: 0,
      closing: false,
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
        this.observePeer(
          manager,
          observation.peer,
          observation.localName ?? observation.peer.name,
          observation.rssi,
          options,
          signal
        )
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
          if (tile !== undefined)
            this.patchTile(event.peer.id, { lastSeenAtMs: this.runtime.now(), rssi: event.peer.rssi })
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

  private startTileSupervisor(
    manager: BleManager,
    peer: BlePeer,
    options: LiveDashboardStartOptions,
    signal: AbortSignal
  ): void {
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
        error: null,
        supervisorState: event.state,
        supervisorAttempt: event.attempt,
        connectionGeneration: event.connectionGeneration ?? this.snapshot().tiles[peerId]?.connectionGeneration ?? null
      })
    } else if (event.state === 'stopped') {
      this.patchTile(peerId, {
        status: event.error === null ? 'off' : 'failed',
        error: this.snapshot().tiles[peerId]?.error ?? (event.error === null ? null : describeError(event.error)),
        supervisorState: event.state,
        supervisorAttempt: event.attempt
      })
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
  private async configureTile(
    peerId: string,
    connection: BleConnection,
    options: LiveDashboardStartOptions
  ): Promise<TileSession> {
    const runtime = this.runtimes.get(peerId)
    if (runtime === undefined) {
      throw new ScenarioError('scenario.not-running', `live-dashboard tile ${peerId} is gone; refusing configure`)
    }
    const generation = connection.connectionGeneration
    const pmdGeneration = ++runtime.pmdGeneration
    const attempted: number[] = []
    this.patchTile(peerId, { connectionGeneration: generation })
    this.recordPmd('generation', peerId, {
      connectionGeneration: generation,
      pmdGeneration,
      peer: toJsonValue(runtime.peer),
      options: toJsonValue(options)
    })
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
        lifecycle: appendRecent(
          tile.lifecycle,
          `#${event.sequence.toString()} ${event.previous} -> ${event.current} (${String(event.cause)})`
        )
      })
      this.recordPmd('generation', peerId, {
        state: event.current,
        cause: String(event.cause),
        connectionGeneration: event.connectionGeneration
      })
    })
    const created: GattSubscription[] = []
    const unwind = async (): Promise<void> => {
      await this.stopPmdStreams(peerId, runtime.controlPoint, attempted, 'configure-unwind')
      runtime.cancelBatteryPoll?.()
      runtime.cancelBatteryPoll = null
      runtime.controlPoint = null
      runtime.ecgWaiters = []
      for (const subscription of created.reverse()) {
        const outcome = await outcomeOf(() => subscription.remove())
        this.emit(
          'tile-cleanup',
          outcome.ok
            ? {
                tile: peerId,
                step: 'configure-unwind',
                state: outcome.value.state,
                receipt: toJsonValue(outcome.value)
              }
            : { tile: peerId, step: 'configure-unwind', state: 'threw', error: outcome.error }
        )
        if (!outcome.ok || outcome.value.state !== 'released')
          this.recordPmd('error', peerId, {
            stage: 'configure-unwind',
            outcome: outcome.ok ? toJsonValue(outcome.value) : outcome.error
          })
      }
    }
    try {
      const gatt = await connection.discover({ timeoutMs: OPERATION_TIMEOUT_MS })
      this.emit('tile-discovered', {
        tile: peerId,
        generation: gatt.generation,
        services: gatt.services.map(service => service.uuid)
      })
      const mtu = await outcomeOf(() =>
        connection.controls.requestMtu(POLAR_PREFERRED_MTU, { timeoutMs: OPERATION_TIMEOUT_MS })
      )
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
      const initialBattery = await outcomeOf(async () =>
        parseBatteryLevel(await batteryCharacteristic.read({ timeoutMs: OPERATION_TIMEOUT_MS }))
      )
      if (initialBattery.ok) this.patchTile(peerId, { batteryPercent: initialBattery.value })
      else this.emit('tile-battery-read-failed', { tile: peerId, error: initialBattery.error })
      let batterySubscription: GattSubscription | null = null
      try {
        const subscription = await batteryCharacteristic.subscribe({
          timeoutMs: OPERATION_TIMEOUT_MS,
          stream: 'balanced'
        })
        batterySubscription = subscription
        created.push(subscription)
        this.patchTile(peerId, { batteryDelivery: subscription.effectiveDelivery })
        this.emit('tile-subscribed', {
          tile: peerId,
          characteristic: 'battery',
          requestedDelivery: subscription.requestedDelivery ?? null,
          effectiveDelivery: subscription.effectiveDelivery
        })
        void this.consume(`dashboard.battery ${peerId}`, subscription.values, {
          value: value => this.recordBattery(peerId, value)
        })
      } catch (error) {
        const described = describeError(error)
        this.emit('tile-battery-poll', { tile: peerId, reason: described.code, error: described })
        this.patchTile(peerId, { batteryDelivery: 'poll' })
        runtime.cancelBatteryPoll = this.pollBattery(peerId, batteryCharacteristic)
      }

      for (const spec of INFO_READS) {
        const outcome = await outcomeOf(async () =>
          decodeDeviceInformationString(
            await gatt
              .characteristic(DEVICE_INFORMATION_SERVICE, spec.characteristic)
              .read({ timeoutMs: OPERATION_TIMEOUT_MS })
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
      this.recordPmd('generation', peerId, {
        pmdGeneration,
        device: toJsonValue(this.snapshot().tiles[peerId]),
        options: toJsonValue(options)
      })

      let controlPointSubscription: GattSubscription | null = null
      let dataSubscription: GattSubscription | null = null
      let controlPoint: GattCharacteristic | null = null
      let ecgStarted = false
      if (options.ecg || options.acc) {
        const outcome = await this.configurePmd(peerId, runtime, gatt, created, options, pmdGeneration, attempted)
        controlPointSubscription = outcome.controlPointSubscription
        dataSubscription = outcome.dataSubscription
        controlPoint = outcome.controlPoint
        ecgStarted = outcome.ecgStarted
      }
      return {
        hrSubscription,
        batterySubscription,
        controlPointSubscription,
        dataSubscription,
        controlPoint,
        ecgStarted,
        attempted,
        generation: pmdGeneration
      }
    } catch (error) {
      this.patchTile(peerId, { error: describeError(error) })
      await unwind()
      this.emit('tile-configure-failed', {
        tile: peerId,
        connectionGeneration: generation,
        error: describeError(error)
      })
      this.recordPmd('error', peerId, { stage: 'configure', error: describeError(error) })
      throw error
    }
  }

  private async configurePmd(
    peerId: string,
    runtime: TileRuntime,
    gatt: GattDatabase,
    created: GattSubscription[],
    options: LiveDashboardStartOptions,
    generation: number,
    attempted: number[]
  ): Promise<{
    readonly controlPointSubscription: GattSubscription | null
    readonly dataSubscription: GattSubscription | null
    readonly controlPoint: GattCharacteristic | null
    readonly ecgStarted: boolean
  }> {
    if (runtime.closing) throw new BleError('operation.aborted', 'gatt', 'live-dashboard.pmd.configure')
    const controlPointCharacteristic = gatt.characteristic(PMD_SERVICE, PMD_CONTROL_POINT)
    runtime.controlPoint = controlPointCharacteristic
    this.patchTile(peerId, {
      accSettings: options.acc
        ? { sampleRateHz: options.accSampleRateHz, resolutionBits: 16, rangeG: options.accRangeG }
        : null,
      accError: null
    })
    // CoreBluetooth reports reads and indications through the same callback.
    // Read/parse features before subscribing; never filter arbitrary callbacks
    // by their leading byte once the control-point stream is active.
    const featureBytes = await outcomeOf(async () =>
      controlPointCharacteristic.read({ timeoutMs: OPERATION_TIMEOUT_MS })
    )
    if (!featureBytes.ok) {
      this.emit('tile-pmd-features', { tile: peerId, ok: false, error: featureBytes.error })
      this.recordPmd('error', peerId, { stage: 'features-read', error: featureBytes.error })
      if (options.acc) this.patchTile(peerId, { accError: featureBytes.error })
      return {
        controlPointSubscription: null,
        dataSubscription: null,
        controlPoint: controlPointCharacteristic,
        ecgStarted: false
      }
    }
    const features = await outcomeOf(async () => parsePmdFeatures(featureBytes.value))
    if (!features.ok) {
      this.emit('tile-pmd-features', { tile: peerId, ok: false, error: features.error })
      this.recordPmd('error', peerId, { stage: 'features-parse', error: features.error })
      if (options.acc) this.patchTile(peerId, { accError: features.error })
      return {
        controlPointSubscription: null,
        dataSubscription: null,
        controlPoint: controlPointCharacteristic,
        ecgStarted: false
      }
    }
    this.emit('tile-pmd-features', { tile: peerId, ok: true, ...features.value, raw: bytesToHex(featureBytes.value) })
    this.recordPmd('control-response', peerId, {
      stage: 'features-read',
      bytesHex: bytesToHex(featureBytes.value),
      features: toJsonValue(features.value)
    })
    if (runtime.closing) throw new BleError('operation.aborted', 'gatt', 'live-dashboard.pmd.feature-read')
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
    void this.consumePmd(peerId, generation, 'control-point', controlPointSubscription.values, value =>
      this.onControlPoint(peerId, value, generation)
    )
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
    void this.consumePmd(peerId, generation, 'data', dataSubscription.values, value =>
      this.onPmdFrame(peerId, value, generation)
    )
    runtime.controlPoint = controlPointCharacteristic

    for (const measurement of [0, 2]) {
      if (measurement === 0 ? !options.ecg : !options.acc) continue
      const name = measurement === 0 ? 'ecg' : 'acc'
      if (measurement === 0 ? !features.value.ecg : !features.value.acc) {
        const error = describeError(new ScenarioError('capability.unsupported', `peer does not advertise PMD ${name}`))
        this.emit(`tile-${name}-unsupported`, { tile: peerId, error, features: toJsonValue(features.value) })
        this.recordPmd('error', peerId, { stage: 'feature-unsupported', measurementType: measurement, error })
        if (measurement === 2) this.patchTile(peerId, { accError: error })
        continue
      }
      const settings = await outcomeOf(async () =>
        parsePmdSettings(
          (
            await this.pmdCommand(
              peerId,
              controlPointCharacteristic,
              measurement === 0 ? buildGetEcgSettingsCommand() : buildGetAccSettingsCommand()
            )
          ).parameters
        )
      )
      this.emit(
        'tile-pmd-settings',
        settings.ok
          ? { tile: peerId, measurementType: measurement, ok: true, settings: toJsonValue(settings.value) }
          : { tile: peerId, measurementType: measurement, ok: false, error: settings.error }
      )
      if (runtime.closing) {
        throw new BleError('operation.aborted', 'gatt', 'live-dashboard.pmd.configure')
      }
      const settleStop = await outcomeOf(() =>
        this.pmdCommand(
          peerId,
          controlPointCharacteristic,
          measurement === 0 ? buildStopEcgCommand() : buildStopAccCommand()
        )
      )
      this.emit(
        'tile-pmd-stop',
        settleStop.ok
          ? { tile: peerId, measurementType: measurement, phase: 'pre-start', ok: true }
          : { tile: peerId, measurementType: measurement, phase: 'pre-start', ok: false, error: settleStop.error }
      )
      attempted.push(measurement)
      try {
        await this.pmdCommand(
          peerId,
          controlPointCharacteristic,
          measurement === 0
            ? buildStartEcgCommand()
            : buildStartAccCommand({
                sampleRateHz: options.accSampleRateHz,
                resolutionBits: 16,
                rangeG: options.accRangeG
              })
        )
      } catch (error) {
        if (measurement === 2) this.patchTile(peerId, { accError: describeError(error) })
        throw error
      }
      this.emit(`tile-${name}-started`, {
        tile: peerId,
        sampleRateHz: measurement === 0 ? H10_ECG_SAMPLE_RATE_HZ : options.accSampleRateHz
      })
    }
    return {
      controlPointSubscription,
      dataSubscription,
      controlPoint: controlPointCharacteristic,
      ecgStarted: attempted.includes(0)
    }
  }

  /** Both PMD channels retain their own loss/error observations for comparison. */
  private async consumePmd(
    peerId: string,
    generation: number,
    channel: string,
    stream: PublicBoundedAsyncStream<GattValueEvent>,
    value: (event: GattValueEvent) => void
  ): Promise<void> {
    const name = `dashboard.pmd-${channel} ${peerId}`
    try {
      for await (const item of stream) {
        if (item.kind === 'value') {
          value(item.value)
          continue
        }
        this.emit(item.kind === 'overflow' ? 'stream-overflow' : 'stream-terminal', {
          stream: name,
          notice: toJsonValue(item),
          droppedItems: item.droppedItems,
          droppedBytes: item.droppedBytes,
          replacedItems: item.replacedItems,
          ...(item.kind === 'terminal' ? { reason: item.reason } : { policy: item.policy })
        })
        if (this.runtimes.get(peerId)?.pmdGeneration !== generation) continue
        const lost = item.droppedItems > 0 || item.droppedBytes > 0 || item.replacedItems > 0
        const error = item.kind === 'terminal' && item.error !== undefined ? describeError(item.error) : null
        this.recordPmd(error !== null ? 'error' : lost ? 'loss' : 'generation', peerId, {
          stream: name,
          notice: toJsonValue(item),
          pmdGeneration: generation
        })
        const tile = this.snapshot().tiles[peerId]
        if (item.kind === 'overflow' && tile !== undefined)
          this.patchTile(peerId, {
            pmdDroppedItems: tile.pmdDroppedItems + item.droppedItems,
            pmdDroppedBytes: tile.pmdDroppedBytes + item.droppedBytes,
            pmdReplacedItems: tile.pmdReplacedItems + item.replacedItems
          })
        if (error !== null)
          this.patchTile(peerId, {
            error,
            ...(tile !== undefined && tile.accSettings !== null ? { accError: error } : {})
          })
      }
      this.emit('stream-ended', { stream: name })
    } catch (thrown) {
      const error = describeError(thrown)
      this.emit('stream-threw', { stream: name, error })
      this.recordPmd('error', peerId, { stream: name, error, pmdGeneration: generation })
      if (this.runtimes.get(peerId)?.pmdGeneration === generation) {
        const tile = this.snapshot().tiles[peerId]
        this.patchTile(peerId, {
          error,
          ...(tile !== undefined && tile.accSettings !== null ? { accError: error } : {})
        })
      }
    }
  }

  /** Writes a PMD control-point request and resolves with its response; a non-SUCCESS status throws. */
  private async pmdCommand(
    peerId: string,
    controlPoint: GattCharacteristic,
    bytes: Uint8Array
  ): Promise<ControlPointResponse> {
    const runtime = this.runtimes.get(peerId)
    if (runtime === undefined || runtime.closing || runtime.controlPoint !== controlPoint) {
      throw new ScenarioError('pmd.not-ready', `live-dashboard tile ${peerId} has no PMD control point`)
    }
    const opCode = byteAt(bytes, 0)
    const measurementType = byteAt(bytes, 1)
    let settle: ((response: ControlPointResponse | Error | null) => void) | null = null
    let cancelled = false
    const response = new Promise<ControlPointResponse | Error | null>(resolve => {
      settle = resolve
    })
    const waiter: PmdWaiter = {
      opCode,
      measurementType,
      generation: runtime.pmdGeneration,
      assembler: new PmdControlPointResponseAssembler(opCode, measurementType, runtime.pmdGeneration),
      resolve: answer => settle?.(answer),
      fail: error => settle?.(error),
      cancel: () => {
        cancelled = true
        settle?.(null)
      }
    }
    runtime.ecgWaiters.push(waiter)
    this.recordPmd('control-command', peerId, {
      bytesHex: bytesToHex(bytes),
      opCode,
      measurementType,
      pmdGeneration: waiter.generation
    })
    try {
      const receipt = await controlPoint.write(bytes, { response: 'required', timeoutMs: OPERATION_TIMEOUT_MS })
      this.emit('tile-pmd-write', { tile: peerId, bytes: bytesToHex(bytes), receipt: toJsonValue(receipt) })
    } catch (error) {
      runtime.ecgWaiters = runtime.ecgWaiters.filter(entry => entry !== waiter)
      this.recordPmd('error', peerId, { stage: 'control-write', opCode, measurementType, error: describeError(error) })
      throw error
    }
    const timeout = new Promise<null>(resolve => {
      const cancel = this.runtime.schedule(() => resolve(null), CONTROL_POINT_TIMEOUT_MS)
      void response.then(() => cancel())
    })
    const settled = await Promise.race([response, timeout])
    if (settled instanceof Error) throw settled
    if (settled === null) {
      runtime.ecgWaiters = runtime.ecgWaiters.filter(entry => entry !== waiter)
      if (cancelled) throw new BleError('operation.aborted', 'gatt', 'live-dashboard.pmd.response')
      throw new ScenarioError(
        'pmd.control-point-timeout',
        `no PMD response to op 0x${opCode.toString(16)} within ${CONTROL_POINT_TIMEOUT_MS.toString()} ms`
      )
    }
    if (settled.statusName !== 'SUCCESS') {
      throw new ScenarioError(
        'pmd.request-rejected',
        `PMD op 0x${opCode.toString(16)} answered ${settled.statusName} (${settled.status.toString()})`
      )
    }
    return settled
  }

  private onControlPoint(peerId: string, value: GattValueEvent, generation: number): void {
    this.recordPmd('control-response', peerId, {
      bytesHex: bytesToHex(value.value),
      sequence: value.sequence,
      pmdGeneration: generation,
      stale: this.runtimes.get(peerId)?.pmdGeneration !== generation
    })
    if (this.runtimes.get(peerId)?.pmdGeneration !== generation) {
      this.emit('tile-pmd-stale', { tile: peerId, stream: 'control-point', generation })
      return
    }
    let message: ControlPointMessage
    try {
      message = parseControlPointMessage(value.value)
    } catch (error) {
      this.emit('tile-pmd-control-point-unparsed', {
        tile: peerId,
        bytes: bytesToHex(value.value),
        error: describeError(error)
      })
      this.recordPmd('error', peerId, {
        stage: 'control-parse',
        bytesHex: bytesToHex(value.value),
        error: describeError(error)
      })
      // A truncated response can still identify its command. Unidentifiable
      // noise remains diagnostic and cannot complete any pending request.
      const runtime = this.runtimes.get(peerId)
      const waiter =
        value.value[0] === 0xf0
          ? runtime?.ecgWaiters.find(
              entry =>
                entry.opCode === value.value[1] &&
                entry.measurementType === value.value[2] &&
                entry.generation === generation
            )
          : undefined
      if (waiter !== undefined && runtime !== undefined) {
        runtime.ecgWaiters = runtime.ecgWaiters.filter(entry => entry !== waiter)
        waiter.fail(error instanceof Error ? error : new Error(String(error)))
      }
      return
    }
    const line =
      message.kind === 'response'
        ? `op 0x${message.opCode.toString(16)} ${message.statusName}${message.more ? ' (more)' : ''} params=${bytesToHex(message.parameters)}`
        : `device stopped measurement(s) ${message.measurementTypes.join(',')}`
    this.emit('tile-pmd-control-point', { tile: peerId, bytes: bytesToHex(value.value), message: line })
    if (message.kind !== 'response') return
    const runtime = this.runtimes.get(peerId)
    const waiter = runtime?.ecgWaiters.find(
      entry =>
        entry.opCode === message.opCode &&
        entry.measurementType === message.measurementType &&
        entry.generation === generation
    )
    if (waiter === undefined) {
      this.emit('tile-pmd-control-point-unsolicited', { tile: peerId, message: line })
      return
    }
    try {
      const response = waiter.assembler.push(message, generation)
      if (response === null) return
      if (runtime !== undefined) runtime.ecgWaiters = runtime.ecgWaiters.filter(entry => entry !== waiter)
      waiter.resolve(response)
    } catch (error) {
      if (runtime !== undefined) runtime.ecgWaiters = runtime.ecgWaiters.filter(entry => entry !== waiter)
      const failure = error instanceof Error ? error : new Error(String(error))
      this.recordPmd('error', peerId, { stage: 'control-assembly', error: describeError(failure) })
      waiter.fail(failure)
    }
  }

  private onPmdFrame(peerId: string, value: GattValueEvent, generation: number): void {
    const runtime = this.runtimes.get(peerId)
    if (runtime?.pmdGeneration !== generation) {
      this.emit('tile-pmd-stale', { tile: peerId, stream: 'data', generation })
      return
    }
    const measurementType = value.value[0]
    const packet: JsonObject = {
      bytesHex: bytesToHex(value.value),
      measurement: measurementType ?? null,
      sequence: value.sequence,
      receivedAtMonotonicMs: value.observedAtMonotonicMs,
      pmdGeneration: generation,
      settings:
        measurementType === 2
          ? toJsonValue(this.snapshot().tiles[peerId]?.accSettings ?? null)
          : { sampleRateHz: H10_ECG_SAMPLE_RATE_HZ, resolutionBits: 14 }
    }
    try {
      if (measurementType === 0) {
        const frame = parseEcgFrame(value.value)
        this.recordPmd('packet', peerId, {
          ...packet,
          sensorTimestampNs: frame.timestampNs.toString(),
          samples: frame.samplesMicroVolts.length
        })
        this.pushEcgSamples(peerId, frame.samplesMicroVolts)
      } else if (measurementType === 2) {
        const frame = parseAccFrame(value.value)
        this.recordPmd('packet', peerId, {
          ...packet,
          sensorTimestampNs: frame.timestampNs.toString(),
          samples: frame.samplesMilliG.length
        })
        const tile = this.snapshot().tiles[peerId]
        if (tile === undefined) return
        runtime.accBuffer.push(...frame.samplesMilliG)
        const rate = tile.accSettings?.sampleRateHz ?? 200
        if (runtime.accBuffer.length > rate * 10) runtime.accBuffer.splice(0, runtime.accBuffer.length - rate * 10)
        this.patchTile(peerId, {
          accSamples: tile.accSamples + frame.samplesMilliG.length,
          accBuffered: runtime.accBuffer.length,
          accDisplay: downsample(runtime.accBuffer, rate * 5, ECG_DISPLAY_MAX_POINTS),
          lastAccMilliG: frame.samplesMilliG[frame.samplesMilliG.length - 1] ?? tile.lastAccMilliG
        })
      } else {
        throw new ScenarioError(
          'pmd.unexpected-measurement',
          `dashboard received PMD measurement ${String(measurementType)}`
        )
      }
    } catch (error) {
      const described = describeError(error)
      this.recordPmd('packet', peerId, { ...packet, parseError: described })
      this.recordPmd('error', peerId, { stage: 'data-parse', measurement: measurementType ?? null, error: described })
      this.emit(measurementType === 2 ? 'tile-acc-parse-failed' : 'tile-ecg-parse-failed', {
        tile: peerId,
        sequence: value.sequence,
        length: value.value.length,
        error: described
      })
      const tile = this.snapshot().tiles[peerId]
      if (tile !== undefined)
        this.patchTile(peerId, {
          parseFailures: tile.parseFailures + 1,
          ...(measurementType === 2 ? { accError: described } : {})
        })
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
      this.emit('tile-parse-failed', {
        tile: peerId,
        sequence: value.sequence,
        bytes: bytesToHex(value.value),
        error: describeError(error)
      })
      this.patchTile(peerId, { valueCount, parseFailures: tile.parseFailures + 1 })
    }
  }

  private recordBattery(peerId: string, value: GattValueEvent): void {
    const tile = this.snapshot().tiles[peerId]
    if (tile === undefined) return
    try {
      const percent = parseBatteryLevel(value.value)
      this.emit('tile-battery', {
        tile: peerId,
        sequence: value.sequence,
        batteryPercent: percent,
        delivery: value.delivery
      })
      this.patchTile(peerId, { batteryPercent: percent })
    } catch (error) {
      this.emit('tile-parse-failed', {
        tile: peerId,
        sequence: value.sequence,
        bytes: bytesToHex(value.value),
        error: describeError(error)
      })
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
      void outcomeOf(async () =>
        parseBatteryLevel(await characteristic.read({ timeoutMs: OPERATION_TIMEOUT_MS }))
      ).then(outcome => {
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
    if (runtime !== undefined && runtime.pmdGeneration === session.generation) {
      runtime.cancelBatteryPoll?.()
      runtime.cancelBatteryPoll = null
      runtime.controlPoint = null
      runtime.ecgWaiters = []
    }
    await this.stopPmdStreams(peerId, session.controlPoint, session.attempted, 'dispose')
    const removals: { readonly step: string; readonly subscription: GattSubscription }[] = [
      { step: `dashboard.hr.remove ${peerId}`, subscription: session.hrSubscription },
      ...(session.batterySubscription === null
        ? []
        : [{ step: `dashboard.battery.remove ${peerId}`, subscription: session.batterySubscription }]),
      ...(session.controlPointSubscription === null
        ? []
        : [{ step: `dashboard.pmd-control-point.remove ${peerId}`, subscription: session.controlPointSubscription }]),
      ...(session.dataSubscription === null
        ? []
        : [{ step: `dashboard.pmd-data.remove ${peerId}`, subscription: session.dataSubscription }])
    ]
    for (const { step, subscription } of removals) {
      const outcome = await outcomeOf(() => subscription.remove())
      this.emit(
        'tile-cleanup',
        outcome.ok
          ? { tile: peerId, step, state: outcome.value.state, receipt: toJsonValue(outcome.value) }
          : { tile: peerId, step, state: 'threw', error: outcome.error }
      )
      if (!outcome.ok || outcome.value.state !== 'released')
        this.recordPmd('error', peerId, {
          stage: 'remove',
          step,
          outcome: outcome.ok ? toJsonValue(outcome.value) : outcome.error
        })
    }
  }

  private async stopPmdStreams(
    peerId: string,
    controlPoint: GattCharacteristic | null,
    attempted: readonly number[],
    phase: string
  ): Promise<void> {
    if (controlPoint === null) return
    for (const measurementType of attempted) {
      const bytes = measurementType === 0 ? buildStopEcgCommand() : buildStopAccCommand()
      this.recordPmd('control-command', peerId, { bytesHex: bytesToHex(bytes), measurementType, phase })
      const stop = await outcomeOf(() =>
        controlPoint.write(bytes, { response: 'required', timeoutMs: OPERATION_TIMEOUT_MS })
      )
      this.emit(
        'tile-pmd-stop',
        stop.ok
          ? { tile: peerId, measurementType, phase, ok: true, receipt: toJsonValue(stop.value) }
          : { tile: peerId, measurementType, phase, ok: false, error: stop.error }
      )
      if (!stop.ok) this.recordPmd('error', peerId, { stage: 'stop', measurementType, phase, error: stop.error })
    }
  }

  private recordPmd(kind: Parameters<PmdRecorder['append']>[0]['kind'], peerId: string, data: JsonObject): void {
    this.recorder.append({
      kind,
      peerId,
      generation: this.snapshot().tiles[peerId]?.connectionGeneration ?? null,
      atMs: this.runtime.now(),
      data
    })
    this.patch({ recording: this.recorder.summary() })
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
    accSamples: 0,
    accBuffered: 0,
    accDisplay: [],
    lastAccMilliG: null,
    accSettings: null,
    accError: null,
    pmdDroppedItems: 0,
    pmdDroppedBytes: 0,
    pmdReplacedItems: 0,
    parseFailures: 0,
    error: null
  }
}
