import { contractError } from '../../backend-contract/errors'
import {
  createNativeContinuationRecordingController,
  type ContinuationRecordingController,
  type ContinuationRecordingPrepareOptions
} from '../../core/continuation-recording'
import { rehydratePublicPromise } from '../../public/error-bridge'
import type { DesktopRustCoreBinding } from './desktop-rust-core-binding'
import { nativeContinuationTransportError } from '../../core/native-continuation-envelope'
import { assertDesktopOrderedReceiptVersion } from './ordered-recording-version'
import { failureEnvelopeError, invokeEnvelopeOrThrow } from '../../core/recording-wire'
import { isRecordingId } from '../../backend-contract/continuation-recording-bounds'
import { utf8ByteLength } from '../../backend-contract/serializable'

/** The raw SQLite row body is retained exactly, including number lexemes. */
export interface DesktopLegacyRecoveryPage {
  readonly version: 1
  readonly token: string
  readonly cursorLast: string
  readonly more: boolean
  readonly bytes: number
  readonly rawRecords: readonly { readonly ordinal: string; readonly body: readonly number[] }[]
}
export interface DesktopContinuationRecordingController extends ContinuationRecordingController {
  /** Trusted-host only exact diagnostic recovery. These calls never infer or
   * acknowledge bytes; retain the returned bytes before any lifecycle step. */
  diagnostics: DesktopContinuationDiagnosticController
  /** Read a bounded chunk without changing any cursor; quiesce writers across calls. */
  inspectRawRecord(
    id: string,
    source: 'records' | 'evidenceClaim',
    ordinal: string | null,
    offset: string,
    maxBytes: number
  ): Promise<DesktopRawRecordChunk>
  /** Replay the current pre-v5 cursor; its earlier preparation had no saved identity. */
  prepareLegacy(id: string, options: ContinuationRecordingPrepareOptions): Promise<DesktopLegacyRecoveryPage>
  /** Restore an exact page retained by a prior process before retrying any ACK. */
  recoverLegacy(id: string): Promise<DesktopLegacyRecoveryPage | null>
  /** Call after durably storing the exact page. Retrying it returns the durable receipt. */
  acknowledgeLegacy(
    id: string,
    page: DesktopLegacyRecoveryPage
  ): Promise<{ readonly version: 1; readonly token: string; readonly acknowledged: true; readonly records: number }>
  /** Retire only after the ACK receipt is durably handled; migration is fenced until then. */
  retireLegacyReceipt(
    id: string,
    page: DesktopLegacyRecoveryPage
  ): Promise<{ readonly version: 1; readonly token: string; readonly retired: true }>
  /** Release the replay slot only after retirement has been durably observed. */
  confirmLegacyRetirement(
    id: string,
    page: DesktopLegacyRecoveryPage
  ): Promise<{ readonly version: 1; readonly token: string; readonly retired: true }>
}

export interface DesktopDiagnosticPage {
  readonly bytes: readonly number[]
  readonly totalBytes: string
  readonly durable: boolean
  readonly reclaimed: boolean
}

export interface DesktopContinuationDiagnosticController {
  summaryPage(
    id: string,
    offset: string,
    maxBytes: number
  ): Promise<{ readonly bytes: readonly number[]; readonly totalBytes: string }>
  occurrencesPage(id: string, after: string | null, limit: number): Promise<readonly string[]>
  occurrencePage(id: string, occurrence: string, offset: string, maxBytes: number): Promise<DesktopDiagnosticPage>
  retry(id: string, occurrence: string): Promise<void>
  acknowledge(id: string, occurrence: string, exactPayload: string): Promise<void>
  retire(id: string, occurrence: string, exactPayload: string): Promise<void>
  confirmRetirement(id: string, occurrence: string, exactPayload: string): Promise<void>
  releaseConfirmation(id: string, occurrence: string, exactPayload: string): Promise<void>
  /** Bounded exact-byte transition. Persist and resume at the returned offset. */
  transitionChunk(
    id: string,
    occurrence: string,
    transition: 'acknowledge' | 'retire' | 'confirm' | 'release',
    offset: string,
    bytes: readonly number[]
  ): Promise<{ readonly offset: string; readonly totalBytes: string; readonly complete: boolean }>
}

