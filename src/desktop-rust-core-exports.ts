// src/desktop-rust-core-exports.ts
//
// The shared-core desktop surface every desktop entrypoint re-exports
// (node/bluez, node/corebluetooth, node/winrt, electron/main): one provider,
// one binding contract, one parity table.

export { loadDesktopCoreBinding } from './desktop-core-addon'
export type {
  DesktopProcessHost,
  DesktopProcessManagerOptions,
  DesktopProcessInternalManager
} from './desktop-process-host'
export { DesktopProcessHostInitializationError } from './desktop-process-host'
export type { DesktopCoreHost } from './desktop-core-addon'
export {
  createNativeContinuationController,
  createNativeContinuationControl
} from './backends/desktop/native-continuation-controller'
export { openNativeContinuationRecordings } from './backends/desktop/native-continuation-recording'
export { openDesktopScanFailureRecovery } from './backends/desktop/scan-failure-recovery-recording'
export type {
  DesktopContinuationRecordingController,
  DesktopLegacyRecoveryPage
} from './backends/desktop/native-continuation-recording'
export type {
  DesktopScanFailureRecovery,
  DesktopScanFailureRecoveryChunk,
  DesktopScanFailureRecoveryHead,
  DesktopScanFailureRecoveryOwner
} from './backends/desktop/scan-failure-recovery-recording'
export type {
  ContinuationRecordingController,
  ContinuationRecordingStatus,
  ContinuationRecordingBatch,
  ContinuationRecordingMetadata,
  ContinuationRecordingFailure,
  ContinuationRecordingDiagnosticMarker,
  ContinuationRecordingRuntimeDiagnostic,
  ContinuationRecordingCollectionDiagnostic,
  ContinuationRecordingPrepareOptions
} from './core/continuation-recording'
export type {
  NativeContinuationController,
  NativeContinuationControllerV1,
  NativeContinuationControl,
  NativeContinuationControlV1,
  NativeContinuationControlAccess,
  NativeContinuationCompleted,
  NativeContinuationFailed,
  NativeContinuationStatus
} from './backends/desktop/native-continuation-controller'
export type { NativeContinuationClaimOptions } from './core/native-continuation-claim'
export type {
  ContinuationBacklog,
  ContinuationRetainedPrefixV1,
  ContinuationClaimAcknowledgement,
  ContinuationBacklogValue,
  ContinuationBacklogStreamEnd
} from './backends/reactnative/react-native-continuation-claim'

export {
  ADAPTER_INITIALIZATION_TIMEOUT_MS,
  DESKTOP_RUST_CORE_IMPLEMENTATION_VERSION,
  DESKTOP_RUST_CORE_PROFILES,
  DesktopRustCoreBackend,
  assertDesktopRustCorePlatform,
  createDesktopRustCoreBackendProvider,
  createDesktopRustCoreFeatureRegistry,
  desktopRustCoreAdapterId,
  desktopRustCoreMissingOperation,
  planDesktopRustCoreScan
} from './backends/desktop/desktop-rust-core-provider'
export type {
  DesktopRustCoreProfile,
  DesktopRustCoreProviderOptions,
  DesktopPeerNotificationFailureRecovery,
  DesktopPeerNotificationFailureRecoveryOffer,
  DesktopDurablePeerNotificationFailureRecovery
} from './backends/desktop/desktop-rust-core-provider'
export {
  desktopRustCoreError,
  desktopRustCoreOperation,
  parseDesktopRustCoreWireError
} from './backends/desktop/desktop-rust-core-binding'
export type {
  DesktopRustCoreAdapterEvent,
  DesktopRustCoreAdapterLossCause,
  DesktopRustCoreAdapterResetEvent,
  DesktopRustCoreAdapterStatus,
  DesktopRustCoreAttachmentTuple,
  DesktopRustCoreAdapterListing,
  DesktopRustCoreAdapterPower,
  DesktopRustCoreAdvertisement,
  DesktopRustCoreBinding,
  DesktopRustCoreCancelInfo,
  DesktopRustCoreCentral,
  DesktopRustCoreCloseReport,
  DesktopRustCoreControl,
  DesktopRustCoreConsumerCounters,
  DesktopRustCoreDispatchCounters,
  DesktopRustCoreLifecycleEvent,
  DesktopRustCoreNotificationPoll,
  DesktopRustCorePath,
  DesktopRustCorePeerRecord,
  DesktopRustCorePlatformDetail,
  DesktopRustCoreScanObservation,
  DesktopRustCorePlatform,
  DesktopRustCoreRadio,
  DesktopRustCoreSelector,
  DesktopRustCoreTicketCancel,
  DesktopRustCoreWireError
} from './backends/desktop/desktop-rust-core-binding'
