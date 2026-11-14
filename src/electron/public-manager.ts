import { BackendContractError, contractError } from '../backend-contract/errors'
import type { NormalizedBleError } from '../backend-contract/errors'
import type { StreamTerminalNotice } from '../backend-contract/streams'
import { markAuthenticatedElectronTerminalEvidenceStageFailure } from '../ipc/electron-terminal-evidence-wire'
import type { SerializableRecord } from '../backend-contract/primitives'
import type { ElectronRendererIpcTransport } from './protocol'
import type {
  IpcBleEvent,
  IpcBleRequest,
  IpcBleResponse,
  IpcClientTransport,
  IpcEventTransportHealthNotice
} from '../ipc/protocol'
import { ElectronRendererBleClient } from './renderer'
import { IpcBleManager } from '../ipc/manager'
import { aggregateEventLossError } from '../ipc/aggregate-event-loss'
import { IpcPublicManagerAdapter } from '../ipc/public-manager'
import type { BleManager } from '../public/ble-manager'
import type { BleManagerCreateOptions } from '../public/host-identity'
import { rehydratePublicPromise } from '../public/error-bridge'

/** Renderer-safe opaque projection of a trusted Electron-main recovery grant. */
export interface ElectronPeerFailureRecoveryHandle {
  readonly handleId: string
  readonly peerId: string
  readonly selector: Readonly<{
    readonly serviceUuid: string
    readonly serviceOccurrence: string
    readonly characteristicUuid: string
    readonly characteristicOccurrence: string
  }>
  readonly contiguousRead: string
  readonly acknowledged: boolean
  readonly producerAck: 'not-started' | 'in-flight' | 'committed'
}
export interface ElectronPeerFailureRecoveryChunk {
  readonly offset: string
  readonly nextOffset: string
  readonly bytes: Uint8Array
  readonly done: boolean
}
export interface ElectronPeerFailureRecovery {
  list(): Promise<readonly ElectronPeerFailureRecoveryHandle[]>
  read(handleId: string, offset: string): Promise<ElectronPeerFailureRecoveryChunk>
  acknowledge(handleId: string): Promise<void>
}
/** One caller-bound opaque durable terminal-evidence recovery record. */
export interface ElectronTerminalEvidenceRecoveryRecord {
  readonly handle: string
  readonly producer: 'scan' | 'subscription'
  readonly byteLength: number
  readonly digest: string
  readonly formatVersion: 1
}
export interface ElectronTerminalEvidenceRecoveryValue {
  readonly canonicalBytes: Uint8Array
  readonly digest: string
}
/**
 * Recovery exposes only aliases selected by trusted Electron-main code for the
 * authenticated renderer lease. Reading never retires evidence; acknowledge
 * is explicit and must follow durable consumption by the application.
 */
export interface ElectronTerminalEvidenceRecovery {
  list(): Promise<readonly ElectronTerminalEvidenceRecoveryRecord[]>
  listWithStatus(): Promise<
    Readonly<{
      records: readonly ElectronTerminalEvidenceRecoveryRecord[]
      busyProducers: readonly ('scan' | 'subscription')[]
    }>
  >
  read(handle: string, maximumBytes: number): Promise<ElectronTerminalEvidenceRecoveryValue>
  acknowledge(handle: string): Promise<void>
}
export interface ElectronScanFailureRecoveryOwner {
  readonly handleId: string
  readonly prepared: boolean
  readonly producerFinished: boolean
  readonly rawRetired: boolean
  readonly rawReleased: boolean
  readonly memberCount: number
  readonly released: boolean
}
export interface ElectronScanFailureRecoveryHead {
  readonly handleId: string
  readonly contiguousRead: string
  readonly acknowledged: boolean
}
export interface ElectronScanFailureRawIdentity {
  readonly host: string
  readonly scan: string
  readonly start: string
  readonly sourceGeneration: string
  readonly occurrence: string
}
export interface ElectronScanFailureRawInfo {
  readonly identity: ElectronScanFailureRawIdentity
  readonly memberCount: number
  readonly tokenCount: number
}
export interface ElectronScanFailureRawMember {
  readonly recipient: string
  readonly delivery: string
}
export type ElectronScanFailureRawToken =
  | Readonly<{ readonly kind: 'null' }>
  | Readonly<{ readonly kind: 'bool'; readonly value: boolean }>
  | Readonly<{ readonly kind: 'i64' | 'u64' | 'f64Bits'; readonly value: string }>
  | Readonly<{ readonly kind: 'map' | 'array'; readonly count: string }>
  | Readonly<{ readonly kind: 'text'; readonly total: string; readonly bytes: Uint8Array }>
  | Readonly<{ readonly kind: 'bytes'; readonly total: string; readonly bytes: Uint8Array }>
