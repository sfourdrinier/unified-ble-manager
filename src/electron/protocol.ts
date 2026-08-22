// src/electron/protocol.ts

/**
 * Electron compatibility surface. The versioned desktop webview IPC protocol
 * itself is host-neutral and canonically defined in `src/ipc/protocol.ts`, so
 * Electron, Tauri, and future desktop webview hosts share one request,
 * response, event, and transport contract instead of one named after a single
 * host. These names keep the Electron-facing spelling of that same contract.
 */
export {
  IPC_BLE_PROTOCOL_CHANNEL as ELECTRON_BLE_IPC_CHANNEL,
  IPC_CONNECTION_EVENTS_STREAM_HANDLE_PREFIX as ELECTRON_CONNECTION_EVENTS_STREAM_HANDLE_PREFIX,
  IPC_CONNECTION_LIFECYCLE_EVENT_SCHEMA_VERSION as ELECTRON_CONNECTION_LIFECYCLE_EVENT_SCHEMA_VERSION,
  isIpcConnectionEventsStreamHandle as isElectronConnectionEventsStreamHandle
} from '../ipc/protocol'

export type {
  IpcAdapterRecordV2 as ElectronAdapterRecordV2,
  IpcAdapterStateV2 as ElectronAdapterStateV2,
  IpcAttachmentRecordV2 as ElectronAttachmentRecordV2,
  IpcBleEvent as ElectronBleIpcEvent,
  IpcBleRequest as ElectronBleIpcRequest,
  IpcBleResponse as ElectronBleIpcResponse,
  IpcBleSuccessResponse as ElectronBleIpcSuccessResponse,
  IpcBootstrapRequest as ElectronBootstrapRequest,
  IpcBootstrapResponse as ElectronBootstrapResponse,
  IpcClientBootstrap as ElectronRendererBootstrap,
  IpcClientTransport as ElectronRendererIpcTransport,
  IpcConnectionEventsSubscribeResponseV2 as ElectronConnectionEventsSubscribeResponseV2,
  IpcConnectionLifecycleEventV2 as ElectronConnectionLifecycleEventV2,
  IpcDiscoveryDescriptor as ElectronDiscoveryDescriptor,
  IpcEventAcknowledgeRequest as ElectronEventAcknowledgeRequest,
  IpcEventAcknowledgeResponse as ElectronEventAcknowledgeResponse,
  IpcFailureResponse as ElectronFailureResponse,
  IpcOperationReceipt as ElectronIpcOperationReceipt,
  IpcOperationRequest as ElectronIpcOperationRequest,
  IpcReleaseRequest as ElectronReleaseRequest,
  IpcReleaseResponse as ElectronReleaseResponse,
  IpcRouteRequest as ElectronRouteRequest,
  IpcRouteResponse as ElectronRouteResponse
} from '../ipc/protocol'
