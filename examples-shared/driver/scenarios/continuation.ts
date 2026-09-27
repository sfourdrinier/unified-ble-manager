// examples-shared/driver/scenarios/continuation.ts
//
// Background continuation (BGS4): the declared standing order the OS wake
// executes. `declare` persists through the host's public construction API;
// an unsupported host never reports an intent as an armed order. `status`
// reports the last wake, and `backlog` drains what the wake queued with its
// loss accounting. Demo flow: declare(native) → arm presence (restoration
// observe-presence) → kill per docs/BACKGROUND.md → strap out/in range →
// relaunch → status shows continuation.completed → backlog drains values
// with droppedItems/droppedBytes/controlLost accounted, never silent.
//
// Hosts without a continuation API report `unregistered`; a platform refusal
// propagates as `capability-unsupported` with the owner's reason. Nothing is
// invented: capability states come from manager.capabilities, the wake
// record and backlog from the host API.

import type { BleManager, ContinuationRecordingController, FeatureId } from 'unified-ble-manager'
import { HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC } from 'unified-ble-manager/profiles/heart-rate'
import type { DriverHost, HostManager, HostContinuationDeclaration } from '../host.ts'
import { recipe, recording, requiredText } from '../continuation-arguments.ts'
import { continuationRecordingCommands } from '../continuation-recording-commands.ts'
import { HEADLESS_CONTINUATION_TASK } from '../headless-continuation-task.ts'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { toJsonValue } from '../protocol.ts'
import {
  ScenarioController,
  ScenarioError,
  args,
  defineCommand,
  type ScenarioCommand
} from '../scenario-core.ts'

/**
 * The owner's answers arrive as `unknown`, and a driver report must be JSON the
 * protocol can carry: narrow to text rather than widening the report's type.
 */
