import type { BlePeer } from 'unified-ble-manager'
import { HEART_RATE_SERVICE } from 'unified-ble-manager/profiles/heart-rate'
import type { DriverHost, HostManager } from '../host.ts'
import { toJsonValue } from '../protocol.ts'
import { args, defineCommand, ScenarioError, type ScenarioCommand } from '../scenario-core.ts'
import { BleScenario, IDLE_BLE_STATE, type BleScenarioState } from './ble-scenario.ts'

export class AccessoryChooserScenario extends BleScenario<BleScenarioState> {
  readonly id = 'accessory-chooser'
  readonly title = 'System accessory chooser'
  readonly description =
    'Public native/browser chooser, explicit connect-selected, and owned cancellation. Selection is not a scan, connection or OS relaunch.'
  private selected: { hosted: HostManager; peer: BlePeer; signal: AbortSignal } | null = null
  private connecting = false
  protected override readonly stopCommand = defineCommand({
    label: 'Cancel / release',
    description:
      'Abort pending picker and release this selection/connection/manager, preserving failed cleanup for retry.',
    parse: args.none,
    run: async () => toJsonValue(await this.stop())
  })
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    choose: defineCommand({
      label: 'Choose SIM H10',
      description:
        'Foreground system picker. {namePrefix?: string (default SIM Polar H10), timeoutMs?: integer 1..60000}. Returns authorized selection only; connect separately.',
      presets: [
        { label: 'Choose SIM H10', args: {} },
        { label: '3-second picker deadline', args: { timeoutMs: 3000 } }
      ],
      parse: raw => {
        const timeoutMs = args.number(raw, 'timeoutMs', 30000, { min: 1, max: 60000 })
        if (!Number.isSafeInteger(timeoutMs))
          throw new ScenarioError('scenario.invalid-argument', 'timeoutMs must be an integer')
        return { namePrefix: args.optionalString(raw, 'namePrefix') ?? 'SIM Polar H10', timeoutMs }
      },
      run: ({ namePrefix, timeoutMs }) =>
        this.runJourney(async signal => {
          this.selected = null
          const appState = this.host.appState?.current() ?? null
          this.emit('chooser-app-state', { appState: toJsonValue(appState) })
          if (appState?.foreground === false)
            throw new ScenarioError(
              'scenario.foreground-required',
              'Bring the app to the foreground before requesting a system picker'
            )
          const hosted = await this.createManager(signal, false)
          const { manager } = hosted
          this.emit('chooser-capability', {
            capability: toJsonValue(manager.capabilities.get('discovery:system-chooser'))
          })
          if (this.host.userGesture !== null) {
            this.patchBase({ phase: 'awaiting-user-gesture' })
            this.emit('user-gesture-required', { reason: 'system accessory chooser' })
            await this.host.userGesture.request(this.id, 'system accessory chooser', signal)
          }
          this.patchBase({ phase: 'choosing' })
          const peer = await manager.choose({
            filters: [{ serviceUuids: [HEART_RATE_SERVICE], localNamePrefix: namePrefix }],
            timeoutMs,
            signal
          })
          if (signal.aborted)
            throw new ScenarioError('scenario.operation-aborted', 'Late selected peer belongs to a stopped run')
          this.selected = { hosted, peer, signal }
          this.patchBase({
            phase: 'selected',
            device: peer.name ?? peer.id,
            peer: { id: peer.id, name: peer.name, query: null }
          })
          const result = { peer: toJsonValue(peer), connection: 'not-requested', relaunch: 'not-observed' }
          this.emit('chooser-selected', result)
          return result
        })
    }),
    'connect-selected': defineCommand({
      label: 'Connect selected',
      description: 'Connect and discover the exact peer from this still-owned selection; no scan or second manager.',
      parse: args.none,
      run: async () => {
        const selected = this.selected
        if (selected === null || !this.isRunning())
          throw new ScenarioError(
            'scenario.no-selected-peer',
            'Choose a peer first; stopped selections cannot be reused'
          )
        if (this.connecting || this.snapshot().phase === 'connected')
          throw new ScenarioError('scenario.busy', 'The selected peer already has a connection acquisition')
        this.connecting = true
        try {
          await selected.hosted.prepare((kind, data) => this.emit(kind, data), selected.signal)
          const connection = await this.connect(selected.hosted.manager, selected.peer, selected.signal)
          const database = await this.discover(connection, selected.signal)
          this.patchBase({ phase: 'connected' })
          return {
            peer: toJsonValue(selected.peer),
            connectionGeneration: connection.connectionGeneration,
            databaseGeneration: database.generation,
            services: toJsonValue(database.services.map(service => service.uuid))
          }
        } catch (error) {
          await this.failRun(error)
          throw error
        } finally {
          this.connecting = false
        }
      }
    }),
    cancel: this.stopCommand,
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, IDLE_BLE_STATE)
  }

  override async stop() {
    const outcome = await super.stop()
    this.selected = null
    return outcome
  }
}
