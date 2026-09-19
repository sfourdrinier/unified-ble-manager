// examples-shared/driver/scenarios/restoration.ts
//
// Known-peer restoration (issue #212): connect to the strap, subscribe to
// heart rate, and record the peer id the platform will restore after the
// process dies. The scenario then stops — the operator kills the app with
// the platform procedure in docs/BACKGROUND.md (never a user force-quit on
// iOS, never an Android force-stop), relaunches, and runs `reconnect` with
// the recorded peer id. The reconnect dials that id directly (no scan) with
// intent "when-available" and subscribes again, so both phones prove the
// same public events with the app doing the reconnecting — the library
// never auto-reconnects by itself.
//
// The platform's own restoration/presence answers are reported verbatim:
// `state:restoration-adoption` and `state:presence-observation` as the
// backend's capability registry answers them (or `unregistered` where the
// host registers neither), never invented by the scenario.

import type { BleManager, ConnectionIntent, FeatureId } from 'unified-ble-manager'
import type { DriverHost, HostManager } from '../host.ts'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { toJsonValue } from '../protocol.ts'
import { ScenarioError, args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import { DEVICE_ARGUMENT_HELP, OPERATION_TIMEOUT_MS } from './ble-scenario.ts'
import {
  HeartRateScenario,
  IDLE_HEART_RATE_STATE,
  parseHeartRateOptions,
  type HeartRateOptions,
  type HeartRateState
} from './heart-rate.ts'

export type RestorationState = HeartRateState & {
  readonly knownPeerId: string | null
  readonly adoptionCapability: string
  readonly presenceCapability: string
  readonly reconnects: number
}

const RESTORATION_ADOPTION_FEATURE: FeatureId = 'state:restoration-adoption'
const RESTORATION_PRESENCE_FEATURE: FeatureId = 'state:presence-observation'

function capabilityState(manager: BleManager, feature: FeatureId): string {
  return manager.capabilities.get(feature)?.state ?? 'unregistered'
}

function restorationCapabilities(manager: BleManager): {
  readonly adoption: JsonValue
  readonly presence: JsonValue
} {
  return {
    adoption: toJsonValue(manager.capabilities.get(RESTORATION_ADOPTION_FEATURE) ?? null),
    presence: toJsonValue(manager.capabilities.get(RESTORATION_PRESENCE_FEATURE) ?? null)
  }
}

export class RestorationScenario extends HeartRateScenario<RestorationState> {
  readonly id = 'restoration'
  readonly title = 'Restoration / known peer'
  readonly description =
    'Connect, subscribe, and record the known peer id; then kill the app with the docs/BACKGROUND.md procedure, relaunch, and reconnect directly to that id with intent "when-available". Same public events on every host; the app reconnects, the library never does.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    start: defineCommand({
      label: 'Start',
      description: `Find the strap, connect, subscribe, record the known peer id. args: {autoReconnect?: boolean (default false), intent?: "direct" | "when-available", ${DEVICE_ARGUMENT_HELP}}. Stop, kill the app per docs/BACKGROUND.md, relaunch, then run reconnect with the recorded peer id.`,
      presets: [{ label: 'Start (supervised)', args: {} }],
      acceptsDevice: true,
      parse: raw => parseHeartRateOptions(raw, { autoReconnect: false, intent: 'direct' }),
      run: options => this.startRestoration(options)
    }),
    reconnect: defineCommand({
      label: 'Reconnect',
      description:
        'Dial a recorded known peer id directly (no scan) and subscribe again. args: {peerId: string (required, from a previous start), intent?: "direct" | "when-available" (default "when-available")}. Refused without a peer id.',
      presets: [],
      parse: raw => ({
        intent: args.oneOf<ConnectionIntent>(raw, 'intent', ['direct', 'when-available'], 'when-available'),
        peerId: args.optionalString(raw, 'peerId')
      }),
      run: options => this.reconnectKnownPeer(options)
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, {
      ...IDLE_HEART_RATE_STATE,
      knownPeerId: null,
      adoptionCapability: 'unregistered',
      presenceCapability: 'unregistered',
      reconnects: 0
    })
  }

  override headline(): string | null {
    const { knownPeerId, reconnects, valueCount } = this.snapshot()
    if (knownPeerId === null) return `${valueCount.toString()} values · no known peer`
    return `${knownPeerId} · ${valueCount.toString()} values · ${reconnects.toString()} reconnects`
  }

  private patchRestoration(patch: Partial<RestorationState>): void {
    this.replace({ ...this.snapshot(), ...patch })
  }

  protected override async afterManagerReady(hosted: HostManager): Promise<void> {
    const manager = hosted.manager
    const capabilities = restorationCapabilities(manager)
    this.patchRestoration({
      adoptionCapability: capabilityState(manager, RESTORATION_ADOPTION_FEATURE),
      presenceCapability: capabilityState(manager, RESTORATION_PRESENCE_FEATURE)
    })
    this.emit('restoration-capabilities', capabilities)
  }

  private async startRestoration(options: HeartRateOptions): Promise<JsonObject> {
    const result = await this.startHeartRate(options)
    const peer = this.peerReport()
    const knownPeerId = peer?.id ?? null
    this.patchRestoration({ knownPeerId })
    if (knownPeerId !== null) {
      this.emit('restoration-known-peer', { peerId: knownPeerId, connectionGeneration: result.connectionGeneration ?? null })
    }
    return { ...result, knownPeerId }
  }

  private async reconnectKnownPeer(options: {
    readonly intent: ConnectionIntent
    readonly peerId: string | null
  }): Promise<JsonObject> {
    const peerId = options.peerId
    if (peerId === null || peerId.length === 0) {
      throw new ScenarioError(
        'scenario.no-known-peer',
        'reconnect needs a recorded known peer id (peerId); run "start" first, then pass its peer id after relaunch'
      )
    }
    return this.runJourney(async signal => {
      const hosted = await this.createManager(signal)
      await this.afterManagerReady(hosted)
      const manager = hosted.manager
      this.patchBase({ phase: 'connecting' })
      const startedAt = this.runtime.now()
      // The known peer id goes straight to connect: no scan, no chooser.
      // A failure is the library's typed error and ends the run here.
      const connection = await manager.connect(peerId, { signal, timeoutMs: OPERATION_TIMEOUT_MS, intent: options.intent })
      this.own('connection.release', () => connection.release())
      this.emit('connected', {
        connectionGeneration: connection.connectionGeneration,
        intent: options.intent,
        attempt: 1,
        connectMs: this.runtime.now() - startedAt
      })
      await this.configureLink(connection, signal)
      this.patchBase({ phase: 'streaming' })
      const snapshot = this.snapshot()
      this.patchRestoration({ knownPeerId: peerId, reconnects: snapshot.reconnects + 1 })
      this.emit('restoration-reconnected', {
        peerId,
        intent: options.intent,
        connectionGeneration: connection.connectionGeneration
      })
      return {
        peerId,
        intent: options.intent,
        connectionGeneration: connection.connectionGeneration,
        reconnects: snapshot.reconnects + 1
      }
    })
  }
}


