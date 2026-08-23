import { contractError } from '../backend-contract/errors'
import type { SerializableRecord } from '../backend-contract/primitives'
import { snapshotScanPlan } from '../backend-contract/scan-planning'
import type { ScanPlan } from '../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../backend-contract/scan-query'
import { decodeIpcScanQuery, encodeIpcScanQuery } from '../ipc/scan-planning'

export const encodeTauriScanQuery = encodeIpcScanQuery

export function decodeTauriScanPlan(value: SerializableRecord, expectedQuery: NormalizedScanQuery): ScanPlan {
  if ('nativeFilter' in value) {
    throw new Error('nativeFilter must not cross the Tauri application boundary')
  }
  if (!isScanPlanRecord(value)) {
    throw contractError('protocol.malformed', 'ipc', 'tauri.scan-plan')
  }
  const sourceQuery = decodeIpcScanQuery(value.sourceQuery, 'tauri.scan-plan.source-query')
  const residual = requiredRecord(value.residual, 'tauri.scan-plan.residual')
  const residualQuery = decodeIpcScanQuery(residual.query, 'tauri.scan-plan.residual-query')
  if (sourceQuery.digest !== expectedQuery.digest || residualQuery.digest !== expectedQuery.digest) {
    throw contractError('protocol.violation', 'ipc', 'tauri.scan-plan-query')
  }
  const candidate: ScanPlan = Object.freeze({
    queryDigest: value.queryDigest,
    residualQueryDigest: value.residualQueryDigest,
    nativeGuarantee: value.nativeGuarantee,
    native: value.native,
    unavailable: value.unavailable,
    limitations: value.limitations,
    estimatedCost: value.estimatedCost,
    sourceQuery,
    residual: Object.freeze({
      predicates: value.residual.predicates,
      complete: value.residual.complete,
      query: residualQuery
    })
  })
  try {
    return snapshotScanPlan(candidate)
  } catch {
    throw contractError('protocol.malformed', 'ipc', 'tauri.scan-plan')
  }
}

function isScanPlanRecord(value: SerializableRecord): value is SerializableRecord & ScanPlan {
  return (
    typeof value.queryDigest === 'string' &&
    typeof value.residualQueryDigest === 'string' &&
    (value.nativeGuarantee === 'exact' || value.nativeGuarantee === 'safe-superset') &&
    isRecord(value.native) &&
    isRecord(value.residual) &&
    Array.isArray(value.unavailable) &&
    Array.isArray(value.limitations) &&
    (value.estimatedCost === 'native-only' ||
      value.estimatedCost === 'low' ||
      value.estimatedCost === 'moderate' ||
      value.estimatedCost === 'high')
  )
}

function isRecord(value: unknown): value is SerializableRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}

function requiredRecord(value: unknown, operation: string): SerializableRecord {
  if (!isRecord(value)) throw contractError('protocol.malformed', 'ipc', operation)
  return value
}
