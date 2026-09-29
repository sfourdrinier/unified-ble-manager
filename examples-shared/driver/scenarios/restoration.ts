// examples-shared/driver/scenarios/restoration.ts
//
// Known-peer restoration (issue #212): connect to the strap, subscribe to
// heart rate, then stop it with the platform procedure in docs/BACKGROUND.md
// (never a user force-quit on iOS, never an Android force-stop). After a
// relaunch, `restored` reports the durable PeerReference the OS supplied.
// `reconnect` passes that reference to the fresh manager with direct intent,
// without a scan, then subscribes again. Android may instead opt into
// when-available with an explicit manager-local peer id. The app reconnects;
// the library never auto-reconnects by itself. This scenario tests the public
// path but does not itself establish physical restoration qualification.
//
// The platform's own restoration/presence answers are reported verbatim:
// `state:restoration-adoption` and `state:presence-observation` as the
// backend's capability registry answers them (or `unregistered` where the
// host registers neither), never invented by the scenario.

import {
  decodePeerReference,
  type BleManager,
  type ConnectionIntent,
  type FeatureId,
  type PeerReference
} from 'unified-ble-manager'
import type { DriverHost, HostManager } from '../host.ts'
import type { ExpoCompanionAssociationResult } from 'unified-ble-manager/expo'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { describeError, toJsonValue } from '../protocol.ts'
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

/** The Expo/RN presence surface the scenario arms; plain BleManager hosts lack it. */
interface PresenceObservationApi {
  readonly observe: (request: { readonly peerId: string }) => Promise<{ readonly state: 'observing' }>
  readonly unobserve: (request: { readonly peerId: string }) => Promise<{ readonly state: 'idle' }>
}

type ManagerWithPresence = BleManager & { readonly presence: PresenceObservationApi }

function hasPresenceApi(manager: BleManager): manager is ManagerWithPresence {
  return 'presence' in manager
}

function hasAssociationApi(manager: BleManager): manager is BleManager & {
  readonly association: { associate(request: { readonly name: string }): Promise<ExpoCompanionAssociationResult> }
} {
  return 'association' in manager
}

