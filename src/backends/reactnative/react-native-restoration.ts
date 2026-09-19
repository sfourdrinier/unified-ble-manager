// src/backends/reactnative/react-native-restoration.ts

import { createFeatureRegistry, type FeatureRegistry, type Limitation } from '../../backend-contract/capabilities'
import { contractError, BackendContractError } from '../../backend-contract/errors'
import {
  type AttachmentRecord,
  type BackendProvider,
  type NativeBackendIdentity
} from '../../backend-contract/identity'
import {
  applicableVersionAxesEqual,
  opaqueId,
  version,
  versionRange,
  type ClientId,
  type NativeVersionAxes,
  type SerializableRecord,
  type SerializableValue
} from '../../backend-contract/primitives'
import type {
  AuthenticatedRestorationClient,
  NativeRestorationBootstrapIdentity,
  RestorationAdoptionRequest,
  RestorationAdoptionResult,
  RestorationBootstrapRequest,
  RestorationCoordinator,
  RestorationJournalRecord
} from '../../backend-contract/restoration'
import { normalizeRestorationBootstrapRequest } from '../../backend-contract/restoration'
import { MAXIMUM_CONTROL_RECORD_BYTES, type RestorationOutcomes } from './react-native-protocol-limits'

/** The restoration-identity request a native host answers (`restorationId`, `generation`). */
export interface ReactNativeRestorationBootstrapRequest {
  readonly restorationId: string
  readonly generation: string
}

/** A native host that answers the app-declared restoration identity. */
export interface ReactNativeRestorationIdentitySource {
  bootstrapRestorationIdentity(
    request: ReactNativeRestorationBootstrapRequest
  ): Promise<NativeRestorationBootstrapIdentity>
}

/** One adoption request against the native restoration journal. */
export interface ReactNativeRestorationAdoptionRequestRecord {
  readonly namespaceValue: string
  readonly attachmentId: string
  readonly expectedBackendInstanceId: string
  readonly expectedEpoch: string
  readonly nativeProtocolMinimum: number
  readonly nativeProtocolMaximum: number
  readonly clientId: string
  readonly hostSessionScope: string
}

/** One replayed journal record, in the native journal's structured transport. */
export interface ReactNativeRestorationReplayRecord {
  readonly recordVersion: number
  readonly namespaceValue: string
  readonly attachmentId: string
  readonly backendInstanceId: string
  readonly backendGeneration: string
  readonly adapterId: string
  readonly adapterGeneration: string
  readonly ordinal: number
  readonly adoptionEpoch: string
  readonly kind: 'adapter' | 'connection'
  readonly peerId: string | null
  readonly connectionId: string | null
  readonly ownerLeaseId: string | null
  readonly connectionGeneration: string | null
}

/** The journal's answer to one adoption request. */
export interface ReactNativeRestorationAdoptionRecord {
  readonly receiptId: string
  readonly outcome: RestorationOutcomes
  readonly boundClientId: string
  readonly adoptionEpoch: string
  readonly replayRecordCount: number
  readonly records: readonly ReactNativeRestorationReplayRecord[]
}

/**
 * The native restoration journal the coordinator adopts from: the Rust-route
 * journal over `peers.restored` (react-native-rust-core-restoration.ts), or a
 * legacy protocol control used as a parity reference.
 */
export interface ReactNativeRestorationJournal {
  adoptRestoration(request: ReactNativeRestorationAdoptionRequestRecord): Promise<ReactNativeRestorationAdoptionRecord>
}

/**
 * Safety bound on records adopted from a native restoration journal.
 *
 * A trust-boundary quota, not host policy: the journal is data the platform
 * hands back across a process restart, and the JavaScript side must not adopt an
 * unbounded number of records from it. Fail-closed bounds on native-supplied
 * data are a 4.x contract invariant, so this is intentionally not configurable.
 */
const maximumRestorationRecords = 1024
const restorationScenarioId = 'restoration.provider-journal-adoption-and-rejection'
const presenceScenarioId = 'restoration.presence-observation-arms-known-peer'
const activationIssuanceToken = Symbol('react-native-restoration-activation')

export type ReactNativeRestorationPlatform = 'android' | 'apple'

