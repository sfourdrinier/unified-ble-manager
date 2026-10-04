import type { BleManager, BlePeer } from 'unified-ble-manager'
import type { DriverHost } from '../host.ts'
import { toJsonValue, type JsonObject } from '../protocol.ts'
import { args, defineCommand, ScenarioError, type ScenarioCommand } from '../scenario-core.ts'
import {
  BleScenario,
  DEVICE_ARGUMENT_HELP,
  IDLE_BLE_STATE,
  parseDevice,
  withTimeout,
  type BleScenarioState
} from './ble-scenario.ts'

function parseBudget(raw: JsonObject) {
  const timeoutMs = args.number(raw, 'timeoutMs', 20000, { min: 1, max: 60000 })
  if (!Number.isSafeInteger(timeoutMs))
    throw new ScenarioError('scenario.invalid-argument', 'timeoutMs must be an integer')
  return timeoutMs
}

type Selection = { readonly manager: BleManager; readonly peer: BlePeer; readonly signal: AbortSignal }

/** Acceptance uses the ordinary host factory and an observed peer. It never
 * fabricates a peer id, requests custom credentials, or unpairs during cleanup. */
export class SecurityScenario extends BleScenario<BleScenarioState> {
  readonly id = 'security'
  readonly title = 'Public security / pairing'
  readonly description =
    'Select an actual peer once, then inspect/pair/watch/cancel explicitly through the same public manager. Unpair requires confirmation; Stop only releases ownership.'
  private selected: Selection | null = null
  private watching = false

  private selection() {
    if (this.selected === null || this.selected.signal.aborted || !this.isRunning())
      throw new ScenarioError('scenario.no-selected-peer', 'Select a still-owned discovered peer first')
    return this.selected
  }

