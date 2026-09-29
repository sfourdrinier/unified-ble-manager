import type { DriverHost, HostNativeContinuation } from '../host.ts'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { toJsonValue } from '../protocol.ts'
import { ScenarioController, ScenarioError, args, defineCommand, type ScenarioCommand, type ScenarioStopOutcome } from '../scenario-core.ts'
import { recipe, recording, requiredText } from '../continuation-arguments.ts'
import { continuationRecordingCommands } from '../continuation-recording-commands.ts'

function isObject(result: JsonValue): result is JsonObject {
  return typeof result === 'object' && result !== null && !Array.isArray(result)
}

function summary(result: JsonValue): JsonValue {
  if (!isObject(result)) return result
  const { values, records, ...facts } = result
  return { ...facts, ...(Array.isArray(values) ? { values: values.length } : {}), ...(Array.isArray(records) ? { records: records.length } : {}) }
}

/** Drives an already-owned process engine; never constructs a manager or OS wake. */
export class ProcessContinuationScenario extends ScenarioController<JsonObject> {
  readonly id = 'process-continuation'
  readonly title = 'Process native continuation'
  readonly description = 'Run native collection on the trusted host process owner. Survives renderer loss, not host-process exit; no OS wake registration.'
  private active: Promise<unknown> | null = null
  private stopping: Promise<ScenarioStopOutcome> | null = null
  private owned = false
  private lastClaim: JsonValue = null
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    execute: defineCommand({ label: 'Execute', description: 'Explicit peerId and H10 measurements; recordingId requires explicit maxBytes/maxRecords. Uses the existing host central.',
      parse: raw => {
        const peerId = requiredText(raw, 'peerId')
        const configured = recipe({ ...raw, measurements: raw.measurements === undefined ? 'hr' : raw.measurements, peerId })
        if (configured === null) throw new ScenarioError('scenario.invalid-continuation', 'H10 recipe required')
        const journal = recording(raw)
        return { ...configured, ...(journal === undefined ? {} : { recording: journal }) }
      }, run: declaration => this.exclusive(async () => {
        const controller = this.controller()
        if (this.owned || await controller.status() !== null) throw new ScenarioError('scenario.busy', 'Claim the existing process continuation before executing another order')
        this.setOwned(true)
        return toJsonValue(await controller.execute(declaration))
      }) }),
    status: defineCommand({ label: 'Status', description: 'Read actual process engine state, including after renderer reload.', parse: args.none, run: async () => toJsonValue(await this.controller().status()) }),
    claim: defineCommand({ label: 'Claim', description: 'Decode the volatile backlog and release its native session; failed disposal remains retryable. Does not acknowledge durable records.', parse: args.none, run: () => this.exclusive(() => this.claim()), summarizeResult: summary }),
    stop: defineCommand({ label: 'Stop and claim', description: 'Same explicit decoded handoff as claim; retain returned values. Durable journal is not cleared or acknowledged.', parse: args.none, run: () => this.exclusive(() => this.claim()), summarizeResult: summary }),
    'last-claim': defineCommand({ label: 'Retained claim', description: 'Read the last decoded handoff retained by this scenario, including values from a failed cleanup.', parse: args.none, run: async () => this.lastClaim, summarizeResult: summary }),
    ...continuationRecordingCommands(() => this.controller().recordings())
  }

  private readonly host: DriverHost
  constructor(host: DriverHost) { super(host.runtime, { owned: null }); this.host = host }

  /** Local cleanup obligation, not proof that native acquisition succeeded. */
  private setOwned(owned: boolean): void {
    this.owned = owned
    this.patch({ owned })
  }

  private controller(): HostNativeContinuation {
    if (this.host.nativeContinuation === undefined) throw new ScenarioError('capability.unsupported', 'Host has no trusted process continuation owner')
    return this.host.nativeContinuation
  }

  private exclusive(work: () => Promise<JsonValue>): Promise<JsonValue> {
    if (this.active !== null || this.stopping !== null) return Promise.reject(new ScenarioError('scenario.busy', 'Process continuation operation in progress'))
    const active = Promise.resolve().then(work)
    this.active = active
    return active.finally(() => { this.active = null })
  }

  private async claim(): Promise<JsonValue> {
    const controller = this.controller()
    this.setOwned(true)
    const result = await controller.claim()
    this.lastClaim = toJsonValue(result)
    this.setOwned(!result.disposed)
    // An untouched engine answers an empty claim with disposed:false: no
    // session was disposed. Only the owner's current null status establishes
    // absence; neither an empty queue nor this renderer's local state does.
    if (!result.disposed && result.disposeFailure === null && result.selectors.length === 0 &&
      result.values.length === 0 && result.streamEnds.length === 0 && result.control.length === 0 &&
      result.controlLost === 0 && result.afterCutoffLoss.items === 0 && result.afterCutoffLoss.bytes === 0) {
      this.setOwned(await controller.status() !== null)
    }
    this.patch({ lastClaim: summary(this.lastClaim) })
    return this.lastClaim
  }

  override stop(): Promise<ScenarioStopOutcome> {
    if (this.stopping !== null) return this.stopping
    const stopping = this.stopOwned()
    this.stopping = stopping
    return stopping.finally(() => { this.stopping = null })
  }

  private async stopOwned(): Promise<ScenarioStopOutcome> {
    // Observe the pending command's failure here; its own caller retains it.
    if (this.active !== null) await this.active.then(() => undefined, () => undefined)
    if (this.host.nativeContinuation === undefined) return { wasRunning: false, cleanup: [] }
    if (!this.owned && await this.controller().status() === null) return { wasRunning: false, cleanup: [] }
    await this.claim()
    // A finite CLI can exit immediately after registry shutdown. Its receipt
    // must carry the actual handoff, not only an in-memory retrieval pointer.
    return { wasRunning: true, cleanup: [{ step: 'native-continuation.claim', state: this.owned ? 'release-failed' : 'released', detail: this.lastClaim }] }
  }
}
