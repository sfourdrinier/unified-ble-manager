// examples-shared/driver/scenarios/ecg.ts
//
// High-rate stream: Polar H10 ECG (130 Hz) over the PMD service. Order follows
// Polar's SDK: enable control-point and data notifications, then write the
// start request and wait for the control point's response. The feature read
// happens while the control point is notifying, on every host, so the run
// also exercises reading a notifying characteristic. Throughput, sensor
// timestamp gaps, notification sequence gaps, overflow notices and parse
// failures are all counted and reported.

import type { GattCharacteristic, GattValueEvent } from 'unified-ble-manager'
import type { DriverHost } from '../host.ts'
import type { JsonObject } from '../protocol.ts'
import { bytesToHex, describeError, toJsonValue } from '../protocol.ts'
import {
  EcgStreamStats,
  byteAt,
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
  parsePmdSettings,
  type ControlPointMessage
} from '../polar-pmd.ts'
import { ScenarioError, args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import {
  BleScenario,
  DEVICE_ARGUMENT_HELP,
  IDLE_BLE_STATE,
  OPERATION_TIMEOUT_MS,
  appendRecent,
  outcomeOf,
  parseDevice,
  type BleScenarioState,
  type DeviceSelector
} from './ble-scenario.ts'

const CONTROL_POINT_TIMEOUT_MS = 5_000
const RATE_REPORT_INTERVAL_MS = 1_000

type ControlPointResponse = Extract<ControlPointMessage, { kind: 'response' }>

export type EcgState = BleScenarioState & {
  readonly mtu: JsonObject | null
  readonly features: JsonObject | null
  readonly settings: JsonObject | null
  readonly controlPoint: readonly string[]
  readonly frames: number
  readonly samples: number
  readonly samplesLastSecond: number
  readonly sensorSampleRateHz: number | null
  readonly timestampGaps: number
  readonly estimatedMissingSamples: number
  readonly missingNotifications: number
  readonly overflowNotices: number
  readonly droppedItems: number
  readonly parseFailures: number
  readonly maxFrameBytes: number
  readonly lastSampleMicroVolts: number | null
}

const IDLE_ECG_STATE: EcgState = {
  ...IDLE_BLE_STATE,
  mtu: null,
  features: null,
  settings: null,
  controlPoint: [],
  frames: 0,
  samples: 0,
  samplesLastSecond: 0,
  sensorSampleRateHz: null,
  timestampGaps: 0,
  estimatedMissingSamples: 0,
  missingNotifications: 0,
  overflowNotices: 0,
  droppedItems: 0,
  parseFailures: 0,
  maxFrameBytes: 0,
  lastSampleMicroVolts: null
}

type Waiter = { readonly opCode: number; readonly resolve: (response: ControlPointResponse) => void }

export class EcgScenario extends BleScenario<EcgState> {
  readonly id = 'ecg'
  readonly title = 'High rate: H10 ECG (PMD)'
  readonly description =
    'Polar PMD ECG at 130 Hz / 14 bit (start command bytes from Polar BLE SDK). Reports samples/s, sensor-clock gaps, notification sequence gaps, overflow notices and drops.'
  private stats = new EcgStreamStats(H10_ECG_SAMPLE_RATE_HZ)
  private waiters: Waiter[] = []
  private controlPoint: GattCharacteristic | null = null
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: defineCommand({
      label: 'Start ECG',
      description: `args: {mtu?: number (default 517; Polar prefers 512+ so a 229-byte frame fits), ${DEVICE_ARGUMENT_HELP}}`,
      presets: [{ label: 'Start ECG (request MTU 517)', args: {} }],
      acceptsDevice: true,
      parse: raw => ({ mtu: args.number(raw, 'mtu', 517, { min: 23, max: 517 }), device: parseDevice(raw) }),
      run: ({ mtu, device }) => this.start(mtu, device)
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, IDLE_ECG_STATE)
  }

  override headline(): string | null {
    const { samplesLastSecond, frames, phase } = this.snapshot()
    return frames === 0 ? phase : `${samplesLastSecond.toString()} samples/s`
  }

  private patchEcg(patch: Partial<EcgState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  private start(mtu: number, device: DeviceSelector): Promise<JsonObject> {
    return this.runJourney(async signal => {
      this.stats = new EcgStreamStats(H10_ECG_SAMPLE_RATE_HZ)
      this.waiters = []
      this.controlPoint = null
      const { connection, gatt } = await this.connectH10(device, signal)
      this.patchBase({ phase: 'negotiating-mtu' })
      const negotiation = await outcomeOf(() => connection.controls.requestMtu(mtu, { signal, timeoutMs: OPERATION_TIMEOUT_MS }))
      const effective = await outcomeOf(() => connection.controls.effectiveMtu())
      this.patchEcg({ mtu: { requested: mtu, negotiation: toJsonValue(negotiation), effective: toJsonValue(effective) } })
      this.emit('mtu', { requested: mtu, negotiation: toJsonValue(negotiation), effective: toJsonValue(effective) })

      const controlPoint = gatt.characteristic(PMD_SERVICE, PMD_CONTROL_POINT)
      const data = gatt.characteristic(PMD_SERVICE, PMD_DATA)
      this.patchBase({ phase: 'enabling-pmd' })
      const cpSubscription = await controlPoint.subscribe({ signal, timeoutMs: OPERATION_TIMEOUT_MS, delivery: 'prefer-indication', stream: 'lossless-bounded' })
      this.own('pmd.control-point.remove', () => cpSubscription.remove())
      this.emit('subscribed', { characteristic: 'pmd-control-point', effectiveDelivery: cpSubscription.effectiveDelivery })
      void this.consume('pmd.control-point', cpSubscription.values, { value: value => this.onControlPoint(value) })
      const dataSubscription = await data.subscribe({ signal, timeoutMs: OPERATION_TIMEOUT_MS, delivery: 'prefer-notification', stream: 'balanced' })
      this.own('pmd.data.remove', () => dataSubscription.remove())
      this.emit('subscribed', { characteristic: 'pmd-data', effectiveDelivery: dataSubscription.effectiveDelivery })
      void this.consume('pmd.data', dataSubscription.values, {
        value: value => this.onFrame(value),
        overflow: notice => this.patchEcg({ overflowNotices: this.snapshot().overflowNotices + 1, droppedItems: this.snapshot().droppedItems + notice.droppedItems }),
        terminal: notice => this.patchEcg({ droppedItems: this.snapshot().droppedItems + notice.droppedItems })
      })
      this.controlPoint = controlPoint

      const featureBytes = await controlPoint.read({ signal, timeoutMs: OPERATION_TIMEOUT_MS })
      const features = parsePmdFeatures(featureBytes)
      this.patchEcg({ features: { ...features, raw: bytesToHex(featureBytes) } })
      this.emit('pmd-features', { ...features, raw: bytesToHex(featureBytes) })
      if (!features.ecg) throw new ScenarioError('pmd.ecg-unsupported', 'PMD feature read does not advertise ECG')

      // Diagnostic only: the start request below carries its own settings.
      const settings = await outcomeOf(async () => ({ ...parsePmdSettings((await this.command(buildGetEcgSettingsCommand())).parameters) }))
      this.patchEcg({ settings: settings.ok ? settings.value : { error: settings.error } })
      this.emit('pmd-settings', settings.ok ? settings.value : { error: settings.error })

      this.patchBase({ phase: 'starting-ecg' })
      // Registered before the start request so a rejected or timed-out start
      // (e.g. ERROR_ALREADY_IN_STATE from an earlier run) still sends stop.
      this.own('pmd.stop-request', async () => toJsonValue(await outcomeOf(() => this.command(buildStopEcgCommand()))))
      await this.command(buildStartEcgCommand())
      const stopTimer = this.startRateReports()
      this.own('rate-reports.stop', async () => {
        stopTimer()
        return null
      })
      this.patchBase({ phase: 'streaming' })
      return { peer: this.peerReport(), mtu: this.snapshot().mtu, features: this.snapshot().features, settings: this.snapshot().settings }
    })
  }

  /** Writes a control-point request and resolves with its response; a non-SUCCESS status throws. */
  private async command(bytes: Uint8Array): Promise<ControlPointResponse> {
    const controlPoint = this.controlPoint
    if (controlPoint === null) throw new ScenarioError('pmd.not-ready', 'PMD control point is not subscribed')
    const opCode = byteAt(bytes, 0)
    // Register before writing: the response indication can arrive before the write completes.
    let settle: ((response: ControlPointResponse) => void) | null = null
    const response = new Promise<ControlPointResponse>(resolve => {
      settle = resolve
    })
    const waiter: Waiter = { opCode, resolve: answer => settle?.(answer) }
    this.waiters.push(waiter)
    let receipt: unknown
    try {
      receipt = await controlPoint.write(bytes, { response: 'required', timeoutMs: OPERATION_TIMEOUT_MS })
    } catch (error) {
      this.waiters = this.waiters.filter(entry => entry !== waiter)
      throw error
    }
    this.emit('pmd-write', { bytes: bytesToHex(bytes), receipt: toJsonValue(receipt) })
    const timeout = new Promise<null>(resolve => {
      const cancel = this.runtime.schedule(() => resolve(null), CONTROL_POINT_TIMEOUT_MS)
      void response.then(() => cancel())
    })
    const settled = await Promise.race([response, timeout])
    if (settled === null) {
      this.waiters = this.waiters.filter(entry => entry !== waiter)
      throw new ScenarioError('pmd.control-point-timeout', `no PMD response to op 0x${opCode.toString(16)} within ${CONTROL_POINT_TIMEOUT_MS.toString()} ms`)
    }
    const answer = settled
    if (answer.statusName !== 'SUCCESS') {
      throw new ScenarioError('pmd.request-rejected', `PMD op 0x${opCode.toString(16)} answered ${answer.statusName} (${answer.status.toString()})`)
    }
    return answer
  }

  private onControlPoint(value: GattValueEvent): void {
    let message: ControlPointMessage
    try {
      message = parseControlPointMessage(value.value)
    } catch (error) {
      this.emit('pmd-control-point-unparsed', { bytes: bytesToHex(value.value), error: describeError(error) })
      return
    }
    const line =
      message.kind === 'response'
        ? `op 0x${message.opCode.toString(16)} ${message.statusName}${message.more ? ' (more)' : ''} params=${bytesToHex(message.parameters)}`
        : `device stopped measurement(s) ${message.measurementTypes.join(',')}`
    this.emit('pmd-control-point', { bytes: bytesToHex(value.value), message: line })
    this.patchEcg({ controlPoint: appendRecent(this.snapshot().controlPoint, line) })
    if (message.kind !== 'response') {
      this.patchBase({ phase: 'stopped-by-device' })
      return
    }
    const waiter = this.waiters.find(entry => entry.opCode === message.opCode)
    if (waiter === undefined) {
      this.emit('pmd-control-point-unsolicited', { message: line })
      return
    }
    this.waiters = this.waiters.filter(entry => entry !== waiter)
    waiter.resolve(message)
  }

  private onFrame(value: GattValueEvent): void {
    const receivedAtMs = this.runtime.now()
    const maxFrameBytes = Math.max(this.snapshot().maxFrameBytes, value.value.length)
    try {
      const frame = parseEcgFrame(value.value)
      const findings = this.stats.record(frame, receivedAtMs, value.sequence)
      if (findings.timestampGap !== null) this.emit('ecg-timestamp-gap', { ...findings.timestampGap, sequence: value.sequence })
      if (findings.sequenceGap !== null) this.emit('ecg-sequence-gap', { ...findings.sequenceGap })
      this.patchEcg({ ...this.stats.summary(receivedAtMs), maxFrameBytes })
    } catch (error) {
      this.emit('ecg-parse-failed', { sequence: value.sequence, bytes: bytesToHex(value.value.slice(0, 32)), length: value.value.length, error: describeError(error) })
      this.patchEcg({ parseFailures: this.snapshot().parseFailures + 1, maxFrameBytes })
    }
  }

  private startRateReports(): () => void {
    let cancel: () => void = () => {}
    const tick = () => {
      const summary = this.stats.summary(this.runtime.now())
      this.emit('ecg-rate', { ...summary, overflowNotices: this.snapshot().overflowNotices, droppedItems: this.snapshot().droppedItems, parseFailures: this.snapshot().parseFailures })
      this.patchEcg({ samplesLastSecond: summary.samplesLastSecond })
      cancel = this.runtime.schedule(tick, RATE_REPORT_INTERVAL_MS)
    }
    cancel = this.runtime.schedule(tick, RATE_REPORT_INTERVAL_MS)
    return () => cancel()
  }
}

