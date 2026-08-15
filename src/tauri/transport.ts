// src/tauri/transport.ts

import type { IpcClientLeaseIdentity } from '../backend-contract/ipc'
import type {
  IpcBleEvent,
  IpcBleRequest,
  IpcBleResponse,
  IpcClientTransport,
  IpcEventAcknowledgeResponse,
  IpcFailureResponse
} from '../ipc/protocol'

/** Tauri v2 plugin command registered by the Rust crate. */
export const TAURI_BLE_PLUGIN_COMMAND = 'plugin:unified-ble-manager|invoke'

/** Structural subset of `@tauri-apps/api/core.invoke` used by this package. */
export type TauriInvoke = <Response>(command: string, args?: Record<string, unknown>) => Promise<Response>

/** Structural subset of a Tauri v2 Channel. */
export interface TauriChannel<Message> {
  onmessage: ((message: Message) => void) | null
}

/** Constructor shape of `Channel` from `@tauri-apps/api/core`. */
export interface TauriChannelConstructor {
  new <Message>(): TauriChannel<Message>
}

export interface TauriBleIpcTransportOptions {
  readonly invoke: TauriInvoke
  readonly Channel: TauriChannelConstructor
  readonly command?: string
}

/**
 * Tauri v2 transport for the shared desktop IPC client. Callers pass the
 * official `invoke` and `Channel` exports, keeping the core package free of a
 * mandatory Tauri runtime dependency and straightforward to test.
 */
export class TauriBleIpcTransport<Attachment extends string, Client extends string>
  implements IpcClientTransport<Attachment, Client>
{
  private readonly invokeCore: TauriInvoke
  private readonly command: string
  private readonly eventChannel: TauriChannel<IpcBleEvent>
  private readonly listeners = new Set<(event: IpcBleEvent) => void>()

  constructor(options: TauriBleIpcTransportOptions) {
    if (typeof options.invoke !== 'function' || typeof options.Channel !== 'function') {
      throw new TypeError('TauriBleIpcTransport requires the official Tauri v2 invoke and Channel APIs')
    }
    this.invokeCore = options.invoke
    this.command = options.command ?? TAURI_BLE_PLUGIN_COMMAND
    if (this.command.length === 0) {
      throw new TypeError('TauriBleIpcTransport command must not be empty')
    }
    this.eventChannel = new options.Channel<IpcBleEvent>()
    this.eventChannel.onmessage = event => {
      for (const listener of [...this.listeners]) {
        listener(event)
      }
    }
  }

  invoke<Operation extends string>(
    request: IpcBleRequest<Attachment, Client, Operation>
  ): Promise<IpcBleResponse<Attachment, Client>> {
    return this.invokeCore<IpcBleResponse<Attachment, Client>>(this.command, {
      request,
      eventChannel: this.eventChannel
    })
  }

  subscribe(listener: (event: IpcBleEvent) => void): () => void {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  acknowledge(
    rendererLease: IpcClientLeaseIdentity,
    eventId: string
  ): Promise<IpcEventAcknowledgeResponse | IpcFailureResponse> {
    return this.invokeCore<IpcEventAcknowledgeResponse | IpcFailureResponse>(this.command, {
      request: { kind: 'event.ack', rendererLease, eventId },
      eventChannel: this.eventChannel
    })
  }
}
