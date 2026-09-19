// examples-shared/driver/scenarios/h10-capture.ts
//
// H10 fidelity fingerprint capture (Round H10). On a given `device` it records
// a versioned JSON fingerprint through the public unified-ble-manager API
// only: advertisement fields + advertising interval + RSSI stats, the full
// GATT database, every readable value, timing distributions, behaviour probes
// and host metadata. The owner runs this against real straps; the same
// scenario run against `tool/h10-sim` produces the simulator fingerprint that
// `h10-sim --compare` checks for equivalence.

import type { BleConnection, BleManager, BlePeer, GattDatabase, PublicScanObservation } from 'unified-ble-manager'
import { BATTERY_LEVEL_CHARACTERISTIC, BATTERY_SERVICE } from 'unified-ble-manager/profiles/battery-service'
import {
  DEVICE_INFORMATION_SERVICE,
  FIRMWARE_REVISION_CHARACTERISTIC,
  HARDWARE_REVISION_CHARACTERISTIC,
  MANUFACTURER_NAME_CHARACTERISTIC,
  MODEL_NUMBER_CHARACTERISTIC,
  PNP_ID_CHARACTERISTIC,
  SERIAL_NUMBER_CHARACTERISTIC,
  SOFTWARE_REVISION_CHARACTERISTIC,
  SYSTEM_ID_CHARACTERISTIC
} from 'unified-ble-manager/profiles/device-information'
import { BODY_SENSOR_LOCATION_CHARACTERISTIC, HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import type { DriverHost } from '../host.ts'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { bytesToHex, toJsonValue } from '../protocol.ts'
import { ScenarioError, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import {
  BleScenario,
  DEVICE_ARGUMENT_HELP,
  IDLE_BLE_STATE,
  OPERATION_TIMEOUT_MS,
  matchesDevice,
  outcomeOf,
  parseDevice,
  type BleScenarioState,
  type DeviceSelector
} from './ble-scenario.ts'
import {
  H10_ECG_SAMPLE_RATE_HZ,
  PMD_CONTROL_POINT,
  PMD_DATA,
  PMD_SERVICE,
  buildGetEcgSettingsCommand,
  buildStartEcgCommand,
  buildStopEcgCommand,
  parseControlPointMessage,
  parseEcgFrame,
  parsePmdFeatures,
  parsePmdSettings
} from '../polar-pmd.ts'

/** Fingerprint schema version. Bump when a field is added, removed or renamed. */
export const FINGERPRINT_VERSION = 1

export interface TimingDistribution {
  readonly n: number
  readonly min: number | null
  readonly p10: number | null
  readonly p50: number | null
  readonly p90: number | null
  readonly max: number | null
  readonly mean: number | null
  readonly stdev: number | null
  readonly [key: string]: number | null
}

/** Summarizes samples into a timing distribution. Empty input reports n: 0 with nulls, never NaN. */
export function summarizeDistribution(samples: readonly number[]): TimingDistribution {
  if (samples.length === 0) {
    return { n: 0, min: null, p10: null, p50: null, p90: null, max: null, mean: null, stdev: null }
  }
  const sorted = [...samples].sort((a, b) => a - b)
  const first = sorted[0] ?? 0
  const last = sorted[sorted.length - 1] ?? 0
  const quantile = (q: number): number => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))] ?? 0
  const mean = samples.reduce((total, value) => total + value, 0) / samples.length
  const variance = samples.reduce((total, value) => total + (value - mean) * (value - mean), 0) / samples.length
  return {
    n: samples.length,
    min: first,
    p10: quantile(0.1),
    p50: quantile(0.5),
    p90: quantile(0.9),
    max: last,
    mean,
    stdev: Math.sqrt(variance)
  }
}

export type H10CaptureState = BleScenarioState & {
  readonly fingerprint: JsonObject | null
}

