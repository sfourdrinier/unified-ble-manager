// src/ipc/ordered-wire.ts

import { contractError, orderedFailureWireFields } from '../backend-contract/errors'
import type { SerializableRecord, SerializableValue } from '../backend-contract/primitives'

/** Copy a data-only ordered record to the bounded IPC representation. */
export function orderedWireRecord(value: object): SerializableRecord {
  const result = orderedWireValue(value)
  if (!isRecord(result)) throw contractError('protocol.malformed', 'ipc', 'ordered-wire.record')
  return result
}

function isRecord(value: SerializableValue): value is SerializableRecord {
  return value !== null && typeof value === 'object' && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function orderedWireValue(value: unknown): SerializableValue {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (value instanceof Uint8Array) return Array.from(value)
  if (Array.isArray(value)) return value.map(orderedWireValue)
  if (typeof value === 'object' && value !== null) {
    const result: Record<string, SerializableValue> = Object.create(null)
    for (const [key, entry] of Object.entries(value)) {
      if (key === '__proto__' || key === 'prototype' || key === 'constructor')
        throw contractError('protocol.malformed', 'ipc', 'ordered-wire.key')
      if (key === 'orderedFailure' && entry !== undefined) {
        if (Object.hasOwn(value, 'orderedFailureJson'))
          throw contractError('protocol.malformed', 'ipc', 'ordered-wire.failure')
        Object.assign(result, orderedFailureWireFields(entry))
      } else if (entry !== undefined) result[key] = orderedWireValue(entry)
    }
    return result
  }
  throw contractError('protocol.malformed', 'ipc', 'ordered-wire.value')
}
