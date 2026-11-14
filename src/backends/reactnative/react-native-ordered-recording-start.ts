// src/backends/reactnative/react-native-ordered-recording-start.ts
import { contractError } from '../../backend-contract/errors'
import {
  isRecordingId,
  MAX_RECORDING_BYTES,
  MAX_RECORDING_RECORDS,
  MIN_RECORDING_BYTES,
  MIN_RECORDING_RECORDS
} from '../../backend-contract/continuation-recording-bounds'
import { parseNativeControlEnvelope, failureEnvelopeError } from '../../core/recording-wire'
import { parseOrderedRecordingContext } from '../../core/ordered-recording'
import { parseOrderedRecordingStartResult } from '../../core/ordered-recording-start'
import type { OrderedRecordingStartResult } from '../../core/ordered-recording-start'
import type { ReactNativeRustCoreSession } from './react-native-rust-core'
import { nextReactNativeReservationKey } from './react-native-reservation-key'

function decode(text: string): unknown {
  const parsed = parseNativeControlEnvelope(text, 'ordered-recording')
  if (!parsed.ok) throw parsed.error
  if (parsed.value.kind === 'failure') throw failureEnvelopeError(parsed.value)
  return parsed.value.value
}
function status(value: unknown): {
  state: 'reserved' | 'pending' | 'ready' | 'failed' | 'retired' | 'revoked'
  receipt: unknown
} {
  if (typeof value !== 'object' || value === null || Array.isArray(value))
    throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.status')
  const fields = Object.entries(value)
  if (
    fields.length !== 3 ||
    !Object.hasOwn(value, 'version') ||
    !Object.hasOwn(value, 'state') ||
    !Object.hasOwn(value, 'receipt')
  )
    throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.status')
  const version = Reflect.get(value, 'version')
  const state = Reflect.get(value, 'state')
  const receipt: unknown = Reflect.get(value, 'receipt')
  if (
    version !== 1 ||
    (state !== 'reserved' &&
      state !== 'pending' &&
      state !== 'ready' &&
      state !== 'failed' &&
      state !== 'retired' &&
      state !== 'revoked') ||
    (state === 'ready' ? receipt === null : receipt !== null)
  )
    throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.status')
  return { state, receipt }
}

