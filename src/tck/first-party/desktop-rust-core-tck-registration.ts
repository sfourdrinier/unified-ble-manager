// src/tck/first-party/desktop-rust-core-tck-registration.ts
//
// The desktop first-party TCK legs run the Rust route (LEGACY-AUDIT-2 N3):
// the production desktop provider (`createDesktopRustCoreBackendProvider`)
// over the identity-verified N-API addon, on the addon's own deterministic
// synthetic radio. Every verb executes `DesktopCentral` in Rust; this module
// only stages the radio world (advertisements, the GATT database, OS events)
// through the addon's synthetic staging surface. No legacy backend, boundary
// or provider is involved, and nothing here is live-radio evidence.

import { contractError } from '../../backend-contract/errors'
import { capacity, opaqueId, type SerializableRecord } from '../../backend-contract/primitives'
import {
  createDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES,
  DesktopRustCoreBackend,
  desktopRustCoreAdapterId,
  type DesktopRustCoreProfile
} from '../../backends/desktop/desktop-rust-core-provider'
import {
  desktopRustCoreOperation,
  type DesktopRustCoreAdapterAuthorization,
  type DesktopRustCoreAdapterPower,
  type DesktopRustCoreBinding,
  type DesktopRustCoreCentral,
  type DesktopRustCorePlatform
} from '../../backends/desktop/desktop-rust-core-binding'
import { loadDesktopCoreBinding } from '../../desktop-core-addon'
import type {
  TckControllerAction,
  TckFeatureSuite,
  TckScenarioController,
  TckScenarioId,
  TckSecurityScenarioAdapter
} from '../contracts'
import type { FirstPartyBackendTckRegistration, FirstPartyTckCapabilityExclusion } from './first-party-tck-registry'

/** One advertisement the synthetic radio reports. */
export interface DesktopRustCoreSyntheticAdvertisement {
  readonly peerId: string
  readonly rssi?: number
  readonly localName?: string
  readonly serviceUuids?: readonly string[]
}

/** One notification the synthetic radio delivers on a characteristic instance. */
export interface DesktopRustCoreSyntheticNotification {
  readonly peerId: string
  readonly serviceUuid: string
  readonly serviceOccurrence?: number
  readonly characteristicUuid: string
  readonly characteristicOccurrence?: number
  readonly value: Uint8Array
}

/** One service of the GATT database the synthetic radio serves. */
export interface DesktopRustCoreSyntheticService {
  readonly uuid: string
  readonly occurrence: number
  readonly characteristics: ReadonlyArray<{
    readonly uuid: string
    readonly occurrence: number
    readonly properties: {
      readonly read: boolean
      readonly write: boolean
      readonly writeWithoutResponse: boolean
      readonly notify: boolean
      readonly indicate: boolean
    }
    readonly descriptors: ReadonlyArray<{ readonly uuid: string; readonly occurrence: number }>
  }>
}

/**
 * The staging surface of a synthetic `UbmCentral` (the addon's
 * `openSynthetic`): what the TCK controller drives. It is refused on a
 * production central, so it can never reach a real radio.
 */
export interface DesktopRustCoreSyntheticRadio {
  stageAdvertisement(input: DesktopRustCoreSyntheticAdvertisement): Promise<void>
  stageServices(peerId: string, services: readonly DesktopRustCoreSyntheticService[]): Promise<void>
  stageMtu(peerId: string, mtu: number): Promise<void>
  stageRssi(peerId: string, rssi: number): Promise<void>
  stageNotification(input: DesktopRustCoreSyntheticNotification): Promise<void>
  stageLinkLoss(peerId: string): Promise<void>
  stageServicesChanged(peerId: string): Promise<void>
  stageAdapterState(state: DesktopRustCoreAdapterPower, announce?: boolean): Promise<void>
  stageAdapterAuthorization(authorization: DesktopRustCoreAdapterAuthorization, announce?: boolean): Promise<void>
  stageSecurity(peerId: string, bond: 'bonded' | 'not-bonded', pairingPossible?: boolean): Promise<void>
  stagePairOutcome(
    peerId: string,
    outcome: 'paired' | 'already-paired' | 'rejected' | 'cancelled',
    reason?: string
  ): Promise<void>
  stageWriteReadiness(peerId: string, ready: boolean, announce?: boolean): Promise<void>
  blockRadioOp(op: string): Promise<void>
  stagedRadioCalls(): Promise<string[]>
}

