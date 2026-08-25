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

const reactNativeAndroidScanObservationFields: readonly ScanObservationField[] = Object.freeze([
  'localName',
  'rssi',
  'connectable',
  'serviceUuids',
  'manufacturerData',
  'serviceData',
  'address'
])

const reactNativeScanObservationFields: readonly ScanObservationField[] = Object.freeze([
  'localName',
  'rssi',
  'connectable',
  'serviceUuids',
  'manufacturerData',
  'serviceData'
])

export const reactNativeAndroidScanPlanningContext: ScanPlanningContext = Object.freeze({
  backendId: 'unified-ble:react-native-android',
  platformId: 'unified-ble:android-gatt',
  availableObservationFields: reactNativeAndroidScanObservationFields
})

export const reactNativeAppleScanPlanningContext: ScanPlanningContext = Object.freeze({
  backendId: 'unified-ble:react-native-apple',
  platformId: 'unified-ble:apple-corebluetooth',
  availableObservationFields: reactNativeScanObservationFields
})

export function planReactNativeAndroidScan(query: NormalizedScanQuery): BackendScanExecutionPlan<ScanFilter> {
  return new ReactNativeScanPlanner().plan(query, reactNativeAndroidScanPlanningContext)
}

export function diagnosticReactNativeAndroidScanPlan(query: NormalizedScanQuery): ScanPlan {
  return diagnosticServiceUuidScanPlan(planReactNativeAndroidScan(query))
}

export function planReactNativeAppleScan(query: NormalizedScanQuery): BackendScanExecutionPlan<ScanFilter> {
  return new ReactNativeScanPlanner().plan(query, reactNativeAppleScanPlanningContext)
}

export function diagnosticReactNativeAppleScanPlan(query: NormalizedScanQuery): ScanPlan {
  return diagnosticServiceUuidScanPlan(planReactNativeAppleScan(query))
}

export class ReactNativeScanPlanner implements BackendScanPlanner<ScanFilter> {
  plan(query: NormalizedScanQuery, context: ScanPlanningContext): BackendScanExecutionPlan<ScanFilter> {
    return createServiceUuidScanPlan(query, context, 'invalid React Native scan planning context')
  }
}
