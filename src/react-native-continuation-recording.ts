// src/react-native-continuation-recording.ts
import { Platform } from 'react-native'
import { rehydratePublicError } from './public/error-bridge'
import { contractError } from './backend-contract/errors'
import { decodeNativeContinuationEnvelope } from './core/native-continuation-envelope'
import { decodeBase64OrThrow } from './core/recording-wire'
import { MAX_RECORDING_BATCH_BYTES, MAX_RECORDING_BATCH_ITEMS } from './backend-contract/continuation-recording-bounds'
import { createReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core-binding'
import { terminalEvidenceScopeFromNative } from './backends/reactnative/terminal-evidence-scope-utf16'
import type { ReactNativeRustCoreBinding } from './backends/reactnative/react-native-rust-core'
import {
  createNativeContinuationRecordingController,
  type ContinuationRecordingController,
  type ContinuationRecordingStatus
} from './core/continuation-recording'

export interface ReactNativeContinuationRecordingController extends ContinuationRecordingController {
  /** Recheck recordings after native cleanup. The hint is not a persistence receipt. */
  onRecoveryHint(listener: () => void): () => void
  /** Explicit trusted private-store status; native refuses a live owner. */
  statusOrderedOffline(id: string): Promise<{
    readonly version: 1
    readonly state: 'not-started' | 'sealed' | 'interrupted'
    readonly journal: ContinuationRecordingStatus
  }>
  /** Native-selected retained terminal evidence. Reading never consumes it; ACK only follows durable consumption. */
  readonly terminalEvidenceRecovery: ReactNativeTerminalEvidenceRecovery
}

export interface ReactNativeTerminalEvidenceRecoveryItem {
  /** Opaque native handle, valid only for this recovery lease. */
  readonly handle: string
  readonly producer: string
  readonly scope: Readonly<{ readonly managerId: string; readonly operationId: string; readonly incarnation: string }>
  readonly state: 'staged' | 'committed'
  /** The exact retained reference, when native has committed one. It is descriptive, never an authority token. */
  readonly reference?: string
  readonly byteLength?: number
  readonly digest?: string
  readonly formatVersion?: number
}

export interface ReactNativeTerminalEvidenceRecoveryPage {
  readonly items: readonly ReactNativeTerminalEvidenceRecoveryItem[]
  readonly more: boolean
}

/** Additive contention-aware page. `busyProducers` reports native-owned
 * records deliberately withheld from this recovery lease; it never denotes
 * an absent record. `more` is only the bounded-page continuation signal, so
 * a busy-only page may have `more: false`. */
export interface ReactNativeTerminalEvidenceRecoveryStatusPage extends ReactNativeTerminalEvidenceRecoveryPage {
  readonly busyProducers: readonly ('scan' | 'subscription')[]
}

export interface ReactNativeTerminalEvidenceRecoveryChunk {
  readonly offset: number
  readonly nextOffset: number
  readonly bytes: Uint8Array
  readonly done: boolean
}

export interface ReactNativeTerminalEvidenceRecovery {
  list(options: {
    readonly maxItems: number
    readonly maxBytes: number
  }): Promise<ReactNativeTerminalEvidenceRecoveryPage>
  listWithStatus(options: {
    readonly maxItems: number
    readonly maxBytes: number
  }): Promise<ReactNativeTerminalEvidenceRecoveryStatusPage>
  read(
    handle: string,
    options: { readonly offset: number; readonly maxBytes: number }
  ): Promise<ReactNativeTerminalEvidenceRecoveryChunk>
  acknowledge(handle: string): Promise<void>
}

/** Opens no BLE session and requests no Bluetooth permission. Native chooses
 * protected app-private storage; callers supply only recording IDs, not paths.
 * The production binding verifies the sealed binary identity before each call. */
export function createReactNativeContinuationRecordings(
  options: { readonly rustCore?: ReactNativeRustCoreBinding } = {}
): ReactNativeContinuationRecordingController {
  let binding: ReactNativeRustCoreBinding
  try {
    binding = options.rustCore ?? defaultBinding()
  } catch (error) {
    throw rehydratePublicError(error)
  }
  const missing = () =>
    Promise.reject(contractError('capability.unsupported', 'restoration', 'continuation.recording.native-controls'))
  const controller = createNativeContinuationRecordingController(
    {
      status: id =>
        typeof binding.continuationRecordingStatus === 'function' ? binding.continuationRecordingStatus(id) : missing(),
      prepare: (id, maxItems, maxBytes) =>
        typeof binding.continuationRecordingPrepare === 'function'
          ? binding.continuationRecordingPrepare(id, maxItems, maxBytes)
          : missing(),
      acknowledge: (id, token) =>
        typeof binding.continuationRecordingAcknowledge === 'function'
          ? binding.continuationRecordingAcknowledge(id, token)
          : missing(),
      stop: id =>
        typeof binding.continuationRecordingStop === 'function' ? binding.continuationRecordingStop(id) : missing(),
      ...(typeof binding.orderedRecordingOfflineContext === 'function'
        ? {
            orderedContext: (id: string) =>
              binding.orderedRecordingOfflineContext ? binding.orderedRecordingOfflineContext(id) : missing()
          }
        : {}),
      ...(typeof binding.orderedRecordingOfflineStop === 'function'
        ? {
            // The offline native control returns the v2 host-stop receipt directly.
            orderedReceiptVersion: async () => 2,
            stopOrdered: (id: string) =>
              binding.orderedRecordingOfflineStop ? binding.orderedRecordingOfflineStop(id) : missing()
          }
        : {}),
      clear: id =>
        typeof binding.continuationRecordingClear === 'function' ? binding.continuationRecordingClear(id) : missing()
    },
    'ubm-mobile'
  )
  const terminalEvidenceRecovery = createTerminalEvidenceRecovery(binding, missing)
  return Object.freeze({
    ...controller,
    terminalEvidenceRecovery,
    async statusOrderedOffline(id: string) {
      try {
        const text = binding.orderedRecordingOfflineStatus
          ? await binding.orderedRecordingOfflineStatus(id)
          : await missing()
        const value = decodeNativeContinuationEnvelope(text, 'ubm-mobile', 'status-ordered-offline')
        if (
          typeof value !== 'object' ||
          value === null ||
          Array.isArray(value) ||
          Object.keys(value).length !== 3 ||
          Reflect.get(value, 'version') !== 1 ||
          !Object.hasOwn(value, 'journal')
        )
          throw contractError('protocol.malformed', 'restoration', 'continuation.recording.status-ordered-offline')
        const state = Reflect.get(value, 'state')
        if (state !== 'not-started' && state !== 'sealed' && state !== 'interrupted')
          throw contractError('protocol.malformed', 'restoration', 'continuation.recording.status-ordered-offline')
        const journalControl = createNativeContinuationRecordingController(
          {
            status: async () => JSON.stringify({ ok: true, value: Reflect.get(value, 'journal') }),
            prepare: missing,
            acknowledge: missing,
            stop: missing,
            clear: missing
          },
          'ubm-mobile'
        )
        const journal = await journalControl.status(id)
        if (state === 'not-started' ? journal.orderedOwner !== undefined : journal.orderedOwner?.state !== state)
          throw contractError('protocol.malformed', 'restoration', 'continuation.recording.status-ordered-offline')
        return Object.freeze({ version: 1, state, journal })
      } catch (error) {
        throw rehydratePublicError(error)
      }
    },
    onRecoveryHint(listener: () => void): () => void {
      try {
        if (binding.onRecoveryHint === undefined) {
          throw contractError('capability.unsupported', 'restoration', 'continuation.recording.recovery-hint')
        }
        return binding.onRecoveryHint(listener)
      } catch (error) {
        throw rehydratePublicError(error)
      }
    }
  })
}

function createTerminalEvidenceRecovery(
  binding: ReactNativeRustCoreBinding,
  missing: () => Promise<never>
): ReactNativeTerminalEvidenceRecovery {
  let lease: Promise<string> | undefined
  const operation = 'terminal-evidence-recovery'
  const malformed = (suffix: string): never => {
    throw contractError('protocol.malformed', 'restoration', `${operation}.${suffix}`)
  }
  const bounded = (value: number, maximum: number, suffix: string): void => {
    if (!Number.isSafeInteger(value) || value < 1 || value > maximum)
      throw contractError('argument.invalid', 'restoration', `${operation}.${suffix}`)
  }
  const opaque = (value: string, suffix: string): void => {
    if (value.length === 0 || value.length > 512)
      throw contractError('argument.invalid', 'restoration', `${operation}.${suffix}`)
  }
  const record = (value: unknown, suffix: string): Record<string, unknown> => {
    if (!isRecord(value)) return malformed(suffix)
    return value
  }
  const exact = (value: Record<string, unknown>, keys: readonly string[], suffix: string): void => {
    if (Object.keys(value).some(key => !keys.includes(key))) malformed(suffix)
  }
  const text = (value: unknown, suffix: string): string => {
    if (typeof value !== 'string' || value.length === 0 || value.length > 512) return malformed(suffix)
    return value
  }
  const decimal = (value: unknown, suffix: string): number => {
    if (typeof value !== 'string' || !/^(?:0|[1-9][0-9]*)$/.test(value)) return malformed(suffix)
    const result = Number(value)
    if (!Number.isSafeInteger(result)) return malformed(suffix)
    return result
  }
  const open = (): Promise<string> => {
    if (lease === undefined) {
      lease = (async () => {
        if (typeof binding.terminalEvidenceRecoveryOpenSelected !== 'function') return missing()
        const value = decodeNativeContinuationEnvelope(
          await binding.terminalEvidenceRecoveryOpenSelected(),
          'ubm-mobile',
          `${operation}.open`
        )
        const response = record(value, 'open')
        exact(response, ['leaseId'], 'open')
        return text(response.leaseId, 'open.lease-id')
      })()
      lease.catch(() => {
        lease = undefined
      })
    }
    return lease
  }
  const item = (value: unknown): ReactNativeTerminalEvidenceRecoveryItem => {
    const source = record(value, 'list.item')
    exact(
      source,
      ['handle', 'producer', 'scope', 'state', 'reference', 'byteLength', 'digest', 'formatVersion'],
      'list.item'
    )
    const scope = record(source.scope, 'list.item.scope')
    const hasRawScope = Object.hasOwn(scope, 'scopeUtf16leBase64')
    exact(
      scope,
      hasRawScope
        ? ['managerId', 'operationId', 'incarnation', 'scopeUtf16leBase64']
        : ['managerId', 'operationId', 'incarnation'],
      'list.item.scope'
    )
    const state = source.state
    if (state !== 'staged' && state !== 'committed') return malformed('list.item.state')
    const reference = source.reference === undefined ? undefined : text(source.reference, 'list.item.reference')
    const byteLength = source.byteLength === undefined ? undefined : decimal(source.byteLength, 'list.item.byte-length')
    const digest = source.digest === undefined ? undefined : text(source.digest, 'list.item.digest')
    const formatVersion = source.formatVersion
    if (digest !== undefined && !/^[0-9a-f]{64}$/.test(digest)) return malformed('list.item.digest')
    if (
      formatVersion !== undefined &&
      (typeof formatVersion !== 'number' || !Number.isSafeInteger(formatVersion) || formatVersion < 1)
    )
      return malformed('list.item.format-version')
    if (
      state === 'committed' &&
      (reference === undefined || byteLength === undefined || digest === undefined || formatVersion === undefined)
    )
      return malformed('list.item.committed')
    const producer = text(source.producer, 'list.item.producer')
    // Historical mobile recovery rows predate the producer vocabulary and
    // retain a descriptive owner such as `mobile-owner`. Their scalar scope
    // remains recoverable. A raw UTF-16 sidecar, however, cryptographically
    // binds its producer and therefore accepts only the current vocabulary.
    const visibleScope = Object.freeze({
      managerId: text(scope.managerId, 'list.item.scope.manager-id'),
      operationId: text(scope.operationId, 'list.item.scope.operation-id'),
      incarnation: text(scope.incarnation, 'list.item.scope.incarnation'),
      ...(hasRawScope ? { scopeUtf16leBase64: text(scope.scopeUtf16leBase64, 'list.item.scope.scope-utf16le') } : {})
    })
    const recoveredScope = (() => {
      if (hasRawScope) {
        if (producer !== 'scan' && producer !== 'subscription') return malformed('list.item.producer')
        return terminalEvidenceScopeFromNative(producer, visibleScope, `${operation}.list.item`)
      }
      return Object.freeze({
        managerId: visibleScope.managerId,
        operationId: visibleScope.operationId,
        incarnation: visibleScope.incarnation
      })
    })()
    return Object.freeze({
      handle: text(source.handle, 'list.item.handle'),
      producer,
      scope: recoveredScope,
      state,
      ...(reference === undefined ? {} : { reference }),
      ...(byteLength === undefined ? {} : { byteLength }),
      ...(digest === undefined ? {} : { digest }),
      ...(formatVersion === undefined ? {} : { formatVersion })
    })
  }
  const recovery: ReactNativeTerminalEvidenceRecovery = {
    async list(options: { readonly maxItems: number; readonly maxBytes: number }) {
      bounded(options.maxItems, MAX_RECORDING_BATCH_ITEMS, 'list.bounds.items')
      bounded(options.maxBytes, MAX_RECORDING_BATCH_BYTES, 'list.bounds.bytes')
      try {
        if (typeof binding.terminalEvidenceRecoveryList !== 'function') return await missing()
        const value = decodeNativeContinuationEnvelope(
          await binding.terminalEvidenceRecoveryList(await open(), options.maxItems, options.maxBytes),
          'ubm-mobile',
          `${operation}.list`
        )
        const response = record(value, 'list')
        exact(response, ['items', 'more'], 'list')
        if (
          !Array.isArray(response.items) ||
          response.items.length > options.maxItems ||
          typeof response.more !== 'boolean'
        )
          return malformed('list')
        return Object.freeze({ items: Object.freeze(response.items.map(item)), more: response.more })
      } catch (error) {
        throw rehydratePublicError(error)
      }
    },
    async listWithStatus(options: { readonly maxItems: number; readonly maxBytes: number }) {
      bounded(options.maxItems, MAX_RECORDING_BATCH_ITEMS, 'list-status.bounds.items')
      bounded(options.maxBytes, MAX_RECORDING_BATCH_BYTES, 'list-status.bounds.bytes')
      try {
        if (typeof binding.terminalEvidenceRecoveryListWithStatus !== 'function') return await missing()
        const value = decodeNativeContinuationEnvelope(
          await binding.terminalEvidenceRecoveryListWithStatus(await open(), options.maxItems, options.maxBytes),
          'ubm-mobile',
          `${operation}.list-status`
        )
        const response = record(value, 'list-status')
        exact(response, ['items', 'more', 'busyProducers'], 'list-status')
        if (
          !Array.isArray(response.items) ||
          response.items.length > options.maxItems ||
          typeof response.more !== 'boolean' ||
          !Array.isArray(response.busyProducers)
        )
          return malformed('list-status')
        const busyProducers = response.busyProducers.map(producer => {
          if (producer !== 'scan' && producer !== 'subscription') return malformed('list-status.busy-producer')
          return producer
        })
        if (new Set(busyProducers).size !== busyProducers.length) return malformed('list-status.busy-producer')
        return Object.freeze({
          items: Object.freeze(response.items.map(item)),
          more: response.more,
          busyProducers: Object.freeze(busyProducers)
        })
      } catch (error) {
        throw rehydratePublicError(error)
      }
    },
    async read(handle: string, options: { readonly offset: number; readonly maxBytes: number }) {
      opaque(handle, 'read.handle')
      if (!Number.isSafeInteger(options.offset) || options.offset < 0)
        throw rehydratePublicError(contractError('argument.invalid', 'restoration', `${operation}.read.offset`))
      bounded(options.maxBytes, MAX_RECORDING_BATCH_BYTES, 'read.bounds.bytes')
      try {
        if (typeof binding.terminalEvidenceRecoveryRead !== 'function') return await missing()
        const value = decodeNativeContinuationEnvelope(
          await binding.terminalEvidenceRecoveryRead(handle, String(options.offset), options.maxBytes),
          'ubm-mobile',
          `${operation}.read`
        )
        const response = record(value, 'read')
        exact(response, ['bytesBase64', 'more'], 'read')
        if (typeof response.more !== 'boolean') return malformed('read.more')
        const bytes = decodeBase64OrThrow(response.bytesBase64, `${operation}.read.bytes`)
        if (bytes.byteLength > options.maxBytes || (response.more && bytes.byteLength === 0))
          return malformed('read.bytes')
        const nextOffset = options.offset + bytes.byteLength
        if (!Number.isSafeInteger(nextOffset)) return malformed('read.offset')
        return Object.freeze({ offset: options.offset, nextOffset, bytes: new Uint8Array(bytes), done: !response.more })
      } catch (error) {
        throw rehydratePublicError(error)
      }
    },
    async acknowledge(handle: string) {
      opaque(handle, 'acknowledge.handle')
      try {
        if (typeof binding.terminalEvidenceRecoveryAcknowledge !== 'function') return await missing()
        const value = decodeNativeContinuationEnvelope(
          await binding.terminalEvidenceRecoveryAcknowledge(handle),
          'ubm-mobile',
          `${operation}.acknowledge`
        )
        const response = record(value, 'acknowledge')
        exact(response, [], 'acknowledge')
      } catch (error) {
        throw rehydratePublicError(error)
      }
    }
  }
  return Object.freeze(recovery)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function defaultBinding(): ReactNativeRustCoreBinding {
  if (Platform.OS !== 'android' && Platform.OS !== 'ios') {
    throw contractError('capability.unsupported', 'platform', 'continuation.recording.mobile-host')
  }
  return createReactNativeRustCoreBinding({ platform: Platform.OS === 'android' ? 'android' : 'apple' })
}