export interface DesktopRawRecordChunk {
  readonly source: 'records' | 'evidenceClaim'
  readonly schemaVersion: number
  readonly ordinal: string
  readonly nextOrdinal: string | null
  readonly offset: string
  readonly length: string
  readonly storedLength: string | null
  readonly bytes: readonly number[]
  readonly complete: boolean
  readonly lengthValid: boolean
  readonly lengthFault: object | null
  readonly jsonValidated: boolean
  readonly jsonValid: boolean | null
  readonly countersValid: null
  readonly counterFault: null
}

function malformed(): never {
  throw contractError('protocol.malformed', 'restoration', 'continuation.recording.legacy-exact')
}
const diagnosticId = (value: unknown): string => {
  if (typeof value !== 'string' || !/^[0-9a-f]{80}$/.test(value)) return malformed()
  return value
}
const diagnosticOffset = (value: unknown): string => {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,18})$/.test(value) || BigInt(value) > 9223372036854775807n)
    return malformed()
  return value
}
const diagnosticBytes = (value: unknown, maxBytes: number): readonly number[] => {
  if (
    !Array.isArray(value) ||
    value.length > maxBytes ||
    value.some(byte => typeof byte !== 'number' || !Number.isInteger(byte) || byte < 0 || byte > 255)
  )
    return malformed()
  return Object.freeze(value.map(byte => byte))
}
function object(value: unknown): object {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return malformed()
  return value
}
function ordinal(value: unknown): string {
  if (typeof value !== 'string' || !/^[1-9][0-9]{0,18}$/.test(value) || BigInt(value) > 9223372036854775807n)
    return malformed()
  return value
}
function recoveryPage(value: unknown): DesktopLegacyRecoveryPage {
  const v = object(value)
  if (Object.keys(v).sort().join(',') !== 'bytes,cursorLast,more,rawRecords,token,version') return malformed()
  const token: unknown = Reflect.get(v, 'token')
  const cursorLast = ordinal(Reflect.get(v, 'cursorLast'))
  const more: unknown = Reflect.get(v, 'more')
  const bytes: unknown = Reflect.get(v, 'bytes')
  const rows: unknown = Reflect.get(v, 'rawRecords')
  if (
    Reflect.get(v, 'version') !== 1 ||
    typeof token !== 'string' ||
    !token ||
    token.length > 96 ||
    typeof more !== 'boolean' ||
    !Number.isSafeInteger(bytes) ||
    typeof bytes !== 'number' ||
    bytes < 0 ||
    bytes > 4194304 ||
    !Array.isArray(rows) ||
    rows.length < 1 ||
    rows.length > 2048
  )
    return malformed()
  let total = 0
  let previous = 0n
  const rawRecords = rows.map((input: unknown) => {
    const row = object(input)
    if (Object.keys(row).sort().join(',') !== 'body,ordinal') return malformed()
    const current = ordinal(Reflect.get(row, 'ordinal'))
    if (BigInt(current) <= previous || BigInt(current) > BigInt(cursorLast)) return malformed()
    previous = BigInt(current)
    const body: unknown = Reflect.get(row, 'body')
    if (!Array.isArray(body)) return malformed()
    total += body.length
    if (
      total > 4194304 ||
      body.some((byte: unknown) => !Number.isInteger(byte) || typeof byte !== 'number' || byte < 0 || byte > 255)
    )
      return malformed()
    return Object.freeze({ ordinal: current, body: Object.freeze(body.map((byte: number) => byte)) })
  })
  if (total !== bytes) return malformed()
  return Object.freeze({ version: 1, token, cursorLast, more, bytes, rawRecords: Object.freeze(rawRecords) })
}

