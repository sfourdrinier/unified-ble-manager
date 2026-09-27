// Generated from src/backend-contract/continuation-setup.ts; do not edit.
function contractError(code: string, domain: string, detail: string): Error {
  return new Error(`${code}: ${domain}: ${detail}`)
}
import { normalizeContinuationSelector, type BackgroundContinuationResubscribeSelector } from './continuation-selector'

/** A correlated, single-packet application acknowledgement, not an ATT acknowledgement.
 * Nonmatching prefixes are unrelated records. A matching prefix with invalid length
 * or an unaccepted status fails the step. All packets remain in the collection path. */
export interface ContinuationSetupResponse {
  readonly subscriptionIndex: number
  readonly prefix: Readonly<Uint8Array>
  readonly minLength: number
  readonly maxLength: number
  readonly status: { readonly offset: number; readonly accepted: readonly number[] }
  /** Optional final byte: exactly minLength or minLength + 1 bytes are allowed.
   * If present this byte must be accepted; it cannot be ignored as padding. */
  readonly trailing?: { readonly offset: number; readonly accepted: readonly number[] }
}

/** Sequential write-with-response, once per fresh database generation. A timeout
 * includes both the ATT write and optional application acknowledgement. An uncertain
 * outcome must not be retried in the same generation. */
export interface ContinuationSetupStep {
  readonly selector: BackgroundContinuationResubscribeSelector
  readonly value: Readonly<Uint8Array>
  readonly timeoutMs: number
  readonly response?: ContinuationSetupResponse
}

/** JSON configuration/wire representation, never a public GATT payload. */
export interface ContinuationSetupWireStep {
  readonly selector: BackgroundContinuationResubscribeSelector
  readonly value: readonly number[]
  readonly timeoutMs: number
  readonly response?: Omit<ContinuationSetupResponse, 'prefix'> & { readonly prefix: readonly number[] }
}

/** Continuing after unsupported negotiation never means a minimum MTU was met. */
export interface ContinuationLinkConfiguration {
  readonly mtu: {
    readonly requested: number
    readonly timeoutMs: number
    readonly onUnsupported: 'continue' | 'fail'
  }
}

/** Opt-in plaintext journal in the host's private storage. No implicit retention
 * quota or caller-selected path; acknowledgement and deletion are explicit. */
export interface ContinuationRecordingConfiguration {
  readonly id: string
  readonly maxBytes: number
  readonly maxRecords: number
}

export function normalizeContinuationRecording(input: unknown): ContinuationRecordingConfiguration {
  const value = record(input, ['id', 'maxBytes', 'maxRecords'], 'recording')
  if (typeof value.id !== 'string' || !/^[A-Za-z0-9_-]{1,64}$/.test(value.id)) return invalid('recording.id')
  return Object.freeze({
    id: value.id,
    maxBytes: integer(value.maxBytes, 1048576, 1073741824, 'recording.maxBytes'),
    maxRecords: integer(value.maxRecords, 1, 1000000, 'recording.maxRecords')
  })
}

function invalid(field: string): never {
  throw contractError('argument.invalid', 'restoration', `background.continuation.setup.${field}`)
}

function record(value: unknown, keys: readonly string[], field: string): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return invalid(field)
  if (Object.keys(value).some(key => !keys.includes(key))) return invalid(`${field}.unknown-key`)
  return Object.fromEntries(Object.entries(value))
}

function integer(value: unknown, min: number, max: number, field: string) {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < min || value > max) return invalid(field)
  return value
}

function bytes(value: unknown, field: string) {
  if (!(value instanceof Uint8Array) || value.length === 0 || value.length > 512) return invalid(field)
  return Uint8Array.from(value)
}

function acceptedBytes(input: unknown, field: string) {
  if (!Array.isArray(input) || input.length === 0 || input.length > 256) return invalid(field)
  const accepted = input.map(item => integer(item, 0, 255, field))
  if (new Set(accepted).size !== accepted.length) return invalid(`${field}.duplicate`)
  return Object.freeze(accepted)
}

export function normalizeContinuationLink(input: unknown): ContinuationLinkConfiguration {
  const link = record(input, ['mtu'], 'link')
  const mtu = record(link.mtu, ['requested', 'timeoutMs', 'onUnsupported'], 'link.mtu')
  if (mtu.onUnsupported !== 'continue' && mtu.onUnsupported !== 'fail') return invalid('link.mtu.onUnsupported')
  return Object.freeze({
    mtu: Object.freeze({
      requested: integer(mtu.requested, 23, 517, 'link.mtu.requested'),
      timeoutMs: integer(mtu.timeoutMs, 1, 20000, 'link.mtu.timeoutMs'),
      onUnsupported: mtu.onUnsupported
    })
  })
}