/** Options every desktop Rust-route TCK registration takes. */
export interface DesktopRustCoreFirstPartyTckRegistrationOptions {
  /** Monotonic clock supplied by the host. */
  readonly now: () => number
  /**
   * The desktop core binding whose synthetic radio the legs drive. Absent,
   * the packaged addon is loaded and its build identity verified, exactly
   * as the production factories load it.
   */
  readonly binding?: DesktopRustCoreBinding
  /** The radio id the synthetic peer advertises under (`tck-peer` by default). */
  readonly nativePeerId?: string
}

/** The CoreBluetooth leg's options (the Rust route; `/testing` export name kept). */
export type CoreBluetoothFirstPartyTckRegistrationOptions = DesktopRustCoreFirstPartyTckRegistrationOptions
/** The BlueZ leg's options (the Rust route; `/testing` export name kept). */
export type BluezFirstPartyTckRegistrationOptions = DesktopRustCoreFirstPartyTckRegistrationOptions
/** The WinRT leg's options (the Rust route; `/testing` export name kept). */
export type WinRtFirstPartyTckRegistrationOptions = DesktopRustCoreFirstPartyTckRegistrationOptions
/** The deterministic radio the CoreBluetooth leg drives: the addon's synthetic central. */
export type DeterministicCoreBluetoothBoundary = DesktopRustCoreSyntheticRadio
/** The deterministic radio the BlueZ leg drives: the addon's synthetic central. */
export type DeterministicBluezTckBoundary = DesktopRustCoreSyntheticRadio
/** The deterministic radio the WinRT leg drives: the addon's synthetic central. */
export type DeterministicWinRtBoundary = DesktopRustCoreSyntheticRadio
/** A notification the BlueZ leg's synthetic radio delivers. */
export type BluezNotificationInput = DesktopRustCoreSyntheticNotification

const TCK_SERVICE_UUID = '0000180d-0000-1000-8000-00805f9b34fb'
const TCK_CHARACTERISTIC_UUID = '00002a37-0000-1000-8000-00805f9b34fb'
const TCK_USER_DESCRIPTION_UUID = '00002901-0000-1000-8000-00805f9b34fb'
const DEFAULT_NATIVE_PEER_ID = 'tck-peer'
const TCK_ATT_MTU = 247
const TCK_RSSI = -48
const SETTLE_POLL_MS = 5
const SETTLE_LIMIT_MS = 5_000

const TCK_BATTERY_SERVICE_UUID = '0000180f-0000-1000-8000-00805f9b34fb'
const TCK_BATTERY_LEVEL_UUID = '00002a19-0000-1000-8000-00805f9b34fb'

const readNotify = Object.freeze({
  read: true,
  write: false,
  writeWithoutResponse: false,
  notify: true,
  indicate: false
})

/**
 * The world the legacy legs served (one heart-rate service with one
 * notifiable characteristic and one descriptor, first in discovery order),
 * followed by the duplicate-UUID world: a second service UUID that repeats
 * (occurrence 1), a repeated characteristic UUID under one service, and a
 * repeated descriptor UUID under one characteristic. Occurrences are the
 * per-parent index of each repeated UUID in discovery order.
 */
