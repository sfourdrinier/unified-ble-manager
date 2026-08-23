import type { ScanFilter } from '../../backend-contract/advertisement'
import { canonicalUuid } from '../../backend-contract/primitives'
import { describeScanPredicates, snapshotScanExecutionPlan } from '../../backend-contract/scan-planning'
import type {
  BackendScanExecutionPlan,
  BackendScanPlanner,
  ScanPlanningContext
} from '../../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'
import type { Uuid } from '../../backend-contract/primitives'

export class BluezScanPlanner implements BackendScanPlanner<ScanFilter> {
  plan(query: NormalizedScanQuery, context: ScanPlanningContext): BackendScanExecutionPlan<ScanFilter> {
    assertPlanningContext(context)
    const nativeFilter: ScanFilter = {
      serviceUuids: commonRequiredServices(query),
      manufacturerData: [],
      localNamePrefix: null
    }
    return snapshotScanExecutionPlan(
      {
        sourceQuery: query,
        queryDigest: query.digest,
        residualQueryDigest: query.digest,
        nativeGuarantee: 'safe-superset',
        native: { predicates: [], complete: false },
        residual: { query, predicates: describeScanPredicates(query), complete: true },
        unavailable: [],
        limitations: [],
        estimatedCost: nativeFilter.serviceUuids.length === 0 ? 'high' : 'moderate',
        nativeFilter
      },
      snapshotScanFilter
    )
  }
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

function assertPlanningContext(context: ScanPlanningContext): void {
  if (
    typeof context !== 'object' ||
    context === null ||
    typeof context.backendId !== 'string' ||
    typeof context.platformId !== 'string' ||
    context.backendId.length === 0 ||
    context.platformId.length === 0 ||
    !Array.isArray(context.availableObservationFields)
  ) {
    throw new Error('invalid BlueZ scan planning context')
  }
}
