// example/src/services/BLEService/BLEService.ts

import {
  canonicalUuid,
} from 'unified-ble-manager/advanced'
import {
  capacity,
  deadline,
  type AdvertisementObservation,
  type Connection,
  type DiscoveredGattDatabase,
  type PeerId,
  type ScanSession,
  type Subscription,
  type Uuid,
} from 'unified-ble-manager/advanced'
import { BackendContractError } from 'unified-ble-manager/backend-sdk'
import { createReactNativeBleManager } from 'unified-ble-manager/react-native'
import { characteristicSelector, resolveCharacteristicPath } from 'unified-ble-manager/profiles/commands'
import {
  BATTERY_LEVEL_CHARACTERISTIC,
  BATTERY_SERVICE,
  parseBatteryLevel
} from 'unified-ble-manager/profiles/battery-service'
import {
  decodeDeviceInformationString,
  DEVICE_INFORMATION_SERVICE,
  FIRMWARE_REVISION_CHARACTERISTIC,
  HARDWARE_REVISION_CHARACTERISTIC,
  MANUFACTURER_NAME_CHARACTERISTIC,
  MODEL_NUMBER_CHARACTERISTIC,
  SERIAL_NUMBER_CHARACTERISTIC,
  SOFTWARE_REVISION_CHARACTERISTIC,
  type DeviceInformationStringField
} from 'unified-ble-manager/profiles/device-information'

type CanonicalManager = Awaited<ReturnType<typeof createReactNativeBleManager>>
type CanonicalConnection = Connection<string, CanonicalManager['identity']>
type CanonicalDatabase = DiscoveredGattDatabase<string, CanonicalManager['identity']>
type CanonicalSubscription = Subscription<string, CanonicalManager['identity']>

export interface ExamplePeer {
  readonly peerId: PeerId<string>
  readonly label: string | null
  readonly rssi: number | null
  readonly isConnectable: boolean | null
  readonly seenAt: number
  /** Full canonical diagnostic record retained for scan troubleshooting. */
  readonly advertisement: AdvertisementObservation<string>
}

export type ProfileRead<Value> = Value | { readonly skipped: true; readonly reason: string } | null

export interface ExampleCommonProfiles {
  readonly battery: ProfileRead<number>
  readonly deviceInformation: Readonly<Partial<Record<DeviceInformationStringField, ProfileRead<string>>>>
}

interface DeviceInformationCharacteristic {
  readonly field: DeviceInformationStringField
  readonly characteristicUuid: Uuid
}

const DEVICE_INFORMATION_CHARACTERISTICS: readonly DeviceInformationCharacteristic[] = [
  { field: 'manufacturer-name', characteristicUuid: MANUFACTURER_NAME_CHARACTERISTIC },
  { field: 'model-number', characteristicUuid: MODEL_NUMBER_CHARACTERISTIC },
  { field: 'serial-number', characteristicUuid: SERIAL_NUMBER_CHARACTERISTIC },
  { field: 'hardware-revision', characteristicUuid: HARDWARE_REVISION_CHARACTERISTIC },
  { field: 'firmware-revision', characteristicUuid: FIRMWARE_REVISION_CHARACTERISTIC },
  { field: 'software-revision', characteristicUuid: SOFTWARE_REVISION_CHARACTERISTIC }
]

let nextExampleManagerId = 1
const BARE_APPLICATION_BLE_CLIENT_ID = 'bare-example-ble-client'
const BARE_APPLICATION_HOST_SESSION_SCOPE = 'com.sfourdrinier.bleplxexample'

/** The bare app owns exactly one canonical 4.0 manager and no legacy compatibility facade. */
class CanonicalBleExampleService {
  private manager: CanonicalManager | null = null
  private managerCreation: Promise<CanonicalManager> | null = null
  private destroying = false
  private ownerGeneration = 0
  private scan: ScanSession<string> | null = null
  private scanAbort: AbortController | null = null
  private connection: CanonicalConnection | null = null
  private database: CanonicalDatabase | null = null
  private notification: CanonicalSubscription | null = null