const tckServices: readonly DesktopRustCoreSyntheticService[] = Object.freeze([
  Object.freeze({
    uuid: TCK_SERVICE_UUID,
    occurrence: 0,
    characteristics: Object.freeze([
      Object.freeze({
        uuid: TCK_CHARACTERISTIC_UUID,
        occurrence: 0,
        properties: Object.freeze({
          read: true,
          write: true,
          writeWithoutResponse: true,
          notify: true,
          indicate: false
        }),
        descriptors: Object.freeze([Object.freeze({ uuid: TCK_USER_DESCRIPTION_UUID, occurrence: 0 })])
      })
    ])
  }),
  Object.freeze({
    uuid: TCK_BATTERY_SERVICE_UUID,
    occurrence: 0,
    characteristics: Object.freeze([
      Object.freeze({
        uuid: TCK_BATTERY_LEVEL_UUID,
        occurrence: 0,
        properties: readNotify,
        descriptors: Object.freeze([
          Object.freeze({ uuid: TCK_USER_DESCRIPTION_UUID, occurrence: 0 }),
          Object.freeze({ uuid: TCK_USER_DESCRIPTION_UUID, occurrence: 1 })
        ])
      }),
      Object.freeze({
        uuid: TCK_BATTERY_LEVEL_UUID,
        occurrence: 1,
        properties: readNotify,
        descriptors: Object.freeze([])
      })
    ])
  }),
  Object.freeze({
    uuid: TCK_BATTERY_SERVICE_UUID,
    occurrence: 1,
    characteristics: Object.freeze([
      Object.freeze({
        uuid: TCK_BATTERY_LEVEL_UUID,
        occurrence: 0,
        properties: readNotify,
        descriptors: Object.freeze([])
      })
    ])
  })
])

const providerContractScenarioIds: readonly TckScenarioId[] = Object.freeze([
  'identity.provider-loadability-and-adapter-availability',
  'identity.adapter-selection-and-unique-instance',
  'identity.valid-all-axis-negotiation',
  'identity.version-skew-and-malformed-offers',
  'capability.truth-limits-evidence-and-binding',
  'gatt.duplicate-uuid-occurrences-route-exactly',
  'scenario.scan-connect-discover-read-notify-destroy'
])

/** The CoreBluetooth leg keeps the legacy leg's full scenario set. */
const coreBluetoothScenarioIds: readonly TckScenarioId[] = Object.freeze([
  'identity.provider-loadability-and-adapter-availability',
  'identity.adapter-selection-and-unique-instance',
  'identity.valid-all-axis-negotiation',
  'identity.version-skew-and-malformed-offers',
  'capability.truth-limits-evidence-and-binding',
  'adapter.atomic-snapshot-and-watch',
  'scan.owner-join-authority-and-signature',
  'scan.fairness-abort-deadline-and-final-cleanup',
  'connection.lease-joins-borrowing-transfer-and-revocation',
  'connection.two-client-arbitration',
  'gatt.discovery-complete-paths-and-services-changed',
  'gatt.duplicate-uuid-occurrences-route-exactly',
  'diagnostics.trace-redaction-and-resource-counters',
  'scenario.scan-connect-discover-read-notify-destroy'
])

/**
 * The connection-control and maximum-write-length suites: every leg runs the
 * ones its provider registers (the runner binds each registered row).
 */
const connectionFeatureSuites: readonly TckFeatureSuite[] = Object.freeze([
  Object.freeze({
    suiteId: 'connection-controls',
    scenarioIds: Object.freeze<TckScenarioId[]>(['connection.rssi-and-att-mtu-capability-contract'])
  }),
  Object.freeze({
    suiteId: 'tck.feature.gatt.maximum-write-length',
    scenarioIds: Object.freeze<TckScenarioId[]>(['gatt.maximum-write-length-boundaries'])
  })
])

const coreBluetoothControllerActions: readonly TckControllerAction[] = Object.freeze([
  'queue-advertisement',
  'emit-notification',
  'force-disconnect',
  'trigger-services-changed',
  'set-adapter-state'
])

const providerContractControllerActions: readonly TckControllerAction[] = Object.freeze([
  'queue-advertisement',
  'emit-notification'
])

interface DesktopLegShape {
  readonly platform: DesktopRustCorePlatform
  readonly suiteId: string
  readonly baseScenarioIds: readonly TckScenarioId[]
  readonly featureSuites: readonly TckFeatureSuite[]
  readonly controllerActions: readonly TckControllerAction[]
  readonly security: Omit<TckSecurityScenarioAdapter, 'peerId' | 'prepareCancellation'> | null
  readonly capabilityExclusions: readonly FirstPartyTckCapabilityExclusion[]
}

