// src/public/ble-manager.ts — non-generic application façade (PR1 skeleton)

import type { AdvertisementObservation } from '../backend-contract/advertisement'
import type { ScanOptions as InternalScanOptions } from '../backend-contract/advertisement'
import type { CleanupRecord } from '../backend-contract/errors'
import type { BackendIdentity } from '../backend-contract/identity'
import { opaqueId } from '../backend-contract/primitives'
import type { BleManager as InternalBleManager } from '../manager/ble-manager'
import type { BleManagerOptions } from '../manager/ble-manager'
import type { BoundedAsyncStream } from '../backend-contract/streams'
import { normalizeOperationOptions } from './operation-options'
import type { OperationOptions } from './operation-options'
import { resolveStreamPreset } from './stream-presets'
import type { StreamPreset } from './stream-presets'
import type { IpcAdvertisement } from '../ipc/manager'
import { rehydratePublicError, rehydratePublicPromise, runWithCleanup } from './error-bridge'
import { PublicBleCapabilities } from './capabilities'
import type { BleCapabilities } from './capabilities'
import { createPublicGattDatabase } from './gatt'
import type { GattDatabase, GattValueEvent } from './gatt'

export type GattSubscriptionValue = GattValueEvent
export type {
  GattDatabase,
  GattDatabaseSnapshot,
  GattService,
  GattCharacteristic,
  GattDescriptor,
  GattSubscription,
  GattValueEvent,
  GattValueStream,
  GattDatabaseChangedEvent,
  GattWriteReceipt,
  GattLongWriteReceipt,
  GattCharacteristicProperties,
  GattAccessRequirements,
  GattServiceReference,
  GattWriteOptions,
  LongWriteOptions,
  DescriptorWriteOptions,
  GattSubscribeOptions,
  OccurrenceSelector,
  GattPathSelector,
  UuidInput
} from './gatt'

// Public peer — opaque backend-scoped identifier, no generic.
export interface BlePeer {
  readonly id: string
  readonly name: string | null
  readonly rssi: number | null
}

export function snapshotBlePeer(peer: BlePeer): BlePeer {
  return Object.freeze({ id: peer.id, name: peer.name, rssi: peer.rssi })
}

// Public connection — generation-bound lease, no generic.
export interface BleConnection {
  readonly peer: BlePeer
  readonly discover: (options?: OperationOptions) => Promise<GattDatabase>
  readonly disconnect: () => Promise<CleanupRecord>
  readonly release: () => Promise<CleanupRecord>
}

// Public scan session — bounded stream, no generic.
// Union embraces both native AdvertisementObservation and Tauri IpcAdvertisement
// until PR4 scan semantics unify; covariance lets each backend stream satisfy the union without casts.
export type PublicScanObservation = AdvertisementObservation<string> | IpcAdvertisement

export interface ScanSession {
  readonly stop: () => Promise<CleanupRecord>
  readonly observations: BoundedAsyncStream<PublicScanObservation>
}

// Non-generic public manager. Lifecycle/ownership/generations stay in core.
export interface BleManager {
  readonly capabilities: BleCapabilities
  readonly destroy: () => Promise<CleanupRecord>
  scan(options?: ScanOptions): Promise<ScanSession>
  connect(peer: BlePeer | string, options?: OperationOptions): Promise<BleConnection>
  withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    action: (connection: BleConnection) => Promise<T>
  ): Promise<T>
}

export { PublicBleManager as BleManagerImpl }

export interface ScanOptions extends OperationOptions {
  readonly preset?: StreamPreset
  readonly filter?: InternalScanOptions<string, string>['filter']
}

// Internal factory used by host entrypoints. Hosts derive identity and call this.
export async function createPublicBleManager(
  internal: InternalBleManager<string, BackendIdentity<string>>,
  now: () => number
): Promise<BleManager> {
  return new PublicBleManager(internal, now)
}

class PublicBleManager implements BleManager {
  readonly capabilities: BleCapabilities

  constructor(
    private readonly internal: InternalBleManager<string, BackendIdentity<string>>,
    private readonly now: () => number
  ) {
    this.capabilities = new PublicBleCapabilities(internal)
  }

  async scan(options: ScanOptions = {}): Promise<ScanSession> {
    try {
      const { signal, deadline } = normalizeOperationOptions(options, this.now)
      const preset = options.preset ?? 'balanced'
      const delivery = resolveStreamPreset({ preset })
      const filter = options.filter ?? { serviceUuids: [], manufacturerData: [], localNamePrefix: null }
      const internalOptions: InternalScanOptions<string, string> = {
        filter,
        duplicatePolicy: 'merged',
        timestampPolicy: 'source-then-receipt',
        delivery: {
          itemCapacity: delivery.itemCapacity,
          byteCapacity: delivery.byteCapacity,
          reservedControlCapacity: delivery.reservedControlCapacity,
          overflowPolicy: delivery.overflowPolicy
        },
        deadline,
        signal,
        sharing: { mode: 'owner', allowSharing: false }
      }
      const session = await this.internal.scan(internalOptions)
      return {
        stop: () => rehydratePublicPromise(session.stop()),
        observations: session.observations
      }
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async connect(peer: BlePeer | string, options: OperationOptions = {}): Promise<BleConnection> {
    try {
      const { signal, deadline } = normalizeOperationOptions(options, this.now)
      const peerIdString = typeof peer === 'string' ? peer : peer.id
      const peerId = opaqueId<'peer', string>(peerIdString, 'peer', 'public-ble-manager')
      const internalConnection = await this.internal.connect(peerId, {
        signal,
        deadline
      })
      const publicPeer =
        typeof peer === 'string' ? snapshotBlePeer({ id: peerIdString, name: null, rssi: null }) : snapshotBlePeer(peer)
      return {
        peer: publicPeer,
        discover: async (discoverOptions: OperationOptions = {}) => {
          try {
            const normalized = normalizeOperationOptions(discoverOptions, this.now)
            const source = await internalConnection.discover({
              signal: normalized.signal,
              deadline: normalized.deadline
            })
            return createPublicGattDatabase(source)
          } catch (error) {
            throw rehydratePublicError(error)
          }
        },
        disconnect: () => rehydratePublicPromise(internalConnection.disconnect()),
        release: () => rehydratePublicPromise(internalConnection.release())
      }
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }

  async withConnection<T>(
    peer: BlePeer | string,
    options: OperationOptions,
    action: (connection: BleConnection) => Promise<T>
  ): Promise<T> {
    const connection = await this.connect(peer, options)
    return runWithCleanup(
      () => action(connection),
      () => connection.release()
    )
  }

  destroy(): Promise<CleanupRecord> {
    return this.internal.destroy().catch(error => {
      throw rehydratePublicError(error)
    })
  }
}

// Re-export for host factories that need the internal type.
export type { BleManagerOptions }
