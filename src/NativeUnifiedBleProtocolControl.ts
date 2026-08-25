// src/NativeUnifiedBleProtocolControl.ts

import type { TurboModule } from 'react-native'
import { TurboModuleRegistry } from 'react-native'

export interface NativeProtocolVersionRange {
  minimum: number
  maximum: number
}

export interface NativeProtocolHandshakeRequest {
  nativeProtocol: NativeProtocolVersionRange
  abi: NativeProtocolVersionRange
  controlSurface: NativeProtocolVersionRange
  backendContract: NativeProtocolVersionRange
  capabilitySchema: NativeProtocolVersionRange
  eventSchema: NativeProtocolVersionRange
  traceFormat: NativeProtocolVersionRange
  attachmentId: string
  backendInstanceId: string
  backendGeneration: string
  adapterId: string
  adapterGeneration: string
  ownerId: string
}

export interface NativeProtocolHandshakeResult {
  nativeProtocol: number
  abi: number
  controlSurface: number
  backendContract: number
  capabilitySchema: number
  eventSchema: number
  traceFormat: number
  maximumControlRecordBytes: number
  maximumBinaryPayloadBytes: number
  /** True only when this Android runtime can execute the API-26+ PHY operations. */
  phyAvailable?: boolean
  /** True only when the native attachment implements the Android security extension. */
  securityAvailable?: boolean
  /** True only when the native attachment can cancel system pairing on this API level. */
  securityCancelPairingAvailable?: boolean
}

export interface NativeAttachmentIdentity {
  attachmentId: string
  backendInstanceId: string
  backendGeneration: string
  adapterId: string
  adapterGeneration: string
}

export interface NativeOperationCorrelation {
  attachment: NativeAttachmentIdentity
  dispatchEpoch: number
  nonce: string
}

export interface NativeRestorationBootstrapRequest {
  restorationId: string
  generation: string
}

export interface NativeRestorationBootstrapIdentity {
  applicationId: string
  restorationId: string
  generation: string
  restoreIdentifier: string
  namespaceValue: string
  clientId: string
  hostSessionScope: string
}

export interface NativeBackgroundLeaseRequest {
  kind: 'connected-device'
  reason: string
}

export interface NativeBackgroundLeaseResult {
  leaseId: string
}

export interface NativeBackgroundLeaseReleaseRequest {
  leaseId: string
}

export interface NativeCompanionAssociationRequest {
  name?: string
  serviceUuid?: string
}

export interface NativeCompanionAssociationResult {
  source: 'associated'
  associationId: number
  peerId: string | null
  displayName: string | null
}

export type NativeRestorationOutcome =
  | 'adopted'
  | 'alreadyConsumed'
  | 'attachmentMismatch'
  | 'backendMismatch'
  | 'namespaceMismatch'
  | 'epochMismatch'

export type NativeCancellationState = 'cancellationRequested' | 'alreadyTerminal' | 'notCancellable'

export interface NativeRestorationAdoptionRequest {
  namespaceValue: string
  attachmentId: string
  expectedBackendInstanceId: string
  expectedEpoch: string
  nativeProtocolMinimum: number
  nativeProtocolMaximum: number
  clientId: string
  hostSessionScope: string
}

export interface NativeRestorationReplayRecord {
  recordVersion: number
  namespaceValue: string
  attachmentId: string
  backendInstanceId: string
  backendGeneration: string
  adapterId: string
  adapterGeneration: string
  ordinal: number
  adoptionEpoch: string
  kind: 'adapter' | 'connection'
  peerId: string | null
  connectionId: string | null
  ownerLeaseId: string | null
  connectionGeneration: string | null
}

export interface NativeRestorationAdoptionControlResult {
  receiptId: string
  outcome: NativeRestorationOutcome
  boundClientId: string
  adoptionEpoch: string
  replayRecordCount: number
  records: NativeRestorationReplayRecord[]
}

export interface NativeCancellationControlResult {
  state: NativeCancellationState
}

export interface Spec extends TurboModule {
  handshake(request: NativeProtocolHandshakeRequest): Promise<NativeProtocolHandshakeResult>
  bootstrapRestorationIdentity(request: NativeRestorationBootstrapRequest): Promise<NativeRestorationBootstrapIdentity>
  acquireBackground(request: NativeBackgroundLeaseRequest): Promise<NativeBackgroundLeaseResult>
  releaseBackground(request: NativeBackgroundLeaseReleaseRequest): Promise<void>
  associateCompanionDevice(request: NativeCompanionAssociationRequest): Promise<NativeCompanionAssociationResult>
  claimRestoration(): Promise<NativeRestorationAdoptionControlResult>
  installExecutionRuntime(): Promise<void>
  cancelOperation(correlation: NativeOperationCorrelation): Promise<NativeCancellationControlResult>
  adoptRestoration(request: NativeRestorationAdoptionRequest): Promise<NativeRestorationAdoptionControlResult>
  closeAttachment(attachment: NativeAttachmentIdentity): Promise<void>
  /** Base64-encoded cryptographically secure random bytes. */
  getRandomBytes(length: number): Promise<string>
}

export default TurboModuleRegistry.getEnforcing<Spec>('UnifiedBleProtocolControl')
