/* eslint-disable @typescript-eslint/no-explicit-any, react-hooks/rules-of-hooks */
// src/public/ble-manager.ts — non-generic application façade (PR1 skeleton)

import type { AdvertisementObservation } from '../backend-contract/advertisement'
import type { ScanOptions as InternalScanOptions } from '../backend-contract/advertisement'
import type { CleanupRecord } from '../backend-contract/errors'
import type { PeerId } from '../backend-contract/primitives'
import { opaqueId } from '../backend-contract/primitives'
import type { BleManager as InternalBleManager } from '../manager/ble-manager'
import type { BleManagerOptions } from '../manager/ble-manager'
import { normalizeOperationOptions } from './operation-options'
import type { OperationOptions } from './operation-options'
import { resolveStreamPreset } from './stream-presets'
import type { StreamPreset } from './stream-presets'

// Public peer — opaque backend-scoped identifier, no generic.
export interface BlePeer {
  readonly id: string
  readonly name: string | null
  readonly rssi: number | null
}

// Public connection — generation-bound lease, no generic.
export interface BleConnection {
  readonly peer: BlePeer
  readonly disconnect: () => Promise<CleanupRecord>
  readonly release: () => Promise<CleanupRecord>
}

// Public scan session — bounded stream, no generic.
export interface ScanSession {
  readonly stop: () => Promise<CleanupRecord>
  readonly observations: AsyncIterable<AdvertisementObservation<string>>
}

// Public GATT placeholders — full object model lands in PR3, but façade exists now.
export interface GattDatabase {
  readonly peer: BlePeer
}
export interface GattService {
  readonly uuid: string
}
export interface GattCharacteristic {
  readonly uuid: string
  read(options?: OperationOptions): Promise<Uint8Array>
  write(value: Uint8Array, options?: OperationOptions): Promise<void>
}
export interface GattDescriptor {
  readonly uuid: string
  read(options?: OperationOptions): Promise<Uint8Array>
  write(value: Uint8Array, options?: OperationOptions): Promise<void>
}
export type GattSubscriptionValue = {
  readonly value: Uint8Array
  readonly delivery: 'notification' | 'indication'
}

// Non-generic public manager. Lifecycle/ownership/generations stay in core.
export interface BleManager {
  readonly destroy: () => Promise<CleanupRecord>
  scan(options?: ScanOptions): Promise<ScanSession>
  connect(peer: BlePeer | string, options?: OperationOptions): Promise<BleConnection>
  // Helpers that preserve deadline/preset normalization
  withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    use: (connection: BleConnection) => Promise<T>
  ): Promise<T>
}

export { PublicBleManager as BleManagerImpl }

export interface ScanOptions extends OperationOptions {
  readonly preset?: StreamPreset
  readonly filter?: InternalScanOptions<string, string>['filter']
}

// Internal factory used by host entrypoints. Hosts derive identity and call this.
export async function createPublicBleManager(
  internal: InternalBleManager<string, any>,
  now: () => number
): Promise<BleManager> {
  return new PublicBleManager(internal, now)
}

class PublicBleManager implements BleManager {
  constructor(
    private readonly internal: InternalBleManager<string, any>,
    private readonly now: () => number
  ) {}

  async scan(options: ScanOptions = {}): Promise<ScanSession> {
    const { signal, deadline } = normalizeOperationOptions(options, this.now)
    const preset = options.preset ?? 'balanced'
    const delivery = resolveStreamPreset({ preset })
    const filter = options.filter ?? { serviceUuids: [], manufacturerData: [], localNamePrefix: null }
    // Delegate to internal manager's scan with normalized deadline/signal/delivery.
    // Internal ScanOptions requires full delivery and deadline/signal; we adapt.
    const internalOptions: InternalScanOptions<string, string> = {
      filter,
      duplicatePolicy: 'merged',
      timestampPolicy: 'source-then-receipt',
      delivery: {
        itemCapacity: delivery.itemCapacity as any,
        byteCapacity: delivery.byteCapacity as any,
        reservedControlCapacity: delivery.reservedControlCapacity as any,
        overflowPolicy: delivery.overflowPolicy
      },
      deadline: deadline as any,
      signal: signal as any,
      sharing: { mode: 'owner', allowSharing: false }
    }
    const session = await (this.internal as any).scan(internalOptions)
    return {
      stop: () => session.stop(),
      observations: session.observations as any
    }
  }

  async connect(peer: BlePeer | string, options: OperationOptions = {}): Promise<BleConnection> {
    const { signal, deadline } = normalizeOperationOptions(options, this.now)
    const peerIdString = typeof peer === 'string' ? peer : peer.id
    // Convert plain string to branded PeerId for internal dispatch.
    const peerId = opaqueId(peerIdString, 'peer', 'public-ble-manager') as PeerId<string>
    const internalConnection = await (this.internal as any).connect(peerId, {
      signal,
      deadline
    })
    const publicPeer: BlePeer = typeof peer === 'string' ? { id: peerIdString, name: null, rssi: null } : peer
    return {
      peer: publicPeer,
      disconnect: () => internalConnection.disconnect(),
      release: () => internalConnection.release()
    }
  }

  async withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    use: (connection: BleConnection) => Promise<T>
  ): Promise<T> {
    const connection = await this.connect(peer, options)
    try {
      return await use(connection)
    } finally {
      await connection.release()
    }
  }

  destroy(): Promise<CleanupRecord> {
    return this.internal.destroy()
  }
}

// Re-export for host factories that need the internal type.
export type { BleManagerOptions }