type AssociationTiming = {
  readonly state: 'deadline-expired'
  readonly budgetMs: number
  readonly elapsedMs: number
  readonly followUp: 'caller-decides'
}

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
    'Connect and subscribe; keep ownership active while backgrounding and terminating the process with the docs/BACKGROUND.md procedure. In the record-only baseline, the app reads restored peers and explicitly reconnects after relaunch. An opt-in native continuation order instead permits native reconnect/setup before JavaScript; test that separately.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    associate: defineCommand({
      label: 'Associate exact companion',
      description: 'Open the Android system consent chooser for an exact supplied name. Does not replace or remove existing associations. Stop releases the scenario manager.',
      parse: raw => {
        const name = args.optionalString(raw, 'name')
        if (name === null || name.length === 0 || name.trim() !== name || name.includes('*') || Object.keys(raw).some(key => key !== 'name')) {
          throw new ScenarioError('scenario.invalid-argument', 'associate requires one exact nonempty name (no wildcard)')
        }
        return name
      },
      run: name => this.associateCompanion(name)
    }),
    start: defineCommand({
      label: 'Start',
      description: `Find the strap, connect, subscribe, and record the manager-local peer id. args: {autoReconnect?: boolean (default false), intent?: "direct" | "when-available", ${DEVICE_ARGUMENT_HELP}}. Keep the subscription active; do not run Stop before the restoration test. Background normally, terminate the process per docs/BACKGROUND.md (never swipe-force-quit or Android force-stop), relaunch, run restored, then explicitly reconnect in the record-only baseline with its durable peerReference.`,
      presets: [{ label: 'Start (supervised)', args: {} }],
      acceptsDevice: true,
      parse: raw => parseHeartRateOptions(raw, { autoReconnect: false, intent: 'direct' }),
      run: options => this.startRestoration(options)
    }),
    reconnect: defineCommand({
      label: 'Reconnect',
      description:
        'Resolve a durable restored peerReference without scanning, then subscribe again. args: {peerReference: PeerReference (from restored), intent?: "direct" | "when-available" (default "direct")}. This command creates a fresh manager; previous manager peerId strings are refused.',
      presets: [],
      parse: raw => {
        if (raw.peerId !== undefined)
          throw new ScenarioError('scenario.invalid-argument', 'reconnect creates a fresh manager; pass a durable peerReference, not a manager-local peerId')
        return {
          intent: args.oneOf<ConnectionIntent>(raw, 'intent', ['direct', 'when-available'], 'direct'),
          peerReference: parsePeerReference(raw)
        }
      },
      run: options => this.reconnectKnownPeer(options)
    }),
    'observe-presence': defineCommand({
      label: 'Observe presence',
      description:
        'Arm cold-start presence observation for a known peer id. args: {peerId?: string (default: the peer recorded by start)}. Stop the run first, then arm before the docs/BACKGROUND.md kill step. Reports the owner\'s verbatim answer ({state: "observing"}); where the platform cannot observe, the owner\'s capability.unsupported error propagates unwrapped. Refused without a peer id.',
      presets: [],
      parse: raw => ({ peerId: args.optionalString(raw, 'peerId') }),
      run: options => this.setPresence(options, 'observe')
    }),
    'unobserve-presence': defineCommand({
      label: 'Unobserve presence',
      description:
        'Release cold-start presence observation for a known peer id. args: {peerId?: string (default: the peer recorded by start)}. Reports the owner\'s verbatim answer ({state: "idle"}); where the platform cannot observe, the owner\'s capability.unsupported error propagates unwrapped. Refused without a peer id.',
      presets: [],
      parse: raw => ({ peerId: args.optionalString(raw, 'peerId') }),
      run: options => this.setPresence(options, 'unobserve')
    }),
    restored: defineCommand({
      label: 'Restored peers',
      description:
        'Query the public restored-peers directory (manager.peers.restored()) and report each restored peer verbatim (id, name, and whatever the record carries). Answers explicitly with an empty list when there are none; where the platform cannot answer, the owner\'s capability.unsupported error propagates unwrapped.',
      presets: [{ label: 'Restored peers', args: {} }],
      parse: args.none,
      run: () => this.queryRestoredPeers()
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

  private async associateCompanion(name: string): Promise<JsonObject> {
    const result = await this.runOneShotJourney(async signal => {
      const { manager } = await this.createManager(signal)
      if (!hasAssociationApi(manager)) throw new ScenarioError('capability.unsupported', 'This host has no companion association API')
      const startedAtMs = this.runtime.now()
      return new Promise<ExpoCompanionAssociationResult & { readonly timing?: AssociationTiming }>((resolve, reject) => {
        let settled = false
        const finish = (answer: ExpoCompanionAssociationResult | null, error?: unknown) => {
          if (settled) {
            this.emit('association-late-result', { result: toJsonValue(answer), error: answer === null ? toJsonValue(describeError(error)) : null, followUp: 'caller-decides' })
            return
          }
          settled = true
          cancelDeadline()
          signal.removeEventListener('abort', cancelled)
          if (answer !== null) {
            const elapsedMs = this.runtime.now() - startedAtMs
            if (elapsedMs >= 60_000) {
              const timing: AssociationTiming = { state: 'deadline-expired', budgetMs: 60_000, elapsedMs, followUp: 'caller-decides' }
              this.emit('association-late-result', { result: toJsonValue(answer), timing: toJsonValue(timing), followUp: 'caller-decides', error: null })
              resolve({ ...answer, timing })
            } else resolve(answer)
          }
          else reject(error)
        }
        const cancelled = () => finish(null, new ScenarioError('operation.cancelled', 'Association scenario was stopped'))
        const cancelDeadline = this.runtime.schedule(() => finish(null, new ScenarioError('operation.timeout', 'Association chooser exceeded its 60-second budget')), 60_000)
        signal.addEventListener('abort', cancelled, { once: true })
        if (signal.aborted) cancelled()
        else Promise.resolve().then(() => manager.association.associate({ name })).then(answer => finish(answer), error => finish(null, error))
      })
    })
    return { ...result }
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

  private async setPresence(
    options: { readonly peerId: string | null },
    mode: 'observe' | 'unobserve'
  ): Promise<JsonObject> {
    const command = mode === 'observe' ? 'observe-presence' : 'unobserve-presence'
    // The default is captured before the journey resets the state to idle.
    const peerId = options.peerId ?? this.snapshot().knownPeerId
    if (peerId === null || peerId.length === 0) {
      throw new ScenarioError(
        'scenario.no-known-peer',
        `${command} needs a known peer id (peerId); run "start" first, then pass its peer id after relaunch`
      )
    }
    return this.runOneShotJourney(async signal => {
      const hosted = await this.createManager(signal)
      await this.afterManagerReady(hosted)
      const manager = hosted.manager
      if (!hasPresenceApi(manager)) {
        throw new ScenarioError(
          'scenario.presence-unavailable',
          'this host exposes no presence observation API; arm presence from an Expo/RN host'
        )
      }
      // The owner's answer is reported verbatim: {state: "observing"} (or
      // {state: "idle"}), or the owner's own error — capability.unsupported
      // on Apple — propagating unwrapped, never a wrapped fake.
      const state =
        mode === 'observe' ? (await manager.presence.observe({ peerId })).state : (await manager.presence.unobserve({ peerId })).state
      this.patchRestoration({ knownPeerId: peerId })
      this.emit(mode === 'observe' ? 'presence-observing' : 'presence-idle', { peerId, state })
      return { peerId, state }
    })
  }

  private async queryRestoredPeers(): Promise<JsonObject> {
    return this.runOneShotJourney(async signal => {
      const hosted = await this.createManager(signal)
      await this.afterManagerReady(hosted)
      // The owner's answer is reported verbatim: one entry per restored
      // peer (id, name, and whatever the record carries), an explicit
      // empty list when there are none — or the owner's own error,
      // capability.unsupported where the platform cannot answer,
      // propagating unwrapped, never a wrapped fake.
      const peers = await hosted.manager.peers.restored()
      const report = { count: peers.length, peers: peers.map(peer => toJsonValue(peer)) }
      this.emit('restoration-restored-peers', report)
      return { count: report.count, peers: report.peers }
    })
  }

  private async reconnectKnownPeer(options: {
    readonly intent: ConnectionIntent
    readonly peerReference: PeerReference | null
  }): Promise<JsonObject> {
    const target = options.peerReference
    if (target === null) {
      throw new ScenarioError(
        'scenario.no-known-peer',
        'reconnect needs a durable peerReference; run "restored" after relaunch, then pass its reference'
      )
    }
    return this.runJourney(async signal => {
      const hosted = await this.createManager(signal)
      await this.afterManagerReady(hosted)
      const manager = hosted.manager
      this.patchBase({ phase: 'connecting' })
      const startedAt = this.runtime.now()
      // A durable reference resolves through this fresh manager before connection.
      // Neither path scans or opens a chooser. A failure is the library's typed
      // error and ends the run here.
      const connection = await manager.connect(target, { signal, timeoutMs: OPERATION_TIMEOUT_MS, intent: options.intent })
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
      const peerId = connection.peer.id
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

function parsePeerReference(raw: JsonObject): PeerReference | null {
  const value = raw.peerReference
  if (value === undefined || value === null) return null
  try {
    return decodePeerReference(JSON.stringify(value))
  } catch {
    throw new ScenarioError('scenario.invalid-argument', 'argument "peerReference" must be a valid PeerReference')
  }
}
