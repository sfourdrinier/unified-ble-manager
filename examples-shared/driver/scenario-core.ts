// examples-shared/driver/scenario-core.ts
//
// One controller per test scenario, one registry for all of them. The UI
// buttons and the remote driver both call `ScenarioRegistry.dispatch`, and both
// observe the same snapshots and events, so a host driven from the control
// server runs exactly the code a tap or click runs, on every host.

import type {
  CommandDescription,
  CommandPreset,
  DriverError,
  JsonObject,
  JsonValue,
  ScenarioDescription,
  ScenarioEvent
} from './protocol.ts'
import { describeError } from './protocol.ts'

export interface ScenarioRuntime {
  /** `<host>/<platform>`, stamped on every event and snapshot. */
  readonly host: string
  now(): number
  schedule(callback: () => void, delayMs: number): () => void
  log(scope: string, message: string, detail?: JsonValue): void
}

/** The runtime every host uses: monotonic `performance.now()`, timers, console lines. */
export function createConsoleRuntime(host: string): ScenarioRuntime {
  return {
    host,
    now: () => performance.now(),
    schedule(callback, delayMs) {
      const handle = setTimeout(callback, delayMs)
      return () => clearTimeout(handle)
    },
    log(scope, message, detail) {
      const stamp = `[driver:${scope}] +${Math.round(performance.now()).toString()}ms ${host} ${message}`
      if (detail === undefined) console.log(stamp)
      else console.log(stamp, JSON.stringify(detail))
    }
  }
}

export type ScenarioUpdate =
  | {
      readonly type: 'snapshot'
      readonly scenario: string
      readonly atMs: number
      readonly host: string
      readonly snapshot: JsonObject
    }
  | { readonly type: 'event'; readonly event: ScenarioEvent }

export type ScenarioListener = (update: ScenarioUpdate) => void

/** A failure the scenario layer itself raises (unknown command, busy, bad arguments). */
export class ScenarioError extends Error {
  readonly code: string

  constructor(code: string, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'ScenarioError'
    this.code = code
  }
}

export interface ScenarioCommand {
  readonly label: string
  readonly description: string
  readonly presets: readonly CommandPreset[]
  /** True when the command acquires a peer and takes the `device` argument. */
  readonly acceptsDevice: boolean
  execute(args: JsonObject): Promise<JsonValue>
}

export interface CommandSpec<Args> {
  readonly label: string
  readonly description: string
  readonly presets?: readonly CommandPreset[]
  /** Declare only when `parse` reads the `device` argument; sequences inject it by this flag. */
  readonly acceptsDevice?: boolean
  readonly parse: (raw: JsonObject) => Args
  readonly run: (args: Args) => Promise<JsonValue>
}

/** Binds an argument parser to its command so remote JSON and UI presets share one validation path. */
export function defineCommand<Args>(spec: CommandSpec<Args>): ScenarioCommand {
  return {
    label: spec.label,
    description: spec.description,
    presets: spec.presets ?? [{ label: spec.label, args: {} }],
    acceptsDevice: spec.acceptsDevice ?? false,
    execute: async raw => spec.run(spec.parse(raw))
  }
}

/** One release a stop performed, with the state its owner reported. */
export type CleanupStep = {
  readonly step: string
  readonly state: string
  readonly detail: JsonValue
}

export type ScenarioStopOutcome = {
  readonly wasRunning: boolean
  readonly cleanup: readonly CleanupStep[]
}

export type StopAllEntry = {
  readonly scenario: string
  /** `null` when the stop threw, so whether it was running is not known. */
  readonly wasRunning: boolean | null
  readonly cleanup: readonly CleanupStep[]
  readonly error: DriverError | null
}

/** A release that did not report `released`, or a stop that threw (`step: "stop"`). */
export type StopAllFailure = CleanupStep & { readonly scenario: string }

export type StopAllReport = {
  readonly scenarios: readonly StopAllEntry[]
  readonly failures: readonly StopAllFailure[]
}

/** `stopAll` finished stopping everything but something was not released; `report` says what. */
export class StopAllError extends ScenarioError {
  readonly report: StopAllReport

  constructor(report: StopAllReport) {
    const summary = report.failures.map(failure => `${failure.scenario}:${failure.step} ${failure.state}`).join(', ')
    super('scenario.stop-all-failed', `stopping every scenario left ${report.failures.length.toString()} failure(s): ${summary}`, { cause: report })
    this.name = 'StopAllError'
    this.report = report
  }
}

