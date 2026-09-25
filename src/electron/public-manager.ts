import { BackendContractError, contractError } from '../backend-contract/errors'
import type { NormalizedBleError } from '../backend-contract/errors'
import type { StreamTerminalNotice } from '../backend-contract/streams'
import type { SerializableRecord } from '../backend-contract/primitives'
import type { ElectronRendererIpcTransport } from './protocol'
import type {
  IpcBleEvent,
  IpcBleRequest,
  IpcBleResponse,
  IpcClientTransport,
  IpcEventTransportHealthNotice
} from '../ipc/protocol'
import { ElectronRendererBleClient } from './renderer'
import { IpcBleManager } from '../ipc/manager'
import { aggregateEventLossError } from '../ipc/aggregate-event-loss'
import { IpcPublicManagerAdapter } from '../ipc/public-manager'
import type { BleManager } from '../public/ble-manager'
import { rehydratePublicPromise } from '../public/error-bridge'

export interface ElectronRendererBleManagerEnvironment {
  readonly transport: ElectronRendererIpcTransport<string, string>
}

/** Creates the common public manager over an authenticated preload transport. */
export async function createElectronRendererBleManager(
  environment: ElectronRendererBleManagerEnvironment
): Promise<BleManager> {
  return rehydratePublicPromise(
    (async () => {
      const rendererClient = new ElectronRendererBleClient(environment.transport)
      const transport = new ElectronClientTransport(rendererClient)
      const ipc = await IpcBleManager.create(transport)
      return new IpcPublicManagerAdapter(ipc, { requireScanPlan: true, gattDeliverySelection: 'controllable' })
    })()
  )
}

/** Explicit injection spelling used by deterministic and packed consumer tests. */
export const createElectronRendererBleManagerWithEnvironment = createElectronRendererBleManager

class ElectronClientTransport implements IpcClientTransport<string, string> {
  private readonly listeners = new Set<(event: IpcBleEvent) => void>()
  private pumping = false
  private pumpTerminated = false
  private healthNotice: IpcEventTransportHealthNotice | null = null
  private readonly healthListeners = new Set<(notice: IpcEventTransportHealthNotice) => void>()
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
        signal: request.signal ?? null
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

  subscribeEventHealth(listener: (notice: IpcEventTransportHealthNotice) => void): () => void {
    this.healthListeners.add(listener)
    if (this.healthNotice !== null) listener(this.healthNotice)
    return () => this.healthListeners.delete(listener)
  }

  acknowledge(): Promise<{ kind: 'event.ack' }> {
    // ElectronRendererBleClient acknowledges the authenticated event itself.
    return Promise.resolve({ kind: 'event.ack' })
  }

  private startPump(): void {
    if (this.pumping || this.pumpTerminated) return
    this.pumping = true
    this.pump()
      .catch(error => this.failTransport('source-failed', transportError(error, 'event-iterator')))
      .finally(() => {
        this.pumping = false
      })
  }

  private async pump(): Promise<void> {
    for await (const item of this.client.events) {
      if (item.kind === 'overflow') {
        this.failTransport(
          'overflow',
          aggregateEventLossError('electron-public-manager.aggregate-event-loss', 'electron-renderer-events', item)
        )
        return
      }
      if (item.kind === 'terminal') {
        this.failTransport(
          item.reason === 'owner-released'
            ? 'owner-released'
            : item.reason === 'overflow'
              ? 'overflow'
              : 'source-failed',
          terminalError(item)
        )
        return
      }
      if (!isRendererStreamRecord(item.value)) {
        this.failTransport('source-failed', transportError(null, 'event-record'))
        return
      }
      let rendererLease
      try {
        rendererLease = this.client.bootstrap.rendererLease
      } catch (error) {
        this.failTransport('source-failed', transportError(error, 'renderer-lease'))
        return
      }
      const event: IpcBleEvent = Object.freeze({
        rendererLease,
        eventId: `electron-public-event-${this.nextEvent++}`,
        streamId: item.value.streamId,
        item: item.value.item
      })
      for (const listener of [...this.listeners]) listener(event)
    }
    this.failTransport('source-failed', transportError(null, 'event-stream-ended'))
  }

  private failTransport(reason: IpcEventTransportHealthNotice['reason'], error: NormalizedBleError | null): void {
    if (this.pumpTerminated) return
    this.pumpTerminated = true
    this.healthNotice = Object.freeze({ reason, error })
    for (const listener of [...this.healthListeners]) listener(this.healthNotice)
  }
}

function terminalError(notice: StreamTerminalNotice): NormalizedBleError | null {
  if (notice.reason === 'owner-released') return null
  if (notice.reason === 'overflow') {
    return (
      notice.error ??
      aggregateEventLossError('electron-public-manager.aggregate-event-loss', 'electron-renderer-events', notice)
    )
  }
  return notice.error ?? transportError(null, `event-terminal-${notice.reason}`)
}

function transportError(error: unknown, operation: string): NormalizedBleError {
  if (error instanceof BackendContractError) return error.normalized
  return contractError('platform.transport', 'ipc', `electron-public-manager.${operation}`, {
    domain: 'electron-renderer-events',
    code: 'delivery-failed',
    safeMessage: error instanceof Error ? error.message : 'Electron event delivery failed',
    metadata: Object.freeze({})
  }).normalized
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
