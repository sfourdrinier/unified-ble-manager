import { contractError } from '../../backend-contract/errors'
import { byteLimit, ownBytes, type OwnedBytes } from '../../backend-contract/primitives'
import type { Spec as NativeRustCore } from '../../NativeUnifiedBleRustCore'

export interface ReactNativeScanFailureRecoveryPage {
  readonly offset: string
  readonly nextOffset: string
  readonly done: boolean
  readonly bytes: OwnedBytes
}

export type ReactNativeRawRecoveryToken =
  | Readonly<{ kind: 'null' }>
  | Readonly<{ kind: 'bool'; value: boolean }>
  | Readonly<{ kind: 'i64' | 'u64' | 'f64Bits'; value: string }>
  | Readonly<{ kind: 'map' | 'array'; count: string }>
  | Readonly<{ kind: 'text' | 'bytes'; total: string; bytes: OwnedBytes }>

export interface ReactNativeRawRecoveryInfo {
  readonly identity: Readonly<{
    readonly host: string
    readonly scan: string
    readonly start: string
    readonly sourceGeneration: string
    readonly occurrence: string
  }>
  readonly memberCount: number
  readonly tokenCount: number
}

export interface ReactNativeRawRecoveryMember {
  readonly member: Readonly<{ readonly recipient: string; readonly delivery: string }>
}

export interface ReactNativeScanFailureRecovery {
  open(): Promise<string>
  owners(leaseHandle: string): Promise<string>
  member(ownerHandle: string, index: number): Promise<string>
  head(ownerHandle: string, index: number): Promise<string>
  read(headHandle: string, offset: string): ReactNativeScanFailureRecoveryPage
  acknowledge(headHandle: string): Promise<string>
  retire(headHandle: string): Promise<string>
  release(headHandle: string): Promise<string>
  finalize(headHandle: string): Promise<string>
  pendingCleanup(leaseHandle: string): Promise<string>
  retireCleanup(cleanupReceiptHandle: string): Promise<void>
  rawInfo(ownerHandle: string): Promise<ReactNativeRawRecoveryInfo>
  rawMember(ownerHandle: string, index: number): Promise<ReactNativeRawRecoveryMember>
  rawToken(ownerHandle: string, index: number, offset: string, maxBytes: string): ReactNativeRawRecoveryToken
  rawRetire(ownerHandle: string): Promise<Readonly<{ completed: true }>>
  rawRelease(ownerHandle: string): Promise<Readonly<{ completed: true }>>
  rawFinalize(ownerHandle: string): Promise<Readonly<{ cleanupReceiptId: string }>>
}

interface NativeScanFailureRecoveryPage {
  readonly offset: unknown
  readonly nextOffset: unknown
  readonly done: unknown
  readonly bytes: unknown
}

interface NativeScanFailureRecoveryRuntime {
  read(headHandle: string, offset: string): NativeScanFailureRecoveryPage
  rawToken(ownerHandle: string, index: number, offset: string, maxBytes: string): unknown
}

declare global {
  var __unifiedBleRustCoreScanFailureRecoveryV1: NativeScanFailureRecoveryRuntime | undefined
}

function canonicalOffset(value: string, operation: string): void {
  if (!/^(?:0|[1-9][0-9]*)$/.test(value)) throw contractError('argument.invalid', 'restoration', operation)
}

function canonicalPositiveBound(value: string, operation: string): void {
  canonicalOffset(value, operation)
  const parsed = Number(value)
  if (!Number.isSafeInteger(parsed) || parsed < 1 || parsed > 65_536)
    throw contractError('argument.invalid', 'restoration', operation)
}

function object(value: unknown, operation: string): Record<string, unknown> {
  if (!isRecord(value)) throw contractError('protocol.malformed', 'restoration', operation)
  return value
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function json(value: string, operation: string): Record<string, unknown> {
  try {
    return object(JSON.parse(value), operation)
  } catch (error) {
    if (error instanceof Error && error.message.includes('protocol.malformed')) throw error
    throw contractError('protocol.malformed', 'restoration', operation)
  }
}

function hex(value: unknown, operation: string): string {
  if (typeof value !== 'string' || !/^[0-9a-f]+$/.test(value))
    throw contractError('protocol.malformed', 'restoration', operation)
  return value
}

function count(value: unknown, operation: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0)
    throw contractError('protocol.malformed', 'restoration', operation)
  return value
}

