import { contractError } from '../../backend-contract/errors'
import { byteLimit, ownBytes, type SerializableRecord, type SerializableValue } from '../../backend-contract/primitives'
import { rehydratePublicPromise } from '../../public/error-bridge'
import type {
  DesktopRustCoreBinding,
  DesktopRustCoreScanFailureRecovery,
  DesktopRustCoreRecordingStore
} from './desktop-rust-core-binding'

/**
 * Trusted Node/Electron-main scan failure recovery access.  This deliberately
 * opens the host-selected recording store and returns only process-local
 * handles; it is not an IPC contract and must never be made available to a
 * renderer/preload context.
 */
export interface DesktopScanFailureRecovery {
  readonly leaseId: string
  owners(): Promise<readonly DesktopScanFailureRecoveryOwner[]>
  member(ownerHandle: string, index: number): Promise<DesktopScanFailureRecoveryMember>
  head(ownerHandle: string, index: number): Promise<DesktopScanFailureRecoveryHead | null>
  read(headHandle: string, offset: string): Promise<DesktopScanFailureRecoveryChunk>
  acknowledge(headHandle: string): Promise<void>
  retire(headHandle: string): Promise<void>
  release(headHandle: string): Promise<void>
  finalizeRelease(headHandle: string): Promise<string>
  pendingCleanupReceipts(): Promise<readonly string[]>
  retireCleanup(cleanupReceiptId: string): Promise<void>
  rawInfo(ownerHandle: string): Promise<SerializableRecord>
  rawMember(ownerHandle: string, index: number): Promise<SerializableRecord>
  rawToken(ownerHandle: string, index: number, offset: number, maxBytes: number): Promise<SerializableRecord>
  rawRetire(ownerHandle: string): Promise<void>
  rawRelease(ownerHandle: string): Promise<void>
  rawFinalize(ownerHandle: string): Promise<string>
}

export interface DesktopScanFailureRecoveryOwner {
  readonly handleId: string
  readonly prepared: boolean
  readonly producerFinished: boolean
  readonly rawRetired: boolean
  readonly rawReleased: boolean
  readonly memberCount: number
  readonly released: boolean
}
export interface DesktopScanFailureRecoveryMember {
  readonly recipient: string
  readonly delivery: string
}

export interface DesktopScanFailureRecoveryHead {
  readonly handleId: string
  readonly contiguousRead: string
  readonly acknowledged: boolean
}

export interface DesktopScanFailureRecoveryChunk {
  readonly offset: string
  readonly nextOffset: string
  readonly bytes: Uint8Array
  readonly done: boolean
}

type RecordValue = Readonly<Record<string, unknown>>