export interface ElectronScanFailureRecovery {
  list(): Promise<readonly { readonly handleId: string }[]>
  owners(recoveryId: string): Promise<readonly ElectronScanFailureRecoveryOwner[]>
  member(
    recoveryId: string,
    ownerHandle: string,
    index: number
  ): Promise<Readonly<{ recipient: string; delivery: string }>>
  head(recoveryId: string, ownerHandle: string, index: number): Promise<ElectronScanFailureRecoveryHead | null>
  read(recoveryId: string, headHandle: string, offset: string): Promise<ElectronPeerFailureRecoveryChunk>
  acknowledge(recoveryId: string, headHandle: string): Promise<void>
  retire(recoveryId: string, headHandle: string): Promise<void>
  release(recoveryId: string, headHandle: string): Promise<void>
  finalize(recoveryId: string, headHandle: string): Promise<string>
  pendingCleanupReceipts(recoveryId: string): Promise<readonly string[]>
  retireCleanup(recoveryId: string, cleanupReceiptId: string): Promise<void>
  rawInfo(recoveryId: string, ownerHandle: string): Promise<ElectronScanFailureRawInfo>
  rawMember(recoveryId: string, ownerHandle: string, index: number): Promise<ElectronScanFailureRawMember>
  rawToken(
    recoveryId: string,
    ownerHandle: string,
    index: number,
    offset: number,
    maxBytes: number
  ): Promise<ElectronScanFailureRawToken>
  rawRetire(recoveryId: string, ownerHandle: string): Promise<void>
  rawRelease(recoveryId: string, ownerHandle: string): Promise<void>
  rawFinalize(recoveryId: string, ownerHandle: string): Promise<string>
}
export type ElectronRendererBleManager = BleManager & {
  readonly peerFailureRecovery: ElectronPeerFailureRecovery
  readonly scanFailureRecovery: ElectronScanFailureRecovery
  readonly terminalEvidenceRecovery: ElectronTerminalEvidenceRecovery
}

export interface ElectronRendererBleManagerEnvironment {
  readonly transport: ElectronRendererIpcTransport<string, string>
  /** Enables source-native terminal evidence; the trusted Electron main owns its private store. */
  readonly terminalEvidence?: BleManagerCreateOptions['terminalEvidence']
}

/** Creates the common public manager over an authenticated preload transport. */
export async function createElectronRendererBleManager(
  environment: ElectronRendererBleManagerEnvironment
): Promise<ElectronRendererBleManager> {
  return rehydratePublicPromise(
    (async () => {
      const rendererClient = new ElectronRendererBleClient(environment.transport)
      const transport = new ElectronClientTransport(rendererClient)
      const ipc = await IpcBleManager.create(transport)
      return Object.assign(
        new IpcPublicManagerAdapter(ipc, {
          requireScanPlan: true,
          gattDeliverySelection: 'controllable',
          ...(environment.terminalEvidence === undefined ? {} : { terminalEvidence: environment.terminalEvidence })
        }),
        {
          peerFailureRecovery: createPeerFailureRecovery(ipc, ipc.bootstrap.versions.ipcProtocol.selected.value >= 10),
          scanFailureRecovery: createScanFailureRecovery(ipc, ipc.bootstrap.versions.ipcProtocol.selected.value >= 10),
          terminalEvidenceRecovery: createTerminalEvidenceRecovery(
            ipc,
            ipc.bootstrap.versions.ipcProtocol.selected.value >= 11
          )
        }
      )
    })()
  )
}

