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
export type {
  ContinuationRecordingController,
  ContinuationRecordingStatus,
  ContinuationRecordingBatch,
  ContinuationRecordingMetadata,
  ContinuationRecordingFailure,
  ContinuationRecordingPrepareOptions
} from './core/continuation-recording'
export type {
  NativeContinuationController,
  NativeContinuationControl,
  NativeContinuationControlAccess,
  NativeContinuationCompleted,
  NativeContinuationFailed,
  NativeContinuationStatus
} from './backends/desktop/native-continuation-controller'
export type { NativeContinuationClaimOptions } from './core/native-continuation-claim'
export type {
  ContinuationBacklog,
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
  DesktopRustCoreProviderOptions
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
