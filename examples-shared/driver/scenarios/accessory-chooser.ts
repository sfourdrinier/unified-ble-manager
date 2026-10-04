import type { BleConnection, BlePeer, GattDatabase, ChooseFilter } from 'unified-ble-manager'
import {
  HEART_RATE_SERVICE,
  HEART_RATE_MEASUREMENT_CHARACTERISTIC,
  parseHeartRateMeasurement
} from 'unified-ble-manager/profiles/heart-rate'
import type { DriverHost, HostManager } from '../host.ts'
import { isJsonObject, toJsonValue, type JsonObject } from '../protocol.ts'
import { args, defineCommand, ScenarioError, type ScenarioCommand } from '../scenario-core.ts'
import { BleScenario, IDLE_BLE_STATE, withTimeout, type BleScenarioState } from './ble-scenario.ts'

function parseManufacturer(raw: JsonObject) {
  const hasCompany = raw.manufacturerCompanyIdentifier !== undefined
  const hasPrefix = raw.manufacturerPrefix !== undefined
  if (hasCompany !== hasPrefix)
    throw new ScenarioError(
      'scenario.invalid-argument',
      'manufacturerCompanyIdentifier and manufacturerPrefix must be paired'
    )
  if (!hasCompany) return undefined
  const companyIdentifier = args.number(raw, 'manufacturerCompanyIdentifier', -1, { min: 0, max: 65535 })
  if (!Number.isSafeInteger(companyIdentifier))
    throw new ScenarioError('scenario.invalid-argument', 'manufacturerCompanyIdentifier must be an integer')
  const prefix = raw.manufacturerPrefix
  if (!Array.isArray(prefix) || prefix.length === 0)
    throw new ScenarioError('scenario.invalid-argument', 'manufacturerPrefix must be a nonempty byte array')
  const bytes = prefix.map(value => {
    if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0 || value > 255)
      throw new ScenarioError('scenario.invalid-argument', 'manufacturerPrefix entries must be bytes')
    return value
  })
  return [{ companyIdentifier, dataPrefix: new Uint8Array(bytes) }]
}

/** JSON representation mapping only; public manager.choose owns UUID and selector semantics. */
function parseAlternative(value: unknown): ChooseFilter {
  if (
    !isJsonObject(value) ||
    Object.keys(value).length === 0 ||
    Object.keys(value).some(
      key => !['serviceUuids', 'localNamePrefix', 'manufacturerCompanyIdentifier', 'manufacturerPrefix'].includes(key)
    )
  )
    throw new ScenarioError(
      'scenario.invalid-argument',
      'alternative filter must contain only supported selector fields'
    )
  const services = value.serviceUuids
  if (services !== undefined && !Array.isArray(services))
    throw new ScenarioError('scenario.invalid-argument', 'alternative serviceUuids must be an array')
  const serviceUuids = services?.map(uuid => {
    if (typeof uuid !== 'string' && (typeof uuid !== 'number' || !Number.isFinite(uuid)))
      throw new ScenarioError('scenario.invalid-argument', 'alternative service UUID must be a string or finite number')
    return uuid
  })
  if (value.localNamePrefix !== undefined && typeof value.localNamePrefix !== 'string')
    throw new ScenarioError('scenario.invalid-argument', 'alternative localNamePrefix must be a string')
  const manufacturerData = parseManufacturer(value)
  return {
    ...(serviceUuids === undefined ? {} : { serviceUuids }),
    ...(value.localNamePrefix === undefined ? {} : { localNamePrefix: value.localNamePrefix }),
    ...(manufacturerData === undefined ? {} : { manufacturerData })
  }
}

