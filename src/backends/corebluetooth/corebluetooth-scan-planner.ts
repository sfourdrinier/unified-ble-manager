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

const coreBluetoothScanObservationFields: readonly ScanObservationField[] = Object.freeze([
  'localName',
  'rssi',
  'connectable',
  'serviceUuids',
  'manufacturerData',
  'serviceData'
])

export const coreBluetoothScanPlanningContext: ScanPlanningContext = Object.freeze({
  backendId: 'corebluetooth',
  platformId: 'corebluetooth',
  availableObservationFields: coreBluetoothScanObservationFields
})

export function planCoreBluetoothScan(query: NormalizedScanQuery): BackendScanExecutionPlan<ScanFilter> {
  return new CoreBluetoothScanPlanner().plan(query, coreBluetoothScanPlanningContext)
}

export function diagnosticCoreBluetoothScanPlan(query: NormalizedScanQuery): ScanPlan {
  return diagnosticServiceUuidScanPlan(planCoreBluetoothScan(query))
}

export class CoreBluetoothScanPlanner implements BackendScanPlanner<ScanFilter> {
  plan(query: NormalizedScanQuery, context: ScanPlanningContext): BackendScanExecutionPlan<ScanFilter> {
    return createServiceUuidScanPlan(query, context, 'invalid CoreBluetooth scan planning context')
  }
}