function response(input: unknown, subscriptionCount: number): ContinuationSetupResponse {
  const value = record(
    input,
    ['subscriptionIndex', 'prefix', 'minLength', 'maxLength', 'status', 'trailing'],
    'response'
  )
  const prefix = bytes(value.prefix, 'response.prefix')
  const minLength = integer(value.minLength, prefix.length + 1, 512, 'response.minLength')
  const maxLength = integer(value.maxLength, minLength, 512, 'response.maxLength')
  const status = record(value.status, ['offset', 'accepted'], 'response.status')
  const offset = integer(status.offset, prefix.length, minLength - 1, 'response.status.offset')
  const accepted = acceptedBytes(status.accepted, 'response.status.accepted')
  const trailing =
    value.trailing === undefined ? undefined : record(value.trailing, ['offset', 'accepted'], 'response.trailing')
  if (trailing !== undefined && maxLength !== minLength + 1) return invalid('response.trailing.length')
  return Object.freeze({
    subscriptionIndex: integer(value.subscriptionIndex, 0, subscriptionCount - 1, 'response.subscriptionIndex'),
    prefix,
    minLength,
    maxLength,
    status: Object.freeze({ offset, accepted }),
    ...(trailing === undefined
      ? {}
      : {
          trailing: Object.freeze({
            offset: integer(trailing.offset, minLength, minLength, 'response.trailing.offset'),
            accepted: acceptedBytes(trailing.accepted, 'response.trailing.accepted')
          })
        })
  })
}

/** Internal contract preparation; declaration admission stays fail-closed until
 * the native executor supports the same recipe schema. */
export function normalizeContinuationSetup(
  input: unknown,
  subscriptionCount: number
): readonly ContinuationSetupStep[] {
  integer(subscriptionCount, 0, 64, 'subscriptionCount')
  if (input === undefined) return Object.freeze([])
  if (!Array.isArray(input) || input.length > 16) return invalid('steps')
  let totalTimeoutMs = 0
  const steps = input.map(entry => {
    const value = record(entry, ['selector', 'value', 'timeoutMs', 'response'], 'step')
    const timeoutMs = integer(value.timeoutMs, 1, 20000, 'timeoutMs')
    totalTimeoutMs += timeoutMs
    if (totalTimeoutMs > 60000) return invalid('totalTimeoutMs')
    return Object.freeze({
      selector: normalizeContinuationSelector(value.selector, 'background.continuation.setup.selector'),
      value: bytes(value.value, 'value'),
      timeoutMs,
      ...(value.response === undefined ? {} : { response: response(value.response, subscriptionCount) })
    })
  })
  return Object.freeze(steps)
}

/** Private JSON transport representation; public payloads remain byte arrays. */
export function serializeContinuationSetup(
  steps: readonly ContinuationSetupStep[]
): readonly ContinuationSetupWireStep[] {
  return steps.map(step => ({
    selector: step.selector,
    value: Array.from(step.value),
    timeoutMs: step.timeoutMs,
    ...(step.response === undefined
      ? {}
      : {
          response: { ...step.response, prefix: Array.from(step.response.prefix) }
        })
  }))
}

/** Strict JSON boundary: validate before constructing bytes to avoid Uint8Array
 * silently truncating floats or wrapping negative/out-of-range integers. */
export function deserializeContinuationSetup(
  input: unknown,
  subscriptionCount: number
): readonly ContinuationSetupStep[] {
  if (!Array.isArray(input) || input.length > 16) return invalid('steps')
  const decodeBytes = (value: unknown, field: string) => {
    if (!Array.isArray(value) || value.length === 0 || value.length > 512) return invalid(field)
    return Uint8Array.from(value.map(item => integer(item, 0, 255, field)))
  }
  return normalizeContinuationSetup(
    input.map(entry => {
      const step = record(entry, ['selector', 'value', 'timeoutMs', 'response'], 'step')
      const acknowledgement =
        step.response === undefined
          ? undefined
          : record(
              step.response,
              ['subscriptionIndex', 'prefix', 'minLength', 'maxLength', 'status', 'trailing'],
              'response'
            )
      return {
        ...step,
        value: decodeBytes(step.value, 'value'),
        ...(acknowledgement === undefined
          ? {}
          : {
              response: { ...acknowledgement, prefix: decodeBytes(acknowledgement.prefix, 'response.prefix') }
            })
      }
    }),
    subscriptionCount
  )
}