interface ActiveRestorationBinding {
  readonly activation: ReactNativeRestorationActivation
  readonly attachment: AttachmentRecord<string>
  readonly versions: NativeVersionAxes
}
/** React Native provider surface with one authority-bound restoration coordinator. */
export interface ReactNativeRestorationBackendProvider extends BackendProvider<string, NativeBackendIdentity<string>> {
  readonly restoration: ReactNativeRestorationCoordinator
}

/**
 * Requests native restoration bootstrap. JavaScript supplies only the one
 * application-facing token and generation; every internal identity value is
 * returned by the trusted native host and is validated before use.
 */
export async function bootstrapReactNativeRestorationIdentity(
  control: ReactNativeRestorationIdentitySource,
  input: { readonly restorationId: string; readonly generation?: string }
): Promise<NativeRestorationBootstrapIdentity> {
  const normalized = normalizeRestorationBootstrapRequest(input)
  const request: ReactNativeRestorationBootstrapRequest = Object.freeze({
    restorationId: normalized.restorationId,
    generation: normalized.generation
  })
  let result: NativeRestorationBootstrapIdentity
  try {
    result = await control.bootstrapRestorationIdentity(request)
  } catch (error) {
    if (error instanceof BackendContractError) {
      throw error
    }
    throw contractError('platform.failure', 'restoration', 'react-native-restoration.native-bootstrap')
  }
  assertNativeBootstrapIdentity(result, normalized)
  return Object.freeze({ ...result })
}

/** Opaque provider-issued binding for one opened React Native native attachment. */
export class ReactNativeRestorationActivation {
  private readonly marker = true

  constructor(issuanceToken: symbol) {
    if (issuanceToken !== activationIssuanceToken || !this.marker) {
      throw contractError('ownership.denied', 'restoration', 'react-native-restoration.activation')
    }
  }
}

/**
 * Provider-owned authority for the one currently open native attachment.
 * It serializes adoption, copies replay bytes, and closes admission before the
 * physical attachment can begin destruction.
 */
export class ReactNativeRestorationCoordinator implements RestorationCoordinator<string> {
  private activeBinding: ActiveRestorationBinding | null = null
  private serial: Promise<void> = Promise.resolve()
  private closing: Promise<void> | null = null
  private consumed: RestorationAdoptionResult<string> | null = null
  private terminalFailure: BackendContractError | null = null

  constructor(private readonly control: ReactNativeRestorationJournal) {}

  activate(attachment: AttachmentRecord<string>, versions: NativeVersionAxes): ReactNativeRestorationActivation {
    if (this.activeBinding !== null || this.closing !== null) {
      throw contractError('lifecycle.invalid-state', 'restoration', 'react-native-restoration.activate')
    }
    const activation = new ReactNativeRestorationActivation(activationIssuanceToken)
    this.activeBinding = Object.freeze({ activation, attachment, versions })
    this.consumed = null
    this.terminalFailure = null
    return activation
  }

  deactivate(activation: ReactNativeRestorationActivation): Promise<void> {
    const active = this.activeBinding
    if (active === null || active.activation !== activation) {
      return Promise.resolve()
    }
    this.activeBinding = null
    const waiting = this.serial
    const closing = waiting.then(
      () => undefined,
      () => undefined
    )
    this.closing = closing
    closing.then(() => {
      if (this.closing === closing) {
        this.closing = null
      }
    })
    return closing
  }

  adopt(
    client: AuthenticatedRestorationClient<string>,
    request: RestorationAdoptionRequest<string>
  ): Promise<RestorationAdoptionResult<string>> {
    const adoption = this.serial.then(() => this.adoptWhenTurn(client, request))
    this.serial = adoption.then(
      () => undefined,
      () => undefined
    )
    return adoption
  }

