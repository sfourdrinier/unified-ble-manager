import type { ScanFilter } from '../backend-contract/advertisement'
import type {
  BackendScanExecutionPlan,
  ScanObservationField,
  ScanPlanLimitation,
  ScanPlan,
  ScanPredicateDescription,
  ScanPlanningContext
} from '../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../backend-contract/scan-query'
import { createServiceUuidScanPlan } from '../backends/scan-planning/service-uuid-scan-planner'
import { describeScanPredicates, snapshotScanPlan } from '../backend-contract/scan-planning'

const webBluetoothChooserObservationFields: readonly ScanObservationField[] = Object.freeze([
  'localName',
  'serviceUuids',
  'manufacturerData'
])

export const webBluetoothScanPlanningContext: ScanPlanningContext = Object.freeze({
  backendId: 'web-bluetooth',
  platformId: 'web-bluetooth-chooser',
  availableObservationFields: webBluetoothChooserObservationFields
})

export function planWebBluetoothScan(query: NormalizedScanQuery): BackendScanExecutionPlan<ScanFilter> {
  return createServiceUuidScanPlan(
    query,
    webBluetoothScanPlanningContext,
    'invalid Web Bluetooth scan planning context'
  )
}

/** A chooser diagnostic only; Web Bluetooth continuous scanning remains unsupported. */
export function diagnosticWebBluetoothScanPlan(query: NormalizedScanQuery): ScanPlan {
  const execution = planWebBluetoothScan(query)
  const chooserUnavailable = describeScanPredicates(query).filter(predicate =>
    isUnavailableToWebChooser(query, predicate)
  )
  const unavailable = uniquePredicates([...execution.unavailable, ...chooserUnavailable])
  const chooserUnavailableKeys = new Set(chooserUnavailable.map(predicateKey))
  const limitations = [
    ...execution.limitations.filter(limitation => !chooserUnavailableKeys.has(predicateKey(limitation.predicate))),
    ...chooserUnavailable.map(createUnavailableLimitation)
  ]
  return snapshotScanPlan({
    sourceQuery: execution.sourceQuery,
    queryDigest: execution.queryDigest,
    residualQueryDigest: execution.residualQueryDigest,
    nativeGuarantee: execution.nativeGuarantee,
    native: execution.native,
    residual: execution.residual,
    unavailable,
    limitations,
    estimatedCost: execution.estimatedCost
  })
}

function isUnavailableToWebChooser(query: NormalizedScanQuery, predicate: ScanPredicateDescription): boolean {
  if (predicate.field === 'names' && predicate.operator === 'exact') return true
  if (predicate.field !== 'manufacturerData') return false
  const clauses = predicate.clauseSet === 'anyOf' ? query.anyOf : query.exclude
  const clause = clauses?.[predicate.clauseIndex]
  if (clause?.manufacturerData === null || clause?.manufacturerData === undefined) return false
  const patterns = predicate.operator === 'any' ? clause.manufacturerData.any : clause.manufacturerData.all
  return patterns.some(pattern => pattern.mask !== undefined)
}

function uniquePredicates(predicates: readonly ScanPredicateDescription[]): readonly ScanPredicateDescription[] {
  return [...new Map(predicates.map(predicate => [predicateKey(predicate), predicate])).values()]
}

function predicateKey(predicate: ScanPredicateDescription): string {
  return JSON.stringify(predicate)
}

function createUnavailableLimitation(predicate: ScanPredicateDescription): ScanPlanLimitation {
  return {
    code: 'observation-field-unavailable',
    predicate,
    explanation: 'required observation field is unavailable on this host',
    effect: 'field-unavailable'
  }
}
