import type { ScanFilter } from '../../backend-contract/advertisement'
import { canonicalUuid, type Uuid } from '../../backend-contract/primitives'
import {
  describeScanPredicates,
  snapshotScanExecutionPlan,
  snapshotScanPlan
} from '../../backend-contract/scan-planning'
import type {
  BackendScanExecutionPlan,
  ScanObservationField,
  ScanPlan,
  ScanPlanLimitation,
  ScanPredicateDescription,
  ScanPlanningContext
} from '../../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'

export function createServiceUuidScanPlan(
  query: NormalizedScanQuery,
  context: ScanPlanningContext,
  invalidContextMessage: string
): BackendScanExecutionPlan<ScanFilter> {
  assertPlanningContext(context, invalidContextMessage)
  const nativeFilter: ScanFilter = {
    serviceUuids: commonRequiredServices(query),
    manufacturerData: [],
    localNamePrefix: null
  }
  const predicates = describeScanPredicates(query)
  const nativePredicates = nativeServicePredicates(predicates, nativeFilter.serviceUuids)
  const unavailable = predicates.filter(
    predicate => !context.availableObservationFields.includes(observationField(predicate.field))
  )
  return snapshotScanExecutionPlan(
    {
      sourceQuery: query,
      queryDigest: query.digest,
      residualQueryDigest: query.digest,
      nativeGuarantee: 'safe-superset',
      native: { predicates: nativePredicates, complete: false },
      residual: { query, predicates, complete: true },
      unavailable,
      limitations: createLimitations(predicates, nativePredicates, unavailable),
      estimatedCost: nativeFilter.serviceUuids.length === 0 ? 'high' : 'moderate',
      nativeFilter
    },
    snapshotScanFilter
  )
}

export function diagnosticServiceUuidScanPlan(execution: BackendScanExecutionPlan<ScanFilter>): ScanPlan {
  return snapshotScanPlan({
    sourceQuery: execution.sourceQuery,
    queryDigest: execution.queryDigest,
    residualQueryDigest: execution.residualQueryDigest,
    nativeGuarantee: execution.nativeGuarantee,
    native: execution.native,
    residual: execution.residual,
    unavailable: execution.unavailable,
    limitations: execution.limitations,
    estimatedCost: execution.estimatedCost
  })
}

function commonRequiredServices(query: NormalizedScanQuery): readonly Uuid[] {
  if (query.anyOf === null || query.anyOf.length === 0) return []
  const requiredByEveryClause = query.anyOf.map(clause => clause.services?.all ?? [])
  if (requiredByEveryClause.some(services => services.length === 0)) return []
  const firstClause = requiredByEveryClause[0]
  if (firstClause === undefined) return []
  return firstClause
    .filter(service => requiredByEveryClause.every(services => services.includes(service)))
    .map(service => canonicalUuid(service))
}

function nativeServicePredicates(
  predicates: readonly ScanPredicateDescription[],
  serviceUuids: readonly Uuid[]
): readonly ScanPredicateDescription[] {
  if (serviceUuids.length === 0) return Object.freeze([])
  return Object.freeze(
    predicates.filter(
      predicate => predicate.clauseSet === 'anyOf' && predicate.field === 'services' && predicate.operator === 'all'
    )
  )
}

function createLimitations(
  predicates: readonly ScanPredicateDescription[],
  nativePredicates: readonly ScanPredicateDescription[],
  unavailable: readonly ScanPredicateDescription[]
): readonly ScanPlanLimitation[] {
  return predicates.slice(0, 32).map(predicate => {
    if (unavailable.includes(predicate)) {
      return {
        code: 'observation-field-unavailable',
        predicate,
        explanation: 'required observation field is unavailable on this host',
        effect: 'field-unavailable'
      }
    }
    if (nativePredicates.includes(predicate)) {
      return {
        code: 'native-filter-incomplete',
        predicate,
        explanation: 'predicate remains in the canonical residual matcher',
        effect: 'performance-only'
      }
    }
    return {
      code: 'host-predicate-restricted',
      predicate,
      explanation: 'host restriction prevents native evaluation',
      effect: 'host-restriction'
    }
  })
}

function observationField(field: ScanPredicateDescription['field']): ScanObservationField {
  if (field === 'peers') return 'peerReference'
  if (field === 'names') return 'localName'
  if (field === 'services') return 'serviceUuids'
  if (field === 'manufacturerData') return 'manufacturerData'
  if (field === 'serviceData') return 'serviceData'
  if (field === 'rssi') return 'rssi'
  return 'connectable'
}

function snapshotScanFilter(filter: ScanFilter): ScanFilter {
  return Object.freeze({
    serviceUuids: Object.freeze([...filter.serviceUuids]),
    manufacturerData: Object.freeze(
      filter.manufacturerData.map(manufacturer =>
        Object.freeze({
          companyIdentifier: manufacturer.companyIdentifier,
          dataPrefix: manufacturer.dataPrefix === null ? null : new Uint8Array(manufacturer.dataPrefix)
        })
      )
    ),
    localNamePrefix: filter.localNamePrefix
  })
}

function assertPlanningContext(context: ScanPlanningContext, invalidContextMessage: string): void {
  if (
    typeof context !== 'object' ||
    context === null ||
    typeof context.backendId !== 'string' ||
    typeof context.platformId !== 'string' ||
    context.backendId.length === 0 ||
    context.platformId.length === 0 ||
    !Array.isArray(context.availableObservationFields) ||
    context.availableObservationFields.some(field => !isScanObservationField(field))
  ) {
    throw new Error(invalidContextMessage)
  }
}

function isScanObservationField(value: ScanObservationField): value is ScanObservationField {
  return (
    value === 'peerReference' ||
    value === 'localName' ||
    value === 'rssi' ||
    value === 'connectable' ||
    value === 'serviceUuids' ||
    value === 'manufacturerData' ||
    value === 'serviceData'
  )
}
