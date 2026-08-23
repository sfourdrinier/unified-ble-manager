import type { ScanFilter } from '../../backend-contract/advertisement'
import { snapshotScanExecutionPlan, describeScanPredicates } from '../../backend-contract/scan-planning'
import type {
  BackendScanExecutionPlan,
  BackendScanPlanner,
  ScanObservationField,
  ScanPlanningContext
} from '../../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'

export class DeterministicScanPlanner implements BackendScanPlanner<ScanFilter> {
  plan(query: NormalizedScanQuery, context: ScanPlanningContext): BackendScanExecutionPlan<ScanFilter> {
    assertPlanningContext(context)

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
        estimatedCost: 'high',
        nativeFilter: { serviceUuids: [], manufacturerData: [], localNamePrefix: null }
      },
      snapshotScanFilter
    )
  }
}

function assertPlanningContext(context: ScanPlanningContext): void {
  if (
    context.backendId.length === 0 ||
    context.platformId.length === 0 ||
    !Array.isArray(context.availableObservationFields) ||
    context.availableObservationFields.some(field => !isScanObservationField(field))
  ) {
    throw new Error('invalid scan planning context')
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
