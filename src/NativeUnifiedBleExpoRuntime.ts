// src/NativeUnifiedBleExpoRuntime.ts

import type { TurboModule } from 'react-native'
import { TurboModuleRegistry } from 'react-native'

export type NativeExpoRuntimePlatform = 'android' | 'apple'
export type NativeExpoLegacyLocationPolicy = 'auto' | 'required' | 'none'

export interface NativeExpoRuntimeConfiguration {
  platform: NativeExpoRuntimePlatform
  configurationDigest: string
  legacyLocationPolicy?: NativeExpoLegacyLocationPolicy
}

export interface NativeExpoPermissionRequest {
  purpose: 'scan-and-connect'
}

export interface NativeExpoPermissionResult {
  requested: string[]
  granted: string[]
  denied: string[]
  recommendedSettingsTarget: string | null
}

export interface NativeExpoSettingsRequest {
  target: 'app' | 'bluetooth' | 'location-services'
}

export interface Spec extends TurboModule {
  getRuntimeConfiguration(): Promise<NativeExpoRuntimeConfiguration>
  requestPermissions(request: NativeExpoPermissionRequest): Promise<NativeExpoPermissionResult>
  openSettings(request: NativeExpoSettingsRequest): Promise<void>
}

export default TurboModuleRegistry.getEnforcing<Spec>('UnifiedBleExpoRuntime')
