import type { ScanFilter } from '../../backend-contract/advertisement'
import { createServiceUuidScanPlan, diagnosticServiceUuidScanPlan } from '../scan-planning/service-uuid-scan-planner'
import type {
  BackendScanExecutionPlan,
  BackendScanPlanner,
  ScanObservationField,
  ScanPlan,
  ScanPlanningContext
} from '../../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'

const bluezScanObservationFields: readonly ScanObservationField[] = Object.freeze([
  'address',
  'localName',
  'rssi',
  'serviceUuids'
])

export const bluezScanPlanningContext: ScanPlanningContext = Object.freeze({
  backendId: 'bluez',
  platformId: 'bluez',
  availableObservationFields: bluezScanObservationFields
})

export function planBluezScan(query: NormalizedScanQuery): BackendScanExecutionPlan<ScanFilter> {
  return new BluezScanPlanner().plan(query, bluezScanPlanningContext)
}

export function diagnosticBluezScanPlan(query: NormalizedScanQuery): ScanPlan {
  return diagnosticServiceUuidScanPlan(planBluezScan(query))
}

export class BluezScanPlanner implements BackendScanPlanner<ScanFilter> {
  plan(query: NormalizedScanQuery, context: ScanPlanningContext): BackendScanExecutionPlan<ScanFilter> {
    return createServiceUuidScanPlan(query, context, 'invalid BlueZ scan planning context')
  }
}