/** Registers CoreBluetooth's Rust route over the addon's synthetic radio. */
export function createCoreBluetoothFirstPartyTckRegistration(
  options: CoreBluetoothFirstPartyTckRegistrationOptions
): FirstPartyBackendTckRegistration {
  return createDesktopRustCoreTckRegistration(options, {
    platform: 'corebluetooth',
    suiteId: 'corebluetooth-provider-contract-v1',
    baseScenarioIds: coreBluetoothScenarioIds,
    featureSuites: connectionFeatureSuites,
    controllerActions: coreBluetoothControllerActions,
    security: null,
    capabilityExclusions: Object.freeze([])
  })
}

/** Registers BlueZ's Rust route over the addon's synthetic radio. */
export function createBluezFirstPartyTckRegistration(
  options: BluezFirstPartyTckRegistrationOptions
): FirstPartyBackendTckRegistration {
  return createDesktopRustCoreTckRegistration(options, {
    platform: 'bluez',
    suiteId: 'bluez-provider-contract-v1',
    baseScenarioIds: providerContractScenarioIds,
    featureSuites: Object.freeze([
      ...connectionFeatureSuites,
      Object.freeze({
        suiteId: 'tck.feature.security.bluez',
        scenarioIds: Object.freeze<TckScenarioId[]>(['security.state-pair-cancel-unpair'])
      })
    ]),
    controllerActions: providerContractControllerActions,
    security: Object.freeze({ customCeremonySupported: false, supportsAlreadyUnpaired: false }),
    capabilityExclusions: Object.freeze([
      Object.freeze({
        featureId: 'bluez:acquire-write',
        state: 'unsupported',
        reason: 'BlueZ AcquireWrite is not implemented by the desktop Rust core.'
      }),
      Object.freeze({
        featureId: 'bluez:acquire-notify',
        state: 'unsupported',
        reason: 'BlueZ AcquireNotify is not implemented by the desktop Rust core.'
      }),
      Object.freeze({
        featureId: 'bluez:pairing-agent',
        state: 'unsupported',
        reason:
          'Device1.Pair/CancelPairing dispatch runs through the Rust core, but Agent1 pairing behavior against a live BlueZ daemon is not proven: a synthetic radio cannot exercise a real SMP exchange.'
      }),
      Object.freeze({
        featureId: 'bluez:deterministic-advanced-scenario-controls',
        state: 'unavailable',
        reason:
          'The BlueZ leg runs the provider contract and the public vertical slice, as the 4.x BlueZ leg did; the advanced scenarios (operation timing, forced disconnects, Services Changed, ATT faults) run on the CoreBluetooth leg over the same Rust core.'
      }),
      Object.freeze({
        featureId: 'bluez:live-radio',
        state: 'unavailable',
        reason:
          'The synthetic radio does not establish behavior of a physical BlueZ daemon, adapter, or peripheral and cannot provide live-radio evidence.'
      })
    ])
  })
}

/** Registers WinRT's Rust route over the addon's synthetic radio. */
export function createWinRtFirstPartyTckRegistration(
  options: WinRtFirstPartyTckRegistrationOptions
): FirstPartyBackendTckRegistration {
  return createDesktopRustCoreTckRegistration(options, {
    platform: 'winrt',
    suiteId: 'winrt-provider-contract-v2',
    baseScenarioIds: providerContractScenarioIds,
    featureSuites: Object.freeze([
      ...connectionFeatureSuites,
      Object.freeze({
        suiteId: 'tck.feature.security.winrt',
        scenarioIds: Object.freeze<TckScenarioId[]>(['security.state-pair-cancel-unpair'])
      })
    ]),
    controllerActions: providerContractControllerActions,
    security: Object.freeze({ customCeremonySupported: false, supportsAlreadyUnpaired: true }),
    capabilityExclusions: Object.freeze([
      Object.freeze({
        featureId: 'winrt:live-radio',
        state: 'unavailable',
        reason: 'The synthetic radio replays controlled OS events and does not establish physical-radio support.'
      })
    ])
  })
}