function createTerminalEvidenceRecovery(ipc: IpcBleManager, available: boolean): ElectronTerminalEvidenceRecovery {
  const operation = 'electron.terminal-evidence-recovery'
  const unavailable = (): never => {
    throw contractError('capability.unsupported', 'restoration', `${operation}.ipc-v11-required`)
  }
  const handle = (value: string, suffix: string): string => {
    if (value.length === 0) throw contractError('argument.invalid', 'restoration', `${operation}.${suffix}`)
    return value
  }
  const maximum = (value: number): number => {
    if (!Number.isSafeInteger(value) || value < 1)
      throw contractError('argument.invalid', 'restoration', `${operation}.read.maximum-bytes`)
    return value
  }
  const digest = (value: unknown, suffix: string): string => {
    if (typeof value !== 'string' || !/^[0-9a-f]{64}$/.test(value))
      throw contractError('protocol.malformed', 'ipc', `${operation}.${suffix}`)
    return value
  }
  const record = (value: unknown): ElectronTerminalEvidenceRecoveryRecord => {
    const candidate = recoveryRecord(value, `${operation}.list`)
    const producer = recoveryString(candidate, 'producer', `${operation}.list`)
    if (producer !== 'scan' && producer !== 'subscription')
      throw contractError('protocol.malformed', 'ipc', `${operation}.list`)
    const byteLength = candidate.byteLength
    if (typeof byteLength !== 'number' || !Number.isSafeInteger(byteLength) || byteLength < 0)
      throw contractError('protocol.malformed', 'ipc', `${operation}.list`)
    if (candidate.formatVersion !== 1) throw contractError('protocol.malformed', 'ipc', `${operation}.list`)
    return Object.freeze({
      handle: recoveryString(candidate, 'handle', `${operation}.list`),
      producer,
      byteLength,
      digest: digest(candidate.digest, 'list'),
      formatVersion: 1
    })
  }
  return Object.freeze({
    list: async () => {
      if (!available) unavailable()
      const response = await ipc.route('terminalEvidence.recovery.list', Object.freeze({ version: 1 }))
      if (!Array.isArray(response.records)) throw contractError('protocol.malformed', 'ipc', `${operation}.list`)
      return Object.freeze(response.records.map(record))
    },
    listWithStatus: async () => {
      if (!available) unavailable()
      const response = await ipc.route('terminalEvidence.recovery.listWithStatus', Object.freeze({ version: 1 }))
      if (!Array.isArray(response.records) || !Array.isArray(response.busyProducers))
        throw contractError('protocol.malformed', 'ipc', `${operation}.list-with-status`)
      const busyProducerSet = new Set<'scan' | 'subscription'>()
      const busyProducers = response.busyProducers.map(value => {
        if (value !== 'scan' && value !== 'subscription')
          throw contractError('protocol.malformed', 'ipc', `${operation}.list-with-status`)
        if (busyProducerSet.has(value))
          throw contractError('protocol.malformed', 'ipc', `${operation}.list-with-status`)
        busyProducerSet.add(value)
        return value
      })
      return Object.freeze({
        records: Object.freeze(response.records.map(record)),
        busyProducers: Object.freeze(busyProducers)
      })
    },
    read: async (opaqueHandle: string, maximumBytes: number) => {
      if (!available) unavailable()
      const response = await ipc.route(
        'terminalEvidence.recovery.read',
        Object.freeze({ version: 1, handle: handle(opaqueHandle, 'read.handle'), maximumBytes: maximum(maximumBytes) })
      )
      if (response.kind !== 'committed' || !(response.canonicalBytes instanceof Uint8Array))
        throw contractError('protocol.malformed', 'ipc', `${operation}.read`)
      if (response.canonicalBytes.byteLength > maximumBytes)
        throw contractError('protocol.violation', 'ipc', `${operation}.read-bound`)
      return Object.freeze({
        canonicalBytes: new Uint8Array(response.canonicalBytes),
        digest: digest(response.digest, 'read')
      })
    },
    acknowledge: async (opaqueHandle: string) => {
      if (!available) unavailable()
      const response = await ipc.route(
        'terminalEvidence.recovery.ack',
        Object.freeze({ version: 1, handle: handle(opaqueHandle, 'ack.handle') })
      )
      if (response.state !== 'released') throw contractError('protocol.malformed', 'ipc', `${operation}.ack`)
    }
  })
}

