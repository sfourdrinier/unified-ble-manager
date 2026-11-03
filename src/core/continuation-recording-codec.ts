// src/core/continuation-recording-codec.ts

import { contractError } from '../backend-contract/errors'
import { MAX_RECORDING_BATCH_BYTES, MAX_RECORDING_BATCH_ITEMS } from '../backend-contract/continuation-recording-bounds'
import { utf8ByteLength } from '../backend-contract/serializable'
import { encodeBase64 } from '../backends/reactnative/rust-core-wire'
import { rehydratePublicValue } from '../public/error-bridge'
import {
  parseContinuationRecordingRecords,
  type ContinuationRecordingBatch,
  type ContinuationRecordingPrepareOptions
} from './continuation-recording'

type RecordingRow = ContinuationRecordingBatch['records'][number]

function encodedBytes(bytes: Readonly<Uint8Array>): string {
  const result = encodeBase64(bytes)
  if (!result.ok) throw result.error
  return result.value
}

function nativeRecord(record: RecordingRow['record']) {
  if (record.t === 'value') {
    if (Object.hasOwn(record, 'valueB64'))
      throw contractError('argument.invalid', 'restoration', 'continuation.recording.rows')
    const { ordinal: _ordinal, value, ...rest } = record
    return { ...rest, valueB64: encodedBytes(value) }
  }
  if (record.t === 'adv') {
    if (
      Object.hasOwn(record, 'rawRecordB64') ||
      (record.manufacturerData !== null && !Array.isArray(record.manufacturerData)) ||
      (record.serviceData !== null && !Array.isArray(record.serviceData))
    )
      throw contractError('argument.invalid', 'restoration', 'continuation.recording.rows')
    const { ordinal: _ordinal, manufacturerData, serviceData, rawRecord, ...rest } = record
    return {
      ...rest,
      manufacturerData:
        manufacturerData === null
          ? null
          : manufacturerData.map(item => {
              if (Object.hasOwn(item, 'payloadB64'))
                throw contractError('argument.invalid', 'restoration', 'continuation.recording.rows')
              const { payload, ...itemFields } = item
              return { ...itemFields, payloadB64: encodedBytes(payload) }
            }),
      serviceData:
        serviceData === null
          ? null
          : serviceData.map(item => {
              if (Object.hasOwn(item, 'payloadB64'))
                throw contractError('argument.invalid', 'restoration', 'continuation.recording.rows')
              const { payload, ...itemFields } = item
              return { ...itemFields, payloadB64: encodedBytes(payload) }
            }),
      rawRecordB64: rawRecord === null ? null : encodedBytes(rawRecord)
    }
  }
  const { ordinal: _ordinal, ...rest } = record
  return rest
}

function checkLimits(limits: ContinuationRecordingPrepareOptions): void {
  if (
    !limits ||
    !Number.isSafeInteger(limits.maxItems) ||
    limits.maxItems < 1 ||
    limits.maxItems > MAX_RECORDING_BATCH_ITEMS ||
    !Number.isSafeInteger(limits.maxBytes) ||
    limits.maxBytes < 1 ||
    limits.maxBytes > MAX_RECORDING_BATCH_BYTES
  ) {
    throw contractError('argument.invalid', 'restoration', 'continuation.recording.bounds')
  }
}

/** Serialize decoded rows as native-shaped JSON for offline storage. This never creates a cursor or acknowledges a recording. */
export function serializeContinuationRecordingRecords(records: ContinuationRecordingBatch['records']): string {
  return rehydratePublicValue(() => {
    if (!Array.isArray(records) || records.length > MAX_RECORDING_BATCH_ITEMS) {
      throw contractError('argument.invalid', 'restoration', 'continuation.recording.rows')
    }
    const rows = records.map(entry => {
      if (
        !entry ||
        typeof entry !== 'object' ||
        Object.keys(entry).length !== 3 ||
        !Object.hasOwn(entry, 'ordinal') ||
        !Object.hasOwn(entry, 'metadata') ||
        !Object.hasOwn(entry, 'record') ||
        !entry.record ||
        typeof entry.record !== 'object' ||
        entry.record.ordinal !== entry.ordinal
      ) {
        throw contractError('argument.invalid', 'restoration', 'continuation.recording.rows')
      }
      return { ordinal: entry.ordinal, metadata: entry.metadata, record: nativeRecord(entry.record) }
    })
    parseContinuationRecordingRecords(rows, {
      maxItems: MAX_RECORDING_BATCH_ITEMS,
      maxBytes: MAX_RECORDING_BATCH_BYTES
    })
    return JSON.stringify(rows)
  })
}

/** Decode offline rows with the native recording validator. `bytes` counts this serialization, not the original native prefix. */
export function deserializeContinuationRecordingRecords(
  serialized: string,
  limits: ContinuationRecordingPrepareOptions
): Pick<ContinuationRecordingBatch, 'records' | 'bytes'> {
  return rehydratePublicValue(() => {
    checkLimits(limits)
    // Native row byte accounting excludes the array brackets and separators.
    const maximumTextBytes = limits.maxBytes + limits.maxItems + 1
    if (
      typeof serialized !== 'string' ||
      serialized.length > maximumTextBytes ||
      utf8ByteLength(serialized) > maximumTextBytes
    ) {
      throw contractError('argument.invalid', 'restoration', 'continuation.recording.serialized-size')
    }
    let input: unknown
    try {
      input = JSON.parse(serialized)
    } catch {
      throw contractError('protocol.malformed', 'restoration', 'continuation.recording.serialized')
    }
    return parseContinuationRecordingRecords(input, limits)
  })
}