/** A synthetic central: the production surface plus its staging surface. */
type SyntheticCentral = DesktopRustCoreCentral & DesktopRustCoreSyntheticRadio

const SYNTHETIC_METHODS: readonly (keyof DesktopRustCoreSyntheticRadio)[] = Object.freeze([
  'stageAdvertisement',
  'stageServices',
  'stageMtu',
  'stageRssi',
  'stageNotification',
  'stageLinkLoss',
  'stageServicesChanged',
  'stageAdapterState',
  'stageAdapterAuthorization',
  'stageSecurity',
  'stagePairOutcome',
  'stageWriteReadiness',
  'blockRadioOp',
  'stagedRadioCalls'
])

function isSyntheticCentral(central: DesktopRustCoreCentral): central is SyntheticCentral {
  return SYNTHETIC_METHODS.every(method => typeof Reflect.get(central, method) === 'function')
}

/** What one fixture drives: its backend, and the synthetic central under it. */
interface OpenedLeg {
  readonly central: SyntheticCentral
  readonly pairing: PairingGate
}

/**
 * Holds the next pairing ceremony at the radio until it is cancelled (the
 * runner's cancellation step). A peer the radio already bonds is answered
 * `already-paired` by the synthetic radio itself, as every OS does.
 */
interface PairingGate {
  prepareCancellation(): void
}

function createDesktopRustCoreTckRegistration(
  options: DesktopRustCoreFirstPartyTckRegistrationOptions,
  shape: DesktopLegShape
): FirstPartyBackendTckRegistration {
  const profile: DesktopRustCoreProfile = DESKTOP_RUST_CORE_PROFILES[shape.platform]
  const nativePeerId = options.nativePeerId ?? DEFAULT_NATIVE_PEER_ID
  if (nativePeerId.length === 0) {
    throw contractError(
      'argument.invalid',
      'core',
      desktopRustCoreOperation(profile.operationPrefix, 'tck.native-peer-id')
    )
  }
  const opened: OpenedLeg[] = []
  const bindingFor = async (): Promise<DesktopRustCoreBinding> =>
    syntheticOnlyBinding(options.binding ?? (await loadDesktopCoreBinding(profile)), profile, async central => {
      await seedSyntheticWorld(central, shape.platform, nativePeerId)
      const leg = legFor(central)
      opened.push(leg.opened)
      return leg.central
    })
  const provider = createDesktopRustCoreBackendProvider({
    platform: shape.platform,
    owner: `${shape.platform}-rust-core-tck`,
    now: options.now,
    radio: 'synthetic',
    // The synthetic radio runs on any host: the leg selects its platform's
    // profile, never the running OS's radio.
    hostPlatform: profile.requiredProcessPlatform,
    loadBinding: bindingFor
  })
  const selectedAdapterId = opaqueId(
    desktopRustCoreAdapterId(shape.platform, 'synthetic'),
    'adapter',
    `${shape.platform}-rust-core`
  )
  return {
    backendId: profile.backendId,
    factory: {
      backendId: profile.backendId,
      provider,
      selection: Object.freeze({ selectedAdapterId }),
      staleSelection: Object.freeze({
        selectedAdapterId: opaqueId(`stale-${shape.platform}-adapter`, 'adapter', `${shape.platform}-rust-core`)
      }),
      create: async _context => {
        const before = opened.length
        const backend = await provider.create({ selectedAdapterId })
        const leg = opened[opened.length - 1]
        if (!(backend instanceof DesktopRustCoreBackend) || leg === undefined || opened.length === before) {
          await backend.destroy()
          throw contractError(
            'lifecycle.invariant-violation',
            'core',
            desktopRustCoreOperation(profile.operationPrefix, 'tck.fixture')
          )
        }
        // The seeded world's OS reports (the CoreBluetooth first state) are
        // applied before any scenario looks, as a legacy boundary's were.
        await backend.settleCoreEvents()
        const securityPeerId = shape.security === null ? null : await primePeer(backend, leg.central, nativePeerId)
        return {
          backend,
          controller: createDesktopController(backend, leg.central, nativePeerId, options.now, shape.controllerActions),
          featureScenarioAdapters: Object.freeze({
            connectionControls: Object.freeze({ requestedMtu: TCK_ATT_MTU }),
            ...(shape.security === null || securityPeerId === null
              ? {}
              : {
                  security: Object.freeze({
                    ...shape.security,
                    peerId: securityPeerId,
                    prepareCancellation: () => leg.pairing.prepareCancellation()
                  })
                })
          }),
          dispose: () => backend.destroy()
        }
      }
    },
    suites: Object.freeze([Object.freeze({ suiteId: shape.suiteId, baseScenarioIds: shape.baseScenarioIds })]),
    featureSuites: shape.featureSuites,
    capabilityExclusions: shape.capabilityExclusions
  }
}

