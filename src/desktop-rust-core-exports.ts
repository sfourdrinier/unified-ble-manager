// src/desktop-rust-core-exports.ts
//
// The shared-core desktop surface every desktop entrypoint re-exports
// (node/bluez, node/corebluetooth, node/winrt, electron/main): one provider,
// one binding contract, one parity table.

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
export { DESKTOP_RUST_CORE_PARITY } from './backends/desktop/desktop-rust-core-parity'
export type { DesktopRustCoreParityRow } from './backends/desktop/desktop-rust-core-parity'
