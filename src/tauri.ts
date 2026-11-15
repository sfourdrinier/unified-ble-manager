// src/tauri.ts — zero-plumbing Tauri application factory

export { createNativeContinuationControl } from './backends/desktop/native-continuation-controller'
export type {
  NativeContinuationControl,
  NativeContinuationControlV1,
  NativeContinuationControlAccess,
  NativeContinuationCompleted,
  NativeContinuationStatus
} from './backends/desktop/native-continuation-controller'
export { createNativeContinuationRecordingController } from './core/continuation-recording'
export type {
  ContinuationRecordingAccess,
  ContinuationRecordingController,
  OrderedRecordingOwnerStatus,
  OrderedRecordingStopReceipt,
  OrderedRecordingStopFailure
} from './core/continuation-recording'
export type {
  OrderedRecordingBatch,
  OrderedRecordingContext,
  OrderedRecordingRecord,
  OrderedRecordingMetadata,
  OrderedRecordingMembership,
  OrderedRecordingScope,
  OrderedRecordingLoss,
  OrderedRecordingFailure,
  OrderedRecordingFailureValue,
  OrderedRecordingRejection,
  OrderedRecordingReason
} from './core/ordered-recording'

import { contractError } from './backend-contract/errors'
import type { BleManagerCreateOptions } from './public/host-identity'
import { normalizeBleManagerCreateOptions } from './public/host-identity'
import { IpcBleManager } from './ipc/manager'
import { IpcPublicManagerAdapter } from './ipc/public-manager'
import { TauriBleIpcTransport } from './tauri/transport'
import type { TauriChannel, TauriInvoke } from './tauri/transport'
import type { BleManager } from './public/ble-manager'
import { rehydratePublicPromise } from './public/error-bridge'
import { TAURI_PLUGIN_COMPATIBILITY } from './tauri/compatibility'
import { contractError as tauriContractError } from './backend-contract/errors'
import { byteLimit, ownBytes, type SerializableRecord, type SerializableValue } from './backend-contract/primitives'

export { TAURI_PLUGIN_COMPATIBILITY }