/**
 * The binding the leg's provider opens through: synthetic centrals only
 * (a production open is refused, so a TCK leg can never reach a radio),
 * each checked to carry the staging surface and seeded before use.
 */
function syntheticOnlyBinding(
  binding: DesktopRustCoreBinding,
  profile: DesktopRustCoreProfile,
  admit: (central: SyntheticCentral) => Promise<DesktopRustCoreCentral>
): DesktopRustCoreBinding {
  return Object.freeze({
    ...(binding.diagnostics === undefined ? {} : { diagnostics: binding.diagnostics }),
    capabilityStates: binding.capabilityStates,
    listAdapters: binding.listAdapters,
    openProduction: async () => {
      throw contractError(
        'capability.unavailable',
        'platform',
        desktopRustCoreOperation(profile.operationPrefix, 'tck.production-radio')
      )
    },
    openSynthetic: async (owner: string, openOptions?: Parameters<DesktopRustCoreBinding['openSynthetic']>[1]) => {
      const central = await binding.openSynthetic(owner, openOptions)
      if (!isSyntheticCentral(central)) {
        await central.close()
        throw contractError(
          'protocol.incompatible',
          'core',
          desktopRustCoreOperation(profile.operationPrefix, 'tck.synthetic-surface')
        )
      }
      return admit(central)
    }
  })
}

/** The radio world every fixture starts from: a powered adapter and one known peer. */
async function seedSyntheticWorld(
  central: SyntheticCentral,
  platform: DesktopRustCorePlatform,
  nativePeerId: string
): Promise<void> {
  // CoreBluetooth reports its first usable state after the manager exists
  // (the legacy first-state wait); the other OSes read it on demand.
  if (platform === 'corebluetooth') {
    await central.stageAdapterAuthorization('granted', false)
    await central.stageAdapterState('powered-on', true)
  }
  await central.stageServices(nativePeerId, tckServices)
  await central.stageMtu(nativePeerId, TCK_ATT_MTU)
  await central.stageRssi(nativePeerId, TCK_RSSI)
  await central.stageSecurity(nativePeerId, 'not-bonded', true)
  await central.stageWriteReadiness(nativePeerId, true)
}

/**
 * The central a fixture's backend uses: the synthetic central itself, except
 * that `pair` waits for a prepared cancellation hold and `cancelPairing`
 * waits until a held ceremony has reached the radio.
 */