export interface H10CaptureOptions {
  readonly device: DeviceSelector
  readonly scanDurationMs: number
  readonly hrDurationMs: number
  readonly hrMinValues: number
  readonly ecgFrames: number
  readonly mtu: number
  /** Keep every raw HR measurement and ECG frame (for simulator replay). */
  readonly recordRaw: boolean
}

export interface RawHrRecord {
  readonly atMs: number
  readonly hex: string
}

export interface RawEcgRecord {
  readonly timestampNs: string
  readonly hex: string
  readonly samplesMicroVolts: readonly number[]
}

function booleanOption(raw: JsonObject, key: string, fallback: boolean): boolean {
  const value = raw[key]
  if (value === undefined) return fallback
  if (typeof value !== 'boolean') {
    throw new ScenarioError('scenario.invalid-argument', `argument "${key}" must be a boolean; received ${JSON.stringify(value)}`)
  }
  return value
}

function numberOption(raw: JsonObject, key: string, fallback: number, min: number, max: number): number {
  const value = raw[key]
  if (value === undefined) return fallback
  if (typeof value !== 'number' || !Number.isFinite(value) || value < min || value > max) {
    throw new ScenarioError('scenario.invalid-argument', `argument "${key}" must be a number ${min}..${max}; received ${JSON.stringify(value)}`)
  }
  return value
}

const READ_SPECS: readonly { field: string; service: string; characteristic: string }[] = [
  { field: 'batteryLevelPercent', service: BATTERY_SERVICE, characteristic: BATTERY_LEVEL_CHARACTERISTIC },
  { field: 'manufacturerName', service: DEVICE_INFORMATION_SERVICE, characteristic: MANUFACTURER_NAME_CHARACTERISTIC },
  { field: 'modelNumber', service: DEVICE_INFORMATION_SERVICE, characteristic: MODEL_NUMBER_CHARACTERISTIC },
  { field: 'serialNumber', service: DEVICE_INFORMATION_SERVICE, characteristic: SERIAL_NUMBER_CHARACTERISTIC },
  { field: 'hardwareRevision', service: DEVICE_INFORMATION_SERVICE, characteristic: HARDWARE_REVISION_CHARACTERISTIC },
  { field: 'firmwareRevision', service: DEVICE_INFORMATION_SERVICE, characteristic: FIRMWARE_REVISION_CHARACTERISTIC },
  { field: 'softwareRevision', service: DEVICE_INFORMATION_SERVICE, characteristic: SOFTWARE_REVISION_CHARACTERISTIC },
  { field: 'systemId', service: DEVICE_INFORMATION_SERVICE, characteristic: SYSTEM_ID_CHARACTERISTIC },
  { field: 'bodySensorLocation', service: HEART_RATE_SERVICE, characteristic: BODY_SENSOR_LOCATION_CHARACTERISTIC },
  { field: 'pnpId', service: DEVICE_INFORMATION_SERVICE, characteristic: PNP_ID_CHARACTERISTIC }
]

const HR_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'