export class AccessoryChooserScenario extends BleScenario<BleScenarioState> {
  readonly id = 'accessory-chooser'
  readonly title = 'System accessory chooser'
  readonly description =
    'Public native/browser chooser, explicit connect-selected, and owned cancellation. Selection is not a scan, connection or OS relaunch.'
  private selected: { hosted: HostManager; peer: BlePeer; signal: AbortSignal } | null = null
  private connecting = false
  private connected: { connection: BleConnection; database: GattDatabase } | null = null
  private sampling = false
  private pendingSubscriptions = 0
  protected override readonly stopCommand = defineCommand({
    label: 'Cancel / release',
    description:
      'Abort pending picker and release this selection/connection/manager, preserving failed cleanup for retry.',
    parse: args.none,
    run: async () => toJsonValue(await this.stop())
  })
  private chooserCommand(inactiveProbe: boolean): ScenarioCommand {
    return defineCommand({
      label: inactiveProbe ? 'Probe native inactive refusal' : 'Choose SIM H10',
      description:
        (inactiveProbe
          ? 'Explicit inactive native-refusal probe; requires observed inactive state and reaches public manager.choose without the ordinary foreground precheck. Unexpected selection fails and releases its owner. '
          : 'Foreground system picker; selection only, connect separately. ') +
        '{namePrefix?: string (default SIM Polar H10), timeoutMs?: integer 1..60000, manufacturerCompanyIdentifier?: uint16, manufacturerPrefix?: nonempty byte array, alternativeFilters?: array of serviceUuids/localNamePrefix/paired manufacturer selectors}. Alternatives append OR branches to the default conjunction. Manufacturer arguments must be paired and captured from the actual advertisement.',
      presets: [
        { label: 'Choose SIM H10', args: {} },
        { label: '3-second picker deadline', args: { timeoutMs: 3000 } }
      ],
      parse: raw => {
        const timeoutMs = args.number(raw, 'timeoutMs', 30000, { min: 1, max: 60000 })
        if (!Number.isSafeInteger(timeoutMs))
          throw new ScenarioError('scenario.invalid-argument', 'timeoutMs must be an integer')
        const manufacturerData = parseManufacturer(raw)
        const alternatives = raw.alternativeFilters
        if (alternatives !== undefined && !Array.isArray(alternatives))
          throw new ScenarioError('scenario.invalid-argument', 'alternativeFilters must be an array')
        return {
          namePrefix: args.optionalString(raw, 'namePrefix') ?? 'SIM Polar H10',
          timeoutMs,
          manufacturerData,
          alternativeFilters: alternatives === undefined ? [] : alternatives.map(parseAlternative)
        }
      },
      run: ({ namePrefix, timeoutMs, manufacturerData, alternativeFilters }) => {
        if (this.sampling || this.connecting || this.pendingSubscriptions > 0)
          throw new ScenarioError('scenario.busy', 'A prior selected subscription is still settling')
        return this.runJourney(async signal => {
          this.selected = null
          this.connected = null
          const appState = this.host.appState?.current() ?? null
          this.emit('chooser-app-state', { appState: toJsonValue(appState) })
          if (inactiveProbe && appState?.foreground !== false)
            throw new ScenarioError(
              'scenario.inactive-required',
              'Native refusal probe requires observed inactive app state'
            )
          if (!inactiveProbe && appState?.foreground === false)
            throw new ScenarioError(
              'scenario.foreground-required',
              'Bring the app to the foreground before requesting a system picker'
            )
          const hosted = await this.createManager(signal, false)
          const { manager } = hosted
          this.emit('chooser-capability', {
            capability: toJsonValue(manager.capabilities.get('discovery:system-chooser'))
          })
          if (!inactiveProbe && this.host.userGesture !== null) {
            this.patchBase({ phase: 'awaiting-user-gesture' })
            this.emit('user-gesture-required', { reason: 'system accessory chooser' })
            await this.host.userGesture.request(this.id, 'system accessory chooser', signal)
          }
          this.patchBase({ phase: 'choosing' })
          const peer = await manager.choose({
            filters: [
              {
                serviceUuids: [HEART_RATE_SERVICE],
                localNamePrefix: namePrefix,
                ...(manufacturerData === undefined ? {} : { manufacturerData })
              },
              ...alternativeFilters
            ],
            timeoutMs,
            signal
          })
          if (inactiveProbe) {
            this.emit('chooser-inactive-unexpected-selection', { peer: toJsonValue(peer) })
            throw new ScenarioError(
              'scenario.expected-refusal',
              'Inactive public chooser unexpectedly returned a selection'
            )
          }
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
      }
    })
  }
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    choose: this.chooserCommand(false),
    'probe-native-inactive-refusal': this.chooserCommand(true),
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
          if (selected.signal.aborted)
            throw new ScenarioError('operation.aborted', 'Selection stopped during discovery')
          this.connected = { connection, database }
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
    'sample-selected-hr': defineCommand({
      label: 'Sample selected HRS',
      description:
        'One positive HRS notification on the chooser-owned connection; no scan/new manager. timeoutMs?: integer1..20000 (default5000), includes subscribe and value wait.',
      parse: raw => {
        const timeoutMs = args.number(raw, 'timeoutMs', 5000, { min: 1, max: 20000 })
        if (!Number.isSafeInteger(timeoutMs))
          throw new ScenarioError('scenario.invalid-argument', 'timeoutMs must be an integer')
        return timeoutMs
      },
      run: async timeoutMs => {
        const selected = this.selected
        const connected = this.connected
        if (selected === null || connected === null || !this.isRunning() || selected.signal.aborted)
          throw new ScenarioError('scenario.no-selected-peer', 'Connect the still-owned chooser selection first')
        if (this.sampling) throw new ScenarioError('scenario.busy', 'A selected HRS sample is already pending')
        this.sampling = true
        const deadline = this.runtime.now() + timeoutMs
        try {
          const characteristic = connected.database.characteristic(
            HEART_RATE_SERVICE,
            HEART_RATE_MEASUREMENT_CHARACTERISTIC
          )
          this.pendingSubscriptions += 1
          const acquisition = (async () => {
            try {
              const subscription = await characteristic.subscribe({
                signal: selected.signal,
                timeoutMs,
                stream: 'lossless-bounded',
                delivery: 'prefer-notification'
              })
              let removalConfirmed = false
              const remove = async () => {
                if (removalConfirmed) return { state: 'released', failures: [] }
                const cleanup = await subscription.remove()
                removalConfirmed = cleanup.state === 'released'
                return cleanup
              }
              this.own('chooser.hrs.subscription.remove', remove)
              return { subscription, remove }
            } finally {
              this.pendingSubscriptions -= 1
            }
          })()
          const { subscription, remove } = await withTimeout(
            acquisition,
            timeoutMs,
            selected.signal,
            'scenario.notification-timeout',
            'HRS sample acquisition'
          )
          if (selected.signal.aborted)
            throw new ScenarioError('operation.aborted', 'Selection stopped during subscription')
          const iterator = subscription.values[Symbol.asyncIterator]()
          this.own('chooser.hrs.iterator.return', async () => {
            await iterator.return?.()
            return { state: 'released', failures: [] }
          })
          const remaining = deadline - this.runtime.now()
          if (remaining <= 0) throw new ScenarioError('scenario.notification-timeout', 'HRS sample deadline expired')
          const item = await withTimeout(
            iterator.next(),
            remaining,
            selected.signal,
            'scenario.notification-timeout',
            'Positive HRS notification'
          )
          if (selected.signal.aborted)
            throw new ScenarioError('operation.aborted', 'Selection stopped during notification')
          if (item.done || item.value.kind !== 'value') {
            this.emit('chooser-hrs-terminal', { item: toJsonValue(item) })
            throw new ScenarioError('scenario.notification-terminal', 'HRS stream ended or overflowed before a value', {
              cause: item.done ? undefined : item.value
            })
          }
          const value = item.value.value
          const measurement = parseHeartRateMeasurement(value.value)
          if (measurement.beatsPerMinute <= 0)
            throw new ScenarioError('scenario.notification-invalid', 'HRS sample is not a positive heart rate')
          const result = {
            peerId: selected.peer.id,
            connectionGeneration: connected.connection.connectionGeneration,
            databaseGeneration: connected.database.generation,
            bytes: Array.from(value.value),
            bpm: measurement.beatsPerMinute,
            delivery: value.delivery,
            sequence: value.sequence,
            observedAtMonotonicMs: value.observedAtMonotonicMs
          }
          this.emit('chooser-hrs-value', result)
          await iterator.return?.()
          const cleanup = await remove()
          this.emit('chooser-hrs-cleanup', { cleanup: toJsonValue(cleanup) })
          if (cleanup.state !== 'released')
            throw new ScenarioError('scenario.cleanup-failed', 'HRS subscription cleanup refused; ownership retained')
          return result
        } catch (error) {
          await this.failRun(error)
          throw error
        } finally {
          this.sampling = false
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
    this.connected = null
    this.selected = null
    const outcome = await super.stop()
    return outcome
  }
}