function createScanFailureRecovery(ipc: IpcBleManager, available: boolean): ElectronScanFailureRecovery {
  const assertAvailable = () => {
    if (!available)
      throw contractError('capability.unsupported', 'restoration', 'electron.scan-failure-recovery.ipc-v10-required')
  }
  const id = (value: string, operation: string) => {
    if (value.length === 0) throw contractError('argument.invalid', 'restoration', operation)
    return value
  }
  const page = async (command: string, payload: SerializableRecord) => {
    assertAvailable()
    return ipc.route(command, Object.freeze(payload))
  }
  const api: ElectronScanFailureRecovery = {
    list: async () => {
      const response = await page('scanFailureRecovery.list', Object.freeze({}))
      if (!Array.isArray(response.recoveries))
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.list')
      return Object.freeze(
        response.recoveries.map(value =>
          Object.freeze({
            handleId: recoveryString(
              recoveryRecord(value, 'electron.scan-failure-recovery.list'),
              'handleId',
              'electron.scan-failure-recovery.list'
            )
          })
        )
      )
    },
    owners: async recoveryId => {
      const response = await page(
        'scanFailureRecovery.owners',
        Object.freeze({ recoveryId: id(recoveryId, 'electron.scan-failure-recovery.owners') })
      )
      if (!Array.isArray(response.owners))
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.owners')
      return Object.freeze(
        response.owners.map(value => {
          const owner = recoveryRecord(value, 'electron.scan-failure-recovery.owners')
          const memberCount = owner.memberCount
          if (typeof memberCount !== 'number' || !Number.isSafeInteger(memberCount) || memberCount < 0)
            throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.owners')
          return Object.freeze({
            handleId: recoveryString(owner, 'handleId', 'electron.scan-failure-recovery.owners'),
            prepared: recoveryBoolean(owner, 'prepared', 'electron.scan-failure-recovery.owners'),
            producerFinished: recoveryBoolean(owner, 'producerFinished', 'electron.scan-failure-recovery.owners'),
            rawRetired: recoveryBoolean(owner, 'rawRetired', 'electron.scan-failure-recovery.owners'),
            rawReleased: recoveryBoolean(owner, 'rawReleased', 'electron.scan-failure-recovery.owners'),
            memberCount,
            released: recoveryBoolean(owner, 'released', 'electron.scan-failure-recovery.owners')
          })
        })
      )
    },
    member: async (recoveryId, ownerHandle, index) => {
      if (!Number.isSafeInteger(index) || index < 0)
        throw contractError('argument.invalid', 'restoration', 'electron.scan-failure-recovery.member')
      const response = await page(
        'scanFailureRecovery.member',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.member'),
          handleId: id(ownerHandle, 'electron.scan-failure-recovery.member'),
          index
        })
      )
      const member = recoveryRecord(response.member, 'electron.scan-failure-recovery.member')
      return Object.freeze({
        recipient: recoveryString(member, 'recipient', 'electron.scan-failure-recovery.member'),
        delivery: recoveryString(member, 'delivery', 'electron.scan-failure-recovery.member')
      })
    },
    head: async (recoveryId, ownerHandle, index) => {
      if (!Number.isSafeInteger(index) || index < 0)
        throw contractError('argument.invalid', 'restoration', 'electron.scan-failure-recovery.head')
      const response = await page(
        'scanFailureRecovery.head',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.head'),
          handleId: id(ownerHandle, 'electron.scan-failure-recovery.head'),
          index
        })
      )
      if (response.head === null) return null
      const head = recoveryRecord(response.head, 'electron.scan-failure-recovery.head')
      return Object.freeze({
        handleId: recoveryString(head, 'handleId', 'electron.scan-failure-recovery.head'),
        contiguousRead: recoveryString(head, 'contiguousRead', 'electron.scan-failure-recovery.head'),
        acknowledged: recoveryBoolean(head, 'acknowledged', 'electron.scan-failure-recovery.head')
      })
    },
    read: async (recoveryId, headHandle, offset) => {
      if (!/^(?:0|[1-9][0-9]*)$/.test(offset))
        throw contractError('argument.invalid', 'restoration', 'electron.scan-failure-recovery.read')
      const response = await page(
        'scanFailureRecovery.read',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.read'),
          handleId: id(headHandle, 'electron.scan-failure-recovery.read'),
          offset
        })
      )
      if (!(response.bytes instanceof Uint8Array))
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.read')
      return Object.freeze({
        offset: recoveryString(response, 'offset', 'electron.scan-failure-recovery.read'),
        nextOffset: recoveryString(response, 'nextOffset', 'electron.scan-failure-recovery.read'),
        bytes: new Uint8Array(response.bytes),
        done: recoveryBoolean(response, 'done', 'electron.scan-failure-recovery.read')
      })
    },
    acknowledge: async (recoveryId, headHandle) => {
      const response = await page(
        'scanFailureRecovery.ack',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.ack'),
          handleId: id(headHandle, 'electron.scan-failure-recovery.ack')
        })
      )
      if (response.acknowledged !== true)
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.ack')
    },
    retire: async (recoveryId, headHandle) => {
      const response = await page(
        'scanFailureRecovery.retire',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.retire'),
          handleId: id(headHandle, 'electron.scan-failure-recovery.retire')
        })
      )
      if (response.completed !== true)
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.retire')
    },
    release: async (recoveryId, headHandle) => {
      const response = await page(
        'scanFailureRecovery.release',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.release'),
          handleId: id(headHandle, 'electron.scan-failure-recovery.release')
        })
      )
      if (response.completed !== true)
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.release')
    },
    finalize: async (recoveryId, headHandle) => {
      const response = await page(
        'scanFailureRecovery.finalize',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.finalize'),
          handleId: id(headHandle, 'electron.scan-failure-recovery.finalize')
        })
      )
      return recoveryString(response, 'cleanupReceiptId', 'electron.scan-failure-recovery.finalize')
    },
    pendingCleanupReceipts: async recoveryId => {
      const response = await page(
        'scanFailureRecovery.cleanup.list',
        Object.freeze({ recoveryId: id(recoveryId, 'electron.scan-failure-recovery.cleanup.list') })
      )
      if (
        !Array.isArray(response.cleanupReceiptIds) ||
        response.cleanupReceiptIds.some(value => typeof value !== 'string' || value.length === 0)
      )
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.cleanup.list')
      return Object.freeze([...response.cleanupReceiptIds])
    },
    retireCleanup: async (recoveryId, cleanupReceiptId) => {
      const response = await page(
        'scanFailureRecovery.cleanup.retire',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.cleanup.retire'),
          handleId: id(cleanupReceiptId, 'electron.scan-failure-recovery.cleanup.retire')
        })
      )
      if (response.retired !== true)
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.cleanup.retire')
    },
    rawInfo: async (recoveryId, ownerHandle) => {
      const response = await page(
        'scanFailureRecovery.raw.info',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.raw.info'),
          handleId: id(ownerHandle, 'electron.scan-failure-recovery.raw.info')
        })
      )
      return parseRawInfo(response.info, 'electron.scan-failure-recovery.raw.info')
    },
    rawMember: async (recoveryId, ownerHandle, index) => {
      if (!Number.isSafeInteger(index) || index < 0)
        throw contractError('argument.invalid', 'restoration', 'electron.scan-failure-recovery.raw.member')
      const response = await page(
        'scanFailureRecovery.raw.member',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.raw.member'),
          handleId: id(ownerHandle, 'electron.scan-failure-recovery.raw.member'),
          index
        })
      )
      return parseRawMember(response.member, 'electron.scan-failure-recovery.raw.member')
    },
    rawToken: async (recoveryId, ownerHandle, index, offset, maxBytes) => {
      if (
        ![index, offset].every(value => Number.isSafeInteger(value) && value >= 0) ||
        !Number.isSafeInteger(maxBytes) ||
        maxBytes < 1 ||
        maxBytes > 65536
      )
        throw contractError('argument.invalid', 'restoration', 'electron.scan-failure-recovery.raw.token')
      const response = await page(
        'scanFailureRecovery.raw.token',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.raw.token'),
          handleId: id(ownerHandle, 'electron.scan-failure-recovery.raw.token'),
          index,
          offset,
          maxBytes
        })
      )
      return parseRawToken(response.token, 'electron.scan-failure-recovery.raw.token')
    },
    rawRetire: async (recoveryId, ownerHandle) => {
      const response = await page(
        'scanFailureRecovery.raw.retire',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.raw.retire'),
          handleId: id(ownerHandle, 'electron.scan-failure-recovery.raw.retire')
        })
      )
      if (response.completed !== true)
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.raw.retire')
    },
    rawRelease: async (recoveryId, ownerHandle) => {
      const response = await page(
        'scanFailureRecovery.raw.release',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.raw.release'),
          handleId: id(ownerHandle, 'electron.scan-failure-recovery.raw.release')
        })
      )
      if (response.completed !== true)
        throw contractError('protocol.malformed', 'ipc', 'electron.scan-failure-recovery.raw.release')
    },
    rawFinalize: async (recoveryId, ownerHandle) => {
      const response = await page(
        'scanFailureRecovery.raw.finalize',
        Object.freeze({
          recoveryId: id(recoveryId, 'electron.scan-failure-recovery.raw.finalize'),
          handleId: id(ownerHandle, 'electron.scan-failure-recovery.raw.finalize')
        })
      )
      return recoveryString(response, 'cleanupReceiptId', 'electron.scan-failure-recovery.raw.finalize')
    }
  }
  return Object.freeze(api)
}