export class H10CaptureScenario extends BleScenario<H10CaptureState> {
  readonly id = 'h10-capture'
  readonly title = 'H10 fidelity fingerprint capture'
  readonly description =
    'Records a versioned JSON fingerprint of the strap: advertisement, GATT database, readable values, timing distributions and behaviour probes.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    capture: defineCommand({
      label: 'Capture fingerprint',
      description: `args: {${DEVICE_ARGUMENT_HELP}, scanDurationMs?: number, hrDurationMs?: number (>= 60000 on real straps), hrMinValues?: number, ecgFrames?: number, mtu?: number, recordRaw?: boolean}. Result: the versioned fingerprint (with a raw section when recordRaw is true).`,
      presets: [{ label: 'Capture (defaults)', args: {} }],
      acceptsDevice: true,
      parse: raw => ({
        device: parseDevice(raw),
        scanDurationMs: numberOption(raw, 'scanDurationMs', 10_000, 50, 120_000),
        hrDurationMs: numberOption(raw, 'hrDurationMs', 60_000, 50, 300_000),
        hrMinValues: numberOption(raw, 'hrMinValues', 50, 1, 10_000),
        ecgFrames: numberOption(raw, 'ecgFrames', 30, 0, 1_000),
        mtu: numberOption(raw, 'mtu', 517, 23, 517),
        recordRaw: booleanOption(raw, 'recordRaw', false)
      }),
      run: options => this.capture(options)
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, { ...IDLE_BLE_STATE, fingerprint: null })
  }

  private async capture(options: H10CaptureOptions): Promise<JsonObject> {
    return this.runJourney(async signal => {
      this.patchBase({ phase: 'capturing' })
      const { manager } = await this.createManager(signal)
      const hostMeta = {
        host: this.host.identity.host,
        platform: this.host.identity.platform,
        backend: this.host.identity.backend,
        model: this.host.identity.model,
        osVersion: this.host.identity.osVersion
      }

      // 1. Advertisement: scan with duplicates on, every field the host exposes.
      this.patchBase({ phase: 'scanning' })
      const advertisement = await this.captureAdvertisement(manager, options.device, options.scanDurationMs, signal)

      // 2. Connect + discovery latencies, read from the platform's own answers.
      // Peer acquisition follows the shared path (scan vs system chooser from
      // the backend capability report), identical to every other scenario.
      this.patchBase({ phase: 'connecting' })
      const peer: BlePeer = await this.findH10(manager, options.device, signal)
      const connectStarted = this.runtime.now()
      const connection: BleConnection = await this.connect(manager, peer, signal)
      this.own('connection.release', () => connection.release())
      const connectMs = this.runtime.now() - connectStarted
      this.emit('capture-connect', { connectMs })
      this.patchBase({ device: peer.name ?? peer.id, peer: { id: peer.id, name: peer.name, query: options.device } })

      this.patchBase({ phase: 'discovering' })
      const discoverStarted = this.runtime.now()
      const gatt: GattDatabase = await connection.discover({ signal, timeoutMs: OPERATION_TIMEOUT_MS })
      const discoveryMs = this.runtime.now() - discoverStarted
      this.emit('capture-discover', { discoveryMs, generation: gatt.generation })

      // 3. GATT database: services, characteristics, properties, descriptors, occurrence order.
      const gattSection = dumpGattDatabase(gatt)

      // 4. Every readable value.
      this.patchBase({ phase: 'reading' })
      const values: Record<string, JsonValue> = {}
      for (const spec of READ_SPECS) {
        const outcome = await outcomeOf(async () => {
          const bytes = await gatt.characteristic(spec.service, spec.characteristic).read({ signal, timeoutMs: OPERATION_TIMEOUT_MS })
          return { text: decodeText(bytes), raw: bytesToHex(bytes) }
        })
        const entry: JsonValue = outcome.ok ? { ok: true, ...outcome.value } : { ok: false, error: outcome.error }
        values[spec.field] = entry
        this.emit('capture-read', { field: spec.field, outcome: entry })
      }
      const pmdFeatures = await outcomeOf(async () => {
        const bytes = await gatt.characteristic(PMD_SERVICE, PMD_CONTROL_POINT).read({ signal, timeoutMs: OPERATION_TIMEOUT_MS })
        return { ...parsePmdFeatures(bytes), raw: bytesToHex(bytes) }
      })
      values.pmdFeatures = pmdFeatures.ok ? { ok: true, ...pmdFeatures.value } : { ok: false, error: pmdFeatures.error }

      // 5. MTU negotiation result (the platform's own answer).
      this.patchBase({ phase: 'negotiating-mtu' })
      const mtuStarted = this.runtime.now()
      const mtuNegotiation = await outcomeOf(() => connection.controls.requestMtu(options.mtu, { signal, timeoutMs: OPERATION_TIMEOUT_MS }))
      const mtuMs = this.runtime.now() - mtuStarted
      const mtuEffective = await outcomeOf(() => connection.controls.effectiveMtu())
      const mtuSection = {
        requested: options.mtu,
        negotiation: mtuNegotiation.ok ? toJsonValue(mtuNegotiation.value) : { error: mtuNegotiation.error },
        effective: mtuEffective.ok ? toJsonValue(mtuEffective.value) : { error: mtuEffective.error },
        negotiationMs: mtuMs
      }

      // 6. HR subscribe: time to first value, notification intervals.
      this.patchBase({ phase: 'streaming-hr' })
      const hr = await this.captureHeartRate(gatt, options.hrDurationMs, options.hrMinValues, options.recordRaw, signal)

      // 7. ECG: PMD command latencies, frame intervals, sample-count consistency.
      this.patchBase({ phase: 'streaming-ecg' })
      const ecg = await this.captureEcg(gatt, options.ecgFrames, options.recordRaw, signal)
      values.pmdSettingsEcg = ecg.pmdSettingsEcg
      this.emit('capture-ecg', { frames: ecg.samplesPerFrame.frames ?? null })

      // 8. Behaviour probes.
      this.patchBase({ phase: 'probing' })
      const behaviour = await this.captureBehaviour(gatt, signal)

      const fingerprint: JsonObject = {
        version: FINGERPRINT_VERSION,
        capturedAt: new Date().toISOString(),
        device: { query: toJsonValue(options.device), name: peer.name, id: peer.id },
        host: hostMeta,
        advertisement,
        gatt: gattSection,
        values,
        timings: {
          connectMs,
          discoveryMs,
          mtu: mtuSection,
          timeToFirstHrMs: hr.timeToFirstMs,
          hrNotificationIntervalMs: hr.intervals,
          pmdGetSettingsMs: ecg.getSettingsMs,
          pmdStartMs: ecg.startMs,
          pmdStopMs: ecg.stopMs,
          pmdResponseMs: ecg.pmdResponseMs,
          ecgFrameIntervalMs: ecg.frameIntervals,
          ecgSamplesPerFrame: ecg.samplesPerFrame
        },
        behaviour,
        ...(options.recordRaw ? { raw: { hrMeasurements: toJsonValue(hr.raw), ecgFrames: toJsonValue(ecg.raw) } } : {})
      }
      this.replace({ ...this.snapshot(), fingerprint })
      await this.teardown('done')
      return fingerprint
    })
  }

  private async captureAdvertisement(manager: BleManager, device: DeviceSelector, durationMs: number, signal: AbortSignal): Promise<JsonObject> {
    const observations: PublicScanObservation[] = []
    const atMs: number[] = []
    let firstDump: JsonObject | null = null
    let txPowerSeen = false
    try {
      const session = await manager.scan({ query: undefined, duplicates: 'all', delivery: 'balanced', signal })
      this.own('scan.stop', () => session.stop())
      const stopAt = this.runtime.now() + durationMs
      const stopTimer = this.runtime.schedule(() => {
        void session.stop().catch(() => {})
      }, durationMs)
      try {
        for await (const item of session.observations) {
          if (item.kind !== 'value') continue
          const observation = item.value
          if (!matchesDevice(observation, device)) continue
          observations.push(observation)
          atMs.push(this.runtime.now())
          if (firstDump === null) firstDump = dumpObservation(observation)
          if ('txPowerLevel' in observation || 'txPower' in observation) txPowerSeen = true
          if (this.runtime.now() >= stopAt) break
        }
      } finally {
        stopTimer()
        // Finding 209: one manager admits one physical scan, so whatever
        // ends the capture loop — the duration break, the stream ending, or
        // the timer above — stops this scan (awaiting the release) before
        // find() opens the next scan. Stop is idempotent, so an already-run
        // timer stop reports the same record; the ledger stop at teardown
        // then re-reports it. A stop failure here is the run's answer, not
        // a later scan.already-active from a scan left open.
        await session.stop()
      }
    } catch (error) {
      return { ok: false, error: toJsonValue(error), observations: 0 }
    }
    const gaps: number[] = []
    for (let index = 1; index < atMs.length; index += 1) {
      const current = atMs[index]
      const previous = atMs[index - 1]
      if (current !== undefined && previous !== undefined) gaps.push(current - previous)
    }
    const rssi = observations.map(entry => entry.rssi).filter((value): value is number => value !== null)
    const first = observations[0]
    return {
      ok: true,
      observations: observations.length,
      localName: first?.localName ?? null,
      connectable: first?.connectable ?? null,
      serviceUuids: first?.serviceUuids ?? null,
      manufacturerData: toJsonValue(first?.manufacturerData ?? null),
      serviceData: toJsonValue(first?.serviceData ?? null),
      firstObservation: firstDump,
      advertisementIntervalMs: summarizeDistribution(gaps),
      rssi: summarizeDistribution(rssi),
      txPowerSeen
    }
  }

  private async captureHeartRate(
    gatt: GattDatabase,
    durationMs: number,
    minValues: number,
    recordRaw: boolean,
    signal: AbortSignal
  ): Promise<{ timeToFirstMs: number | null; intervals: TimingDistribution; raw: readonly RawHrRecord[] }> {
    const subscribedAt = this.runtime.now()
    const subscription = await gatt.characteristic(HEART_RATE_SERVICE, HR_MEASUREMENT).subscribe({
      signal,
      timeoutMs: OPERATION_TIMEOUT_MS,
      stream: 'balanced'
    })
    this.own('hr-subscription.remove', () => subscription.remove())
    const stamps: number[] = []
    const raw: RawHrRecord[] = []
    let firstAt: number | null = null
    const deadline = this.runtime.now() + durationMs
    const done = new Promise<void>(resolve => {
      const cancel = this.runtime.schedule(() => resolve(), durationMs)
      void (async () => {
        try {
          for await (const item of subscription.values) {
            if (signal.aborted) break
            if (item.kind !== 'value') continue
            const at = this.runtime.now()
            if (firstAt === null) {
              firstAt = at
              this.emit('capture-first-hr', { afterMs: at - subscribedAt })
            }
            stamps.push(item.value.observedAtMonotonicMs)
            if (recordRaw) raw.push({ atMs: item.value.observedAtMonotonicMs, hex: bytesToHex(item.value.value) })
            this.emit('capture-hr', { sequence: item.value.sequence })
            if (stamps.length >= minValues && at >= deadline) break
          }
        } finally {
          cancel()
          resolve()
        }
      })()
    })
    await done
    // Let values already delivered before the deadline drain out of the
    // stream before summarizing; without this a timer firing on the same
    // clock tick as a delivery would drop that delivery from the window.
    await new Promise<void>(resolve => {
      setTimeout(resolve, 0)
    })
    const intervals: number[] = []
    for (let index = 1; index < stamps.length; index += 1) {
      const current = stamps[index]
      const previous = stamps[index - 1]
      if (current !== undefined && previous !== undefined) intervals.push(current - previous)
    }
    return { timeToFirstMs: firstAt === null ? null : firstAt - subscribedAt, intervals: summarizeDistribution(intervals), raw }
  }

  private async captureEcg(
    gatt: GattDatabase,
    frameBudget: number,
    recordRaw: boolean,
    signal: AbortSignal
  ): Promise<{
    raw: readonly RawEcgRecord[]
    getSettingsMs: number | null
    startMs: number | null
    stopMs: number | null
    pmdResponseMs: TimingDistribution
    frameIntervals: TimingDistribution
    samplesPerFrame: JsonObject
    pmdSettingsEcg: JsonValue
  }> {
    const controlPoint = gatt.characteristic(PMD_SERVICE, PMD_CONTROL_POINT)
    const data = gatt.characteristic(PMD_SERVICE, PMD_DATA)
    const cpSubscription = await controlPoint.subscribe({ signal, timeoutMs: OPERATION_TIMEOUT_MS, delivery: 'prefer-indication', stream: 'lossless-bounded' })
    this.own('pmd.control-point.remove', () => cpSubscription.remove())
    const pending: { resolve: (value: Uint8Array) => void }[] = []
    void (async () => {
      try {
        for await (const item of cpSubscription.values) {
          if (item.kind !== 'value') continue
          const waiter = pending.shift()
          if (waiter !== undefined) waiter.resolve(item.value.value)
        }
      } catch {
        // Stream end: waiters time out below; never swallowed silently (they throw).
      }
    })()
    const command = async (bytes: Uint8Array): Promise<{ ms: number; status: number; statusName: string; parameters: Uint8Array }> => {
      const started = this.runtime.now()
      const answer = new Promise<Uint8Array>(resolve => pending.push({ resolve }))
      await controlPoint.write(bytes, { response: 'required', timeoutMs: OPERATION_TIMEOUT_MS })
      const timeoutMs = 5_000
      const winner = await Promise.race([
        answer,
        new Promise<null>(resolve => {
          const cancel = this.runtime.schedule(() => resolve(null), timeoutMs)
          void answer.then(() => cancel())
        })
      ])
      if (winner === null) throw new ScenarioError('pmd.control-point-timeout', `no PMD response to op 0x${byteHex(bytes, 0)} within ${timeoutMs.toString()} ms`)
      const message = parseControlPointMessage(winner)
      if (message.kind !== 'response') throw new ScenarioError('pmd.unexpected-stop', 'PMD answered with device-stop instead of a response')
      return { ms: this.runtime.now() - started, status: message.status, statusName: message.statusName, parameters: message.parameters }
    }
    const dataSubscription = await data.subscribe({ signal, timeoutMs: OPERATION_TIMEOUT_MS, delivery: 'prefer-notification', stream: 'balanced' })
    this.own('pmd.data.remove', () => dataSubscription.remove())

    // Control-point read while notifying happens here; the probe section reuses the pattern.
    // get-settings is issued three times so the fingerprint carries a real
    // response-latency distribution, not a single sample.
    const responseSamples: number[] = []
    const getSettings = await outcomeOf(() => command(buildGetEcgSettingsCommand()))
    if (getSettings.ok) responseSamples.push(getSettings.value.ms)
    for (let repeat = 1; repeat < 3; repeat += 1) {
      const extra = await outcomeOf(() => command(buildGetEcgSettingsCommand()))
      if (extra.ok) responseSamples.push(extra.value.ms)
    }
    let pmdSettingsEcg: JsonValue
    if (getSettings.ok && getSettings.value.status === 0) {
      const settings = await outcomeOf(async () => toJsonValue(parsePmdSettings(getSettings.value.parameters)))
      pmdSettingsEcg = settings.ok ? { ok: true, value: settings.value } : { ok: false, error: settings.error }
    } else {
      pmdSettingsEcg = getSettings.ok
        ? { ok: false, error: { code: 'pmd.request-rejected', message: getSettings.value.statusName, detail: null } }
        : { ok: false, error: getSettings.error }
    }
    const start = await outcomeOf(() => command(buildStartEcgCommand()))

    const frameStampsNs: bigint[] = []
    const sampleCounts: number[] = []
    const raw: RawEcgRecord[] = []
    if (start.ok && start.value.status === 0 && frameBudget > 0) {
      const frames = new Promise<void>(resolve => {
        const cancel = this.runtime.schedule(() => resolve(), 30_000)
        void (async () => {
          try {
            for await (const item of dataSubscription.values) {
              if (item.kind !== 'value') continue
              try {
                const frame = parseEcgFrame(item.value.value)
                frameStampsNs.push(frame.timestampNs)
                sampleCounts.push(frame.samplesMicroVolts.length)
                if (recordRaw) {
                  raw.push({
                    timestampNs: frame.timestampNs.toString(),
                    hex: bytesToHex(item.value.value),
                    samplesMicroVolts: [...frame.samplesMicroVolts]
                  })
                }
              } catch {
                // Parse failures are counted in the ecg scenario; here a bad
                // frame is a sample-count of -1 so the fingerprint shows it.
                sampleCounts.push(-1)
              }
              if (frameStampsNs.length + sampleCounts.filter(count => count === -1).length >= frameBudget) break
            }
          } finally {
            cancel()
            resolve()
          }
        })()
      })
      await frames
    }
    const stop = await outcomeOf(() => command(buildStopEcgCommand()))

    const intervalsMs: number[] = []
    for (let index = 1; index < frameStampsNs.length; index += 1) {
      const current = frameStampsNs[index]
      const previous = frameStampsNs[index - 1]
      if (current !== undefined && previous !== undefined) intervalsMs.push(Number(current - previous) / 1_000_000)
    }
    const goodCounts = sampleCounts.filter(count => count >= 0)
    return {
      raw,
      getSettingsMs: getSettings.ok ? getSettings.value.ms : null,
      startMs: start.ok ? start.value.ms : null,
      stopMs: stop.ok ? stop.value.ms : null,
      pmdResponseMs: summarizeDistribution(responseSamples),
      frameIntervals: summarizeDistribution(intervalsMs),
      samplesPerFrame: {
        frames: sampleCounts.length,
        min: goodCounts.length === 0 ? null : Math.min(...goodCounts),
        max: goodCounts.length === 0 ? null : Math.max(...goodCounts),
        consistent: goodCounts.length > 0 && goodCounts.every(count => count === goodCounts[0]),
        expectedPerSecond: H10_ECG_SAMPLE_RATE_HZ
      },
      pmdSettingsEcg
    }
  }

  private async captureBehaviour(gatt: GattDatabase, signal: AbortSignal): Promise<JsonObject> {
    const controlPoint = gatt.characteristic(PMD_SERVICE, PMD_CONTROL_POINT)
    const subscription = await controlPoint.subscribe({ signal, timeoutMs: OPERATION_TIMEOUT_MS, delivery: 'prefer-indication', stream: 'lossless-bounded' })
    this.own('probe.control-point.remove', () => subscription.remove())
    const pending: { resolve: (value: Uint8Array) => void }[] = []
    void (async () => {
      try {
        for await (const item of subscription.values) {
          if (item.kind !== 'value') continue
          const waiter = pending.shift()
          if (waiter !== undefined) waiter.resolve(item.value.value)
        }
      } catch {
        // Waiters below time out with a loud error.
      }
    })()
    const rawCommand = async (bytes: Uint8Array, label: string): Promise<JsonObject> => {
      const started = this.runtime.now()
      const answer = new Promise<Uint8Array>(resolve => pending.push({ resolve }))
      const writeOutcome = await outcomeOf(() => controlPoint.write(bytes, { response: 'required', timeoutMs: OPERATION_TIMEOUT_MS }))
      if (!writeOutcome.ok) return { op: label, writeError: writeOutcome.error, ms: this.runtime.now() - started }
      const winner = await Promise.race([
        answer,
        new Promise<null>(resolve => {
          const cancel = this.runtime.schedule(() => resolve(null), 5_000)
          void answer.then(() => cancel())
        })
      ])
      if (winner === null) return { op: label, error: 'pmd.control-point-timeout', ms: this.runtime.now() - started }
      try {
        const message = parseControlPointMessage(winner)
        if (message.kind !== 'response') return { op: label, kind: message.kind, ms: this.runtime.now() - started }
        return { op: label, errorCode: message.status, errorName: message.statusName, ms: this.runtime.now() - started }
      } catch (error) {
        return { op: label, parseError: String(error), ms: this.runtime.now() - started }
      }
    }
    // Start without a fresh MTU negotiation on this connection: the run's
    // earlier MTU probe already ran, so this start deliberately skips it.
    const startWithoutMtu = await rawCommand(buildStartEcgCommand(), 'start-without-mtu')
    const repeatedStart = await rawCommand(buildStartEcgCommand(), 'repeated-start')
    const stopOnce = await rawCommand(buildStopEcgCommand(), 'stop')
    const stopWhenStopped = await rawCommand(buildStopEcgCommand(), 'stop-when-stopped')
    const invalid = await rawCommand(new Uint8Array([0xff, 0x00]), 'invalid-op')
    const readWhileNotifying = await outcomeOf(async () => {
      const bytes = await controlPoint.read({ signal, timeoutMs: OPERATION_TIMEOUT_MS })
      return { ...parsePmdFeatures(bytes), raw: bytesToHex(bytes) }
    })
    const batteryNotify = await outcomeOf(async () => {
      const sub = await gatt.characteristic(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC).subscribe({
        signal,
        timeoutMs: OPERATION_TIMEOUT_MS,
        stream: 'balanced'
      })
      const delivery = sub.effectiveDelivery
      const record = await sub.remove()
      return { effectiveDelivery: delivery, release: record.state }
    })
    return {
      pmdStartWithoutMtu: startWithoutMtu,
      repeatedStart,
      stop: stopOnce,
      stopWhenStopped,
      invalidPmdCommand: invalid,
      controlPointReadWhileNotifying: readWhileNotifying.ok ? { ok: true, ...readWhileNotifying.value } : { ok: false, error: readWhileNotifying.error },
      batteryNotify: batteryNotify.ok ? { ok: true, ...batteryNotify.value } : { ok: false, error: batteryNotify.error }
    }
  }
}