  async adapterState() {
    return (await this.ensureManager()).adapterState()
  }

  async scanForPeers(serviceUuids: readonly string[], onPeer: (peer: ExamplePeer) => void): Promise<void> {
    await this.stopScan()
    const manager = await this.ensureManager()
    const abort = new AbortController()
    this.scanAbort = abort
    const scan = await manager.scan({
      filter: {
        serviceUuids: serviceUuids.map(canonicalUuid),
        manufacturerData: [],
        localNamePrefix: null
      },
      duplicatePolicy: 'merged',
      timestampPolicy: 'receipt-monotonic',
      delivery: {
        itemCapacity: capacity(32),
        byteCapacity: capacity(64 * 1024),
        reservedControlCapacity: capacity(2),
        overflowPolicy: 'drop-oldest'
      },
      signal: abort.signal,
      deadline: null,
      sharing: { mode: 'owner', allowSharing: false }
    })
    this.scan = scan
    void this.consumeScan(scan, onPeer)
  }

  async stopScan(): Promise<void> {
    const abort = this.scanAbort
    this.scanAbort = null
    abort?.abort()
    const scan = this.scan
    if (scan === null) {
      return
    }
    assertReleased(await scan.stop(), 'scan stop')
    if (this.scan === scan) {
      this.scan = null
    }
  }

  async connect(peer: ExamplePeer): Promise<void> {
    await this.stopScan()
    if (this.connection !== null) {
      await this.disconnect()
    }
    const connection = await (await this.ensureManager()).connect(peer.peerId, this.operation())
    try {
      const database = await connection.discover(this.operation())
      this.connection = connection
      this.database = database
    } catch (error) {
      this.connection = connection
      this.database = null
      try {
        await this.disconnect()
      } catch (cleanupError) {
        console.error('[CanonicalBleExampleService.connect] Discovery cleanup failed:', cleanupError)
      }
      throw error
    }
  }

  async disconnect(): Promise<void> {
    await this.stopNotification()
    const connection = this.connection
    if (connection === null) {
      return
    }
    assertReleased(await connection.disconnect(), 'connection disconnect')
    this.connection = null
    this.database = null
  }

  async snapshot() {
    return this.requireDatabase().snapshot()
  }

  /**
   * Reads profile values through occurrence-safe canonical paths. Missing or
   * unreadable optional profile values are surfaced as explicit results.
   */
  async readCommonProfiles(): Promise<ExampleCommonProfiles> {
    return {
      battery: await this.readProfileValue(BATTERY_SERVICE, BATTERY_LEVEL_CHARACTERISTIC, 'Battery Level', parseBatteryLevel),
      deviceInformation: await this.readDeviceInformation()
    }
  }

  async readCharacteristic(serviceUuid: string, characteristicUuid: string): Promise<Uint8Array> {
    const database = this.requireDatabase()
    return database.read(await this.characteristicPath(serviceUuid, characteristicUuid), this.operation())
  }

  async writeCharacteristic(
    serviceUuid: string,
    characteristicUuid: string,
    bytes: Uint8Array,
    mode: 'with-response' | 'without-response'
  ): Promise<void> {
    const database = this.requireDatabase()
    await database.write(await this.characteristicPath(serviceUuid, characteristicUuid), bytes, {
      ...this.operation(),
      mode
    })
  }

  async readRssi(): Promise<number> {
    return this.requireConnection()
      .readRssi(this.operation())
      .then(measurement => measurement.rssi)
  }

  async requestMtu(requestedMtu: number): Promise<number> {
    return this.requireConnection()
      .requestMtu(requestedMtu, this.operation())
      .then(result => result.negotiatedMtu)
  }

  async subscribeCharacteristic(
    serviceUuid: string,
    characteristicUuid: string,
    onValue: (value: Uint8Array) => void
  ): Promise<void> {
    await this.stopNotification()
    const database = this.requireDatabase()
    const subscription = await database.subscribe(await this.characteristicPath(serviceUuid, characteristicUuid), {
      ...this.operation(),
      delivery: {
        itemCapacity: capacity(16),
        byteCapacity: capacity(32 * 1024),
        reservedControlCapacity: capacity(2),
        overflowPolicy: 'drop-oldest'
      }
    })
    this.notification = subscription
    void this.consumeNotification(subscription, onValue)
  }

