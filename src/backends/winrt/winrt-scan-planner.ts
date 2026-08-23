import { createServiceUuidScanPlan, diagnosticServiceUuidScanPlan } from '../scan-planning/service-uuid-scan-planner'
import type { ScanFilter } from '../../backend-contract/advertisement'
import type {
  BackendScanExecutionPlan,
  BackendScanPlanner,
  ScanObservationField,
  ScanPlan,
  ScanPlanningContext
} from '../../backend-contract/scan-planning'
import type { NormalizedScanQuery } from '../../backend-contract/scan-query'

const winRtScanObservationFields: readonly ScanObservationField[] = Object.freeze(['localName', 'rssi', 'serviceUuids'])

export const winRtScanPlanningContext: ScanPlanningContext = Object.freeze({
  backendId: 'winrt',
  platformId: 'windows-winrt',
  availableObservationFields: winRtScanObservationFields
})

export function planWinRtScan(query: NormalizedScanQuery): BackendScanExecutionPlan<ScanFilter> {
  return new WinRtScanPlanner().plan(query, winRtScanPlanningContext)
}

export function diagnosticWinRtScanPlan(query: NormalizedScanQuery): ScanPlan {
  return diagnosticServiceUuidScanPlan(planWinRtScan(query))
}

export class WinRtScanPlanner implements BackendScanPlanner<ScanFilter> {
  plan(query: NormalizedScanQuery, context: ScanPlanningContext): BackendScanExecutionPlan<ScanFilter> {
    return createServiceUuidScanPlan(query, context, 'invalid WinRT scan planning context')
  }
}
