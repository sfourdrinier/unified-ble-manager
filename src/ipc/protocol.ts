// src/ipc/protocol.ts

/** Framework-neutral names for the versioned desktop webview IPC protocol. */
export {
  ELECTRON_BLE_IPC_CHANNEL as IPC_BLE_PROTOCOL_CHANNEL,
  ELECTRON_CONNECTION_EVENTS_STREAM_HANDLE_PREFIX as IPC_CONNECTION_EVENTS_STREAM_HANDLE_PREFIX,
  ELECTRON_CONNECTION_LIFECYCLE_EVENT_SCHEMA_VERSION as IPC_CONNECTION_LIFECYCLE_EVENT_SCHEMA_VERSION,
  isElectronConnectionEventsStreamHandle as isIpcConnectionEventsStreamHandle
} from '../electron/protocol'

export type {
  ElectronAttachmentRecordV1 as IpcAttachmentRecordV1,
  ElectronAdapterRecordV1 as IpcAdapterRecordV1,
  ElectronAdapterStateV1 as IpcAdapterStateV1,
  ElectronBleIpcEvent as IpcBleEvent,
  ElectronBleIpcRequest as IpcBleRequest,
  ElectronBleIpcResponse as IpcBleResponse,
  ElectronBootstrapRequest as IpcBootstrapRequest,
  ElectronBootstrapResponse as IpcBootstrapResponse,
  ElectronConnectionEventsSubscribeResponseV1 as IpcConnectionEventsSubscribeResponseV1,
  ElectronConnectionLifecycleEventV1 as IpcConnectionLifecycleEventV1,
  ElectronEventAcknowledgeRequest as IpcEventAcknowledgeRequest,
  ElectronEventAcknowledgeResponse as IpcEventAcknowledgeResponse,
  ElectronFailureResponse as IpcFailureResponse,
  ElectronIpcOperationReceipt as IpcOperationReceipt,
  ElectronIpcOperationRequest as IpcOperationRequest,
  ElectronReleaseRequest as IpcReleaseRequest,
  ElectronReleaseResponse as IpcReleaseResponse,
  ElectronRendererBootstrap as IpcClientBootstrap,
  ElectronRendererIpcTransport as IpcClientTransport,
  ElectronRouteRequest as IpcRouteRequest,
  ElectronRouteResponse as IpcRouteResponse
} from '../electron/protocol'