function isRecoveryRecord(value: unknown): value is SerializableRecord {
  return value !== null && typeof value === 'object' && !Array.isArray(value) && !(value instanceof Uint8Array)
}
function recoveryRecord(value: unknown, operation: string): SerializableRecord {
  if (!isRecoveryRecord(value)) throw contractError('protocol.malformed', 'ipc', operation)
  return value
}
function recoveryString(value: SerializableRecord, key: string, operation: string): string {
  const field = value[key]
  if (typeof field !== 'string' || field.length === 0) throw contractError('protocol.malformed', 'ipc', operation)
  return field
}
function recoveryBoolean(value: SerializableRecord, key: string, operation: string): boolean {
  const field = value[key]
  if (typeof field !== 'boolean') throw contractError('protocol.malformed', 'ipc', operation)
  return field
}
function recoveryCount(value: SerializableRecord, key: string, operation: string): number {
  const field = value[key]
  if (typeof field !== 'number' || !Number.isSafeInteger(field) || field < 0)
    throw contractError('protocol.malformed', 'ipc', operation)
  return field
}
function recoveryDecimal(value: SerializableRecord, key: string, operation: string): string {
  const field = recoveryString(value, key, operation)
  if (!/^(?:0|[1-9][0-9]*)$/.test(field)) throw contractError('protocol.malformed', 'ipc', operation)
  return field
}
function recoveryBytes(value: SerializableRecord, key: string, operation: string): Uint8Array {
  const field = value[key]
  const bytes = field instanceof Uint8Array ? field : Array.isArray(field) ? field : null
  if (
    bytes === null ||
    bytes.some(byte => typeof byte !== 'number' || !Number.isInteger(byte) || byte < 0 || byte > 255)
  )
    throw contractError('protocol.malformed', 'ipc', operation)
  return new Uint8Array(bytes)
}
function parseRawMember(value: unknown, operation: string): ElectronScanFailureRawMember {
  const record = recoveryRecord(value, operation)
  return Object.freeze({
    recipient: recoveryString(record, 'recipient', operation),
    delivery: recoveryString(record, 'delivery', operation)
  })
}
function parseRawInfo(value: unknown, operation: string): ElectronScanFailureRawInfo {
  const record = recoveryRecord(value, operation)
  const identity = recoveryRecord(record.identity, operation)
  return Object.freeze({
    identity: Object.freeze({
      host: recoveryString(identity, 'host', operation),
      scan: recoveryString(identity, 'scan', operation),
      start: recoveryString(identity, 'start', operation),
      sourceGeneration: recoveryString(identity, 'sourceGeneration', operation),
      occurrence: recoveryString(identity, 'occurrence', operation)
    }),
    memberCount: recoveryCount(record, 'memberCount', operation),
    tokenCount: recoveryCount(record, 'tokenCount', operation)
  })
}
function parseRawToken(value: unknown, operation: string): ElectronScanFailureRawToken {
  const record = recoveryRecord(value, operation)
  const kind = recoveryString(record, 'kind', operation)
  if (kind === 'null') return Object.freeze({ kind })
  if (kind === 'bool') return Object.freeze({ kind, value: recoveryBoolean(record, 'value', operation) })
  if (kind === 'i64' || kind === 'u64' || kind === 'f64Bits')
    return Object.freeze({ kind, value: recoveryString(record, 'value', operation) })
  if (kind === 'map' || kind === 'array')
    return Object.freeze({ kind, count: recoveryDecimal(record, 'count', operation) })
  if (kind === 'text' || kind === 'bytes') {
    const total = recoveryDecimal(record, 'total', operation)
    const bytes = recoveryBytes(record, 'bytes', operation)
    if (BigInt(bytes.byteLength) > BigInt(total)) throw contractError('protocol.malformed', 'ipc', operation)
    return Object.freeze({ kind, total, bytes })
  }
  throw contractError('protocol.malformed', 'ipc', operation)
}
function createPeerFailureRecovery(ipc: IpcBleManager, available: boolean): ElectronPeerFailureRecovery {
  const assertAvailable = () => {
    if (!available)
      throw contractError('capability.unsupported', 'restoration', 'electron.peer-failure-recovery.ipc-v10-required')
  }
  return Object.freeze({
    list: async () => {
      assertAvailable()
      const response = await ipc.route('peerFailureRecovery.list', Object.freeze({}))
      if (!Array.isArray(response.recoveries))
        throw contractError('protocol.malformed', 'ipc', 'electron.peer-failure-recovery.list')
      return Object.freeze(
        response.recoveries.map(item => {
          const record = recoveryRecord(item, 'electron.peer-failure-recovery.list')
          const selector = recoveryRecord(record.selector, 'electron.peer-failure-recovery.list')
          const producerAck = recoveryString(record, 'producerAck', 'electron.peer-failure-recovery.list')
          if (producerAck !== 'not-started' && producerAck !== 'in-flight' && producerAck !== 'committed')
            throw contractError('protocol.malformed', 'ipc', 'electron.peer-failure-recovery.list')
          return Object.freeze({
            handleId: recoveryString(record, 'handleId', 'electron.peer-failure-recovery.list'),
            peerId: recoveryString(record, 'peerId', 'electron.peer-failure-recovery.list'),
            selector: Object.freeze({
              serviceUuid: recoveryString(selector, 'serviceUuid', 'electron.peer-failure-recovery.list'),
              serviceOccurrence: recoveryString(selector, 'serviceOccurrence', 'electron.peer-failure-recovery.list'),
              characteristicUuid: recoveryString(selector, 'characteristicUuid', 'electron.peer-failure-recovery.list'),
              characteristicOccurrence: recoveryString(
                selector,
                'characteristicOccurrence',
                'electron.peer-failure-recovery.list'
              )
            }),
            contiguousRead: recoveryString(record, 'contiguousRead', 'electron.peer-failure-recovery.list'),
            acknowledged: recoveryBoolean(record, 'acknowledged', 'electron.peer-failure-recovery.list'),
            producerAck
          })
        })
      )
    },
    read: async (handleId: string, offset: string) => {
      assertAvailable()
      if (handleId.length === 0 || !/^(?:0|[1-9][0-9]*)$/.test(offset))
        throw contractError('argument.invalid', 'restoration', 'electron.peer-failure-recovery.read')
      const response = await ipc.route('peerFailureRecovery.read', Object.freeze({ handleId, offset }))
      if (!(response.bytes instanceof Uint8Array))
        throw contractError('protocol.malformed', 'ipc', 'electron.peer-failure-recovery.read')
      return Object.freeze({
        offset: recoveryString(response, 'offset', 'electron.peer-failure-recovery.read'),
        nextOffset: recoveryString(response, 'nextOffset', 'electron.peer-failure-recovery.read'),
        bytes: new Uint8Array(response.bytes),
        done: recoveryBoolean(response, 'done', 'electron.peer-failure-recovery.read')
      })
    },
    acknowledge: async (handleId: string) => {
      assertAvailable()
      if (handleId.length === 0)
        throw contractError('argument.invalid', 'restoration', 'electron.peer-failure-recovery.ack')
      const response = await ipc.route('peerFailureRecovery.ack', Object.freeze({ handleId }))
      if (response.acknowledged !== true)
        throw contractError('protocol.malformed', 'ipc', 'electron.peer-failure-recovery.ack')
    }
  })
}

