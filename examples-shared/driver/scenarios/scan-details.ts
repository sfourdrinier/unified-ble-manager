// examples-shared/driver/scenarios/scan-details.ts
//
// Scans with and without a query and with either duplicates policy, dumping
// every field the public observation carries (first sighting per peer) and
// counting observations per second. The public normalized observation has no
// TX power field; `txPowerSeen` reports whether any observation carried one
// anyway, rather than assuming either way.

import type { DiscoveryEvent, PublicScanObservation, ScanQuery, ScanSession } from 'unified-ble-manager'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import type { DriverHost } from '../host.ts'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { describeError, toJsonValue } from '../protocol.ts'
import { args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import { BleScenario, IDLE_BLE_STATE, POLAR_H10_QUERY, appendRecent, type BleScenarioState } from './ble-scenario.ts'

const FILTERS = ['none', 'h10', 'heart-rate-service'] as const
type ScanFilter = (typeof FILTERS)[number]

const QUERIES: Readonly<Record<ScanFilter, ScanQuery | undefined>> = {
  none: undefined,
  h10: POLAR_H10_QUERY,
  'heart-rate-service': { anyOf: [{ services: { any: [HEART_RATE_SERVICE] } }] }
}

export type PeerRow = {
  readonly id: string
  readonly name: string | null
  readonly rssi: number | null
  readonly observations: number
  readonly connectable: boolean | null
  readonly serviceUuids: readonly string[]
  readonly manufacturerCompanyIds: readonly number[]
  readonly firstSeenAtMs: number
}

export type ScanDetailsState = BleScenarioState & {
  readonly filter: ScanFilter | null
  readonly duplicates: 'coalesced' | 'all' | null
  readonly durationMs: number | null
  readonly planDigest: string | null
  readonly scanState: string | null
  readonly observations: number
  readonly uniquePeers: number
  readonly observationsLastSecond: number
  readonly perSecond: readonly number[]
  readonly txPowerSeen: boolean
  readonly peers: readonly PeerRow[]
}

const IDLE_SCAN_STATE: ScanDetailsState = {
  ...IDLE_BLE_STATE,
  filter: null,
  duplicates: null,
  durationMs: null,
  planDigest: null,
  scanState: null,
  observations: 0,
  uniquePeers: 0,
  observationsLastSecond: 0,
  perSecond: [],
  txPowerSeen: false,
  peers: []
}

const PEER_TABLE_LIMIT = 30

export class ScanDetailsScenario extends BleScenario<ScanDetailsState> {
  readonly id = 'scan-details'
  readonly title = 'Scan details'
  readonly description = 'Scan for durationMs with a filter (none | h10 | heart-rate-service) and duplicates policy; dumps advertisement fields and counts per second.'
  private peers = new Map<string, PeerRow>()
  private bucketStartMs = 0
  private bucketCount = 0
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    scan: defineCommand({
      label: 'Scan',
      description: `args: {filter?: ${FILTERS.join(' | ')}, duplicates?: "coalesced" | "all", durationMs?: number (default 10000)}`,
      presets: [
        { label: 'Unfiltered, all duplicates, 10 s', args: { filter: 'none', duplicates: 'all' } },
        { label: 'Unfiltered, coalesced, 10 s', args: { filter: 'none', duplicates: 'coalesced' } },
        { label: 'H10 filter, all duplicates, 10 s', args: { filter: 'h10', duplicates: 'all' } }
      ],
      parse: raw => ({
        filter: args.oneOf(raw, 'filter', FILTERS, 'none'),
        duplicates: args.oneOf(raw, 'duplicates', ['coalesced', 'all'], 'all'),
        durationMs: args.number(raw, 'durationMs', 10_000, { min: 500, max: 120_000 })
      }),
      run: options => this.scan(options)
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, IDLE_SCAN_STATE)
  }

  override headline(): string | null {
    const { observations, uniquePeers, observationsLastSecond, phase } = this.snapshot()
    return observations === 0 ? phase : `${observations.toString()} obs · ${uniquePeers.toString()} peers · ${observationsLastSecond.toString()}/s`
  }

  private patchScan(patch: Partial<ScanDetailsState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  private scan(options: { filter: ScanFilter; duplicates: 'coalesced' | 'all'; durationMs: number }): Promise<JsonObject> {
    return this.runJourney(async signal => {
      this.peers = new Map()
      this.patchScan({ filter: options.filter, duplicates: options.duplicates, durationMs: options.durationMs })
      const { manager } = await this.createManager(signal)
      this.patchBase({ phase: 'scanning' })
      const session = await manager.scan({ query: QUERIES[options.filter], duplicates: options.duplicates, delivery: 'balanced', signal })
      this.own('scan.stop', () => session.stop())
      this.patchScan({ planDigest: session.plan?.queryDigest ?? null })
      this.emit('scan-started', { plan: toJsonValue(session.plan), options: { ...options } })
      void this.watchScanState(session)
      if (session.events !== undefined) void this.watchDiscovery(session.events)
      const startedAt = this.runtime.now()
      this.bucketStartMs = startedAt
      this.bucketCount = 0
      const cancelTimer = this.runtime.schedule(() => {
        void session.stop().then(
          record => this.emit('scan-duration-elapsed', { stop: toJsonValue(record) }),
          error => this.emit('scan-duration-elapsed', { stopError: describeError(error) })
        )
      }, options.durationMs)
      await this.consume('scan.observations', session.observations, { value: observation => this.record(observation) })
      cancelTimer()
      this.closeBucket(this.runtime.now())
      const summary = this.summary(this.runtime.now() - startedAt)
      await this.teardown('done')
      return summary
    })
  }

  private record(observation: PublicScanObservation): void {
    const now = this.runtime.now()
    while (now - this.bucketStartMs >= 1_000) this.closeBucket(this.bucketStartMs + 1_000)
    this.bucketCount += 1
    const state = this.snapshot()
    const existing = this.peers.get(observation.peer.id)
    const txPowerPresent = 'txPowerLevel' in observation || 'txPower' in observation
    const row: PeerRow = {
      id: observation.peer.id,
      name: observation.localName ?? observation.peer.name,
      rssi: observation.rssi,
      observations: (existing?.observations ?? 0) + 1,
      connectable: observation.connectable,
      serviceUuids: observation.serviceUuids ?? [],
      manufacturerCompanyIds: (observation.manufacturerData ?? []).map(entry => entry.companyId),
      firstSeenAtMs: existing?.firstSeenAtMs ?? now
    }
    this.peers.set(row.id, row)
    if (existing === undefined) this.emit('peer', { advertisement: dumpObservation(observation) })
    this.patchScan({
      observations: state.observations + 1,
      uniquePeers: this.peers.size,
      txPowerSeen: state.txPowerSeen || txPowerPresent,
      peers: [...this.peers.values()].sort((a, b) => (b.rssi ?? -999) - (a.rssi ?? -999)).slice(0, PEER_TABLE_LIMIT)
    })
  }

  private closeBucket(endMs: number): void {
    this.emit('rate', { windowEndMs: endMs, observations: this.bucketCount, uniquePeers: this.peers.size })
    this.patchScan({ observationsLastSecond: this.bucketCount, perSecond: appendRecent(this.snapshot().perSecond, this.bucketCount) })
    this.bucketStartMs = endMs
    this.bucketCount = 0
  }

  private summary(elapsedMs: number): JsonObject {
    const state = this.snapshot()
    return {
      filter: state.filter,
      duplicates: state.duplicates,
      elapsedMs,
      observations: state.observations,
      uniquePeers: state.uniquePeers,
      observationsPerSecond: elapsedMs > 0 ? (state.observations * 1000) / elapsedMs : null,
      perSecond: state.perSecond,
      txPowerSeen: state.txPowerSeen,
      planDigest: state.planDigest,
      peers: state.peers
    }
  }

  private async watchScanState(session: ScanSession): Promise<void> {
    try {
      for await (const event of session.state) {
        this.emit('scan-state', { state: event.state, reason: event.reason ?? null })
        this.patchScan({ scanState: event.reason === undefined ? event.state : `${event.state} (${event.reason})` })
      }
    } catch (error) {
      this.emit('stream-threw', { stream: 'scan.state', error: describeError(error) })
    }
  }

  private async watchDiscovery(events: AsyncIterable<DiscoveryEvent>): Promise<void> {
    try {
      for await (const event of events) this.emit('discovery', toJsonObjectOf(event))
    } catch (error) {
      this.emit('stream-threw', { stream: 'scan.events', error: describeError(error) })
    }
  }
}

/** Every field the observation carries, bytes as hex; the peer object is summarized. */
function dumpObservation(observation: PublicScanObservation): JsonObject {
  const { peer, ...fields } = observation
  return {
    ...toJsonObjectOf(fields),
    peer: { id: peer.id, name: peer.name, rssi: peer.rssi, hasReference: peer.reference !== null, sources: toJsonValue(peer.sources) }
  }
}

function toJsonObjectOf(value: object): JsonObject {
  const out: Record<string, JsonValue> = {}
  for (const [key, field] of Object.entries(value)) out[key] = toJsonValue(field)
  return out
}