function parseMobileStartResult(
  input: unknown,
  id: string,
  memberCount: number,
  retain: (collectionId: string, capability: string) => void
): OrderedRecordingStartResult {
  if (typeof input !== 'object' || input === null || Array.isArray(input))
    throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.start-result')
  if (Reflect.get(input, 'kind') !== 'started') return parseOrderedRecordingStartResult(input, id, memberCount)
  const capability: unknown = Reflect.get(input, 'reclaimCapability')
  if (
    Object.keys(input).length !== 3 ||
    !Object.hasOwn(input, 'receipt') ||
    typeof capability !== 'string' ||
    capability.length === 0 ||
    capability.length > 512
  )
    throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.start-result')
  const result = parseOrderedRecordingStartResult(
    { kind: 'started', receipt: Reflect.get(input, 'receipt') },
    id,
    memberCount
  )
  if (result.kind !== 'started')
    throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.start-result')
  retain(result.receipt.collectionId, capability)
  return result
}
/** Session-scoped, effect-free reservation handle. No live-owner lookup uses an ID alone. */
export function createReactNativeOrderedRecordingStart(
  session: ReactNativeRustCoreSession,
  retain: (collectionId: string, capability: string) => void = () => undefined,
  admit: (id: string) => void = () => undefined,
  onRevoked: () => void = () => undefined
) {
  let callerKey: string | null = null
  let recordingId: string | null = null
  const reservation = (id: string) => {
    if (callerKey === null)
      throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.ordered-recording.identity')
    return JSON.stringify([id, callerKey])
  }
  let identity: string | null = null
  let memberCount = 0
  let nativeHandle: string | null = null
  let reserveAttempted = false
  let reserveInFlight: Promise<void> | null = null
  let retired = false
  let retireRequested = false
  let retireInFlight: Promise<void> | null = null
  let dispatched = false
  let result: OrderedRecordingStartResult | null = null
  const check = async () => {
    if (
      session.orderedRecordingVersion === undefined ||
      session.orderedRecordingReserve === undefined ||
      session.orderedRecordingStart === undefined ||
      session.orderedRecordingStartStatus === undefined ||
      session.orderedRecordingRetire === undefined ||
      session.orderedRecordingContext === undefined ||
      session.orderedRecordingStop === undefined
    )
      throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.version')
    if ((await session.orderedRecordingVersion()) !== 1)
      throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.version')
  }
  return {
    async start(input: {
      readonly id: string
      readonly maxBytes: number
      readonly maxRecords: number
      readonly declaration: unknown
      readonly budgetMs: number
      readonly refreshBudget: () => number
      readonly signal: AbortSignal | null
    }): Promise<OrderedRecordingStartResult> {
      if (retired || retireRequested)
        throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.ordered-recording.retired')
      if (
        !isRecordingId(input.id) ||
        !Number.isSafeInteger(input.maxBytes) ||
        input.maxBytes < MIN_RECORDING_BYTES ||
        input.maxBytes > MAX_RECORDING_BYTES ||
        !Number.isSafeInteger(input.maxRecords) ||
        input.maxRecords < MIN_RECORDING_RECORDS ||
        input.maxRecords > MAX_RECORDING_RECORDS ||
        !Number.isSafeInteger(input.budgetMs) ||
        input.budgetMs < 1
      )
        throw contractError('argument.invalid', 'restoration', 'react-native.ordered-recording.start')
      if (input.signal?.aborted)
        throw contractError('operation.aborted', 'gatt', 'react-native.ordered-recording.start')
      const declaration = parseOrderedRecordingContext(input.declaration)
      const serialized = JSON.stringify(declaration)
      const expected = JSON.stringify([input.id, input.maxBytes, input.maxRecords, declaration])
      if (identity !== null && identity !== expected)
        throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.ordered-recording.identity')
      await check()
      if (retired || retireRequested)
        throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.ordered-recording.retired')
      if (input.signal?.aborted)
        throw contractError('operation.aborted', 'gatt', 'react-native.ordered-recording.start')
      // Negotiation can overlap another start on this handle. Recheck identity
      // after the await, before either attempt claims the public owner.
      if (identity !== null && identity !== expected)
        throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.ordered-recording.identity')
      admit(input.id)
      identity = expected
      recordingId = input.id
      memberCount = declaration.members.length
      callerKey ??= nextReactNativeReservationKey()
      if (nativeHandle === null) {
        const reserve = session.orderedRecordingReserve
        if (reserve === undefined)
          throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.reserve')
        if (reserveInFlight === null) {
          reserveAttempted = true
          const flight = (async () => {
            const reply = decode(await reserve.call(session, reservation(input.id)))
            if (
              typeof reply !== 'object' ||
              reply === null ||
              Reflect.get(reply, 'version') !== 2 ||
              Reflect.get(reply, 'state') !== 'reserved' ||
              typeof Reflect.get(reply, 'handle') !== 'string' ||
              Reflect.get(reply, 'handle').length === 0 ||
              Object.keys(reply).length !== 3
            )
              throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.reserve')
            nativeHandle = Reflect.get(reply, 'handle')
          })()
          reserveInFlight = flight
          flight.then(
            () => {
              if (reserveInFlight === flight) reserveInFlight = null
            },
            () => {
              if (reserveInFlight === flight) reserveInFlight = null
            }
          )
        }
        await reserveInFlight
      }
      if (retired || retireRequested)
        throw contractError('lifecycle.invalid-state', 'restoration', 'react-native.ordered-recording.retired')
      if (input.signal?.aborted)
        throw contractError('operation.aborted', 'gatt', 'react-native.ordered-recording.start')
      const budgetMs = input.refreshBudget()
      const start = session.orderedRecordingStart
      if (start === undefined)
        throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.start')
      if (nativeHandle === null)
        throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.reserve')
      dispatched = true
      const pending = start
        .call(session, nativeHandle, input.id, String(input.maxBytes), String(input.maxRecords), serialized, budgetMs)
        .then(text => {
          const parsed = parseMobileStartResult(decode(text), input.id, memberCount, retain)
          result = parsed
          return parsed
        })
      if (input.signal === null) return pending
      const listener: { remove?: () => void } = {}
      const cancelled = new Promise<never>((_, reject) => {
        const onAbort = () => reject(contractError('operation.aborted', 'gatt', 'react-native.ordered-recording.start'))
        input.signal?.addEventListener('abort', onAbort, { once: true })
        listener.remove = () => input.signal?.removeEventListener('abort', onAbort)
        if (input.signal?.aborted) onAbort()
      })
      try {
        return await Promise.race([pending, cancelled])
      } finally {
        listener.remove?.()
      }
    },
    async status(): Promise<
      | { readonly state: 'pending' | 'unknown' | 'failed' | 'retired' }
      | { readonly state: 'result'; readonly result: OrderedRecordingStartResult }
    > {
      if (retired) return result === null ? { state: 'retired' } : { state: 'result', result }
      if (recordingId === null || !reserveAttempted) return { state: 'unknown' }
      if (reserveInFlight !== null) return { state: 'pending' }
      if (nativeHandle === null) return { state: 'unknown' }
      const inspect = session.orderedRecordingStartStatus
      if (inspect === undefined)
        throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.status')
      const reply = status(decode(await inspect.call(session, nativeHandle)))
      if (reply.state === 'ready') {
        result = parseMobileStartResult(reply.receipt, recordingId, memberCount, retain)
        return { state: 'result', result }
      }
      if (reply.state === 'revoked') {
        onRevoked()
        throw contractError('ownership.denied', 'restoration', 'react-native.ordered-recording.revoked')
      }
      if (reply.state === 'failed' || reply.state === 'retired') return { state: reply.state }
      return { state: reply.state === 'pending' || (reply.state === 'reserved' && dispatched) ? 'pending' : 'unknown' }
    },
    async retire(): Promise<void> {
      if (retired) return
      if (retireInFlight !== null) return retireInFlight
      const flight = (async () => {
        retireRequested = true
        if (recordingId === null || !reserveAttempted) {
          retired = true
          return
        }
        if (reserveInFlight !== null) await reserveInFlight
        if (nativeHandle === null) {
          const reserve = session.orderedRecordingReserve
          if (reserve === undefined)
            throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.reserve')
          const reply = decode(await reserve.call(session, reservation(recordingId)))
          if (
            typeof reply !== 'object' ||
            reply === null ||
            Reflect.get(reply, 'version') !== 2 ||
            Reflect.get(reply, 'state') !== 'reserved' ||
            typeof Reflect.get(reply, 'handle') !== 'string' ||
            Reflect.get(reply, 'handle').length === 0 ||
            Object.keys(reply).length !== 3
          )
            throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.reserve')
          nativeHandle = Reflect.get(reply, 'handle')
        }
        // A lost start reply may be the only copy of the private handoff. Do
        // not delete the native receipt until it has been reconciled locally.
        if (dispatched) {
          const inspect = session.orderedRecordingStartStatus
          if (inspect === undefined)
            throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.status')
          if (nativeHandle === null)
            throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.reserve')
          const current = status(decode(await inspect.call(session, nativeHandle)))
          if (current.state === 'revoked') onRevoked()
          else if (current.state === 'ready' && result === null)
            result = parseMobileStartResult(current.receipt, recordingId, memberCount, retain)
          else if (result === null && (current.state === 'pending' || current.state === 'reserved')) {
            retireRequested = false
            throw contractError('lifecycle.invalid-state', 'cleanup', 'react-native.ordered-recording.receipt-pending')
          }
        }
        const retire = session.orderedRecordingRetire
        if (retire === undefined)
          throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.retire')
        if (nativeHandle === null)
          throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.reserve')
        const reply = decode(await retire.call(session, nativeHandle))
        if (
          typeof reply !== 'object' ||
          reply === null ||
          Reflect.get(reply, 'version') !== 1 ||
          Reflect.get(reply, 'state') !== 'retired' ||
          Object.keys(reply).length !== 2
        )
          throw contractError('protocol.malformed', 'boundary', 'react-native.ordered-recording.retire')
        retired = true
      })()
      retireInFlight = flight
      try {
        await flight
      } finally {
        retireInFlight = null
      }
    }
  }
}
