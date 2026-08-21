import { contractError } from '../backend-contract/errors'
import type { SerializableRecord } from '../backend-contract/primitives'
import type { ElectronRendererIpcTransport } from './protocol'
import type { IpcBleEvent, IpcBleRequest, IpcBleResponse, IpcClientTransport } from '../ipc/protocol'
import { ElectronRendererBleClient } from './renderer'
import { IpcBleManager } from '../ipc/manager'
import { IpcPublicManagerAdapter } from '../ipc/public-manager'
import type { BleManager } from '../public/ble-manager'

export interface ElectronRendererBleManagerEnvironment {
  readonly transport: ElectronRendererIpcTransport<string, string>
}

/** Creates the common public manager over an authenticated preload transport. */
export async function createElectronRendererBleManager(
  environment: ElectronRendererBleManagerEnvironment
): Promise<BleManager> {
  const rendererClient = new ElectronRendererBleClient(environment.transport)
  const transport = new ElectronClientTransport(rendererClient)
  const ipc = await IpcBleManager.create(transport)
  return new IpcPublicManagerAdapter(ipc)
}

/** Explicit injection spelling used by deterministic and packed consumer tests. */
export const createElectronRendererBleManagerWithEnvironment = createElectronRendererBleManager

class ElectronClientTransport implements IpcClientTransport<string, string> {
  private readonly listeners = new Set<(event: IpcBleEvent) => void>()
  private pumping = false
  private nextEvent = 1

  constructor(private readonly client: ElectronRendererBleClient<string, string>) {}

  async invoke<Operation extends string>(
    request: IpcBleRequest<string, string, Operation>
  ): Promise<IpcBleResponse<string, string>> {
    if (request.kind === 'bootstrap') {
      return { kind: 'bootstrap', bootstrap: await this.client.initialize() }
    }
    if (request.kind === 'route') {
      const receipt = await this.client.request({
        command: request.envelope.command,
        payload: request.envelope.payload,
        binaryPayload: request.envelope.binaryPayload,
        signal: null
      })
      return { kind: 'route', payload: receipt.payload }
    }
    if (request.kind === 'release') {
      return { kind: 'release', cleanup: await this.client.destroy() }
    }
    if (request.kind === 'event.ack') return { kind: 'event.ack' }
    throw contractError('protocol.malformed', 'ipc', 'electron-public-manager.request')
  }

  subscribe(listener: (event: IpcBleEvent) => void): () => void {
    this.listeners.add(listener)
    this.startPump()
    return () => this.listeners.delete(listener)
  }

  acknowledge(): Promise<{ kind: 'event.ack' }> {
    // ElectronRendererBleClient acknowledges the authenticated event itself.
    return Promise.resolve({ kind: 'event.ack' })
  }

  private startPump(): void {
    if (this.pumping) return
    this.pumping = true
    this.pump().catch(() => undefined)
  }

  private async pump(): Promise<void> {
    for await (const item of this.client.events) {
      if (item.kind !== 'value' || !isRendererStreamRecord(item.value)) continue
      let rendererLease
      try {
        rendererLease = this.client.bootstrap.rendererLease
      } catch {
        continue
      }
      const event: IpcBleEvent = Object.freeze({
        rendererLease,
        eventId: `electron-public-event-${this.nextEvent++}`,
        streamId: item.value.streamId,
        item: item.value.item
      })
      for (const listener of [...this.listeners]) listener(event)
    }
  }
}

function isRendererStreamRecord(value: SerializableRecord): value is SerializableRecord & {
  readonly streamId: string
  readonly item: SerializableRecord
} {
  return typeof value.streamId === 'string' && isSerializableRecord(value.item)
}

function isSerializableRecord(value: unknown): value is SerializableRecord {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array)
}