function dumpObservation(observation: PublicScanObservation): JsonObject {
  const { peer, ...fields } = observation
  const out: Record<string, JsonValue> = {}
  for (const [key, field] of Object.entries(fields)) out[key] = toJsonValue(field)
  out.peer = { id: peer.id, name: peer.name, rssi: peer.rssi, hasReference: peer.reference !== null, sources: toJsonValue(peer.sources) }
  return out
}

function dumpGattDatabase(gatt: GattDatabase): JsonObject {
  try {
    const snapshot = gatt.snapshot()
    return {
      generation: snapshot.generation,
      services: snapshot.services.map(service => ({ uuid: service.uuid, occurrence: service.occurrence, primary: service.primary })),
      characteristics: snapshot.characteristics.map(characteristic => ({
        uuid: characteristic.uuid,
        occurrence: characteristic.occurrence,
        properties: toJsonValue(characteristic.properties)
      })),
      descriptors: snapshot.descriptors.map(descriptor => ({ uuid: descriptor.uuid, occurrence: descriptor.occurrence })),
      serviceCount: snapshot.services.length,
      characteristicCount: snapshot.characteristics.length,
      descriptorCount: snapshot.descriptors.length
    }
  } catch {
    // Older backends without snapshot(): walk the live objects instead.
    const characteristics: JsonObject[] = []
    const descriptors: JsonObject[] = []
    for (const service of gatt.services) {
      for (const characteristic of service.characteristics) {
        characteristics.push({ service: service.uuid, uuid: characteristic.uuid, occurrence: characteristic.occurrence, properties: toJsonValue(characteristic.properties) })
        for (const descriptor of characteristic.descriptors) {
          descriptors.push({ characteristic: characteristic.uuid, uuid: descriptor.uuid, occurrence: descriptor.occurrence })
        }
      }
    }
    return {
      generation: gatt.generation,
      services: gatt.services.map(service => ({ uuid: service.uuid, occurrence: service.occurrence, primary: service.primary })),
      characteristics,
      descriptors,
      serviceCount: gatt.services.length,
      characteristicCount: characteristics.length,
      descriptorCount: descriptors.length
    }
  }
}

function decodeText(bytes: Uint8Array): string {
  try {
    return new TextDecoder().decode(bytes)
  } catch {
    return bytesToHex(bytes)
  }
}

function byteHex(bytes: Uint8Array, offset: number): string {
  const value = bytes[offset]
  return value === undefined ? '??' : value.toString(16).padStart(2, '0')
}