/** Renderer-safe projection of a trusted-host durable recovery handle. */
export interface TauriPeerFailureRecoveryHandle {
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

export interface TauriPeerFailureRecoveryChunk {
  readonly offset: string
  readonly nextOffset: string
  readonly bytes: Uint8Array
  readonly done: boolean
}

/**
 * Additive recovery surface. It has no renderer-side enable operation:
 * trusted Rust application code grants one authenticated window after it has
 * attached an existing recording-owned ledger.
 */
export interface TauriPeerFailureRecovery {
  list(): Promise<readonly TauriPeerFailureRecoveryHandle[]>
  read(handleId: string, offset: string): Promise<TauriPeerFailureRecoveryChunk>
  acknowledge(handleId: string): Promise<void>
}

/** Renderer-safe scan-failure recovery projection.  Every identifier is an
 * opaque, caller-generation-bound host alias; none is a filesystem path or
 * native recovery capability. */
export interface TauriScanFailureRecoveryOwner {
  readonly handleId: string
  readonly prepared: boolean
  readonly producerFinished: boolean
  readonly rawRetired: boolean
  readonly rawReleased: boolean
  readonly memberCount: number
  readonly released: boolean
}

export interface TauriScanFailureRecoveryHead {
  readonly handleId: string
  readonly contiguousRead: string
  readonly acknowledged: boolean
}

export interface TauriScanFailureRecoveryChunk {
  readonly offset: string
  readonly nextOffset: string
  readonly bytes: Uint8Array
  readonly done: boolean
}

/** One caller-bound opaque durable-terminal-evidence recovery record. */
export interface TauriTerminalEvidenceRecoveryRecord {
  readonly handle: string
  readonly producer: 'scan' | 'subscription'
  readonly byteLength: number
  readonly digest: string
  readonly formatVersion: 1
}

export interface TauriTerminalEvidenceRecoveryValue {
  readonly canonicalBytes: Uint8Array
  readonly digest: string
}

/**
 * Recovery is constrained to native-selected aliases for the current
 * authenticated caller. No method accepts a ledger reference or storage path.
 */
export interface TauriTerminalEvidenceRecovery {
  list(): Promise<readonly TauriTerminalEvidenceRecoveryRecord[]>
  listWithStatus(): Promise<
    Readonly<{
      readonly records: readonly TauriTerminalEvidenceRecoveryRecord[]
      readonly busyProducers: readonly ('scan' | 'subscription')[]
    }>
  >
  read(handle: string, maximumBytes: number): Promise<TauriTerminalEvidenceRecoveryValue>
  acknowledge(handle: string): Promise<void>
}

/**
 * Version-10-only, trusted-host-granted scan failure recovery.  Renderers
 * cannot attach a recording or select a directory; they can only operate on
 * aliases returned from `list`/`owners` for their current lease.
 */
export interface TauriScanFailureRecovery {
  list(): Promise<readonly string[]>
  owners(recoveryId: string): Promise<readonly TauriScanFailureRecoveryOwner[]>
  member(
    recoveryId: string,
    ownerHandle: string,
    index: number
  ): Promise<Readonly<{ readonly recipient: string; readonly delivery: string }>>
  head(recoveryId: string, ownerHandle: string, index: number): Promise<TauriScanFailureRecoveryHead | null>
  read(recoveryId: string, headHandle: string, offset: string): Promise<TauriScanFailureRecoveryChunk>
  acknowledge(recoveryId: string, headHandle: string): Promise<void>
  retire(recoveryId: string, headHandle: string): Promise<void>
  release(recoveryId: string, headHandle: string): Promise<void>
  finalize(recoveryId: string, headHandle: string): Promise<string>
  pendingCleanupReceipts(recoveryId: string): Promise<readonly string[]>
  retireCleanup(recoveryId: string, cleanupHandle: string): Promise<void>
  rawInfo(recoveryId: string, ownerHandle: string): Promise<import('./public/cleanup').PublicSerializableRecord>
  rawMember(
    recoveryId: string,
    ownerHandle: string,
    index: number
  ): Promise<import('./public/cleanup').PublicSerializableRecord>
  rawToken(
    recoveryId: string,
    ownerHandle: string,
    index: number,
    offset: number,
    maxBytes: number
  ): Promise<import('./public/cleanup').PublicSerializableRecord>
  rawRetire(recoveryId: string, ownerHandle: string): Promise<void>
  rawRelease(recoveryId: string, ownerHandle: string): Promise<void>
  rawFinalize(recoveryId: string, ownerHandle: string): Promise<string>
}

export type TauriBleManager = BleManager & {
  readonly peerFailureRecovery: TauriPeerFailureRecovery
  readonly scanFailureRecovery: TauriScanFailureRecovery
  readonly terminalEvidenceRecovery: TauriTerminalEvidenceRecovery
}

// Tauri and Electron deliberately share the same public IPC projection. The
// host-specific module owns only transport loading and host-option admission;
// scan, connection, GATT, stream, lifecycle, and cleanup policy live in the
// common adapter.
const createTauriPublicManager = (
  ipc: IpcBleManager,
  terminalEvidence?: BleManagerCreateOptions['terminalEvidence']
): TauriBleManager => {
  const manager = new IpcPublicManagerAdapter(ipc, { gattDeliverySelection: 'controllable', terminalEvidence })
  return Object.assign(manager, {
    peerFailureRecovery: createTauriPeerFailureRecovery(ipc),
    scanFailureRecovery: createTauriScanFailureRecovery(ipc, ipc.bootstrap.versions.ipcProtocol.selected.value >= 10),
    terminalEvidenceRecovery: createTauriTerminalEvidenceRecovery(
      ipc,
      ipc.bootstrap.versions.ipcProtocol.selected.value >= 11
    )
  })
}

function recoveryString(record: SerializableRecord, key: string, operation: string): string {
  const value = record[key]
  if (typeof value !== 'string' || value.length === 0) throw tauriContractError('protocol.malformed', 'ipc', operation)
  return value
}

function recoveryBoolean(record: SerializableRecord, key: string, operation: string): boolean {
  const value = record[key]
  if (typeof value !== 'boolean') throw tauriContractError('protocol.malformed', 'ipc', operation)
  return value
}

function isRecoveryRecord(value: SerializableValue): value is SerializableRecord {
  return value !== null && typeof value === 'object' && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function recoveryRecord(record: SerializableRecord, key: string, operation: string): SerializableRecord {
  const value = record[key]
  if (value === undefined || !isRecoveryRecord(value)) {
    throw tauriContractError('protocol.malformed', 'ipc', operation)
  }
  return value
}

function recoveryIdentity(value: SerializableValue | undefined, operation: string): string {
  if (typeof value !== 'string' || !/^[0-9a-f]{32}$/.test(value))
    throw tauriContractError('protocol.malformed', 'ipc', operation)
  return value
}

function recoveryDecimal(value: SerializableValue | undefined, operation: string, signed = false): string {
  const pattern = signed ? /^(?:0|-?[1-9][0-9]*)$/ : /^(?:0|[1-9][0-9]*)$/
  if (typeof value !== 'string' || !pattern.test(value))
    throw tauriContractError('protocol.malformed', 'ipc', operation)
  return value
}

function rawRecoveryInfo(record: SerializableRecord, operation: string): SerializableRecord {
  const identity = recoveryRecord(record, 'identity', operation)
  return Object.freeze({
    identity: Object.freeze({
      host: recoveryIdentity(identity.host, operation),
      scan: recoveryIdentity(identity.scan, operation),
      start: recoveryIdentity(identity.start, operation),
      sourceGeneration: recoveryIdentity(identity.sourceGeneration, operation),
      occurrence: recoveryIdentity(identity.occurrence, operation)
    }),
    memberCount: recoveryCount(record.memberCount, operation),
    tokenCount: recoveryCount(record.tokenCount, operation)
  })
}

function rawRecoveryMember(record: SerializableRecord, operation: string): SerializableRecord {
  return Object.freeze({
    recipient: recoveryIdentity(record.recipient, operation),
    delivery: recoveryIdentity(record.delivery, operation)
  })
}

function rawRecoveryToken(record: SerializableRecord, operation: string): SerializableRecord {
  const kind = record.kind
  if (kind === 'null') return Object.freeze({ kind })
  if (kind === 'bool' && typeof record.value === 'boolean') return Object.freeze({ kind, value: record.value })
  if ((kind === 'i64' || kind === 'u64' || kind === 'f64Bits') && typeof record.value === 'string')
    return Object.freeze({ kind, value: recoveryDecimal(record.value, operation, kind === 'i64') })
  if ((kind === 'map' || kind === 'array') && typeof record.count === 'string')
    return Object.freeze({ kind, count: recoveryDecimal(record.count, operation) })
  if ((kind === 'text' || kind === 'bytes') && typeof record.total === 'string' && record.bytes instanceof Uint8Array) {
    if (record.bytes.byteLength > 65_536) throw tauriContractError('protocol.malformed', 'ipc', operation)
    return Object.freeze({
      kind,
      total: recoveryDecimal(record.total, operation),
      bytes: ownBytes(record.bytes, byteLimit(record.bytes.byteLength))
    })
  }
  throw tauriContractError('protocol.malformed', 'ipc', operation)
}

function createTauriPeerFailureRecovery(ipc: IpcBleManager): TauriPeerFailureRecovery {
  return Object.freeze({
    list: async (): Promise<readonly TauriPeerFailureRecoveryHandle[]> => {
      const response = await ipc.route('peerFailureRecovery.list', Object.freeze({}))
      const values = response.recoveries
      if (!Array.isArray(values))
        throw tauriContractError('protocol.malformed', 'ipc', 'tauri.peer-failure-recovery.list')
      return Object.freeze(
        values.map(value => {
          if (!isRecoveryRecord(value)) {
            throw tauriContractError('protocol.malformed', 'ipc', 'tauri.peer-failure-recovery.list')
          }
          const selector = recoveryRecord(value, 'selector', 'tauri.peer-failure-recovery.list')
          const producerAck = recoveryString(value, 'producerAck', 'tauri.peer-failure-recovery.list')
          if (producerAck !== 'not-started' && producerAck !== 'in-flight' && producerAck !== 'committed') {
            throw tauriContractError('protocol.malformed', 'ipc', 'tauri.peer-failure-recovery.list')
          }
          return Object.freeze({
            handleId: recoveryString(value, 'handleId', 'tauri.peer-failure-recovery.list'),
            peerId: recoveryString(value, 'peerId', 'tauri.peer-failure-recovery.list'),
            selector: Object.freeze({
              serviceUuid: recoveryString(selector, 'serviceUuid', 'tauri.peer-failure-recovery.list'),
              serviceOccurrence: recoveryString(selector, 'serviceOccurrence', 'tauri.peer-failure-recovery.list'),
              characteristicUuid: recoveryString(selector, 'characteristicUuid', 'tauri.peer-failure-recovery.list'),
              characteristicOccurrence: recoveryString(
                selector,
                'characteristicOccurrence',
                'tauri.peer-failure-recovery.list'
              )
            }),
            contiguousRead: recoveryString(value, 'contiguousRead', 'tauri.peer-failure-recovery.list'),
            acknowledged: recoveryBoolean(value, 'acknowledged', 'tauri.peer-failure-recovery.list'),
            producerAck
          })
        })
      )
    },
    read: async (handleId: string, offset: string): Promise<TauriPeerFailureRecoveryChunk> => {
      if (handleId.length === 0 || !/^(0|[1-9][0-9]*)$/.test(offset)) {
        throw tauriContractError('argument.invalid', 'restoration', 'tauri.peer-failure-recovery.read')
      }
      const response = await ipc.route('peerFailureRecovery.read', Object.freeze({ handleId, offset }))
      const bytes = response.bytes
      if (!(bytes instanceof Uint8Array))
        throw tauriContractError('protocol.malformed', 'ipc', 'tauri.peer-failure-recovery.read')
      return Object.freeze({
        offset: recoveryString(response, 'offset', 'tauri.peer-failure-recovery.read'),
        nextOffset: recoveryString(response, 'nextOffset', 'tauri.peer-failure-recovery.read'),
        bytes: ownBytes(bytes, byteLimit(18 * 1024 * 1024)),
        done: recoveryBoolean(response, 'done', 'tauri.peer-failure-recovery.read')
      })
    },
    acknowledge: async (handleId: string): Promise<void> => {
      if (handleId.length === 0)
        throw tauriContractError('argument.invalid', 'restoration', 'tauri.peer-failure-recovery.ack')
      const response = await ipc.route('peerFailureRecovery.ack', Object.freeze({ handleId }))
      if (response.acknowledged !== true)
        throw tauriContractError('protocol.malformed', 'ipc', 'tauri.peer-failure-recovery.ack')
    }
  })
}

function createTauriTerminalEvidenceRecovery(ipc: IpcBleManager, available: boolean): TauriTerminalEvidenceRecovery {
  const operation = 'tauri.terminal-evidence-recovery'
  const unavailable = (): never => {
    throw tauriContractError('capability.unsupported', 'restoration', `${operation}.ipc-v11-required`)
  }
  const handle = (value: string, suffix: string): string => {
    if (value.length === 0) throw tauriContractError('argument.invalid', 'restoration', `${operation}.${suffix}`)
    return value
  }
  const maximum = (value: number): number => {
    if (!Number.isSafeInteger(value) || value < 1)
      throw tauriContractError('argument.invalid', 'restoration', `${operation}.read.maximum-bytes`)
    return value
  }
  const digest = (value: SerializableValue | undefined, suffix: string): string => {
    if (typeof value !== 'string' || !/^[0-9a-f]{64}$/.test(value))
      throw tauriContractError('protocol.malformed', 'ipc', `${operation}.${suffix}`)
    return value
  }
  const record = (value: SerializableValue): TauriTerminalEvidenceRecoveryRecord => {
    if (!isRecoveryRecord(value)) throw tauriContractError('protocol.malformed', 'ipc', `${operation}.list`)
    const producer = recoveryString(value, 'producer', `${operation}.list`)
    if (producer !== 'scan' && producer !== 'subscription')
      throw tauriContractError('protocol.malformed', 'ipc', `${operation}.list`)
    const rawByteLength = value.byteLength
    if (typeof rawByteLength !== 'number' || !Number.isSafeInteger(rawByteLength) || rawByteLength < 0)
      throw tauriContractError('protocol.malformed', 'ipc', `${operation}.list`)
    if (value.formatVersion !== 1) throw tauriContractError('protocol.malformed', 'ipc', `${operation}.list`)
    return Object.freeze({
      handle: recoveryString(value, 'handle', `${operation}.list`),
      producer,
      byteLength: rawByteLength,
      digest: digest(value.digest, 'list'),
      formatVersion: 1
    })
  }
  return Object.freeze({
    list: async () => {
      if (!available) unavailable()
      const response = await ipc.route('terminalEvidence.recovery.list', Object.freeze({ version: 1 }))
      if (!Array.isArray(response.records)) throw tauriContractError('protocol.malformed', 'ipc', `${operation}.list`)
      return Object.freeze(response.records.map(record))
    },
    listWithStatus: async () => {
      if (!available) unavailable()
      const response = await ipc.route('terminalEvidence.recovery.listWithStatus', Object.freeze({ version: 1 }))
      if (!Array.isArray(response.records) || !Array.isArray(response.busyProducers))
        throw tauriContractError('protocol.malformed', 'ipc', `${operation}.list-with-status`)
      const busyProducers = response.busyProducers.map(value => {
        if (value !== 'scan' && value !== 'subscription')
          throw tauriContractError('protocol.malformed', 'ipc', `${operation}.list-with-status`)
        return value
      })
      if (new Set(busyProducers).size !== busyProducers.length)
        throw tauriContractError('protocol.malformed', 'ipc', `${operation}.list-with-status`)
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
        throw tauriContractError('protocol.malformed', 'ipc', `${operation}.read`)
      if (response.canonicalBytes.byteLength > maximumBytes)
        throw tauriContractError('protocol.violation', 'ipc', `${operation}.read-bound`)
      return Object.freeze({
        canonicalBytes: ownBytes(response.canonicalBytes, byteLimit(response.canonicalBytes.byteLength)),
        digest: digest(response.digest, 'read')
      })
    },
    acknowledge: async (opaqueHandle: string) => {
      if (!available) unavailable()
      const response = await ipc.route(
        'terminalEvidence.recovery.ack',
        Object.freeze({ version: 1, handle: handle(opaqueHandle, 'ack.handle') })
      )
      if (response.state !== 'released') throw tauriContractError('protocol.malformed', 'ipc', `${operation}.ack`)
    }
  })
}

function recoveryIndex(index: number, operation: string): number {
  if (!Number.isSafeInteger(index) || index < 0) throw tauriContractError('argument.invalid', 'restoration', operation)
  return index
}

function recoveryCount(value: SerializableValue | undefined, operation: string): number {
  if (typeof value !== 'number') throw tauriContractError('protocol.malformed', 'ipc', operation)
  return recoveryIndex(value, operation)
}

function recoveryHandle(value: string, operation: string): string {
  if (value.length === 0) throw tauriContractError('argument.invalid', 'restoration', operation)
  return value
}

function createTauriScanFailureRecovery(ipc: IpcBleManager, available: boolean): TauriScanFailureRecovery {
  const assertAvailable = () => {
    if (!available)
      throw tauriContractError('capability.unsupported', 'restoration', 'tauri.scan-failure-recovery.ipc-v10-required')
  }
  const route = (command: string, payload: SerializableRecord) => {
    assertAvailable()
    return ipc.route(`scanFailureRecovery.${command}`, payload)
  }
  const result = (response: SerializableRecord, key: string, operation: string): string =>
    recoveryString(response, key, operation)
  const recovery: TauriScanFailureRecovery = {
    list: async () => {
      const response = await route('list', Object.freeze({}))
      if (!Array.isArray(response.recoveries))
        throw tauriContractError('protocol.malformed', 'ipc', 'tauri.scan-failure-recovery.list')
      return Object.freeze(
        response.recoveries.map(value =>
          recoveryString(
            recoveryRecord({ value }, 'value', 'tauri.scan-failure-recovery.list'),
            'handleId',
            'tauri.scan-failure-recovery.list'
          )
        )
      )
    },
    owners: async recoveryId => {
      const operation = 'tauri.scan-failure-recovery.owners'
      const response = await route('owners', Object.freeze({ recoveryId: recoveryHandle(recoveryId, operation) }))
      if (!Array.isArray(response.owners)) throw tauriContractError('protocol.malformed', 'ipc', operation)
      return Object.freeze(
        response.owners.map(value => {
          if (!isRecoveryRecord(value)) throw tauriContractError('protocol.malformed', 'ipc', operation)
          return Object.freeze({
            handleId: recoveryString(value, 'handleId', operation),
            prepared: recoveryBoolean(value, 'prepared', operation),
            producerFinished: recoveryBoolean(value, 'producerFinished', operation),
            rawRetired: recoveryBoolean(value, 'rawRetired', operation),
            rawReleased: recoveryBoolean(value, 'rawReleased', operation),
            memberCount: recoveryCount(value.memberCount, operation),
            released: recoveryBoolean(value, 'released', operation)
          })
        })
      )
    },
    member: async (recoveryId, ownerHandle, index) => {
      const operation = 'tauri.scan-failure-recovery.member'
      const response = await route(
        'member',
        Object.freeze({
          recoveryId: recoveryHandle(recoveryId, operation),
          handleId: recoveryHandle(ownerHandle, operation),
          index: recoveryIndex(index, operation)
        })
      )
      const member = recoveryRecord(response, 'member', operation)
      return Object.freeze({
        recipient: recoveryString(member, 'recipient', operation),
        delivery: recoveryString(member, 'delivery', operation)
      })
    },
    head: async (recoveryId, ownerHandle, index) => {
      const operation = 'tauri.scan-failure-recovery.head'
      const response = await route(
        'head',
        Object.freeze({
          recoveryId: recoveryHandle(recoveryId, operation),
          handleId: recoveryHandle(ownerHandle, operation),
          index: recoveryIndex(index, operation)
        })
      )
      if (response.head === null) return null
      const head = recoveryRecord(response, 'head', operation)
      return Object.freeze({
        handleId: recoveryString(head, 'handleId', operation),
        contiguousRead: recoveryString(head, 'contiguousRead', operation),
        acknowledged: recoveryBoolean(head, 'acknowledged', operation)
      })
    },
    read: async (recoveryId, headHandle, offset) => {
      const operation = 'tauri.scan-failure-recovery.read'
      if (!/^(0|[1-9][0-9]*)$/.test(offset)) throw tauriContractError('argument.invalid', 'restoration', operation)
      const response = await route(
        'read',
        Object.freeze({
          recoveryId: recoveryHandle(recoveryId, operation),
          handleId: recoveryHandle(headHandle, operation),
          offset
        })
      )
      if (!(response.bytes instanceof Uint8Array)) throw tauriContractError('protocol.malformed', 'ipc', operation)
      return Object.freeze({
        offset: recoveryString(response, 'offset', operation),
        nextOffset: recoveryString(response, 'nextOffset', operation),
        bytes: ownBytes(response.bytes, byteLimit(18 * 1024 * 1024)),
        done: recoveryBoolean(response, 'done', operation)
      })
    },
    acknowledge: async (recoveryId, handleId) => {
      const operation = 'tauri.scan-failure-recovery.ack'
      if (
        (
          await route(
            'ack',
            Object.freeze({
              recoveryId: recoveryHandle(recoveryId, operation),
              handleId: recoveryHandle(handleId, operation)
            })
          )
        ).acknowledged !== true
      )
        throw tauriContractError('protocol.malformed', 'ipc', operation)
    },
    retire: async (recoveryId, handleId) => {
      const operation = 'tauri.scan-failure-recovery.retire'
      if (
        (
          await route(
            'retire',
            Object.freeze({
              recoveryId: recoveryHandle(recoveryId, operation),
              handleId: recoveryHandle(handleId, operation)
            })
          )
        ).completed !== true
      )
        throw tauriContractError('protocol.malformed', 'ipc', operation)
    },
    release: async (recoveryId, handleId) => {
      const operation = 'tauri.scan-failure-recovery.release'
      if (
        (
          await route(
            'release',
            Object.freeze({
              recoveryId: recoveryHandle(recoveryId, operation),
              handleId: recoveryHandle(handleId, operation)
            })
          )
        ).completed !== true
      )
        throw tauriContractError('protocol.malformed', 'ipc', operation)
    },
    finalize: async (recoveryId, handleId) =>
      result(
        await route(
          'finalize',
          Object.freeze({
            recoveryId: recoveryHandle(recoveryId, 'tauri.scan-failure-recovery.finalize'),
            handleId: recoveryHandle(handleId, 'tauri.scan-failure-recovery.finalize')
          })
        ),
        'cleanupReceiptId',
        'tauri.scan-failure-recovery.finalize'
      ),
    pendingCleanupReceipts: async recoveryId => {
      const operation = 'tauri.scan-failure-recovery.cleanup.list'
      const response = await route('cleanup.list', Object.freeze({ recoveryId: recoveryHandle(recoveryId, operation) }))
      if (
        !Array.isArray(response.cleanupReceiptIds) ||
        response.cleanupReceiptIds.some(value => typeof value !== 'string' || value.length === 0)
      )
        throw tauriContractError('protocol.malformed', 'ipc', operation)
      return Object.freeze([...response.cleanupReceiptIds])
    },
    retireCleanup: async (recoveryId, cleanupHandle) => {
      const operation = 'tauri.scan-failure-recovery.cleanup.retire'
      if (
        (
          await route(
            'cleanup.retire',
            Object.freeze({
              recoveryId: recoveryHandle(recoveryId, operation),
              handleId: recoveryHandle(cleanupHandle, operation)
            })
          )
        ).completed !== true
      )
        throw tauriContractError('protocol.malformed', 'ipc', operation)
    },
    rawInfo: async (recoveryId, ownerHandle) => {
      const operation = 'tauri.scan-failure-recovery.raw.info'
      return rawRecoveryInfo(
        recoveryRecord(
          await route(
            'raw.info',
            Object.freeze({
              recoveryId: recoveryHandle(recoveryId, operation),
              handleId: recoveryHandle(ownerHandle, operation)
            })
          ),
          'info',
          operation
        ),
        operation
      )
    },
    rawMember: async (recoveryId, ownerHandle, index) => {
      const operation = 'tauri.scan-failure-recovery.raw.member'
      return Object.freeze({
        member: rawRecoveryMember(
          recoveryRecord(
            await route(
              'raw.member',
              Object.freeze({
                recoveryId: recoveryHandle(recoveryId, operation),
                handleId: recoveryHandle(ownerHandle, operation),
                index: recoveryIndex(index, operation)
              })
            ),
            'member',
            operation
          ),
          operation
        )
      })
    },
    rawToken: async (recoveryId, ownerHandle, index, offset, maxBytes) => {
      const operation = 'tauri.scan-failure-recovery.raw.token'
      if (!Number.isSafeInteger(maxBytes) || maxBytes < 1 || maxBytes > 65_536)
        throw tauriContractError('argument.invalid', 'restoration', operation)
      return rawRecoveryToken(
        recoveryRecord(
          await route(
            'raw.token',
            Object.freeze({
              recoveryId: recoveryHandle(recoveryId, operation),
              handleId: recoveryHandle(ownerHandle, operation),
              index: recoveryIndex(index, operation),
              offset: recoveryIndex(offset, operation),
              maxBytes
            })
          ),
          'token',
          operation
        ),
        operation
      )
    },
    rawRetire: async (recoveryId, ownerHandle) => {
      const operation = 'tauri.scan-failure-recovery.raw.retire'
      if (
        (
          await route(
            'raw.retire',
            Object.freeze({
              recoveryId: recoveryHandle(recoveryId, operation),
              handleId: recoveryHandle(ownerHandle, operation)
            })
          )
        ).completed !== true
      )
        throw tauriContractError('protocol.malformed', 'ipc', operation)
    },
    rawRelease: async (recoveryId, ownerHandle) => {
      const operation = 'tauri.scan-failure-recovery.raw.release'
      if (
        (
          await route(
            'raw.release',
            Object.freeze({
              recoveryId: recoveryHandle(recoveryId, operation),
              handleId: recoveryHandle(ownerHandle, operation)
            })
          )
        ).completed !== true
      )
        throw tauriContractError('protocol.malformed', 'ipc', operation)
    },
    rawFinalize: async (recoveryId, ownerHandle) =>
      result(
        await route(
          'raw.finalize',
          Object.freeze({
            recoveryId: recoveryHandle(recoveryId, 'tauri.scan-failure-recovery.raw.finalize'),
            handleId: recoveryHandle(ownerHandle, 'tauri.scan-failure-recovery.raw.finalize')
          })
        ),
        'cleanupReceiptId',
        'tauri.scan-failure-recovery.raw.finalize'
      )
  }
  return Object.freeze(recovery)
}

// Normal Tauri factory — imports invoke/Channel from @tauri-apps/api/core internally.
// No transport plumbing from application code.
export async function createTauriBleManager(options: BleManagerCreateOptions = {}): Promise<TauriBleManager> {
  return rehydratePublicPromise(createTauriBleManagerInternal(options))
}

async function createTauriBleManagerInternal(options: BleManagerCreateOptions): Promise<TauriBleManager> {
  const normalized = normalizeTauriCreateOptions(options)
  let tauriCore: { invoke: TauriInvoke; Channel: new <T>() => TauriChannel<T> }
  try {
    const imported: { invoke: TauriInvoke; Channel: new <T>() => TauriChannel<T> } = await import(
      '@tauri-apps/api/core'
    )
    tauriCore = imported
  } catch {
    throw new Error(
      '[unified-ble-manager/tauri] @tauri-apps/api is required for createTauriBleManager(). ' +
        'Add it as a dependency or use createTauriBleManagerWithEnvironment for tests.'
    )
  }
  const transport = new TauriBleIpcTransport({
    invoke: tauriCore.invoke,
    Channel: tauriCore.Channel
  })
  const ipcManager = await IpcBleManager.create(transport, {
    traceMaximumRecords: normalized.diagnostics?.traceMaximumRecords,
    traceMaximumBytes: normalized.diagnostics?.traceMaximumBytes
  })
  await admitTauriCompatibility(ipcManager)
  await admitTauriCreateOptions(normalized, ipcManager)
  return createTauriPublicManager(ipcManager, normalized.terminalEvidence)
}

// Test/custom-host injection — retains explicit transport for deterministic tests.
export interface TauriBleManagerEnvironment {
  readonly invoke: TauriInvoke
  readonly Channel: new <T>() => TauriChannel<T>
}

export async function createTauriBleManagerWithEnvironment(
  environment: TauriBleManagerEnvironment,
  options: BleManagerCreateOptions = {}
): Promise<TauriBleManager> {
  return rehydratePublicPromise(createTauriBleManagerWithEnvironmentInternal(environment, options))
}

async function createTauriBleManagerWithEnvironmentInternal(
  environment: TauriBleManagerEnvironment,
  options: BleManagerCreateOptions
): Promise<TauriBleManager> {
  const normalized = normalizeTauriCreateOptions(options)
  const transport = new TauriBleIpcTransport(environment)
  const ipcManager = await IpcBleManager.create(transport, {
    traceMaximumRecords: normalized.diagnostics?.traceMaximumRecords,
    traceMaximumBytes: normalized.diagnostics?.traceMaximumBytes
  })
  await admitTauriCompatibility(ipcManager)
  await admitTauriCreateOptions(normalized, ipcManager)
  return createTauriPublicManager(ipcManager, normalized.terminalEvidence)
}

function normalizeTauriCreateOptions(options: BleManagerCreateOptions): BleManagerCreateOptions {
  const normalized = normalizeBleManagerCreateOptions(options)
  if (normalized.background !== undefined) {
    throw contractError('capability.unsupported', 'capability', 'tauri-manager.background')
  }
  if (normalized.randomBytes !== undefined) {
    throw contractError('capability.unsupported', 'capability', 'tauri-manager.random-bytes')
  }
  return normalized
}

function assertTauriCreateOptions(options: BleManagerCreateOptions, ipc: IpcBleManager): void {
  if (options.adapterId !== undefined && options.adapterId !== String(ipc.bootstrap.attachment.adapter.adapterId)) {
    throw contractError('adapter.unavailable', 'adapter', 'tauri-manager.adapter')
  }
  if (options.restoration !== undefined) {
    throw contractError('capability.unsupported', 'restoration', 'tauri-manager.restoration')
  }
  if (options.instanceId !== undefined) {
    throw contractError('capability.unsupported', 'core', 'tauri-manager.instance-id')
  }
}

async function admitTauriCompatibility(ipc: IpcBleManager): Promise<void> {
  const selected = ipc.bootstrap.versions.ipcProtocol.selected.value
  if (selected < TAURI_PLUGIN_COMPATIBILITY.ipcProtocolMinimum || selected > TAURI_PLUGIN_COMPATIBILITY.ipcProtocol) {
    return rejectTauriCompatibility(
      ipc,
      contractError('protocol.incompatible', 'ipc', 'tauri-manager.ipc-protocol', {
        domain: 'tauri-compatibility',
        code: 'ipc-protocol',
        safeMessage: `The Tauri plugin selected IPC protocol ${String(selected)}; this package supports ${String(TAURI_PLUGIN_COMPATIBILITY.ipcProtocolMinimum)} through ${String(TAURI_PLUGIN_COMPATIBILITY.ipcProtocol)}.`,
        metadata: {
          selected,
          minimum: TAURI_PLUGIN_COMPATIBILITY.ipcProtocolMinimum,
          maximum: TAURI_PLUGIN_COMPATIBILITY.ipcProtocol
        }
      })
    )
  }
  // F01: the factory serves traffic only from a host linked against the
  // pinned shared-core revision. A missing identity is a legacy (or
  // substituted) host, never an implicit pass.
  const reported = ipc.bootstrap.core?.contractRevision
  if (reported === TAURI_PLUGIN_COMPATIBILITY.contractRevision) {
    return
  }
  return rejectTauriCompatibility(
    ipc,
    new Error(
      reported === undefined
        ? '[unified-ble-manager/tauri] incompatible native host: bootstrap carries no shared-core contract revision'
        : `[unified-ble-manager/tauri] incompatible contract revision: host linked ${String(reported)}, npm package requires ${String(TAURI_PLUGIN_COMPATIBILITY.contractRevision)}`
    )
  )
}

async function rejectTauriCompatibility(ipc: IpcBleManager, error: Error): Promise<void> {
  try {
    const cleanup = await ipc.destroy()
    if (cleanup.state === 'release-failed') {
      throw new AggregateError([error, new Error('Tauri compatibility cleanup failed')], 'BLE manager admission failed')
    }
  } catch (cleanupError) {
    if (cleanupError instanceof AggregateError) throw cleanupError
    throw new AggregateError([error, cleanupError], 'BLE manager admission cleanup failed')
  }
  throw error
}

async function admitTauriCreateOptions(options: BleManagerCreateOptions, ipc: IpcBleManager): Promise<void> {
  try {
    assertTauriCreateOptions(options, ipc)
  } catch (error) {
    try {
      const cleanup = await ipc.destroy()
      if (cleanup.state === 'release-failed') {
        throw new AggregateError(
          [error, new Error('Tauri option rejection cleanup failed')],
          'BLE manager admission failed'
        )
      }
    } catch (cleanupError) {
      if (cleanupError instanceof AggregateError) throw cleanupError
      throw new AggregateError([error, cleanupError], 'BLE manager admission cleanup failed')
    }
    throw error
  }
}

export interface TauriBleProvider {
  /** Creates one independently leased manager for the authenticated Tauri webview. */
  createManager(): Promise<BleManager>
}

export function createTauriBleProvider(options: BleManagerCreateOptions = {}): TauriBleProvider {
  return Object.freeze({
    createManager: () => createTauriBleManager(options)
  })
}
export { createDesktopOrderedDiagnostics } from './ipc/ordered-diagnostics'
export type {
  DesktopOrderedDiagnosticClient,
  DesktopOrderedOrphanReceipt,
  DesktopOrderedOrphanSummary,
  DesktopOrderedRecordingStartSummary,
  DesktopOrderedMarkerWriteEvidence
} from './ipc/ordered-diagnostics'
