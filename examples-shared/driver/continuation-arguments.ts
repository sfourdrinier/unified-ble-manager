import type { HostContinuationDeclaration } from './host.ts'
import type { JsonObject } from './protocol.ts'
import { buildH10Continuation } from './polar-continuation.ts'
import { H10_ACC_SAMPLE_RATES_HZ, H10_ACC_RANGES_G } from './polar-pmd.ts'
import { args, ScenarioError } from './scenario-core.ts'

export function recipe(raw: JsonObject): HostContinuationDeclaration | null {
  const measurements = raw.measurements
  if (measurements === undefined) return null
  if (typeof measurements !== 'string' || !['hr', 'hr-ecg', 'hr-acc', 'hr-ecg-acc'].includes(measurements)) {
    throw new ScenarioError('scenario.invalid-continuation', 'Unknown H10 measurements')
  }
  const accEnabled = measurements === 'hr-acc' || measurements === 'hr-ecg-acc'
  const sampleRateHz = H10_ACC_SAMPLE_RATES_HZ.find(value => value === (raw.sampleRateHz ?? 50))
  const rangeG = H10_ACC_RANGES_G.find(value => value === (raw.rangeG ?? 4))
  if (sampleRateHz === undefined || rangeG === undefined) throw new ScenarioError('scenario.invalid-continuation', 'Invalid H10 ACC rate or range')
  const peerId = args.optionalString(raw, 'peerId')
  return buildH10Continuation({ ...(peerId === null ? {} : { peerId }),
    ecg: measurements === 'hr-ecg' || measurements === 'hr-ecg-acc',
    ...(accEnabled ? { acc: { sampleRateHz, rangeG, resolutionBits: 16 } } : {}) })
}

export function recording(raw: JsonObject) {
  if (raw.recordingId === undefined) {
    if (raw.maxBytes !== undefined || raw.maxRecords !== undefined) throw new ScenarioError('scenario.invalid-continuation', 'Recording quotas require recordingId')
    return undefined
  }
  const id = raw.recordingId
  const maxBytes = raw.maxBytes
  const maxRecords = raw.maxRecords
  if (typeof id !== 'string' || !/^[A-Za-z0-9_-]{1,64}$/.test(id) ||
    typeof maxBytes !== 'number' || !Number.isSafeInteger(maxBytes) || maxBytes < 1048576 || maxBytes > 1073741824 ||
    typeof maxRecords !== 'number' || !Number.isSafeInteger(maxRecords) || maxRecords < 1 || maxRecords > 1000000) {
    throw new ScenarioError('scenario.invalid-continuation', 'Recording needs an explicit safe id, maxBytes and maxRecords')
  }
  return { id, maxBytes, maxRecords }
}

export function requiredText(raw: JsonObject, key: string): string {
  const value = args.optionalString(raw, key)
  if (value === null || value.length === 0) throw new ScenarioError('scenario.invalid-continuation', `${key} is required`)
  return value
}
