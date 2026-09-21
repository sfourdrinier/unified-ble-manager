// examples-shared/driver/scenarios/continuation.ts
//
// Background continuation (BGS4): the declared standing order the OS wake
// executes. `declare` records the order the operator arms the app with
// (creation options — a scenario cannot set them post-hoc, so it records
// the intent and reports every strategy capability verbatim), `status`
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

import type { BleManager, FeatureId } from 'unified-ble-manager'
import type { DriverHost } from '../host.ts'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { toJsonValue } from '../protocol.ts'
import {
  ScenarioController,
  ScenarioError,
  args,
  defineCommand,
  type ScenarioCommand
} from '../scenario-core.ts'

const WAKE_FEATURE: FeatureId = 'background:wake-on-appearance'
const NATIVE_FEATURE: FeatureId = 'background:native-resubscribe'
const HEADLESS_FEATURE: FeatureId = 'background:headless-task'
const NOTIFICATION_FEATURE: FeatureId = 'background:wake-notification'

const STRATEGIES = ['record-only', 'native', 'headless-task', 'foreground-service'] as const
type ContinuationStrategy = (typeof STRATEGIES)[number]

/** The host continuation API (Expo `manager.continuation`); plain hosts lack it. */
interface ContinuationHostApi {
  readonly status: () => Promise<JsonObject>
  readonly claim: () => Promise<JsonObject>
}

function hasContinuationApi(manager: BleManager): manager is BleManager & { readonly continuation: ContinuationHostApi } {
  return 'continuation' in manager
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
  if (typeof value === 'string' && (STRATEGIES as readonly string[]).includes(value)) {
    return value as ContinuationStrategy
  }
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
    declare: defineCommand({
      label: 'Declare',
      description:
        'Record the standing order the app is armed with. args: {onAppearance?: "record-only" | "native" | "headless-task" | "foreground-service" (default "native"), peerId?: string, serviceUuid?: string, characteristicUuid?: string}. Reports each strategy capability verbatim from manager.capabilities.',
      presets: [
        { label: 'Declare native HR', args: { onAppearance: 'native' } },
        { label: 'Declare record-only', args: { onAppearance: 'record-only' } }
      ],
      parse: raw => ({
        onAppearance: parseStrategy(raw),
        peerId: args.optionalString(raw, 'peerId'),
        serviceUuid: args.optionalString(raw, 'serviceUuid'),
        characteristicUuid: args.optionalString(raw, 'characteristicUuid')
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
      description: 'Clear the recorded order; owns nothing between commands.',
      parse: args.none,
      run: async () => {
        this.replace({ ...IDLE_CONTINUATION_STATE })
        return { cleared: true }
      }
    })
  }

  protected readonly host: DriverHost

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
    const hosted = await this.host.createManager('continuation')
    try {
      return await body(hosted.manager)
    } finally {
      await hosted.manager.destroy()
    }
  }

  private async declareOrder(options: {
    onAppearance: ContinuationStrategy
    peerId: string | null
    serviceUuid: string | null
    characteristicUuid: string | null
  }): Promise<JsonObject> {
    const resubscribe =
      options.serviceUuid === null || options.characteristicUuid === null
        ? []
        : [{ serviceUuid: options.serviceUuid, characteristicUuid: options.characteristicUuid }]
    return this.withManager(async manager => {
      const capabilities = {
        [WAKE_FEATURE]: capabilityState(manager, WAKE_FEATURE),
        [NATIVE_FEATURE]: capabilityState(manager, NATIVE_FEATURE),
        [HEADLESS_FEATURE]: capabilityState(manager, HEADLESS_FEATURE),
        [NOTIFICATION_FEATURE]: capabilityState(manager, NOTIFICATION_FEATURE)
      }
      this.replace({
        strategy: options.onAppearance,
        peerId: options.peerId,
        resubscribe,
        capabilities,
        lastWake: this.snapshot().lastWake,
        backlog: this.snapshot().backlog
      })
      this.emit('continuation-declared', {
        onAppearance: options.onAppearance,
        peerId: options.peerId ?? null,
        resubscribe: toJsonValue(resubscribe)
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
      const status = await manager.continuation.status()
      this.replace({ ...this.snapshot(), lastWake: status.lastWake ?? null })
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
        claim = await manager.continuation.claim()
      } catch (error) {
        const code = (error as { code?: unknown }).code
        if (code === 'capability.unsupported') {
          const reported = {
            state: 'capability-unsupported',
            operation: (error as { operation?: unknown }).operation ?? null,
            reason: (error as { message?: unknown }).message ?? null
          }
          this.emit('continuation-backlog', reported)
          return reported
        }
        throw error
      }
      const values = Array.isArray(claim.values) ? claim.values : []
      const streamEnds = Array.isArray(claim.streamEnds) ? claim.streamEnds : []
      const droppedItems = streamEnds.reduce(
        (total, end) => total + (typeof (end as { droppedItems?: unknown }).droppedItems === 'number' ? (end as { droppedItems: number }).droppedItems : 0),
        0
      )
      const droppedBytes = streamEnds.reduce(
        (total, end) => total + (typeof (end as { droppedBytes?: unknown }).droppedBytes === 'number' ? (end as { droppedBytes: number }).droppedBytes : 0),
        0
      )
      const report = {
        values: values.length,
        consumers: values.map(value => (value as { consumer?: unknown }).consumer ?? null),
        droppedItems,
        droppedBytes,
        controlLost: typeof claim.controlLost === 'number' ? claim.controlLost : null,
        disposed: claim.disposed ?? null
      }
      this.replace({ ...this.snapshot(), backlog: report })
      this.emit('continuation-backlog', report)
      return report
    })
  }
}