  async stopNotification(): Promise<void> {
    const subscription = this.notification
    if (subscription === null) {
      return
    }
    assertReleased(await subscription.remove(), 'notification removal')
    if (this.notification === subscription) {
      this.notification = null
    }
  }

  async destroy(): Promise<void> {
    const generation = this.ownerGeneration + 1
    this.ownerGeneration = generation
    this.destroying = true
    const failures: unknown[] = []
    const pendingCreation = this.managerCreation
    try {
      await this.stopScan()
    } catch (error) {
      failures.push(error)
    }
    try {
      await this.stopNotification()
    } catch (error) {
      failures.push(error)
    }
    const connection = this.connection
    this.connection = null
    this.database = null
    if (connection !== null) {
      try {
        assertReleased(await connection.disconnect(), 'connection disconnect')
      } catch (error) {
        failures.push(error)
      }
    }
    let manager = this.manager
    if (pendingCreation !== null) {
      try {
        manager = await pendingCreation
      } catch (error) {
        failures.push(error)
      }
    }
    if (manager !== null) {
      try {
        assertReleased(await manager.destroy(), 'manager destruction')
      } catch (error) {
        failures.push(error)
      }
    }
    this.manager = null
    this.managerCreation = null
    if (this.ownerGeneration === generation) {
      this.destroying = false
    }
    if (failures.length === 1) {
      throw failures[0]
    }
    if (failures.length > 1) {
      throw new AggregateError(failures, 'CanonicalBleExampleService cleanup failed')
    }
  }

  private operation() {
    const abort = new AbortController()
    const now = this.manager === null ? monotonicNow() : this.manager.monotonicNow()
    return { signal: abort.signal, deadline: deadline(now + 15_000) }
  }

  private async ensureManager(): Promise<CanonicalManager> {
    if (this.destroying) {
      throw new Error('CanonicalBleExampleService is destroying.')
    }
    if (this.manager !== null) {
      return this.manager
    }
    if (this.managerCreation !== null) {
      return this.managerCreation
    }
    const generation = this.ownerGeneration
    const creation = this.createOwnedManager(generation)
    this.managerCreation = creation
    try {
      return await creation
    } finally {
      if (this.managerCreation === creation) {
        this.managerCreation = null
      }
    }
  }

  private async createOwnedManager(generation: number): Promise<CanonicalManager> {
    const managerId = nextExampleManagerId
    nextExampleManagerId += 1
    const manager = await createReactNativeBleManager({
      clientId: BARE_APPLICATION_BLE_CLIENT_ID,
      managerId: `bare-example-manager-${managerId.toString()}`,
      hostSessionScope: BARE_APPLICATION_HOST_SESSION_SCOPE
    })
    if (this.destroying || this.ownerGeneration !== generation) {
      await manager.destroy()
      throw new Error('CanonicalBleExampleService is destroying.')
    }
    this.manager = manager
    return manager
  }

  private requireConnection(): CanonicalConnection {
    if (this.connection === null) {
      throw new Error('Connect to a peer before requesting connection controls.')
    }
    return this.connection
  }

  private requireDatabase(): CanonicalDatabase {
    if (this.database === null) {
      throw new Error('Discover the connected peer before accessing GATT.')
    }
    return this.database
  }

  private async characteristicPath(serviceUuid: string, characteristicUuid: string) {
    return resolveCharacteristicPath(
      await this.requireDatabase().snapshot(),
      characteristicSelector(canonicalUuid(serviceUuid), canonicalUuid(characteristicUuid))
    )
  }

  private async readProfileValue<Value>(
    serviceUuid: Uuid,
    characteristicUuid: Uuid,
    label: string,
    decode: (bytes: Readonly<Uint8Array>) => Value
  ): Promise<ProfileRead<Value>> {
    try {
      return decode(await this.requireDatabase().read(await this.characteristicPath(serviceUuid, characteristicUuid), this.operation()))
    } catch (error) {
      if (isOptionalFeatureAbsence(error)) {
        return { skipped: true, reason: error.normalized.code }
      }
      console.error(`[CanonicalBleExampleService.readProfileValue] ${label} read failed:`, error)
      throw error
    }
  }