/** Explicit injection spelling used by deterministic and packed consumer tests. */
export const createElectronRendererBleManagerWithEnvironment = createElectronRendererBleManager

class ElectronClientTransport implements IpcClientTransport<string, string> {
  private readonly listeners = new Set<(event: IpcBleEvent) => void>()
  private pumping = false
  private pumpTerminated = false
  private healthNotice: IpcEventTransportHealthNotice | null = null
  private readonly healthListeners = new Set<(notice: IpcEventTransportHealthNotice) => void>()
  private nextEvent = 1

  constructor(private readonly client: ElectronRendererBleClient<string, string>) {}

  async invoke<Operation extends string>(
    request: IpcBleRequest<string, string, Operation>
  ): Promise<IpcBleResponse<string, string>> {
    if (request.kind === 'bootstrap') {
      return { kind: 'bootstrap', bootstrap: await this.client.initialize() }
    }
    if (request.kind === 'route') {
      const receipt = await this.client.request({
        command: request.envelope.command,
        payload: request.envelope.payload,
        binaryPayload: request.envelope.binaryPayload,
        signal: request.signal ?? null
      })
      return { kind: 'route', payload: receipt.payload }
    }
    if (request.kind === 'release') {
      return { kind: 'release', cleanup: await this.client.destroy() }
    }
    if (request.kind === 'event.ack') return { kind: 'event.ack' }
    throw contractError('protocol.malformed', 'ipc', 'electron-public-manager.request')
  }