export interface Scenario {
  readonly id: string
  readonly title: string
  readonly description: string
  describe(): ScenarioDescription
  snapshot(): JsonObject
  headline(): string | null
  recentEvents(): readonly ScenarioEvent[]
  subscribe(listener: ScenarioListener): () => void
  dispatch(command: string, args: JsonObject): Promise<JsonValue>
  /** Ends whatever the scenario is running and releases what it owns; idle scenarios report `wasRunning: false`. */
  stop(): Promise<ScenarioStopOutcome>
}

export const SNAPSHOT_MIN_INTERVAL_MS = 250
export const RECENT_EVENT_LIMIT = 200

export abstract class ScenarioController<State extends JsonObject> implements Scenario {
  abstract readonly id: string
  abstract readonly title: string
  abstract readonly description: string
  protected abstract readonly commands: Readonly<Record<string, ScenarioCommand>>

  protected readonly runtime: ScenarioRuntime
  private state: State
  private readonly listeners = new Set<ScenarioListener>()
  private events: ScenarioEvent[] = []
  private nextSeq = 1
  private lastPublishedAtMs: number | null = null
  private cancelPendingPublish: (() => void) | null = null

  protected constructor(runtime: ScenarioRuntime, initial: State) {
    this.runtime = runtime
    this.state = initial
  }

  describe(): ScenarioDescription {
    const commands: CommandDescription[] = Object.entries(this.commands).map(([name, command]) => ({
      name,
      label: command.label,
      description: command.description,
      presets: command.presets,
      acceptsDevice: command.acceptsDevice
    }))
    return { id: this.id, title: this.title, description: this.description, commands }
  }

  snapshot(): State {
    return this.state
  }

  headline(): string | null {
    return null
  }

  recentEvents(): readonly ScenarioEvent[] {
    return this.events
  }

  /** A scenario that owns nothing between commands has nothing to stop. */
  async stop(): Promise<ScenarioStopOutcome> {
    return { wasRunning: false, cleanup: [] }
  }

  subscribe(listener: ScenarioListener): () => void {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  /**
   * The single entry both UI and remote use. Every command is bracketed by
   * `command` and `command-result`/`command-failed` events; a failure is
   * reported and then rethrown so the caller sees it too.
   */
  async dispatch(command: string, args: JsonObject): Promise<JsonValue> {
    const entry = Object.prototype.hasOwnProperty.call(this.commands, command) ? this.commands[command] : undefined
    this.emit('command', { command, args })
    try {
      if (entry === undefined) {
        throw new ScenarioError(
          'scenario.unknown-command',
          `${this.id} has no command "${command}"; available: ${Object.keys(this.commands).join(', ')}`
        )
      }
      const result = await entry.execute(args)
      this.emit('command-result', { command, result })
      return result
    } catch (error) {
      const described = describeError(error)
      this.runtime.log(this.id, `command ${command} failed`, described)
      this.emit('command-failed', { command, error: described })
      throw error
    } finally {
      this.flushSnapshot()
    }
  }

  protected patch(patch: Partial<State>): void {
    this.replace({ ...this.state, ...patch })
  }

  protected replace(next: State): void {
    this.state = next
    this.schedulePublish()
  }

  protected emit(kind: string, data: JsonObject = {}): ScenarioEvent {
    const event: ScenarioEvent = {
      scenario: this.id,
      seq: this.nextSeq,
      atMs: this.runtime.now(),
      host: this.runtime.host,
      kind,
      data
    }
    this.nextSeq += 1
    this.events = [...this.events.slice(-(RECENT_EVENT_LIMIT - 1)), event]
    this.runtime.log(this.id, kind, data)
    this.notify({ type: 'event', event })
    return event
  }

  /** Publishes the current snapshot now, cancelling any throttled publish. */
  flushSnapshot(): void {
    this.cancelPendingPublish?.()
    this.cancelPendingPublish = null
    const atMs = this.runtime.now()
    this.lastPublishedAtMs = atMs
    this.notify({ type: 'snapshot', scenario: this.id, atMs, host: this.runtime.host, snapshot: this.state })
  }

  private schedulePublish(): void {
    if (this.cancelPendingPublish !== null) return
    const elapsed = this.lastPublishedAtMs === null ? Number.POSITIVE_INFINITY : this.runtime.now() - this.lastPublishedAtMs
    if (elapsed >= SNAPSHOT_MIN_INTERVAL_MS) {
      this.flushSnapshot()
      return
    }
    this.cancelPendingPublish = this.runtime.schedule(() => {
      this.cancelPendingPublish = null
      this.flushSnapshot()
    }, SNAPSHOT_MIN_INTERVAL_MS - elapsed)
  }

  private notify(update: ScenarioUpdate): void {
    for (const listener of this.listeners) {
      try {
        listener(update)
      } catch (error) {
        this.runtime.log(this.id, 'listener threw', describeError(error))
      }
    }
  }
}

export class ScenarioRegistry {
  private readonly scenarios = new Map<string, Scenario>()

