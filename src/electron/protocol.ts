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
  IpcAdapterRecordV1 as ElectronAdapterRecordV1,
  IpcAdapterStateV1 as ElectronAdapterStateV1,
  IpcAttachmentRecordV1 as ElectronAttachmentRecordV1,
  IpcBleEvent as ElectronBleIpcEvent,
  IpcBleRequest as ElectronBleIpcRequest,
  IpcBleResponse as ElectronBleIpcResponse,
  IpcBleSuccessResponse as ElectronBleIpcSuccessResponse,
  IpcBootstrapRequest as ElectronBootstrapRequest,
  IpcBootstrapResponse as ElectronBootstrapResponse,
  IpcClientBootstrap as ElectronRendererBootstrap,
  IpcClientTransport as ElectronRendererIpcTransport,
  IpcConnectionEventsSubscribeResponseV1 as ElectronConnectionEventsSubscribeResponseV1,
  IpcConnectionLifecycleEventV1 as ElectronConnectionLifecycleEventV1,
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