  private async adoptWhenTurn(
    client: AuthenticatedRestorationClient<string>,
    request: RestorationAdoptionRequest<string>
  ): Promise<RestorationAdoptionResult<string>> {
    const binding = this.requireActiveBinding()
    assertClient(client)
    assertRequest(request)
    // Issue #212: Android adopts the peers a Companion Device Manager
    // presence wake restored, through the same journal path as iOS.
    const mismatch = requestMismatch(binding, request)
    if (mismatch !== null) {
      return mismatchResult(request, mismatch)
    }
    if (this.terminalFailure !== null) {
      throw this.terminalFailure
    }
    if (this.consumed !== null) {
      return alreadyConsumedResult(this.consumed)
    }

    let nativeResult: ReactNativeRestorationAdoptionRecord
    try {
      nativeResult = await this.control.adoptRestoration({
        namespaceValue: request.namespace,
        attachmentId: String(request.attachmentId),
        expectedBackendInstanceId: String(request.expectedBackendInstanceId),
        expectedEpoch: String(request.expectedEpoch),
        nativeProtocolMinimum: request.expectedVersions.nativeProtocol.selected.value,
        nativeProtocolMaximum: request.expectedVersions.nativeProtocol.selected.value,
        clientId: String(client.clientId),
        hostSessionScope: client.hostSessionScope
      })
    } catch (error) {
      // A capability answer is the platform's reply, not a failure to
      // diagnose: the typed rejection still reaches the caller, but it is
      // not logged as an error. Genuine failures keep the log line below.
      if (!(error instanceof BackendContractError) || error.normalized.code !== 'capability.unsupported') {
        console.error('[ReactNativeRestorationCoordinator.adopt] Native restoration adoption failed:', error)
      }
      if (error instanceof BackendContractError) {
        throw error
      }
      throw contractError('platform.failure', 'restoration', 'react-native-restoration.native-adopt')
    }

    try {
      const result = decodeAdoptionResult(nativeResult, client, request, binding)
      if (result.outcome === 'adopted' || result.outcome === 'already-consumed') {
        this.consumed = result
      }
      return result
    } catch (error) {
      const normalized =
        error instanceof BackendContractError
          ? error
          : contractError('protocol.malformed', 'restoration', 'react-native-restoration.decode-adoption')
      this.terminalFailure = normalized
      console.error('[ReactNativeRestorationCoordinator.adopt] Native restoration replay was malformed:', error)
      throw normalized
    }
  }

  private requireActiveBinding(): ActiveRestorationBinding {
    if (this.activeBinding === null) {
      throw contractError('lifecycle.destroyed', 'restoration', 'react-native-restoration.adopt')
    }
    return this.activeBinding
  }
}

/** Registers the provider-owned restoration capability independently of host inference. */
export function createReactNativeRestorationFeatureRegistry(
  platform: ReactNativeRestorationPlatform,
  implementationVersion: string
): FeatureRegistry {
  const limitation = restorationLimitation(platform)
  const presenceState = platform === 'android' ? 'limited' : 'unsupported'
  const presenceLimitation = presenceObservationLimitation(platform)
  return createFeatureRegistry(
    Object.freeze([
      Object.freeze({
        id: 'state:restoration-adoption',
        state: 'limited',
        selectedSchemaRange: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
        implementationOrigin: 'backend-native',
        implementation: Object.freeze({
          async invoke(_input: SerializableRecord): Promise<SerializableRecord> {
            throw contractError(
              'lifecycle.invalid-state',
              'restoration',
              'state:restoration-adoption.invoke-without-manager'
            )
          }
        }),
        tck: Object.freeze({
          suiteId: 'restoration',
          requiredScenarioIds: Object.freeze([restorationScenarioId]),
          contractRange: versionRange(version('capability-schema', 1), version('capability-schema', 1))
        }),
        evidence: Object.freeze({
          receiptId: `react-native-${platform}-restoration-adoption-v1:deterministic`,
          evidenceLevel: 'deterministic',
          implementationVersion,
          sourceDigest: `react-native-${platform}-restoration-adoption-v1`,
          scenarioIds: Object.freeze([restorationScenarioId]),
          limitations: Object.freeze([limitation])
        }),
        limitations: Object.freeze([limitation]),
        limits: Object.freeze({
          restorationRecords: Object.freeze({ maximum: maximumRestorationRecords, minimum: null, unit: 'items' }),
          restorationBytes: Object.freeze({ maximum: MAXIMUM_CONTROL_RECORD_BYTES, minimum: null, unit: 'bytes' }),
          automaticReconnects: Object.freeze({ maximum: 0, minimum: null, unit: 'connections' }),
          automaticSubscriptionResumptions: Object.freeze({ maximum: 0, minimum: null, unit: 'subscriptions' })
        })
      }),
      Object.freeze({
        id: 'state:presence-observation',
        state: presenceState,
        selectedSchemaRange: versionRange(version('capability-schema', 1), version('capability-schema', 1)),
        implementationOrigin: 'backend-native',
        implementation: Object.freeze({
          async invoke(_input: SerializableRecord): Promise<SerializableRecord> {
            throw contractError(
              'lifecycle.invalid-state',
              'restoration',
              'state:presence-observation.invoke-without-manager'
            )
          }
        }),
        tck: Object.freeze({
          suiteId: 'restoration',
          requiredScenarioIds: Object.freeze([presenceScenarioId]),
          contractRange: versionRange(version('capability-schema', 1), version('capability-schema', 1))
        }),
        evidence: Object.freeze({
          receiptId: `react-native-${platform}-presence-observation-v1:deterministic`,
          evidenceLevel: presenceState === 'limited' ? 'deterministic' : 'blocked',
          implementationVersion,
          sourceDigest: `react-native-${platform}-presence-observation-v1`,
          scenarioIds: Object.freeze([presenceScenarioId]),
          limitations: Object.freeze([presenceLimitation])
        }),
        limitations: Object.freeze([presenceLimitation]),
        limits: Object.freeze({
          automaticReconnects: Object.freeze({ maximum: 0, minimum: null, unit: 'connections' }),
          automaticSubscriptionResumptions: Object.freeze({ maximum: 0, minimum: null, unit: 'subscriptions' })
        })
      })
    ])
  )
}