  private async readDeviceInformation(): Promise<Readonly<Partial<Record<DeviceInformationStringField, ProfileRead<string>>>>> {
    const values: Partial<Record<DeviceInformationStringField, ProfileRead<string>>> = {}
    for (const characteristic of DEVICE_INFORMATION_CHARACTERISTICS) {
      values[characteristic.field] = await this.readProfileValue(
        DEVICE_INFORMATION_SERVICE,
        characteristic.characteristicUuid,
        characteristic.field,
        decodeDeviceInformationString
      )
    }
    return values
  }

  private async consumeScan(scan: ScanSession<string>, onPeer: (peer: ExamplePeer) => void): Promise<void> {
    try {
      for await (const item of scan.observations) {
        if (item.kind === 'terminal') {
          console.error('[CanonicalBleExampleService.consumeScan] Scan terminal:', item.reason)
          return
        }
        if (item.kind === 'overflow') {
          console.error('[CanonicalBleExampleService.consumeScan] Scan overflow:', item)
          continue
        }
        if (item.kind === 'value') {
          onPeer(peerFromObservation(item.value))
        }
      }
    } catch (error) {
      console.error('[CanonicalBleExampleService.consumeScan] Scan observation failed:', error)
    } finally {
      try {
        assertReleased(await scan.stop(), 'scan observer cleanup')
        if (this.scan === scan) {
          this.scan = null
        }
      } catch (cleanupError) {
        console.error('[CanonicalBleExampleService.consumeScan] Scan observer cleanup failed:', cleanupError)
      }
    }
  }

  private async consumeNotification(
    subscription: CanonicalSubscription,
    onValue: (value: Uint8Array) => void
  ): Promise<void> {
    try {
      for await (const item of subscription.values) {
        if (item.kind === 'terminal') {
          console.error('[CanonicalBleExampleService.consumeNotification] Notification terminal:', item.reason)
          return
        }
        if (item.kind === 'overflow') {
          console.error('[CanonicalBleExampleService.consumeNotification] Notification overflow:', item)
          continue
        }
        if (item.kind === 'value') {
          onValue(item.value.value)
        }
      }
    } catch (error) {
      console.error('[CanonicalBleExampleService.consumeNotification] Notification stream failed:', error)
    } finally {
      try {
        assertReleased(await subscription.remove(), 'notification observer cleanup')
        if (this.notification === subscription) {
          this.notification = null
        }
      } catch (cleanupError) {
        console.error('[CanonicalBleExampleService.consumeNotification] Notification cleanup failed:', cleanupError)
      }
    }
  }
}

function monotonicNow(): number {
  if (globalThis.performance === undefined) {
    throw new Error('React Native did not provide a monotonic performance clock.')
  }
  return globalThis.performance.now()
}

function peerFromObservation(observation: AdvertisementObservation<string>): ExamplePeer {
  return Object.freeze({
    peerId: observation.device.id,
    label: observation.localName.state === 'present' ? observation.localName.value : null,
    rssi: observation.rssi.state === 'present' ? observation.rssi.value : null,
    isConnectable: observation.connectable.state === 'present' ? observation.connectable.value : null,
    seenAt: observation.receivedAtMonotonicMs,
    advertisement: observation
  })
}

function assertReleased(cleanup: { readonly state: 'released' | 'release-failed' }, operationName: string): void {
  if (cleanup.state !== 'released') {
    throw new Error(`${operationName} reported cleanup failures; retry the operation before continuing.`)
  }
}

function isOptionalFeatureAbsence(error: unknown): error is BackendContractError {
  return (
    error instanceof BackendContractError &&
    (error.normalized.code === 'gatt.not-found' || error.normalized.code === 'gatt.property-not-supported')
  )
}

export const BLEService = new CanonicalBleExampleService()