function legFor(central: SyntheticCentral): { readonly central: DesktopRustCoreCentral; readonly opened: OpenedLeg } {
  let hold: Promise<void> | null = null
  let heldCeremony: Promise<void> | null = null
  const pairingGate: PairingGate = Object.freeze({
    prepareCancellation: () => {
      const blocked = (async () => {
        const before = countPairs(await central.stagedRadioCalls())
        await central.blockRadioOp('pair')
        return before
      })()
      hold = blocked.then(() => undefined)
      heldCeremony = blocked.then(before => waitForRadioPairs(central, before + 1))
    }
  })
  const pair: DesktopRustCoreCentral['pair'] = async pairOptions => {
    const prepared = hold
    hold = null
    if (prepared !== null) await prepared
    return central.pair(pairOptions)
  }
  const cancelPairing: DesktopRustCoreCentral['cancelPairing'] = async cancelOptions => {
    const ceremony = heldCeremony
    heldCeremony = null
    if (ceremony !== null) await ceremony
    return central.cancelPairing(cancelOptions)
  }
  const wrapped = new Proxy(central, {
    get(target, property) {
      if (property === 'pair') return pair
      if (property === 'cancelPairing') return cancelPairing
      const value: unknown = Reflect.get(target, property)
      return typeof value === 'function' ? (...args: unknown[]) => Reflect.apply(value, target, args) : value
    }
  })
  return { central: wrapped, opened: Object.freeze({ central, pairing: pairingGate }) }
}

function countPairs(calls: readonly string[]): number {
  return calls.filter(call => call === 'pair').length
}

async function waitForRadioPairs(central: SyntheticCentral, expected: number): Promise<void> {
  await pollUntil(async () => countPairs(await central.stagedRadioCalls()) >= expected, 'tck.pair-reached-radio')
}

async function pollUntil(condition: () => Promise<boolean>, operation: string): Promise<void> {
  const started = Date.now()
  while (!(await condition())) {
    if (Date.now() - started > SETTLE_LIMIT_MS) {
      throw contractError('operation.timed-out', 'core', `desktop-rust-core.${operation}`)
    }
    await new Promise(resolve => setTimeout(resolve, SETTLE_POLL_MS))
  }
}

/** Observe the synthetic peer once so its public peer id exists (the security leg's target). */
async function primePeer(
  backend: DesktopRustCoreBackend,
  central: SyntheticCentral,
  nativePeerId: string
): Promise<string> {
  const scan = await backend.scanner.start(
    {
      filter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null },
      duplicatePolicy: 'all',
      timestampPolicy: 'receipt-monotonic',
      delivery: {
        itemCapacity: capacity(4),
        byteCapacity: capacity(4096),
        reservedControlCapacity: capacity(1),
        overflowPolicy: 'drop-oldest'
      },
      deadline: null,
      signal: null,
      sharing: { mode: 'owner', allowSharing: false }
    },
    opaqueId('desktop-rust-core-tck-client', 'client', 'desktop-rust-core:tck')
  )
  const iterator = scan.observations[Symbol.asyncIterator]()
  const observation = iterator.next()
  await central.stageAdvertisement(tckAdvertisement(nativePeerId))
  const item = await observation
  await iterator.return?.()
  await scan.stop()
  if (item.done || item.value.kind !== 'value') {
    throw contractError('lifecycle.invariant-violation', 'core', 'desktop-rust-core.tck.prime-peer')
  }
  return String(item.value.value.device.id)
}

function tckAdvertisement(nativePeerId: string): DesktopRustCoreSyntheticAdvertisement {
  return Object.freeze({
    peerId: nativePeerId,
    rssi: TCK_RSSI,
    localName: 'Polar H10',
    serviceUuids: Object.freeze([TCK_SERVICE_UUID])
  })
}