/** Trusted Node/Electron main only. The application selects its private data
 * directory; never forward arbitrary renderer paths. This verifies/opens the
 * store through the loaded binding without creating or enumerating a radio. */
export function openNativeContinuationRecordings(
  binding: DesktopRustCoreBinding,
  directory: string
): Promise<DesktopContinuationRecordingController> {
  return rehydratePublicPromise(
    (async () => {
      if (typeof directory !== 'string' || directory.length === 0) {
        throw contractError('argument.invalid', 'restoration', 'continuation.recording.directory')
      }
      if (typeof binding.openRecordingStore !== 'function') {
        throw contractError('capability.unsupported', 'restoration', 'continuation.recording.open')
      }
      const open = binding.openRecordingStore
      const store = await Promise.resolve()
        .then(() => open.call(binding, directory))
        .catch(error => {
          throw nativeContinuationTransportError(error, 'ubm-desktop', 'continuation.recording.open')
        })
      const controller = createNativeContinuationRecordingController(store, 'ubm-desktop', async () => {
        assertDesktopOrderedReceiptVersion(binding, 'continuation.recording.stop-ordered')
        return 2
      })
      const call = async (operation: string, invoke: () => Promise<string>): Promise<unknown> => {
        try {
          const envelope = invokeEnvelopeOrThrow(await invoke(), operation, false, 18 * 1024 * 1024)
          if (envelope.kind === 'failure') throw failureEnvelopeError(envelope)
          return envelope.value
        } catch (error) {
          throw nativeContinuationTransportError(error, 'ubm-desktop', operation)
        }
      }
      const required = () => {
        const confirm: unknown = Reflect.get(store, 'legacyRecordingConfirmExactRetirement')
        if (
          typeof store.legacyRecordingPrepareExact !== 'function' ||
          typeof store.legacyRecordingAcknowledgeExact !== 'function' ||
          typeof store.legacyRecordingRetireExact !== 'function' ||
          typeof confirm !== 'function'
        )
          throw contractError('capability.unsupported', 'restoration', 'continuation.recording.legacy-exact')
        return {
          prepare: store.legacyRecordingPrepareExact,
          acknowledge: store.legacyRecordingAcknowledgeExact,
          retire: store.legacyRecordingRetireExact,
          confirm
        }
      }
      const recoverLegacy = async (id: string): Promise<DesktopLegacyRecoveryPage | null> => {
        if (!isRecordingId(id)) return malformed()
        const method: unknown = Reflect.get(store, 'legacyRecordingRecoverExact')
        if (typeof method !== 'function')
          throw contractError('capability.unsupported', 'restoration', 'continuation.recording.legacy-exact-recover')
        const value = await call('continuation.recording.recover-legacy', () => Reflect.apply(method, store, [id]))
        return value === null ? null : recoveryPage(value)
      }
      const diagnostics = (): DesktopContinuationDiagnosticController => {
        const methods = {
          summary: Reflect.get(store, 'diagnosticSummaryPage'),
          occurrences: Reflect.get(store, 'diagnosticOccurrencesPage'),
          occurrence: Reflect.get(store, 'diagnosticOccurrencePage'),
          retry: Reflect.get(store, 'retryDiagnosticOccurrence'),
          acknowledge: Reflect.get(store, 'acknowledgeDiagnosticOccurrence'),
          retire: Reflect.get(store, 'retireDiagnosticOccurrence'),
          confirm: Reflect.get(store, 'confirmDiagnosticRetirement'),
          release: Reflect.get(store, 'releaseDiagnosticConfirmation'),
          chunk: Reflect.get(store, 'transitionDiagnosticOccurrenceChunk')
        }
        if (Object.values(methods).some(method => typeof method !== 'function'))
          throw contractError('capability.unsupported', 'restoration', 'continuation.recording.diagnostic')
        const invokeDiagnostic = (operation: string, method: unknown, args: readonly unknown[]) => {
          if (typeof method !== 'function') return Promise.reject(malformed())
          return call(operation, () => Reflect.apply(method, store, args))
        }
        const summaryPage = async (id: string, offset: string, maxBytes: number) => {
          const value = object(
            await invokeDiagnostic('continuation.recording.diagnostic-summary', methods.summary, [id, offset, maxBytes])
          )
          if (Object.keys(value).sort().join(',') !== 'bytes,totalBytes') return malformed()
          const bytes = diagnosticBytes(Reflect.get(value, 'bytes'), maxBytes)
          const totalBytes = diagnosticOffset(Reflect.get(value, 'totalBytes'))
          if (BigInt(bytes.length) > BigInt(totalBytes)) return malformed()
          return Object.freeze({ bytes, totalBytes })
        }
        const occurrencePage = async (id: string, occurrence: string, offset: string, maxBytes: number) => {
          const value = object(
            await invokeDiagnostic('continuation.recording.diagnostic-occurrence', methods.occurrence, [
              id,
              occurrence,
              offset,
              maxBytes
            ])
          )
          if (Object.keys(value).sort().join(',') !== 'bytes,durable,reclaimed,totalBytes') return malformed()
          const bytes = diagnosticBytes(Reflect.get(value, 'bytes'), maxBytes)
          const totalBytes = diagnosticOffset(Reflect.get(value, 'totalBytes'))
          if (BigInt(bytes.length) > BigInt(totalBytes)) return malformed()
          if (
            typeof Reflect.get(value, 'durable') !== 'boolean' ||
            typeof Reflect.get(value, 'reclaimed') !== 'boolean'
          )
            return malformed()
          return Object.freeze({
            bytes,
            totalBytes,
            durable: Reflect.get(value, 'durable') === true,
            reclaimed: Reflect.get(value, 'reclaimed') === true
          })
        }
        const transition = async (
          operation: string,
          method: unknown,
          id: string,
          occurrence: string,
          exactPayload: string,
          expected: string
        ) => {
          if (
            !isRecordingId(id) ||
            diagnosticId(occurrence) !== occurrence ||
            typeof exactPayload !== 'string' ||
            utf8ByteLength(exactPayload) > 18 * 1024 * 1024
          )
            return malformed()
          const value = object(await invokeDiagnostic(operation, method, [id, occurrence, exactPayload]))
          if (
            Object.keys(value).sort().join(',') !== 'occurrence,transition' ||
            diagnosticId(Reflect.get(value, 'occurrence')) !== occurrence ||
            Reflect.get(value, 'transition') !== expected
          )
            return malformed()
        }
        return Object.freeze({
          summaryPage: (id: string, offset: string, maxBytes: number) => {
            if (
              !isRecordingId(id) ||
              diagnosticOffset(offset) !== offset ||
              !Number.isInteger(maxBytes) ||
              maxBytes < 1 ||
              maxBytes > 65536
            )
              return Promise.reject(malformed())
            return summaryPage(id, offset, maxBytes)
          },
          occurrencesPage: async (id: string, after: string | null, limit: number) => {
            if (
              !isRecordingId(id) ||
              (after !== null && diagnosticId(after) !== after) ||
              !Number.isInteger(limit) ||
              limit < 1 ||
              limit > 128
            )
              return malformed()
            const value = object(
              await invokeDiagnostic('continuation.recording.diagnostic-occurrences', methods.occurrences, [
                id,
                after,
                limit
              ])
            )
            const occurrences = Reflect.get(value, 'occurrences')
            if (
              Object.keys(value).join(',') !== 'occurrences' ||
              !Array.isArray(occurrences) ||
              occurrences.length > limit
            )
              return malformed()
            const ids = occurrences.map(diagnosticId)
            if (ids.some((entry, index) => index > 0 && entry <= ids[index - 1]!)) return malformed()
            return Object.freeze(ids)
          },
          occurrencePage: (id: string, occurrence: string, offset: string, maxBytes: number) => {
            if (
              !isRecordingId(id) ||
              diagnosticId(occurrence) !== occurrence ||
              diagnosticOffset(offset) !== offset ||
              !Number.isInteger(maxBytes) ||
              maxBytes < 1 ||
              maxBytes > 65536
            )
              return Promise.reject(malformed())
            return occurrencePage(id, occurrence, offset, maxBytes)
          },
          retry: async (id: string, occurrence: string) => {
            if (!isRecordingId(id) || diagnosticId(occurrence) !== occurrence) return malformed()
            const value = object(
              await invokeDiagnostic('continuation.recording.diagnostic-retry', methods.retry, [id, occurrence])
            )
            if (
              Object.keys(value).sort().join(',') !== 'occurrence,retried' ||
              diagnosticId(Reflect.get(value, 'occurrence')) !== occurrence ||
              Reflect.get(value, 'retried') !== true
            )
              return malformed()
          },
          acknowledge: (id: string, occurrence: string, exactPayload: string) =>
            transition(
              'continuation.recording.diagnostic-acknowledge',
              methods.acknowledge,
              id,
              occurrence,
              exactPayload,
              'acknowledge'
            ),
          retire: (id: string, occurrence: string, exactPayload: string) =>
            transition(
              'continuation.recording.diagnostic-retire',
              methods.retire,
              id,
              occurrence,
              exactPayload,
              'retire'
            ),
          confirmRetirement: (id: string, occurrence: string, exactPayload: string) =>
            transition(
              'continuation.recording.diagnostic-confirm',
              methods.confirm,
              id,
              occurrence,
              exactPayload,
              'confirm'
            ),
          releaseConfirmation: (id: string, occurrence: string, exactPayload: string) =>
            transition(
              'continuation.recording.diagnostic-release',
              methods.release,
              id,
              occurrence,
              exactPayload,
              'release'
            ),
          transitionChunk: async (
            id: string,
            occurrence: string,
            transitionName: 'acknowledge' | 'retire' | 'confirm' | 'release',
            offset: string,
            bytes: readonly number[]
          ) => {
            if (
              !isRecordingId(id) ||
              diagnosticId(occurrence) !== occurrence ||
              diagnosticOffset(offset) !== offset ||
              !Array.isArray(bytes) ||
              bytes.length < 1 ||
              bytes.length > 65536 ||
              bytes.some(byte => !Number.isInteger(byte) || byte < 0 || byte > 255)
            )
              return malformed()
            const value = object(
              await invokeDiagnostic('continuation.recording.diagnostic-transition-chunk', methods.chunk, [
                id,
                occurrence,
                transitionName,
                offset,
                bytes
              ])
            )
            if (
              Object.keys(value).sort().join(',') !== 'complete,occurrence,offset,totalBytes,transition' ||
              diagnosticId(Reflect.get(value, 'occurrence')) !== occurrence ||
              Reflect.get(value, 'transition') !== transitionName ||
              typeof Reflect.get(value, 'complete') !== 'boolean'
            )
              return malformed()
            const next = diagnosticOffset(Reflect.get(value, 'offset'))
            const total = diagnosticOffset(Reflect.get(value, 'totalBytes'))
            if (BigInt(next) > BigInt(total) || BigInt(next) <= BigInt(offset)) return malformed()
            return Object.freeze({ offset: next, totalBytes: total, complete: Reflect.get(value, 'complete') === true })
          }
        })
      }
      return Object.freeze({
        ...controller,
        get diagnostics() {
          return diagnostics()
        },
        inspectRawRecord: (
          id: string,
          source: 'records' | 'evidenceClaim',
          selected: string | null,
          offset: string,
          maxBytes: number
        ) =>
          rehydratePublicPromise(
            (async () => {
              if (
                typeof id !== 'string' ||
                !/^[a-zA-Z0-9_-]{1,64}$/.test(id) ||
                (source !== 'records' && source !== 'evidenceClaim') ||
                (selected !== null &&
                  (typeof selected !== 'string' ||
                    !/^[1-9][0-9]{0,18}$/.test(selected) ||
                    BigInt(selected) > 9223372036854775807n)) ||
                typeof offset !== 'string' ||
                !/^(0|[1-9][0-9]{0,18})$/.test(offset) ||
                BigInt(offset) > 9223372036854775807n ||
                !Number.isInteger(maxBytes) ||
                maxBytes < 1 ||
                maxBytes > 4194304
              )
                throw contractError('argument.invalid', 'restoration', 'continuation.recording.inspect-raw')
              const method: unknown = Reflect.get(store, 'inspectRawRecord')
              if (typeof method !== 'function')
                throw contractError('capability.unsupported', 'restoration', 'continuation.recording.inspect-raw')
              const value = object(
                await call('continuation.recording.inspect-raw', () =>
                  Reflect.apply(method, store, [id, source, selected, offset, maxBytes])
                )
              )
              const get = (key: string): unknown => Reflect.get(value, key)
              if (
                Object.keys(value).sort().join(',') !==
                'bytes,complete,counterFault,countersValid,jsonValid,jsonValidated,length,lengthFault,lengthValid,nextOrdinal,offset,ordinal,schemaVersion,source,storedLength'
              )
                return malformed()
              const ord = ordinal(get('ordinal'))
              const next = get('nextOrdinal') === null ? null : ordinal(get('nextOrdinal'))
              const length = get('length'),
                stored = get('storedLength'),
                bytes = get('bytes'),
                lengthFault = get('lengthFault'),
                jsonValid = get('jsonValid')
              const decimal = (v: unknown): v is string =>
                typeof v === 'string' && /^(0|[1-9][0-9]{0,19})$/.test(v) && BigInt(v) <= 18446744073709551615n
              if (
                get('source') !== source ||
                !Number.isInteger(get('schemaVersion')) ||
                Number(get('schemaVersion')) < 1 ||
                Number(get('schemaVersion')) > 5 ||
                get('offset') !== offset ||
                !decimal(length) ||
                (stored !== null &&
                  (typeof stored !== 'string' ||
                    !/^-?(0|[1-9][0-9]{0,18})$/.test(stored) ||
                    BigInt(stored) < -9223372036854775808n ||
                    BigInt(stored) > 9223372036854775807n)) ||
                !Array.isArray(bytes) ||
                bytes.length > maxBytes ||
                bytes.some((b: unknown) => typeof b !== 'number' || !Number.isInteger(b) || b < 0 || b > 255) ||
                typeof get('complete') !== 'boolean' ||
                typeof get('lengthValid') !== 'boolean' ||
                (lengthFault !== null && (typeof lengthFault !== 'object' || Array.isArray(lengthFault))) ||
                typeof get('jsonValidated') !== 'boolean' ||
                (jsonValid !== null && typeof jsonValid !== 'boolean') ||
                get('countersValid') !== null ||
                get('counterFault') !== null ||
                (selected !== null && ord !== selected) ||
                BigInt(offset) + BigInt(bytes.length) > BigInt(length) ||
                get('complete') !== (BigInt(offset) + BigInt(bytes.length) === BigInt(length)) ||
                get('jsonValidated') !== (offset === '0' && get('complete') === true) ||
                (get('jsonValidated') === true) !== (typeof jsonValid === 'boolean') ||
                get('lengthValid') !== (stored !== null && stored === length) ||
                (lengthFault === null) !== (get('lengthValid') === true) ||
                (next !== null && BigInt(next) <= BigInt(ord))
              )
                return malformed()
              return Object.freeze({
                source,
                schemaVersion: Number(get('schemaVersion')),
                ordinal: ord,
                nextOrdinal: next,
                offset,
                length,
                storedLength: stored,
                bytes: Object.freeze(bytes.map((b: number) => b)),
                complete: get('complete') === true,
                lengthValid: get('lengthValid') === true,
                lengthFault,
                jsonValidated: get('jsonValidated') === true,
                jsonValid,
                countersValid: null,
                counterFault: null
              })
            })()
          ),
        prepareLegacy: (id: string, options: ContinuationRecordingPrepareOptions) =>
          rehydratePublicPromise(
            (async () => {
              const methods = required()
              if (
                typeof id !== 'string' ||
                !/^[a-zA-Z0-9_-]{1,64}$/.test(id) ||
                !options ||
                !Number.isInteger(options.maxItems) ||
                options.maxItems < 1 ||
                options.maxItems > 2048 ||
                !Number.isInteger(options.maxBytes) ||
                options.maxBytes < 1 ||
                options.maxBytes > 4194304
              )
                throw contractError('argument.invalid', 'restoration', 'continuation.recording.prepare-legacy')
              const page = recoveryPage(
                await call('continuation.recording.prepare-legacy', () =>
                  methods.prepare.call(store, id, options.maxItems, options.maxBytes)
                )
              )
              if (page.rawRecords.length > options.maxItems || page.bytes > options.maxBytes) return malformed()
              return page
            })()
          ),
        recoverLegacy: (id: string) => rehydratePublicPromise(recoverLegacy(id)),
        acknowledgeLegacy: (id: string, input: DesktopLegacyRecoveryPage) =>
          rehydratePublicPromise(
            (async () => {
              const methods = required()
              const page = recoveryPage(input)
              const value = object(
                await call('continuation.recording.acknowledge-legacy', () =>
                  methods.acknowledge.call(store, id, JSON.stringify(page))
                )
              )
              if (
                Object.keys(value).sort().join(',') !== 'acknowledged,records,token,version' ||
                Reflect.get(value, 'version') !== 1 ||
                Reflect.get(value, 'acknowledged') !== true ||
                Reflect.get(value, 'token') !== page.token ||
                Reflect.get(value, 'records') !== page.rawRecords.length
              )
                return malformed()
              return Object.freeze({
                version: 1,
                token: page.token,
                acknowledged: true,
                records: page.rawRecords.length
              })
            })()
          ),
        retireLegacyReceipt: (id: string, input: DesktopLegacyRecoveryPage) =>
          rehydratePublicPromise(
            (async () => {
              const methods = required()
              const page = recoveryPage(input)
              const value = object(
                await call('continuation.recording.retire-legacy', () =>
                  methods.retire.call(store, id, JSON.stringify(page))
                )
              )
              if (
                Object.keys(value).sort().join(',') !== 'retired,token,version' ||
                Reflect.get(value, 'version') !== 1 ||
                Reflect.get(value, 'retired') !== true ||
                Reflect.get(value, 'token') !== page.token
              )
                return malformed()
              return Object.freeze({ version: 1, token: page.token, retired: true })
            })()
          ),
        confirmLegacyRetirement: (id: string, input: DesktopLegacyRecoveryPage) =>
          rehydratePublicPromise(
            (async () => {
              const methods = required()
              const page = recoveryPage(input)
              const value = object(
                await call('continuation.recording.confirm-legacy-retirement', () =>
                  Reflect.apply(methods.confirm, store, [id, JSON.stringify(page)])
                )
              )
              if (
                Object.keys(value).sort().join(',') !== 'retired,token,version' ||
                Reflect.get(value, 'version') !== 1 ||
                Reflect.get(value, 'retired') !== true ||
                Reflect.get(value, 'token') !== page.token
              )
                return malformed()
              return Object.freeze({ version: 1, token: page.token, retired: true })
            })()
          ),
        stopOrdered: (id: string) =>
          rehydratePublicPromise(
            Promise.resolve().then(() => {
              assertDesktopOrderedReceiptVersion(binding, 'continuation.recording.stop-ordered')
              const stopOrdered = controller.stopOrdered
              if (stopOrdered === undefined)
                throw contractError('capability.unsupported', 'restoration', 'continuation.recording.stop-ordered')
              return stopOrdered.call(controller, id)
            })
          )
      })
    })()
  )
}