function isJsonObject(value: JsonValue | undefined): value is JsonObject {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function asText(value: unknown): string | null {
  return typeof value === 'string' ? value : value === undefined || value === null ? null : String(value)
}

const WAKE_FEATURE: FeatureId = 'background:wake-on-appearance'
const NATIVE_FEATURE: FeatureId = 'background:native-resubscribe'
const HEADLESS_FEATURE: FeatureId = 'background:headless-task'
const NOTIFICATION_FEATURE: FeatureId = 'background:wake-notification'

const STRATEGIES = ['record-only', 'native', 'headless-task', 'foreground-service'] as const
type ContinuationStrategy = (typeof STRATEGIES)[number]

/** The host continuation API (Expo `manager.continuation`); plain hosts lack it. */
interface ContinuationHostApi {
  readonly status: () => Promise<unknown>
  readonly claim: () => Promise<unknown>
}

function hasContinuationApi(manager: BleManager): manager is BleManager & { readonly continuation: ContinuationHostApi } {
  return 'continuation' in manager && typeof manager.continuation === 'object' && manager.continuation !== null &&
    'status' in manager.continuation && typeof manager.continuation.status === 'function' &&
    'claim' in manager.continuation && typeof manager.continuation.claim === 'function'
}

function field(value: unknown, key: string): unknown {
  return typeof value === 'object' && value !== null ? Reflect.get(value, key) : undefined
}

function objectReport(value: unknown): JsonObject {
  const json = toJsonValue(value)
  if (!isJsonObject(json)) throw new ScenarioError('protocol.malformed', 'Continuation response must be an object')
  return json
}


function wakeConfiguration(raw: JsonObject, strategy: ContinuationStrategy) {
  const headlessTaskName = raw.headlessTaskName === undefined ? undefined : requiredText(raw, 'headlessTaskName')
  if ((strategy === 'headless-task') !== (headlessTaskName !== undefined)) {
    throw new ScenarioError('scenario.invalid-continuation', 'headless-task requires headlessTaskName; other strategies must omit it')
  }
  if ((strategy === 'foreground-service') !== (raw.foregroundService !== undefined)) {
    throw new ScenarioError('scenario.invalid-continuation', 'foreground-service requires its notification configuration; other strategies must omit it')
  }
  if (raw.foregroundService === undefined) return headlessTaskName === undefined ? {} : { headlessTaskName }
  const service = raw.foregroundService
  if (!isJsonObject(service) || Object.keys(service).some(key => key !== 'notification') || !isJsonObject(service.notification)) {
    throw new ScenarioError('scenario.invalid-continuation', 'foregroundService must contain notification')
  }
  const notification = service.notification
  if (Object.keys(notification).some(key => !['channelId', 'channelName', 'title', 'body', 'icon'].includes(key))) {
    throw new ScenarioError('scenario.invalid-continuation', 'Unknown notification option')
  }
  return { foregroundService: { notification: {
    channelId: requiredText(notification, 'channelId'), channelName: requiredText(notification, 'channelName'), title: requiredText(notification, 'title'),
    ...(notification.body === undefined ? {} : { body: requiredText(notification, 'body') }),
    ...(notification.icon === undefined ? {} : { icon: requiredText(notification, 'icon') })
  } } }
}

function capabilityState(manager: BleManager, feature: FeatureId): string {
  return manager.capabilities.get(feature)?.state ?? 'unregistered'
}

export type ContinuationState = {
  readonly strategy: ContinuationStrategy | null
  readonly peerId: string | null
  readonly resubscribe: readonly JsonValue[]
  readonly capabilities: Readonly<Record<string, string>>
  readonly lastWake: JsonObject | null
  readonly backlog: JsonObject | null
}

const IDLE_CONTINUATION_STATE: ContinuationState = {
  strategy: null,
  peerId: null,
  resubscribe: [],
  capabilities: {},
  lastWake: null,
  backlog: null
}

function parseStrategy(raw: JsonObject): ContinuationStrategy {
  const value = raw.onAppearance
  if (value === undefined) return 'native'
  const strategy = STRATEGIES.find(candidate => candidate === value)
  if (strategy !== undefined) return strategy
  throw new ScenarioError(
    'scenario.invalid-continuation',
    `argument "onAppearance" must be one of ${STRATEGIES.join(' | ')}; received ${JSON.stringify(value ?? null)}`
  )
}

export class ContinuationScenario extends ScenarioController<ContinuationState> {
  readonly id = 'continuation'
  readonly title = 'Background continuation'
  readonly description =
    'Declare the background standing order the OS wake executes (record-only, native, headless-task, foreground-service); report every strategy capability verbatim; after a kill-and-wake, report the wake outcome and drain the backlog with its loss accounting.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    'headless-history': defineCommand({
      label: 'Headless task receipts',
      description: 'Read bounded Android reference-task summaries without opening BLE; native task dispatch alone is not job completion.',
      parse: args.none,
      run: async () => {
        if (this.host.readHeadlessContinuationHistory === undefined) throw new ScenarioError('capability.unsupported', 'Host has no reference headless task history')
        return this.host.readHeadlessContinuationHistory()
      }
    }),
    ...continuationRecordingCommands(async () => this.recordingController()),
    declare: defineCommand({
      label: 'Declare',
      description:
        'Persist via the public host factory. Native defaults to HR; measurements accepts hr, hr-ecg, hr-acc, or hr-ecg-acc (ACC sampleRateHz/rangeG optional). Durable recording requires recordingId, maxBytes and maxRecords. Headless requires headlessTaskName registered by the app; foreground-service requires foregroundService.notification {channelId, channelName, title, body?, icon?}. Validate device settings in foreground before arming. Custom serviceUuid/characteristicUuid cannot accompany a recipe.',
      presets: [
        { label: 'Declare native HR', args: { onAppearance: 'native' } },
        { label: 'Android headless battery check', args: { onAppearance: 'headless-task', headlessTaskName: HEADLESS_CONTINUATION_TASK } },
        { label: 'Declare H10 HR + ECG + ACC', args: { measurements: 'hr-ecg-acc', sampleRateHz: 50, rangeG: 4 } },
        { label: 'Record H10 HR + ECG + ACC', args: { measurements: 'hr-ecg-acc', sampleRateHz: 50, rangeG: 4, recordingId: 'h10_capture', maxBytes: 16777216, maxRecords: 100000 } },
        { label: 'Declare record-only', args: { onAppearance: 'record-only' } }
      ],
      parse: raw => ({
        onAppearance: parseStrategy(raw),
        peerId: args.optionalString(raw, 'peerId'),
        serviceUuid: args.optionalString(raw, 'serviceUuid'),
        characteristicUuid: args.optionalString(raw, 'characteristicUuid'),
        recipe: recipe(raw),
        recording: recording(raw),
        wake: wakeConfiguration(raw, parseStrategy(raw))
      }),
      run: options => this.declareOrder(options)
    }),
    status: defineCommand({
      label: 'Wake status',
      description:
        'Report the last wake outcome through the host continuation API. Hosts without one answer `unregistered` verbatim.',
      presets: [{ label: 'Wake status', args: {} }],
      parse: args.none,
      run: () => this.readStatus()
    }),
    backlog: defineCommand({
      label: 'Drain backlog',
      description:
        'Drain what the wake queued through the host continuation API, with loss accounting (values, droppedItems, droppedBytes, controlLost). Hosts without one answer verbatim; a platform refusal propagates as capability-unsupported.',
      presets: [{ label: 'Drain backlog', args: {} }],
      parse: args.none,
      run: () => this.drainBacklog()
    }),
    stop: defineCommand({
      label: 'Stop',
      description: 'Persist record-only to disarm an order configured by this scenario, then clear its display state.',
      parse: args.none,
      run: () => this.stopOrder()
    })
  }

  protected readonly host: DriverHost
  private readonly retainedManagers = new Set<BleManager>()

  constructor(host: DriverHost) {
    super(host.runtime, { ...IDLE_CONTINUATION_STATE })
    this.host = host
  }

  override headline(): string | null {
    const { strategy, backlog } = this.snapshot()
    if (strategy === null) return 'no standing order declared'
    if (backlog === null) return `${strategy} declared`
    return `${strategy} declared · backlog drained`
  }

  private async withManager<Result>(body: (manager: BleManager) => Promise<Result>): Promise<Result> {
    await this.retryCleanup()
    const hosted = await this.host.createManager('continuation')
    return this.withHosted(hosted, body)
  }

  private recordingController(): ContinuationRecordingController {
    if (this.host.continuationRecordings === undefined) throw new ScenarioError('capability.unsupported', 'Host has no offline continuation recording controls')
    return this.host.continuationRecordings()
  }

  private async release(manager: BleManager): Promise<void> {
    const receipt = await manager.destroy()
    if (receipt.state !== 'released') {
      this.emit('continuation-cleanup-failed', { receipt: toJsonValue(receipt) })
      throw new ScenarioError('scenario.cleanup-failed', 'Continuation manager remains owned; retry cleanup')
    }
    this.retainedManagers.delete(manager)
  }

  private async retryCleanup(): Promise<void> {
    for (const manager of this.retainedManagers) await this.release(manager)
  }

  private async withHosted<Result>(hosted: HostManager, body: (manager: BleManager) => Promise<Result>): Promise<Result> {
    this.retainedManagers.add(hosted.manager)
    let result: Result
    try {
      result = await body(hosted.manager)
    } catch (operationError) {
      try { await this.release(hosted.manager) }
      catch (cleanupError) { throw new AggregateError([operationError, cleanupError], 'Continuation operation and cleanup failed') }
      throw operationError
    }
    await this.release(hosted.manager)
    return result
  }

  private async stopOrder(): Promise<JsonObject> {
    await this.retryCleanup()
    // Persisted orders survive this scenario instance. An empty UI snapshot is
    // not evidence that the native owner is disarmed.
    if (this.snapshot().strategy !== 'record-only' &&
        (this.host.configureContinuation !== undefined || this.snapshot().strategy !== null)) {
      if (typeof this.host.configureContinuation !== 'function') throw new ScenarioError('capability.unsupported', 'Host cannot persist continuation configuration')
      const hosted = await this.host.configureContinuation({ onAppearance: 'record-only', resubscribe: [] })
      this.replace({ ...this.snapshot(), strategy: 'record-only', resubscribe: [] })
      await this.withHosted(hosted, async () => undefined)
    }
    this.replace({ ...IDLE_CONTINUATION_STATE })
    return { cleared: true }
  }

  private async declareOrder(options: {
    onAppearance: ContinuationStrategy
    peerId: string | null
    serviceUuid: string | null
    characteristicUuid: string | null
    recipe: HostContinuationDeclaration | null
    recording: ReturnType<typeof recording>
    wake: ReturnType<typeof wakeConfiguration>
  }): Promise<JsonObject> {
    if (typeof this.host.configureContinuation !== 'function') {
      throw new ScenarioError('capability.unsupported', 'Host cannot persist continuation configuration')
    }
    if ((options.serviceUuid === null) !== (options.characteristicUuid === null)) {
      throw new ScenarioError('scenario.invalid-continuation', 'Both serviceUuid and characteristicUuid are required for a custom selector')
    }
    const resubscribe =
      options.onAppearance !== 'native' ? [] : [{
        serviceUuid: options.serviceUuid ?? HEART_RATE_SERVICE,
        characteristicUuid: options.characteristicUuid ?? HEART_RATE_MEASUREMENT_CHARACTERISTIC,
        serviceOccurrence: 1, characteristicOccurrence: 1
      }]
    if ((options.recipe !== null || options.recording !== undefined) && options.onAppearance !== 'native') {
      throw new ScenarioError('scenario.invalid-continuation', 'H10 setup and recording require native strategy')
    }
    if (options.recipe !== null && options.serviceUuid !== null) throw new ScenarioError('scenario.invalid-continuation', 'H10 recipe cannot also select a custom characteristic')
    await this.retryCleanup()
    const declaration = {
      ...(options.recipe ?? {
      onAppearance: options.onAppearance,
      ...(options.peerId === null ? {} : { peerId: options.peerId }),
      resubscribe
      }),
      ...(options.recording === undefined ? {} : { recording: options.recording }),
      ...options.wake
    }
    const hosted = await this.host.configureContinuation(declaration)
    return this.withHosted(hosted, async manager => {
      const capabilities = {
        [WAKE_FEATURE]: capabilityState(manager, WAKE_FEATURE),
        [NATIVE_FEATURE]: capabilityState(manager, NATIVE_FEATURE),
        [HEADLESS_FEATURE]: capabilityState(manager, HEADLESS_FEATURE),
        [NOTIFICATION_FEATURE]: capabilityState(manager, NOTIFICATION_FEATURE)
      }
      this.replace({
        strategy: options.onAppearance,
        peerId: options.peerId,
        resubscribe: declaration.resubscribe.map(toJsonValue),
        capabilities,
        lastWake: this.snapshot().lastWake,
        backlog: this.snapshot().backlog
      })
      this.emit('continuation-declared', {
        receipt: { state: 'persisted' },
        onAppearance: options.onAppearance,
        peerId: options.peerId ?? null,
        resubscribe: toJsonValue(declaration.resubscribe)
      })
      this.emit('continuation-capabilities', { capabilities: toJsonValue(capabilities) })
      return { onAppearance: options.onAppearance, peerId: options.peerId ?? null, capabilities: toJsonValue(capabilities) }
    })
  }

  private async readStatus(): Promise<JsonObject> {
    return this.withManager(async manager => {
      if (!hasContinuationApi(manager)) {
        this.emit('continuation-status', { state: 'unregistered' })
        return { state: 'unregistered' }
      }
      const status = objectReport(await manager.continuation.status())
      // `status.lastWake` is the owner's own record: keep it only when it is an
      // object the protocol can carry, never a stringified shadow of one.
      const lastWake = toJsonValue(status.lastWake ?? null)
      this.replace({
        ...this.snapshot(),
        lastWake: isJsonObject(lastWake) ? lastWake : null
      })
      this.emit('continuation-status', { state: 'reported', status })
      return status
    })
  }

  private async drainBacklog(): Promise<JsonObject> {
    return this.withManager(async manager => {
      if (!hasContinuationApi(manager)) {
        this.emit('continuation-backlog', { state: 'unregistered' })
        return { state: 'unregistered' }
      }
      let claim: JsonObject
      try {
        claim = objectReport(await manager.continuation.claim())
      } catch (error) {
        const code = field(error, 'code')
        if (code === 'capability.unsupported') {
          const reported = {
            state: 'capability-unsupported',
            operation: asText(field(error, 'operation')),
            reason: asText(field(error, 'message'))
          }
          this.emit('continuation-backlog', reported)
          return reported
        }
        throw error
      }
      const values = Array.isArray(claim.values) ? claim.values : []
      const streamEnds = Array.isArray(claim.streamEnds) ? claim.streamEnds : []
      const droppedItems = streamEnds.reduce(
        (total, end) => { const count = field(end, 'droppedItems'); return total + (typeof count === 'number' ? count : 0) },
        0
      )
      const droppedBytes = streamEnds.reduce(
        (total, end) => { const count = field(end, 'droppedBytes'); return total + (typeof count === 'number' ? count : 0) },
        0
      )
      const report = {
        values: values.length,
        consumers: values.map(value => asText(field(value, 'consumer'))),
        droppedItems,
        droppedBytes,
        streamEnds,
        ...(claim.afterCutoffLoss === undefined ? {} : { afterCutoffLoss: claim.afterCutoffLoss }),
        ...(claim.recording === undefined ? {} : { recording: claim.recording }),
        ...(claim.disposeFailure === undefined ? {} : { disposeFailure: claim.disposeFailure }),
        controlLost: typeof claim.controlLost === 'number' ? claim.controlLost : null,
        disposed: typeof claim.disposed === 'boolean' ? claim.disposed : null
      }
      this.replace({ ...this.snapshot(), backlog: report })
      this.emit('continuation-backlog', report)
      return report
    })
  }
}