export function combineReactNativeFeatureRegistries(...registries: readonly FeatureRegistry[]): FeatureRegistry {
  return createFeatureRegistry(Object.freeze(registries.flatMap(registry => registry.registrations)))
}

function restorationLimitation(platform: 'android' | 'apple'): Limitation {
  if (platform === 'android') {
    return Object.freeze({
      code: 'android-restoration-needs-presence-observation',
      explanation:
        'Android has no OS restoration journal: known peers are restored through Companion Device Manager device presence (API 31+) for an armed associated peer, then claimed with the same once-per-process semantics as iOS.',
      affectedGuarantee: 'replay of state restored before JavaScript starts'
    })
  }
  return Object.freeze({
    code: 'configured-native-restoration-authority-required',
    explanation:
      'Apple replays bounded restored state only after explicit authenticated adoption against its native authority configuration; it never reconnects or resumes subscriptions by itself — the app reconnects known peers and replays subscriptions through the public API.',
    affectedGuarantee: 'automatic restoration of radio activity'
  })
}

function presenceObservationLimitation(platform: 'android' | 'apple'): Limitation {
  if (platform === 'android') {
    return Object.freeze({
      code: 'companion-presence-needs-api-31-and-association',
      explanation:
        'Device presence observation needs Android API 31+ and an associated peer (associateCompanion); the system wakes the process through the library CompanionDeviceService only for armed peers.',
      affectedGuarantee: 'restoration of known peers after process termination'
    })
  }
  return Object.freeze({
    code: 'apple-restoration-needs-no-presence-observation',
    explanation:
      'CoreBluetooth delivers restoration through willRestoreState after a system relaunch; there is no presence observation to arm.',
    affectedGuarantee: 'restoration of known peers after process termination'
  })
}

function assertClient(client: AuthenticatedRestorationClient<string>): void {
  if (String(client.clientId).length === 0 || client.hostSessionScope.length === 0) {
    throw contractError('argument.invalid', 'restoration', 'react-native-restoration.client')
  }
}

function assertNativeBootstrapIdentity(
  result: NativeRestorationBootstrapIdentity,
  expected: RestorationBootstrapRequest
): void {
  if (typeof result !== 'object' || result === null || Array.isArray(result)) {
    throw contractError('protocol.malformed', 'restoration', 'react-native-restoration.bootstrap-result')
  }
  const requiredFields: readonly [keyof NativeRestorationBootstrapIdentity, string][] = [
    ['applicationId', 'application-id'],
    ['restorationId', 'restoration-id'],
    ['generation', 'generation'],
    ['restoreIdentifier', 'restore-identifier'],
    ['namespaceValue', 'namespace'],
    ['clientId', 'client-id'],
    ['hostSessionScope', 'host-session-scope']
  ]
  for (const [field, label] of requiredFields) {
    if (typeof result[field] !== 'string' || result[field].length === 0) {
      throw contractError('protocol.malformed', 'restoration', `react-native-restoration.bootstrap-${label}`)
    }
  }
  if (
    !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(result.applicationId) ||
    result.restorationId !== expected.restorationId ||
    result.generation !== expected.generation ||
    !result.restoreIdentifier.startsWith(`${result.applicationId}.ubm.`) ||
    !result.namespaceValue.startsWith('ubm-ns:') ||
    !result.clientId.startsWith('ubm-client:') ||
    !result.hostSessionScope.startsWith('ubm-host:')
  ) {
    throw contractError('protocol.violation', 'restoration', 'react-native-restoration.bootstrap-authority')
  }
}