function createDesktopController(
  backend: DesktopRustCoreBackend,
  central: SyntheticCentral,
  nativePeerId: string,
  now: () => number,
  availableActions: readonly TckControllerAction[]
): TckScenarioController {
  const perform = async (action: TckControllerAction, input: SerializableRecord): Promise<void> => {
    if (!availableActions.includes(action)) {
      throw new Error(`the desktop Rust-core leg does not register ${action}`)
    }
    if (action === 'queue-advertisement') {
      requireEmptyInput(action, input)
      await central.stageAdvertisement(tckAdvertisement(nativePeerId))
      return
    }
    if (action === 'emit-notification') {
      await central.stageNotification({
        peerId: nativePeerId,
        serviceUuid: stringField(action, input, 'serviceUuid'),
        serviceOccurrence: nonNegativeIntegerField(action, input, 'serviceOccurrence'),
        characteristicUuid: stringField(action, input, 'characteristicUuid'),
        characteristicOccurrence: nonNegativeIntegerField(action, input, 'characteristicOccurrence'),
        value: bytesField(action, input, 'value')
      })
      return
    }
    if (action === 'force-disconnect') {
      stringField(action, input, 'peerId')
      await central.stageLinkLoss(nativePeerId)
      await pollUntil(async () => {
        const peer = (await central.peerRecords()).find(record => record.peerId === nativePeerId)
        return peer?.connectionState !== 'connected' && peer?.connectionState !== 'disconnecting'
      }, 'tck.force-disconnect')
      await backend.settleCoreEvents()
      return
    }
    if (action === 'trigger-services-changed') {
      stringField(action, input, 'peerId')
      await central.stageServicesChanged(nativePeerId)
      await pollUntil(async () => {
        const peer = (await central.peerRecords()).find(record => record.peerId === nativePeerId)
        return peer !== undefined && peer.databaseState !== 'current'
      }, 'tck.services-changed')
      await backend.settleCoreEvents()
      return
    }
    if (action === 'set-adapter-state') {
      await stageAdapterState(backend, central, action, input)
      return
    }
    throw new Error(`the desktop Rust-core leg cannot perform ${action}`)
  }
  return Object.freeze({
    availableActions,
    now,
    settle: <Value>(promise: Promise<Value>) => promise,
    flush: flushMicrotasks,
    perform
  })
}

/** Map a TCK adapter snapshot onto the synthetic OS facts and wait for the backend to see them. */
async function stageAdapterState(
  backend: DesktopRustCoreBackend,
  central: SyntheticCentral,
  action: string,
  input: SerializableRecord
): Promise<void> {
  const power = powerField(action, input)
  const authorization = authorizationField(action, input)
  // One snapshot is one OS report: the authorization is staged as a fact,
  // and the power change is the event that announces the new snapshot.
  if (authorization !== null) await central.stageAdapterAuthorization(authorization, false)
  await central.stageAdapterState(power, true)
  await pollUntil(async () => central.adapterStatus().power === power, 'tck.adapter-state')
  await backend.settleCoreEvents()
}

function powerField(action: string, input: SerializableRecord): DesktopRustCoreAdapterPower {
  const power = stringField(action, input, 'power')
  if (power === 'on') return 'powered-on'
  if (power === 'off') return 'powered-off'
  if (power === 'resetting' || power === 'unsupported' || power === 'unknown') return power
  throw new Error(`${action}.power is invalid`)
}

function authorizationField(action: string, input: SerializableRecord): DesktopRustCoreAdapterAuthorization | null {
  const authorization = stringField(action, input, 'authorization')
  if (
    authorization === 'granted' ||
    authorization === 'denied' ||
    authorization === 'restricted' ||
    authorization === 'not-determined'
  ) {
    return authorization
  }
  if (authorization === 'unavailable' || authorization === 'unknown') return null
  throw new Error(`${action}.authorization is invalid`)
}

async function flushMicrotasks(): Promise<void> {
  for (let turn = 0; turn < 8; turn += 1) {
    await Promise.resolve()
  }
}

function requireEmptyInput(action: string, input: SerializableRecord): void {
  if (Object.keys(input).length !== 0) {
    throw new Error(`${action} must not receive input`)
  }
}

function stringField(action: string, input: SerializableRecord, field: string): string {
  const value = input[field]
  if (typeof value !== 'string' || value.length === 0) {
    throw new Error(`${action}.${field} must be a non-empty string`)
  }
  return value
}

function nonNegativeIntegerField(action: string, input: SerializableRecord, field: string): number {
  const value = input[field]
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${action}.${field} must be a non-negative safe integer`)
  }
  return value
}

function bytesField(action: string, input: SerializableRecord, field: string): Uint8Array {
  const value = input[field]
  if (!(value instanceof Uint8Array)) {
    throw new Error(`${action}.${field} must be Uint8Array`)
  }
  return new Uint8Array(value)
}