function isRecord(value: unknown): value is RecordValue {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function record(value: unknown, operation: string): RecordValue {
  if (!isRecord(value)) throw contractError('protocol.malformed', 'restoration', operation)
  return value
}

function string(value: RecordValue, field: string, operation: string): string {
  const result = value[field]
  if (typeof result !== 'string' || result.length === 0)
    throw contractError('protocol.malformed', 'restoration', operation)
  return result
}

function boolean(value: RecordValue, field: string, operation: string): boolean {
  const result = value[field]
  if (typeof result !== 'boolean') throw contractError('protocol.malformed', 'restoration', operation)
  return result
}

function number(value: RecordValue, field: string, operation: string): number {
  const result = value[field]
  if (typeof result !== 'number' || !Number.isSafeInteger(result) || result < 0)
    throw contractError('protocol.malformed', 'restoration', operation)
  return result
}

function json(value: string, operation: string): RecordValue {
  try {
    return record(JSON.parse(value), operation)
  } catch (error) {
    if (error instanceof Error && error.name === 'BackendContractError') throw error
    throw contractError('protocol.malformed', 'restoration', operation)
  }
}

function serializable(value: unknown, operation: string): SerializableValue {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (Array.isArray(value)) return Object.freeze(value.map(item => serializable(item, operation)))
  if (isRecord(value)) {
    const result: Record<string, SerializableValue> = {}
    for (const [key, item] of Object.entries(value)) result[key] = serializable(item, operation)
    return Object.freeze(result)
  }
  throw contractError('protocol.malformed', 'restoration', operation)
}

function serializableRecord(value: string, operation: string): SerializableRecord {
  const result = serializable(json(value, operation), operation)
  if (!isRecord(result)) throw contractError('protocol.malformed', 'restoration', operation)
  return result
}

/** Converts N-API's JSON byte array into an owned public binary value at the native boundary. */
function rawTokenRecord(value: string, operation: string): SerializableRecord {
  const token = serializableRecord(value, operation)
  const kind = token.kind
  if (kind !== 'text' && kind !== 'bytes') return token
  const source = token.bytes
  if (
    !Array.isArray(source) ||
    source.some(byte => typeof byte !== 'number' || !Number.isInteger(byte) || byte < 0 || byte > 255)
  )
    throw contractError('protocol.malformed', 'restoration', operation)
  const bytes = new Uint8Array(source)
  return Object.freeze({ ...token, bytes: ownBytes(bytes, byteLimit(bytes.byteLength)) })
}

function assertHandle(value: string, operation: string): void {
  if (value.length === 0) throw contractError('argument.invalid', 'restoration', operation)
}

function recoveryStore(store: DesktopRustCoreRecordingStore): DesktopRustCoreScanFailureRecovery {
  const recovery = store.scanFailureRecovery?.()
  if (recovery === undefined) throw contractError('capability.unsupported', 'restoration', 'scan-failure-recovery.open')
  return recovery
}

/** Opens host-selected durable recovery without initializing or enumerating a radio. */
export function openDesktopScanFailureRecovery(
  binding: DesktopRustCoreBinding,
  directory: string,
  recordingId: string
): Promise<DesktopScanFailureRecovery> {
  return rehydratePublicPromise(
    (async () => {
      if (directory.length === 0 || recordingId.length === 0)
        throw contractError('argument.invalid', 'restoration', 'scan-failure-recovery.open')
      if (binding.openRecordingStore === undefined)
        throw contractError('capability.unsupported', 'restoration', 'scan-failure-recovery.open')
      const store = await binding.openRecordingStore(directory)
      const recovery = recoveryStore(store)
      const attached = json(await recovery.attach(recordingId), 'scan-failure-recovery.attach')
      const leaseId = string(attached, 'leaseId', 'scan-failure-recovery.attach')
      const api: DesktopScanFailureRecovery = {
        leaseId,
        owners: async () => {
          const response = json(await recovery.owners(leaseId), 'scan-failure-recovery.owners')
          if (!Array.isArray(response.owners))
            throw contractError('protocol.malformed', 'restoration', 'scan-failure-recovery.owners')
          return Object.freeze(
            response.owners.map(value => {
              const owner = record(value, 'scan-failure-recovery.owners')
              return Object.freeze({
                handleId: string(owner, 'handleId', 'scan-failure-recovery.owners'),
                prepared: boolean(owner, 'prepared', 'scan-failure-recovery.owners'),
                producerFinished: boolean(owner, 'producerFinished', 'scan-failure-recovery.owners'),
                rawRetired: boolean(owner, 'rawRetired', 'scan-failure-recovery.owners'),
                rawReleased: boolean(owner, 'rawReleased', 'scan-failure-recovery.owners'),
                memberCount: number(owner, 'memberCount', 'scan-failure-recovery.owners'),
                released: boolean(owner, 'released', 'scan-failure-recovery.owners')
              })
            })
          )
        },
        member: async (ownerHandle: string, index: number) => {
          assertHandle(ownerHandle, 'scan-failure-recovery.member')
          if (!Number.isSafeInteger(index) || index < 0)
            throw contractError('argument.invalid', 'restoration', 'scan-failure-recovery.member')
          const member = record(
            json(await recovery.member(ownerHandle, index), 'scan-failure-recovery.member').member,
            'scan-failure-recovery.member'
          )
          return Object.freeze({
            recipient: string(member, 'recipient', 'scan-failure-recovery.member'),
            delivery: string(member, 'delivery', 'scan-failure-recovery.member')
          })
        },
        head: async (ownerHandle: string, index: number) => {
          assertHandle(ownerHandle, 'scan-failure-recovery.head')
          if (!Number.isSafeInteger(index) || index < 0)
            throw contractError('argument.invalid', 'restoration', 'scan-failure-recovery.head')
          const response = json(await recovery.head(ownerHandle, index), 'scan-failure-recovery.head')
          if (response.head === null) return null
          const head = record(response.head, 'scan-failure-recovery.head')
          return Object.freeze({
            handleId: string(head, 'handleId', 'scan-failure-recovery.head'),
            contiguousRead: string(head, 'contiguousRead', 'scan-failure-recovery.head'),
            acknowledged: boolean(head, 'acknowledged', 'scan-failure-recovery.head')
          })
        },
        read: async (headHandle: string, offset: string) => {
          assertHandle(headHandle, 'scan-failure-recovery.read')
          if (!/^(?:0|[1-9][0-9]*)$/.test(offset))
            throw contractError('argument.invalid', 'restoration', 'scan-failure-recovery.read')
          const response = json(await recovery.read(headHandle, offset), 'scan-failure-recovery.read')
          if (
            !Array.isArray(response.bytes) ||
            response.bytes.some(byte => typeof byte !== 'number' || !Number.isInteger(byte) || byte < 0 || byte > 255)
          )
            throw contractError('protocol.malformed', 'restoration', 'scan-failure-recovery.read')
          return Object.freeze({
            offset: string(response, 'offset', 'scan-failure-recovery.read'),
            nextOffset: string(response, 'nextOffset', 'scan-failure-recovery.read'),
            bytes: new Uint8Array(response.bytes),
            done: boolean(response, 'done', 'scan-failure-recovery.read')
          })
        },
        acknowledge: async (headHandle: string) => {
          assertHandle(headHandle, 'scan-failure-recovery.ack')
          await recovery.acknowledge(headHandle)
        },
        retire: async (headHandle: string) => {
          assertHandle(headHandle, 'scan-failure-recovery.retire')
          await recovery.retire(headHandle)
        },
        release: async (headHandle: string) => {
          assertHandle(headHandle, 'scan-failure-recovery.release')
          await recovery.release(headHandle)
        },
        finalizeRelease: async (headHandle: string) => {
          assertHandle(headHandle, 'scan-failure-recovery.finalize-release')
          return string(
            json(await recovery.finalizeRelease(headHandle), 'scan-failure-recovery.finalize-release'),
            'cleanupReceiptId',
            'scan-failure-recovery.finalize-release'
          )
        },
        pendingCleanupReceipts: async () => {
          const response = json(await recovery.pendingCleanupReceipts(leaseId), 'scan-failure-recovery.pending-cleanup')
          if (
            !Array.isArray(response.cleanupReceiptIds) ||
            response.cleanupReceiptIds.some(value => typeof value !== 'string' || value.length === 0)
          )
            throw contractError('protocol.malformed', 'restoration', 'scan-failure-recovery.pending-cleanup')
          return Object.freeze([...response.cleanupReceiptIds])
        },
        retireCleanup: async (cleanupReceiptId: string) => {
          assertHandle(cleanupReceiptId, 'scan-failure-recovery.retire-cleanup')
          await recovery.retireCleanup(cleanupReceiptId)
        },
        rawInfo: async ownerHandle => {
          assertHandle(ownerHandle, 'scan-failure-recovery.raw-info')
          return serializableRecord(await recovery.rawInfo(ownerHandle), 'scan-failure-recovery.raw-info')
        },
        rawMember: async (ownerHandle, index) => {
          assertHandle(ownerHandle, 'scan-failure-recovery.raw-member')
          if (!Number.isSafeInteger(index) || index < 0)
            throw contractError('argument.invalid', 'restoration', 'scan-failure-recovery.raw-member')
          return serializableRecord(await recovery.rawMember(ownerHandle, index), 'scan-failure-recovery.raw-member')
        },
        rawToken: async (ownerHandle, index, offset, maxBytes) => {
          assertHandle(ownerHandle, 'scan-failure-recovery.raw-token')
          if (
            ![index, offset].every(value => Number.isSafeInteger(value) && value >= 0) ||
            !Number.isSafeInteger(maxBytes) ||
            maxBytes < 1 ||
            maxBytes > 65_536
          )
            throw contractError('argument.invalid', 'restoration', 'scan-failure-recovery.raw-token')
          return rawTokenRecord(
            await recovery.rawToken(ownerHandle, index, offset, maxBytes),
            'scan-failure-recovery.raw-token'
          )
        },
        rawRetire: async ownerHandle => {
          assertHandle(ownerHandle, 'scan-failure-recovery.raw-retire')
          await recovery.rawRetire(ownerHandle)
        },
        rawRelease: async ownerHandle => {
          assertHandle(ownerHandle, 'scan-failure-recovery.raw-release')
          await recovery.rawRelease(ownerHandle)
        },
        rawFinalize: async ownerHandle => {
          assertHandle(ownerHandle, 'scan-failure-recovery.raw-finalize')
          return string(
            json(await recovery.rawFinalize(ownerHandle), 'scan-failure-recovery.raw-finalize'),
            'cleanupReceiptId',
            'scan-failure-recovery.raw-finalize'
          )
        }
      }
      return Object.freeze(api)
    })()
  )
}
