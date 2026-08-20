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
const TAURI_BYTES_WIRE_TAG = '$__unifiedBleBytesV1'

/**
 * The only request that carries the event Channel. Attaching binds the sink
 * for the attachment's lifetime; see `eventChannelArgument`.
 */
export const TAURI_ATTACH_REQUEST_KIND = 'bootstrap'

function isAttachRequest(request: { readonly kind?: unknown }): boolean {
  return request.kind === TAURI_ATTACH_REQUEST_KIND
}

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
  private readonly eventChannel: TauriChannel<unknown>
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
    this.eventChannel = new options.Channel<unknown>()
    this.eventChannel.onmessage = wireEvent => {
      const event = decodeTauriWireValue(wireEvent) as IpcBleEvent
      for (const listener of [...this.listeners]) {
        listener(event)
      }
    }
  }

  async invoke<Operation extends string>(
    request: IpcBleRequest<Attachment, Client, Operation>
  ): Promise<IpcBleResponse<Attachment, Client>> {
    const response = await this.invokeCore<unknown>(this.command, {
      request: encodeTauriWireValue(request),
      ...this.eventChannelArgument(request)
    })
    return decodeTauriWireValue(response) as IpcBleResponse<Attachment, Client>
  }

  /**
   * Supplies the event Channel on the attach request only.
   *
   * Tauri deserializes every `Channel` command argument into a *new* Rust
   * `Channel` bound to this one JavaScript callback id, and dropping any of
   * them evals `{ end: true, index }` for that shared id. The Tauri JS runtime
   * answers an end message whose index matches its next expected index by
   * calling `unregisterCallback`, which tears the callback down permanently.
   *
   * Passing the Channel on a second request therefore destroys the event
   * stream established by the first: the request/response path would silently
   * kill the event path. The event sink is bound once, at attach, and lives
   * for the attachment, which is the lifetime every other host already gives
   * it via `subscribe`.
   */
  private eventChannelArgument(request: IpcBleRequest<Attachment, Client, string>): {
    eventChannel?: TauriChannel<unknown>
  } {
    return isAttachRequest(request) ? { eventChannel: this.eventChannel } : {}
  }

  subscribe(listener: (event: IpcBleEvent) => void): () => void {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  async acknowledge(
    rendererLease: IpcClientLeaseIdentity,
    eventId: string
  ): Promise<IpcEventAcknowledgeResponse | IpcFailureResponse> {
    const response = await this.invokeCore<unknown>(this.command, {
      request: encodeTauriWireValue({ kind: 'event.ack', rendererLease, eventId })
    })
    return decodeTauriWireValue(response) as IpcEventAcknowledgeResponse | IpcFailureResponse
  }
}

/** Encodes bytes explicitly before Tauri serializes nested command arguments as JSON. */
export function encodeTauriWireValue(value: unknown): unknown {
  if (value instanceof Uint8Array) {
    return { [TAURI_BYTES_WIRE_TAG]: Array.from(value) }
  }
  if (Array.isArray(value)) {
    return value.map(item => encodeTauriWireValue(item))
  }
  if (isWireRecord(value)) {
    const encoded: Record<string, unknown> = {}
    for (const [key, item] of Object.entries(value)) {
      encoded[key] = encodeTauriWireValue(item)
    }
    return encoded
  }
  return value
}

/** Reconstructs independently owned bytes from Rust responses and Channel messages. */
export function decodeTauriWireValue(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(item => decodeTauriWireValue(item))
  }
  if (!isWireRecord(value)) {
    return value
  }
  if (Object.prototype.hasOwnProperty.call(value, TAURI_BYTES_WIRE_TAG)) {
    const keys = Object.keys(value)
    const bytes = value[TAURI_BYTES_WIRE_TAG]
    if (keys.length !== 1 || !Array.isArray(bytes) || !bytes.every(isByte)) {
      throw new TypeError('Malformed Unified BLE Tauri byte value')
    }
    return new Uint8Array(bytes)
  }
  const decoded: Record<string, unknown> = {}
  for (const [key, item] of Object.entries(value)) {
    decoded[key] = decodeTauriWireValue(item)
  }
  return decoded
}

function isWireRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function isByte(value: unknown): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= 255
}