  subscribe(listener: (event: IpcBleEvent) => void): () => void {
    this.listeners.add(listener)
    this.startPump()
    return () => this.listeners.delete(listener)
  }

  subscribeEventHealth(listener: (notice: IpcEventTransportHealthNotice) => void): () => void {
    this.healthListeners.add(listener)
    if (this.healthNotice !== null) listener(this.healthNotice)
    return () => this.healthListeners.delete(listener)
  }

  acknowledge(): Promise<{ kind: 'event.ack' }> {
    // ElectronRendererBleClient acknowledges the authenticated event itself.
    return Promise.resolve({ kind: 'event.ack' })
  }

  private startPump(): void {
    if (this.pumping || this.pumpTerminated) return
    this.pumping = true
    this.pump()
      .catch(error => this.failTransport('source-failed', transportError(error, 'event-iterator')))
      .finally(() => {
        this.pumping = false
      })
  }

  private async pump(): Promise<void> {
    for await (const item of this.client.events) {
      if (item.kind === 'overflow') {
        this.failTransport(
          'overflow',
          aggregateEventLossError('electron-public-manager.aggregate-event-loss', 'electron-renderer-events', item)
        )
        return
      }
      if (item.kind === 'terminal') {
        this.failTransport(
          item.reason === 'owner-released'
            ? 'owner-released'
            : item.reason === 'overflow'
              ? 'overflow'
              : 'source-failed',
          terminalError(item)
        )
        return
      }
      if (!isRendererStreamRecord(item.value)) {
        this.failTransport('source-failed', transportError(null, 'event-record'))
        return
      }
      let rendererLease
      try {
        rendererLease = this.client.bootstrap.rendererLease
      } catch (error) {
        this.failTransport('source-failed', transportError(error, 'renderer-lease'))
        return
      }
      const event: IpcBleEvent = Object.freeze({
        rendererLease,
        eventId: `electron-public-event-${this.nextEvent++}`,
        streamId: item.value.streamId,
        item: item.value.item
      })
      if (event.item.terminalEvidenceStageFailed !== undefined) {
        if (
          event.item.terminalEvidenceStageFailed !== true ||
          event.item.kind !== 'terminal' ||
          event.item.reason !== 'source-failed' ||
          event.item.evidence !== undefined
        ) {
          this.failTransport('source-failed', transportError(null, 'terminal-evidence-stage-failure-wire'))
          return
        }
        markAuthenticatedElectronTerminalEvidenceStageFailure(event.item)
      }
      for (const listener of [...this.listeners]) listener(event)
    }
    this.failTransport('source-failed', transportError(null, 'event-stream-ended'))
  }