function decimal(value: unknown, operation: string, signed = false): string {
  const pattern = signed ? /^(?:0|-?[1-9][0-9]*)$/ : /^(?:0|[1-9][0-9]*)$/
  if (typeof value !== 'string' || !pattern.test(value))
    throw contractError('protocol.malformed', 'restoration', operation)
  return value
}

function rawInfo(value: string): ReactNativeRawRecoveryInfo {
  const operation = 'scan-failure-recovery.raw.info'
  const source = json(value, operation)
  const identity = object(source.identity, operation)
  return Object.freeze({
    identity: Object.freeze({
      host: hex(identity.host, operation),
      scan: hex(identity.scan, operation),
      start: hex(identity.start, operation),
      sourceGeneration: hex(identity.sourceGeneration, operation),
      occurrence: hex(identity.occurrence, operation)
    }),
    memberCount: count(source.memberCount, operation),
    tokenCount: count(source.tokenCount, operation)
  })
}

function rawMember(value: string): ReactNativeRawRecoveryMember {
  const operation = 'scan-failure-recovery.raw.member'
  const source = object(json(value, operation).member, operation)
  return Object.freeze({
    member: Object.freeze({ recipient: hex(source.recipient, operation), delivery: hex(source.delivery, operation) })
  })
}

function rawControlComplete(value: string, operation: string): Readonly<{ completed: true }> {
  const source = json(value, operation)
  const receipt = source.cleanupReceiptId
  if (receipt !== null) throw contractError('protocol.malformed', 'restoration', operation)
  return Object.freeze({ completed: true })
}

function rawControlFinalize(value: string, operation: string): Readonly<{ cleanupReceiptId: string }> {
  const receipt = json(value, operation).cleanupReceiptId
  if (typeof receipt !== 'string' || receipt.length === 0)
    throw contractError('protocol.malformed', 'restoration', operation)
  return Object.freeze({ cleanupReceiptId: receipt })
}

/** Reads one canonical tagged raw token without JSON byte-array expansion. */
export function readReactNativeRawRecoveryToken(
  ownerHandle: string,
  index: number,
  offset: string,
  maxBytes: string
): ReactNativeRawRecoveryToken {
  const operation = 'scan-failure-recovery.raw.token'
  if (ownerHandle.length === 0 || !Number.isSafeInteger(index) || index < 0)
    throw contractError('argument.invalid', 'restoration', operation)
  canonicalOffset(offset, operation)
  canonicalPositiveBound(maxBytes, operation)
  const runtime = globalThis.__unifiedBleRustCoreScanFailureRecoveryV1
  if (runtime === undefined) throw contractError('capability.unsupported', 'restoration', operation)
  const token = object(runtime.rawToken(ownerHandle, index, offset, maxBytes), operation)
  const kind = token.kind
  if (kind === 'null') return Object.freeze({ kind })
  if (kind === 'bool' && typeof token.value === 'boolean') return Object.freeze({ kind, value: token.value })
  if ((kind === 'i64' || kind === 'u64' || kind === 'f64Bits') && typeof token.value === 'string')
    return Object.freeze({ kind, value: decimal(token.value, operation, kind === 'i64') })
  if ((kind === 'map' || kind === 'array') && typeof token.count === 'string')
    return Object.freeze({ kind, count: decimal(token.count, operation) })
  if ((kind === 'text' || kind === 'bytes') && typeof token.total === 'string' && token.bytes instanceof Uint8Array) {
    if (token.bytes.byteLength > 65_536) throw contractError('protocol.malformed', 'restoration', operation)
    return Object.freeze({
      kind,
      total: decimal(token.total, operation),
      bytes: ownBytes(new Uint8Array(token.bytes), byteLimit(token.bytes.byteLength))
    })
  }
  throw contractError('protocol.malformed', 'restoration', operation)
}

/**
 * Reads one page from a recording selected by native application bootstrap.
 * The caller can only present opaque head handles it already received from the
 * native recovery flow; it cannot name a recording or filesystem location.
 */
