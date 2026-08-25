import type { OwnerScanOptions, ScanFilter } from '../../backend-contract/advertisement'
import { contractError } from '../../backend-contract/errors'
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
import { snapshotNormalizedScanQuery } from '../../backend-contract/scan-query'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'

export function trustedServiceUuidFilter(
  options: OwnerScanOptions<string, string>,
  planScan: (query: NormalizedScanQuery) => BackendScanExecutionPlan<ScanFilter>,
  operation: string
): ScanFilter {
  if (options.plan === undefined) return options.filter
  const snapshot = snapshotScanPlan(options.plan)
  if (options.query === undefined) throw contractError('protocol.violation', 'scan', `${operation}.plan-query`)
  const query = snapshotNormalizedScanQuery(options.query)
  if (query.digest !== snapshot.queryDigest) {
    throw contractError('protocol.violation', 'scan', `${operation}.plan-query`)
  }
  return planScan(query).nativeFilter
}

export function createServiceUuidScanPlan(
  query: NormalizedScanQuery,
  context: ScanPlanningContext,
  invalidContextMessage: string
): BackendScanExecutionPlan<ScanFilter> {
  assertPlanningContext(context, invalidContextMessage)
  const deviceAddresses = nativeAddressList(query)
  const nativeFilter: ScanFilter = {
    serviceUuids: commonRequiredServices(query),
    manufacturerData: [],
    localNamePrefix: null,
    ...(deviceAddresses.length === 0 ? {} : { deviceAddresses })
  }
  const predicates = describeScanPredicates(query)
  const nativePredicates = [
    ...nativeServicePredicates(query, predicates, nativeFilter.serviceUuids),
    ...nativeAddressPredicates(query, predicates, deviceAddresses)
  ]
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

function nativeAddressList(query: NormalizedScanQuery): readonly string[] {
  if (query.anyOf === null || query.anyOf.length === 0) return []
  if (query.anyOf.some(clause => clause.addresses === null || clause.addresses.length === 0)) return []
  const seen = new Set<string>()
  const addresses: string[] = []
  for (const clause of query.anyOf) {
    if (clause.addresses === null) continue
    for (const address of clause.addresses) {
      if (seen.has(address)) continue
      seen.add(address)
      addresses.push(address)
    }
  }
  return Object.freeze(addresses)
}

function nativeAddressPredicates(
  query: NormalizedScanQuery,
  predicates: readonly ScanPredicateDescription[],
  addresses: readonly string[]
): readonly ScanPredicateDescription[] {
  if (addresses.length === 0) return Object.freeze([])
  return Object.freeze(
    predicates.filter(
      predicate =>
        predicate.clauseSet === 'anyOf' &&
        predicate.field === 'addresses' &&
        predicate.operator === 'equals' &&
        fullyPushedAddressPredicate(query, predicate, addresses)
    )
  )
}

function fullyPushedAddressPredicate(
  query: NormalizedScanQuery,
  predicate: ScanPredicateDescription,
  addresses: readonly string[]
): boolean {
  const clause = query.anyOf?.[predicate.clauseIndex]
  if (clause === undefined || clause.addresses === null) return false
  return clause.addresses.every(address => addresses.includes(address))
}

function nativeServicePredicates(
  query: NormalizedScanQuery,
  predicates: readonly ScanPredicateDescription[],
  serviceUuids: readonly Uuid[]
): readonly ScanPredicateDescription[] {
  if (serviceUuids.length === 0) return Object.freeze([])
  return Object.freeze(
    predicates.filter(
      predicate =>
        predicate.clauseSet === 'anyOf' &&
        predicate.field === 'services' &&
        predicate.operator === 'all' &&
        fullyPushedServicePredicate(query, predicate, serviceUuids)
    )
  )
}

function fullyPushedServicePredicate(
  query: NormalizedScanQuery,
  predicate: ScanPredicateDescription,
  serviceUuids: readonly Uuid[]
): boolean {
  const clause = query.anyOf?.[predicate.clauseIndex]
  if (clause === undefined || clause.services === null) return false
  const requiredServices = clause.services.all.map(service => canonicalUuid(service))
  return (
    requiredServices.length === serviceUuids.length &&
    serviceUuids.every(serviceUuid => requiredServices.includes(serviceUuid))
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
  if (field === 'addresses') return 'address'
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
    localNamePrefix: filter.localNamePrefix,
    ...(filter.deviceAddresses === undefined ? {} : { deviceAddresses: Object.freeze([...filter.deviceAddresses]) })
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
    value === 'address' ||
    value === 'localName' ||
    value === 'rssi' ||
    value === 'connectable' ||
    value === 'serviceUuids' ||
    value === 'manufacturerData' ||
    value === 'serviceData'
  )
}