  private failTransport(reason: IpcEventTransportHealthNotice['reason'], error: NormalizedBleError | null): void {
    if (this.pumpTerminated) return
    this.pumpTerminated = true
    this.healthNotice = Object.freeze({ reason, error })
    for (const listener of [...this.healthListeners]) listener(this.healthNotice)
  }
}

function terminalError(notice: StreamTerminalNotice): NormalizedBleError | null {
  if (notice.reason === 'owner-released') return null
  if (notice.reason === 'overflow') {
    return (
      notice.error ??
      aggregateEventLossError('electron-public-manager.aggregate-event-loss', 'electron-renderer-events', notice)
    )
  }
  return notice.error ?? transportError(null, `event-terminal-${notice.reason}`)
}

function transportError(error: unknown, operation: string): NormalizedBleError {
  if (error instanceof BackendContractError) return error.normalized
  return contractError('platform.transport', 'ipc', `electron-public-manager.${operation}`, {
    domain: 'electron-renderer-events',
    code: 'delivery-failed',
    safeMessage: error instanceof Error ? error.message : 'Electron event delivery failed',
    metadata: Object.freeze({})
  }).normalized
}

function isRendererStreamRecord(value: SerializableRecord): value is SerializableRecord & {
  readonly streamId: string
  readonly item: SerializableRecord
} {
  return typeof value.streamId === 'string' && isSerializableRecord(value.item)
}

function isSerializableRecord(value: unknown): value is SerializableRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}