function assertRequest(request: RestorationAdoptionRequest<string>): void {
  if (
    request.namespace.length === 0 ||
    String(request.attachmentId).length === 0 ||
    String(request.expectedBackendInstanceId).length === 0 ||
    String(request.expectedEpoch).length === 0
  ) {
    throw contractError('argument.invalid', 'restoration', 'react-native-restoration.request')
  }
}

function requestMismatch(
  binding: ActiveRestorationBinding,
  request: RestorationAdoptionRequest<string>
): 'attachment-mismatch' | 'backend-mismatch' | null {
  if (request.attachmentId !== binding.attachment.attachmentId) {
    return 'attachment-mismatch'
  }
  if (request.expectedBackendInstanceId !== binding.attachment.backendInstanceId) {
    return 'backend-mismatch'
  }
  if (!applicableVersionAxesEqual(request.expectedVersions, binding.versions)) {
    throw contractError('protocol.incompatible', 'restoration', 'react-native-restoration.request-versions')
  }
  return null
}

function mismatchResult(
  request: RestorationAdoptionRequest<string>,
  outcome: 'attachment-mismatch' | 'backend-mismatch'
): RestorationAdoptionResult<string> {
  return Object.freeze({
    attachmentId: request.attachmentId,
    receiptId: null,
    namespace: request.namespace,
    boundClientId: null,
    adoptionEpoch: null,
    outcome,
    replayedRecords: Object.freeze([])
  })
}

function decodeAdoptionResult(
  result: ReactNativeRestorationAdoptionRecord,
  client: AuthenticatedRestorationClient<string>,
  request: RestorationAdoptionRequest<string>,
  binding: ActiveRestorationBinding
): RestorationAdoptionResult<string> {
  assertNativeResultShape(result)
  const outcome = outcomeFor(result.outcome)
  if (outcome === 'adopted') {
    if (
      result.receiptId.length === 0 ||
      result.boundClientId !== String(client.clientId) ||
      result.adoptionEpoch !== String(request.expectedEpoch)
    ) {
      throw contractError('protocol.violation', 'restoration', 'react-native-restoration.adopted-authority')
    }
    const replayedRecords = decodeReplayedRecords(result, request, binding)
    return Object.freeze({
      attachmentId: binding.attachment.attachmentId,
      receiptId: result.receiptId,
      namespace: request.namespace,
      boundClientId: client.clientId,
      adoptionEpoch: request.expectedEpoch,
      outcome,
      replayedRecords
    })
  }
  if (outcome === 'already-consumed') {
    if (
      result.receiptId.length !== 0 ||
      result.boundClientId.length === 0 ||
      result.adoptionEpoch.length === 0 ||
      result.replayRecordCount !== 0 ||
      result.records.length !== 0
    ) {
      throw contractError('protocol.violation', 'restoration', 'react-native-restoration.already-consumed-authority')
    }
    return Object.freeze({
      attachmentId: binding.attachment.attachmentId,
      receiptId: null,
      namespace: request.namespace,
      boundClientId: restorationClientId(result.boundClientId, binding.attachment),
      adoptionEpoch: restorationEpoch(result.adoptionEpoch),
      outcome,
      replayedRecords: Object.freeze([])
    })
  }
  if (
    result.receiptId.length !== 0 ||
    result.boundClientId.length !== 0 ||
    result.adoptionEpoch.length === 0 ||
    result.replayRecordCount !== 0 ||
    result.records.length !== 0
  ) {
    throw contractError('protocol.violation', 'restoration', 'react-native-restoration.rejection-authority')
  }
  return Object.freeze({
    attachmentId: request.attachmentId,
    receiptId: null,
    namespace: request.namespace,
    boundClientId: null,
    adoptionEpoch: restorationEpoch(result.adoptionEpoch),
    outcome,
    replayedRecords: Object.freeze([])
  })
}