export function readReactNativeScanFailureRecoveryPage(
  headHandle: string,
  offset: string
): ReactNativeScanFailureRecoveryPage {
  const operation = 'scan-failure-recovery.read'
  if (headHandle.length === 0) throw contractError('argument.invalid', 'restoration', operation)
  canonicalOffset(offset, operation)
  const runtime = globalThis.__unifiedBleRustCoreScanFailureRecoveryV1
  if (runtime === undefined) throw contractError('capability.unsupported', 'restoration', operation)
  const page = runtime.read(headHandle, offset)
  if (
    typeof page.offset !== 'string' ||
    typeof page.nextOffset !== 'string' ||
    typeof page.done !== 'boolean' ||
    !(page.bytes instanceof Uint8Array)
  )
    throw contractError('protocol.malformed', 'restoration', operation)
  canonicalOffset(page.offset, operation)
  canonicalOffset(page.nextOffset, operation)
  return Object.freeze({
    offset: page.offset,
    nextOffset: page.nextOffset,
    done: page.done,
    bytes: ownBytes(new Uint8Array(page.bytes), byteLimit(page.bytes.byteLength))
  })
}

/** Creates the complete selected-recording lifecycle using opaque native handles. */
export function createReactNativeScanFailureRecovery(native: NativeRustCore): ReactNativeScanFailureRecovery {
  const handle = (value: string, operation: string): void => {
    if (value.length === 0) throw contractError('argument.invalid', 'restoration', operation)
  }
  const indexed = (handleValue: string, index: number, operation: string): void => {
    handle(handleValue, operation)
    if (!Number.isSafeInteger(index) || index < 0) throw contractError('argument.invalid', 'restoration', operation)
  }
  const control = (operation: 'acknowledge' | 'retire' | 'release' | 'finalize') => async (headHandle: string) => {
    handle(headHandle, `scan-failure-recovery.${operation}`)
    return native.scanFailureRecoveryControl(headHandle, operation)
  }
  return Object.freeze({
    open: () => native.scanFailureRecoveryOpenSelected(),
    owners: async (leaseHandle: string) => {
      handle(leaseHandle, 'scan-failure-recovery.owners')
      return native.scanFailureRecoveryOwners(leaseHandle)
    },
    member: async (ownerHandle: string, index: number) => {
      indexed(ownerHandle, index, 'scan-failure-recovery.member')
      return native.scanFailureRecoveryMember(ownerHandle, index)
    },
    head: async (ownerHandle: string, index: number) => {
      indexed(ownerHandle, index, 'scan-failure-recovery.head')
      return native.scanFailureRecoveryHead(ownerHandle, index)
    },
    read: readReactNativeScanFailureRecoveryPage,
    acknowledge: control('acknowledge'),
    retire: control('retire'),
    release: control('release'),
    finalize: control('finalize'),
    pendingCleanup: async (leaseHandle: string) => {
      handle(leaseHandle, 'scan-failure-recovery.cleanup.list')
      return native.scanFailureRecoveryPendingCleanup(leaseHandle)
    },
    retireCleanup: async (cleanupReceiptHandle: string) => {
      handle(cleanupReceiptHandle, 'scan-failure-recovery.cleanup.retire')
      await native.scanFailureRecoveryRetireCleanup(cleanupReceiptHandle)
    },
    rawInfo: async (ownerHandle: string) => {
      handle(ownerHandle, 'scan-failure-recovery.raw.info')
      return rawInfo(await native.scanFailureRecoveryRawInfo(ownerHandle))
    },
    rawMember: async (ownerHandle: string, index: number) => {
      indexed(ownerHandle, index, 'scan-failure-recovery.raw.member')
      return rawMember(await native.scanFailureRecoveryRawMember(ownerHandle, index))
    },
    rawToken: readReactNativeRawRecoveryToken,
    rawRetire: async (ownerHandle: string) => {
      handle(ownerHandle, 'scan-failure-recovery.raw.retire')
      return rawControlComplete(
        await native.scanFailureRecoveryRawControl(ownerHandle, 'retire'),
        'scan-failure-recovery.raw.retire'
      )
    },
    rawRelease: async (ownerHandle: string) => {
      handle(ownerHandle, 'scan-failure-recovery.raw.release')
      return rawControlComplete(
        await native.scanFailureRecoveryRawControl(ownerHandle, 'release'),
        'scan-failure-recovery.raw.release'
      )
    },
    rawFinalize: async (ownerHandle: string) => {
      handle(ownerHandle, 'scan-failure-recovery.raw.finalize')
      return rawControlFinalize(
        await native.scanFailureRecoveryRawControl(ownerHandle, 'finalize'),
        'scan-failure-recovery.raw.finalize'
      )
    }
  })
}