  constructor(scenarios: readonly Scenario[]) {
    for (const scenario of scenarios) {
      if (this.scenarios.has(scenario.id)) {
        throw new ScenarioError('scenario.duplicate-id', `scenario id "${scenario.id}" is registered twice`)
      }
      this.scenarios.set(scenario.id, scenario)
    }
  }

  list(): readonly Scenario[] {
    return [...this.scenarios.values()]
  }

  describe(): readonly ScenarioDescription[] {
    return this.list().map(scenario => scenario.describe())
  }

  get(id: string): Scenario {
    const scenario = this.scenarios.get(id)
    if (scenario === undefined) {
      throw new ScenarioError(
        'scenario.unknown',
        `no scenario "${id}"; available: ${[...this.scenarios.keys()].join(', ')}`
      )
    }
    return scenario
  }

  async dispatch(scenario: string, command: string, args: JsonObject): Promise<JsonValue> {
    return this.get(scenario).dispatch(command, args)
  }

  /**
   * Stops every scenario at once and waits for each cleanup, for a host that
   * is going away (Fast Refresh, HMR, shutdown): an orphaned run must never
   * keep a connection. Every scenario is stopped even when another fails; a
   * release that did not report `released`, or a stop that threw, rejects
   * with a {@link StopAllError} carrying the whole report.
   */
  async stopAll(): Promise<StopAllReport> {
    const settled = await Promise.all(
      this.list().map(scenario =>
        scenario.stop().then(
          outcome => ({ scenario: scenario.id, stopped: true as const, outcome }),
          (reason: unknown) => ({ scenario: scenario.id, stopped: false as const, error: describeError(reason) })
        )
      )
    )
    const entries: StopAllEntry[] = []
    const failures: StopAllFailure[] = []
    for (const entry of settled) {
      if (!entry.stopped) {
        entries.push({ scenario: entry.scenario, wasRunning: null, cleanup: [], error: entry.error })
        failures.push({ scenario: entry.scenario, step: 'stop', state: 'threw', detail: entry.error })
        continue
      }
      entries.push({ scenario: entry.scenario, ...entry.outcome, error: null })
      for (const step of entry.outcome.cleanup) {
        if (step.state !== 'released') failures.push({ scenario: entry.scenario, ...step })
      }
    }
    const report: StopAllReport = { scenarios: entries, failures }
    if (failures.length > 0) throw new StopAllError(report)
    return report
  }

  subscribe(listener: ScenarioListener): () => void {
    const unsubscribers = this.list().map(scenario => scenario.subscribe(listener))
    return () => {
      for (const unsubscribe of unsubscribers) unsubscribe()
    }
  }
}

/** Argument readers shared by every command parser; each rejects with a typed ScenarioError. */
export const args = {
  boolean(raw: JsonObject, key: string, fallback: boolean): boolean {
    const value = raw[key]
    if (value === undefined) return fallback
    if (typeof value !== 'boolean') throw invalidArgument(key, 'a boolean', value)
    return value
  },
  number(raw: JsonObject, key: string, fallback: number, bounds: { min?: number; max?: number } = {}): number {
    const value = raw[key]
    if (value === undefined) return fallback
    if (typeof value !== 'number' || !Number.isFinite(value)) throw invalidArgument(key, 'a finite number', value)
    if (bounds.min !== undefined && value < bounds.min) throw invalidArgument(key, `>= ${bounds.min.toString()}`, value)
    if (bounds.max !== undefined && value > bounds.max) throw invalidArgument(key, `<= ${bounds.max.toString()}`, value)
    return value
  },
  oneOf<Value extends string>(raw: JsonObject, key: string, allowed: readonly Value[], fallback: Value): Value {
    const value = raw[key]
    if (value === undefined) return fallback
    const match = allowed.find(candidate => candidate === value)
    if (match === undefined) throw invalidArgument(key, `one of ${allowed.join(' | ')}`, value)
    return match
  },
  optionalString(raw: JsonObject, key: string): string | null {
    const value = raw[key]
    if (value === undefined || value === null) return null
    if (typeof value !== 'string') throw invalidArgument(key, 'a string', value)
    return value
  },
  none(raw: JsonObject): Record<string, never> {
    const keys = Object.keys(raw)
    if (keys.length > 0) throw new ScenarioError('scenario.invalid-argument', `unexpected argument(s): ${keys.join(', ')}`)
    return {}
  }
}

function invalidArgument(key: string, expected: string, value: JsonValue | undefined): ScenarioError {
  return new ScenarioError('scenario.invalid-argument', `argument "${key}" must be ${expected}; received ${JSON.stringify(value)}`)
}