function assertNativeResultShape(result: ReactNativeRestorationAdoptionRecord): void {
  if (
    !Number.isSafeInteger(result.replayRecordCount) ||
    result.replayRecordCount < 0 ||
    result.replayRecordCount > maximumRestorationRecords ||
    result.replayRecordCount !== result.records.length
  ) {
    throw contractError('protocol.malformed', 'restoration', 'react-native-restoration.native-result')
  }
}

function outcomeFor(outcome: RestorationOutcomes): RestorationAdoptionResult<string>['outcome'] {
  if (outcome === 'adopted') {
    return 'adopted'
  }
  if (outcome === 'alreadyConsumed') {
    return 'already-consumed'
  }
  if (outcome === 'attachmentMismatch') {
    return 'attachment-mismatch'
  }
  if (outcome === 'backendMismatch') {
    return 'backend-mismatch'
  }
  if (outcome === 'namespaceMismatch') {
    return 'namespace-mismatch'
  }
  if (outcome === 'epochMismatch') {
    return 'epoch-mismatch'
  }
  throw contractError('protocol.malformed', 'restoration', 'react-native-restoration.native-outcome')
}

function decodeReplayedRecords(
  result: ReactNativeRestorationAdoptionRecord,
  request: RestorationAdoptionRequest<string>,
  binding: ActiveRestorationBinding
): readonly RestorationJournalRecord<string>[] {
  const records: RestorationJournalRecord<string>[] = []
  let expectedOrdinal = 1
  for (const nativeRecord of result.records) {
    const replayed = replayedRecordFromStructuredTransport(nativeRecord, request, binding)
    if (replayed.ordinal !== expectedOrdinal) {
      throw contractError('protocol.violation', 'restoration', 'react-native-restoration.replay-ordinal')
    }
    expectedOrdinal += 1
    records.push(replayed)
  }
  return Object.freeze(records)
}

function replayedRecordFromStructuredTransport(
  record: ReactNativeRestorationReplayRecord,
  request: RestorationAdoptionRequest<string>,
  binding: ActiveRestorationBinding
): RestorationJournalRecord<string> {
  const recordVersion = requiredPositiveNativeInteger(record.recordVersion, 'record-version')
  const namespaceValue = requiredNativeString(record.namespaceValue, 'namespace')
  const ordinal = requiredPositiveNativeInteger(record.ordinal, 'ordinal')
  const epoch = requiredNativeString(record.adoptionEpoch, 'epoch')
  assertStructuredAttachment(record, binding.attachment)
  if (namespaceValue !== request.namespace || epoch !== String(request.expectedEpoch)) {
    throw contractError('protocol.violation', 'restoration', 'react-native-restoration.replay-authority')
  }
  const kind = record.kind
  if (kind !== 'adapter' && kind !== 'connection') {
    throw contractError('protocol.malformed', 'restoration', 'react-native-restoration.replay-kind')
  }
  const peerValue = requiredNativeNullableString(record.peerId, 'peer-id')
  const connectionId = requiredNativeNullableString(record.connectionId, 'connection-id')
  const ownerLeaseId = requiredNativeNullableString(record.ownerLeaseId, 'owner-lease-id')
  const connectionGeneration = requiredNativeNullableString(record.connectionGeneration, 'connection-generation')
  if (
    kind === 'adapter' &&
    (peerValue !== null || connectionId !== null || ownerLeaseId !== null || connectionGeneration !== null)
  ) {
    throw contractError('protocol.violation', 'restoration', 'react-native-restoration.adapter-payload')
  }
  if (
    kind === 'connection' &&
    (peerValue === null || connectionId === null || ownerLeaseId === null || connectionGeneration === null)
  ) {
    throw contractError('protocol.violation', 'restoration', 'react-native-restoration.connection-payload')
  }
  const protocolRecord = structuredProtocolRecord(record, peerValue, connectionId, ownerLeaseId, connectionGeneration)
  return Object.freeze({
    recordVersion,
    namespace: namespaceValue,
    attachmentId: binding.attachment.attachmentId,
    backendInstanceId: binding.attachment.backendInstanceId,
    backendGeneration: binding.attachment.backendGeneration,
    ordinal,
    adoptionEpoch: restorationEpoch(epoch),
    kind,
    peerId: peerValue === null ? null : opaqueId(peerValue, 'peer', 'react-native-restoration'),
    payload: Object.freeze({
      protocolRecord
    })
  })
}

