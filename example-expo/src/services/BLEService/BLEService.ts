// example-expo/src/services/BLEService/BLEService.ts

import {
  BleError,
  type BleConnection,
  type GattDatabase,
  type GattSubscription,
  type PublicScanObservation,
  type ScanSession
} from 'unified-ble-manager'
import { createExpoBleManager } from 'unified-ble-manager/expo'
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

type CanonicalManager = Awaited<ReturnType<typeof createExpoBleManager>>
type CanonicalConnection = BleConnection
type CanonicalDatabase = GattDatabase
type CanonicalSubscription = GattSubscription

export interface ExamplePeer {
  readonly peerId: string
  readonly label: string | null
  readonly rssi: number | null
  readonly isConnectable: boolean | null
  readonly seenAt: number
  /** Full canonical diagnostic record retained for scan troubleshooting. */
  readonly advertisement: PublicScanObservation
}

export type ProfileRead<Value> = Value | { readonly skipped: true; readonly reason: string } | null

export interface ExampleCommonProfiles {
  readonly battery: ProfileRead<number>
  readonly deviceInformation: Readonly<Partial<Record<DeviceInformationStringField, ProfileRead<string>>>>
}

interface DeviceInformationCharacteristic {
  readonly field: DeviceInformationStringField
  readonly characteristicUuid: string
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

/** The Expo app owns exactly one canonical 4.0 manager and no legacy compatibility facade. */
class CanonicalBleExampleService {
  private manager: CanonicalManager | null = null
  private managerCreation: Promise<CanonicalManager> | null = null
  private destroying = false
  private ownerGeneration = 0
  private scan: ScanSession | null = null
  private scanAbort: AbortController | null = null
  private connection: CanonicalConnection | null = null
  private database: CanonicalDatabase | null = null
  private notification: CanonicalSubscription | null = null

  async adapterState() {
    return (await this.ensureManager()).adapter.state()
  }

  async readiness() {
    return (await this.ensureManager()).readiness()
  }

  async claimRestoration() {
    return (await this.ensureManager()).restoration.claim()
  }

  diagnosticsSnapshot() {
    return this.manager?.diagnostics.snapshot() ?? null
  }

  scanPlan() {
    return this.scan?.plan ?? null
  }

  async redactedSupportBundle() {
    const manager = await this.ensureManager()
    const readiness = await manager.readiness()
    const diagnostics = manager.diagnostics.snapshot()
    const plan = this.scan?.plan
    return Object.freeze({
      schema: 'unified-ble-expo-support-v1',
      readiness: {
        state: readiness.state,
        actions: readiness.actions.map(action => action.kind)
      },
      resources: diagnostics.resourceCounters,
      scan:
        plan === null || plan === undefined
          ? null
          : {
              queryDigest: plan.queryDigest,
              nativeGuarantee: plan.nativeGuarantee,
            nativePredicateCount: plan.native.predicates.length,
            residualPredicateCount: plan.residual.predicates.length,
              unavailablePredicateCount: plan.unavailable.length
            },
      host: {
        restoration: 'native-authoritative',
        background: 'explicit-connected-device-lease',
        association: 'android-system-ui-only'
      }
    })
  }

  async scanForPeers(serviceUuids: readonly string[], onPeer: (peer: ExamplePeer) => void): Promise<void> {
    await this.stopScan()
    const manager = await this.ensureManager()
    const abort = new AbortController()
    this.scanAbort = abort
    const scan = await manager.scan({
      query: serviceUuids.length === 0 ? undefined : { anyOf: [{ services: { any: serviceUuids } }] },
      duplicates: 'all',
      delivery: 'balanced',
      signal: abort.signal
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
      battery: await this.readProfileValue(
        BATTERY_SERVICE,
        BATTERY_LEVEL_CHARACTERISTIC,
        'Battery Level',
        parseBatteryLevel
      ),
      deviceInformation: await this.readDeviceInformation()
    }
  }

  async readCharacteristic(serviceUuid: string, characteristicUuid: string): Promise<Uint8Array> {
    const database = this.requireDatabase()
    return database.characteristic(serviceUuid, characteristicUuid).read(this.operation())
  }

  async writeCharacteristic(
    serviceUuid: string,
    characteristicUuid: string,
    bytes: Uint8Array,
    mode: 'with-response' | 'without-response'
  ): Promise<void> {
    const database = this.requireDatabase()
    await database.characteristic(serviceUuid, characteristicUuid).write(bytes, {
      ...this.operation(),
      response: mode === 'with-response' ? 'required' : 'not-required'
    })
  }

  async readRssi(): Promise<number> {
    const observation = await this.requireConnection().controls.readRssi(this.operation())
    if (observation.rssi === null) {
      throw new Error('The connected backend did not provide an RSSI measurement.')
    }
    return observation.rssi
  }

  async requestMtu(requestedMtu: number): Promise<number> {
    const negotiation = await this.requireConnection().controls.requestMtu(requestedMtu, this.operation())
    const attMtu = negotiation.observation?.attMtu
    if (attMtu === null || attMtu === undefined) {
      throw new Error('The connected backend did not provide an effective ATT MTU observation.')
    }
    return attMtu
  }

  async subscribeCharacteristic(
    serviceUuid: string,
    characteristicUuid: string,
    onValue: (value: Uint8Array) => void
  ): Promise<void> {
    await this.stopNotification()
    const database = this.requireDatabase()
    const subscription = await database.characteristic(serviceUuid, characteristicUuid).subscribe({
      ...this.operation(),
      stream: 'balanced'
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
    return { signal: abort.signal, timeoutMs: 15_000 }
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
    const manager = await createExpoBleManager({ instanceId: `expo-example-${managerId.toString()}` })
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

  private async readProfileValue<Value>(
    serviceUuid: string,
    characteristicUuid: string,
    label: string,
    decode: (bytes: Readonly<Uint8Array>) => Value
  ): Promise<ProfileRead<Value>> {
    try {
      return decode(await this.requireDatabase().characteristic(serviceUuid, characteristicUuid).read(this.operation()))
    } catch (error) {
      if (isOptionalFeatureAbsence(error)) {
        return { skipped: true, reason: error.code }
      }
      console.error(`[CanonicalBleExampleService.readProfileValue] ${label} read failed:`, error)
      throw error
    }
  }

  private async readDeviceInformation(): Promise<
    Readonly<Partial<Record<DeviceInformationStringField, ProfileRead<string>>>>
  > {
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

  private async consumeScan(scan: ScanSession, onPeer: (peer: ExamplePeer) => void): Promise<void> {
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

function peerFromObservation(observation: PublicScanObservation): ExamplePeer {
  return Object.freeze({
    peerId: observation.peer.id,
    label: observation.localName,
    rssi: observation.rssi,
    isConnectable: observation.connectable,
    seenAt: observation.observedAtMonotonicMs ?? 0,
    advertisement: observation
  })
}

function assertReleased(cleanup: { readonly state: 'released' | 'release-failed' }, operationName: string): void {
  if (cleanup.state !== 'released') {
    throw new Error(`${operationName} reported cleanup failures; retry the operation before continuing.`)
  }
}

function isOptionalFeatureAbsence(error: unknown): error is BleError {
  return error instanceof BleError && (error.code === 'gatt.not-found' || error.code === 'gatt.property-not-supported')
}

export const BLEService = new CanonicalBleExampleService()