  private async report(operation: string, pending: Promise<unknown>, selected: Selection, timeoutMs: number) {
    const result = await withTimeout(pending, timeoutMs, selected.signal, 'scenario.security-timeout', operation)
    if (selected.signal.aborted || this.selected !== selected)
      throw new ScenarioError('operation.aborted', 'Security operation belongs to a stopped selection')
    const report = { peer: toJsonValue(selected.peer), result: toJsonValue(result) }
    this.emit(operation, report)
    return report
  }

  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    select: defineCommand({
      label: 'Select security peer',
      description: `Discover one real peer through this host's public factory; no connection or pairing. {${DEVICE_ARGUMENT_HELP}}.`,
      acceptsDevice: true,
      presets: [{ label: 'Select SIM H10', args: { device: 'SIM Polar H10*' } }],
      parse: parseDevice,
      run: device =>
        this.runJourney(async signal => {
          this.selected = null
          const { manager } = await this.createManager(signal)
          const peer = await this.findH10(manager, device, signal)
          if (signal.aborted) throw new ScenarioError('operation.aborted', 'Security selection stopped')
          this.selected = { manager, peer, signal }
          this.patchBase({ phase: 'selected' })
          return { peer: toJsonValue(peer), securityRequested: false }
        })
    }),
    state: defineCommand({
      label: 'Read security state',
      description: 'Native state of the selected peer, preserving unknown/unsupported. timeoutMs?: integer1..60000.',
      parse: parseBudget,
      run: timeoutMs => {
        const selected = this.selection()
        return this.report(
          'security-state',
          selected.manager.security.state(selected.peer, { signal: selected.signal, timeoutMs }),
          selected,
          timeoutMs
        )
      }
    }),
    pair: defineCommand({
      label: 'Pair using system ceremony',
      description:
        'Explicit system pairing; native outcome is retained, never inferred from requesting it. timeoutMs?: integer1..60000.',
      parse: parseBudget,
      run: timeoutMs => {
        const selected = this.selection()
        return this.report(
          'security-pair',
          selected.manager.security.pair(selected.peer, { ceremony: 'system', signal: selected.signal, timeoutMs }),
          selected,
          timeoutMs
        )
      }
    }),
    'cancel-pairing': defineCommand({
      label: 'Cancel pairing',
      description: 'Explicit native cancel of the selected peer pairing. timeoutMs?: integer1..60000.',
      parse: parseBudget,
      run: timeoutMs => {
        const selected = this.selection()
        return this.report(
          'security-cancel-pairing',
          selected.manager.security.cancelPairing(selected.peer, { signal: selected.signal, timeoutMs }),
          selected,
          timeoutMs
        )
      }
    }),
    unpair: defineCommand({
      label: 'Unpair explicitly',
      description:
        'Destructive native bond removal of the selected peer only. Requires confirm:true; timeoutMs?: integer1..60000. Never called by Stop.',
      presets: [],
      parse: raw => {
        if (!args.boolean(raw, 'confirm', false))
          throw new ScenarioError('scenario.invalid-argument', 'Unpair requires confirm:true')
        return parseBudget(raw)
      },
      run: timeoutMs => {
        const selected = this.selection()
        return this.report(
          'security-unpair',
          selected.manager.security.unpair(selected.peer, { signal: selected.signal, timeoutMs }),
          selected,
          timeoutMs
        )
      }
    }),
    watch: defineCommand({
      label: 'Watch security state',
      description:
        'Bounded native watch; timeoutMs?: integer1..60000, maxEvents?: integer1..16 (default1). Stop cancels and releases its iterator, not the bond.',
      parse: raw => {
        const maxEvents = args.number(raw, 'maxEvents', 1, { min: 1, max: 16 })
        if (!Number.isSafeInteger(maxEvents))
          throw new ScenarioError('scenario.invalid-argument', 'maxEvents must be an integer')
        return { timeoutMs: parseBudget(raw), maxEvents }
      },
      run: async ({ timeoutMs, maxEvents }) => {
        const selected = this.selection()
        if (this.watching) throw new ScenarioError('scenario.busy', 'A security watch is already active')
        const iterator = selected.manager.security.watch(selected.peer)[Symbol.asyncIterator]()
        this.watching = true
        const ownership = { returned: false, started: false, pending: Promise.resolve() }
        const release = async () => {
          if (ownership.returned) return { state: 'released', failures: [] }
          if (!ownership.started) {
            ownership.started = true
            ownership.pending = Promise.resolve().then(async () => {
              try {
                await iterator.return?.()
                ownership.returned = true
              } catch (error) {
                ownership.started = false
                throw error
              }
            })
          }
          await withTimeout(
            ownership.pending,
            1000,
            new AbortController().signal,
            'scenario.security-cleanup-timeout',
            'Security watch release'
          )
          return { state: 'released', failures: [] }
        }
        this.own('security.watch.return', release)
        const deadline = this.runtime.now() + timeoutMs
        const events = []
        const failures = new Array<unknown>()
        try {
          while (events.length < maxEvents) {
            const remaining = deadline - this.runtime.now()
            if (remaining <= 0)
              throw new ScenarioError('scenario.security-watch-timeout', 'Security watch deadline expired')
            const item = await withTimeout(
              iterator.next(),
              remaining,
              selected.signal,
              'scenario.security-watch-timeout',
              'Security state event'
            )
            if (selected.signal.aborted) throw new ScenarioError('operation.aborted', 'Security watch stopped')
            if (item.done)
              throw new ScenarioError(
                'scenario.security-watch-terminal',
                'Security watch ended before the requested events'
              )
            events.push(item.value)
            this.emit('security-watch-event', { event: toJsonValue(item.value) })
          }
          return { peer: toJsonValue(selected.peer), events: toJsonValue(events) }
        } catch (error) {
          failures.push(error)
          throw error
        } finally {
          this.watching = false
          try {
            await release()
          } catch (cleanupError) {
            if (failures.length > 0)
              throw new AggregateError([...failures, cleanupError], 'Security watch and cleanup failed')
            throw cleanupError
          }
        }
      }
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, IDLE_BLE_STATE)
  }
}