function assertStructuredAttachment(
  record: ReactNativeRestorationReplayRecord,
  expected: AttachmentRecord<string>
): void {
  if (
    requiredNativeString(record.attachmentId, 'attachment-id') !== String(expected.attachmentId) ||
    requiredNativeString(record.backendInstanceId, 'backend-instance-id') !== String(expected.backendInstanceId) ||
    requiredNativeString(record.backendGeneration, 'backend-generation') !== String(expected.backendGeneration) ||
    requiredNativeString(record.adapterId, 'adapter-id') !== String(expected.adapter.adapterId) ||
    requiredNativeString(record.adapterGeneration, 'adapter-generation') !== String(expected.adapter.adapterGeneration)
  ) {
    throw contractError('protocol.violation', 'restoration', 'react-native-restoration.replay-attachment')
  }
}

function requiredPositiveNativeInteger(value: number, fieldName: string): number {
  if (!Number.isSafeInteger(value) || value < 1) {
    throw contractError('protocol.malformed', 'restoration', `react-native-restoration.replay-${fieldName}`)
  }
  return value
}

function requiredNativeString(value: string, fieldName: string): string {
  if (value.length === 0) {
    throw contractError('protocol.malformed', 'restoration', `react-native-restoration.replay-${fieldName}`)
  }
  return value
}

function requiredNativeNullableString(value: string | null, fieldName: string): string | null {
  if (value !== null && value.length === 0) {
    throw contractError('protocol.malformed', 'restoration', `react-native-restoration.replay-${fieldName}`)
  }
  return value
}

function structuredProtocolRecord(
  record: ReactNativeRestorationReplayRecord,
  peerId: string | null,
  connectionId: string | null,
  ownerLeaseId: string | null,
  connectionGeneration: string | null
): SerializableRecord {
  const attachment = Object.freeze({
    kind: 'attachment',
    fields: Object.freeze([
      Object.freeze({ id: 1, value: record.attachmentId }),
      Object.freeze({ id: 2, value: record.backendInstanceId }),
      Object.freeze({ id: 3, value: record.backendGeneration }),
      Object.freeze({ id: 4, value: record.adapterId }),
      Object.freeze({ id: 5, value: record.adapterGeneration })
    ])
  })
  const fields: SerializableValue[] = [
    Object.freeze({ id: 1, value: record.recordVersion }),
    Object.freeze({ id: 2, value: record.namespaceValue }),
    Object.freeze({ id: 3, value: attachment }),
    Object.freeze({ id: 4, value: record.ordinal }),
    Object.freeze({ id: 5, value: record.adoptionEpoch }),
    Object.freeze({ id: 6, value: record.kind })
  ]
  if (
    record.kind === 'connection' &&
    peerId !== null &&
    connectionId !== null &&
    ownerLeaseId !== null &&
    connectionGeneration !== null
  ) {
    fields.push(
      Object.freeze({
        id: 8,
        value: Object.freeze({
          kind: 'connectionPath',
          fields: Object.freeze([
            Object.freeze({ id: 1, value: attachment }),
            Object.freeze({ id: 2, value: peerId }),
            Object.freeze({ id: 3, value: connectionId }),
            Object.freeze({ id: 4, value: ownerLeaseId }),
            Object.freeze({ id: 5, value: connectionGeneration })
          ])
        })
      })
    )
  }
  return Object.freeze({
    kind: 'restorationRecord',
    fields: Object.freeze(fields)
  })
}

function restorationClientId(value: string, attachment: AttachmentRecord<string>): ClientId<string, string> {
  const scope: `${string}:${string}` = `restoration:${String(attachment.attachmentId)}`
  return opaqueId(value, 'client', scope)
}

function restorationEpoch(value: string) {
  return opaqueId(value, 'restoration-epoch', 'react-native-restoration')
}

function alreadyConsumedResult(result: RestorationAdoptionResult<string>): RestorationAdoptionResult<string> {
  return Object.freeze({
    attachmentId: result.attachmentId,
    receiptId: null,
    namespace: result.namespace,
    boundClientId: result.boundClientId,
    adoptionEpoch: result.adoptionEpoch,
    outcome: 'already-consumed',
    replayedRecords: Object.freeze([])
  })
}
