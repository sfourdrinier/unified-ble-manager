import { canonicalUuid, resourceCount } from '../backend-contract/primitives'
import type { StreamItem } from '../backend-contract/streams'
import { contractError } from '../backend-contract/errors'
import type {
  GattAccessRequirements,
  GattCharacteristicPropertyAvailability,
  GattDescriptorProperties
} from '../backend-contract/gatt'
import type {
  DiscoveredGattDatabaseHandle,
  PortableCurrentCharacteristicPath,
  PortableCurrentDescriptorPath,
  PortableGattDatabaseSnapshot
} from '../manager/consumer-handles'
import { normalizeOperationOptions, type OperationOptions } from './operation-options'
import { resolveStreamPreset, type StreamPreset } from './stream-presets'
import { rehydratePublicError } from './error-bridge'

export type { GattAccessRequirements, GattCharacteristicPropertyAvailability } from '../backend-contract/gatt'

export type UuidInput = string | number

export interface OccurrenceSelector {
  readonly occurrence?: number
}

export interface GattPathSelector {
  readonly serviceOccurrence?: number
  readonly characteristicOccurrence?: number
}

export interface GattDatabaseChangedEvent {
  readonly previousGeneration: string
  readonly reason: 'service-changed' | 'reconnect' | 'backend-reset' | 'manual-rediscovery'
  readonly affectedHandleRange: { readonly start: number; readonly end: number } | null
}

export interface GattServiceReference {
  readonly uuid: string
  readonly occurrence: number
}

export interface GattCharacteristicProperties {
  readonly broadcast: boolean
  readonly read: boolean
  readonly writeWithResponse: boolean
  readonly writeWithoutResponse: boolean
  readonly authenticatedSignedWrites: boolean
  readonly notify: boolean
  readonly indicate: boolean
  readonly extendedProperties: boolean
  readonly reliableWrite: boolean
  readonly writableAuxiliaries: boolean
  readonly availability: GattCharacteristicPropertyAvailability
}

export interface GattWriteOptions extends OperationOptions {
  readonly response?: 'required' | 'not-required' | 'automatic'
}

export interface LongWriteOptions extends GattWriteOptions {
  readonly chunkSize?: number
}

export interface DescriptorWriteOptions extends OperationOptions {
  readonly response?: 'required' | 'automatic'
}

export interface GattSubscribeOptions extends OperationOptions {
  readonly delivery?: 'prefer-notification' | 'prefer-indication' | 'require-notification' | 'require-indication'
  readonly stream?: StreamPreset
}

export type GattDelivery = 'notification' | 'indication' | 'unknown'

export interface GattValueEvent {
  readonly value: Uint8Array
  readonly delivery: GattDelivery
  readonly observedAtMonotonicMs: number
  readonly sequence: number
}

export interface GattValueStream extends AsyncIterable<StreamItem<GattValueEvent>> {
  readonly limits: {
    readonly itemCapacity: number
    readonly byteCapacity: number
    readonly reservedControlCapacity: number
  }
  readonly overflowPolicy: 'latest' | 'drop-oldest' | 'drop-newest' | 'error'
  close(): Promise<import('../manager/consumer-handles').PortableCleanupRecord>
}

export interface GattDatabaseSnapshot {
  readonly generation: string
  readonly services: readonly GattService[]
  readonly characteristics: readonly GattCharacteristic[]
  readonly descriptors: readonly GattDescriptor[]
}

export interface GattDatabase {
  readonly generation: string
  readonly services: readonly GattService[]
  readonly changed: AsyncIterable<StreamItem<GattDatabaseChangedEvent>>
  service(uuid: UuidInput, selector?: OccurrenceSelector): GattService
  servicesByUuid(uuid: UuidInput): readonly GattService[]
  characteristic(serviceUuid: UuidInput, characteristicUuid: UuidInput, selector?: GattPathSelector): GattCharacteristic
  snapshot(): GattDatabaseSnapshot
}

export interface GattService {
  readonly uuid: string
  readonly occurrence: number
  readonly primary: boolean
  readonly includedServices: readonly GattServiceReference[]
  readonly characteristics: readonly GattCharacteristic[]
  characteristic(uuid: UuidInput, selector?: OccurrenceSelector): GattCharacteristic
  characteristicsByUuid(uuid: UuidInput): readonly GattCharacteristic[]
}

export interface GattCharacteristic {
  readonly uuid: string
  readonly occurrence: number
  readonly service: GattService
  readonly properties: GattCharacteristicProperties
  readonly access: GattAccessRequirements
  readonly descriptors: readonly GattDescriptor[]
  read(options?: OperationOptions): Promise<Uint8Array>
  write(value: Uint8Array, options?: GattWriteOptions): Promise<GattWriteReceipt>
  writeLong(value: Uint8Array, options?: LongWriteOptions): Promise<GattLongWriteReceipt>
  subscribe(options?: GattSubscribeOptions): Promise<GattSubscription>
  descriptor(uuid: UuidInput, selector?: OccurrenceSelector): GattDescriptor
}

export interface GattDescriptor {
  readonly uuid: string
  readonly occurrence: number
  readonly characteristic: GattCharacteristic
  readonly properties: GattDescriptorProperties
  read(options?: OperationOptions): Promise<Uint8Array>
  write(value: Uint8Array, options?: DescriptorWriteOptions): Promise<GattWriteReceipt>
}

export type GattWriteReceipt = import('../manager/consumer-handles').PortableWriteReceipt
export type GattLongWriteReceipt = import('../manager/consumer-handles').PortableLongWriteReceipt

export interface GattSubscription {
  readonly requestedDelivery: GattSubscribeOptions['delivery']
  readonly effectiveDelivery: GattDelivery
  readonly values: GattValueStream
  remove(): Promise<import('../manager/consumer-handles').PortableCleanupRecord>
}

export interface PublicGattDatabaseSource extends DiscoveredGattDatabaseHandle {
  readonly changed?: AsyncIterable<StreamItem<GattDatabaseChangedEvent>>
  assertCurrent?(): void
}

export async function createPublicGattDatabase(source: PublicGattDatabaseSource): Promise<GattDatabase> {
  const snapshot = await source.snapshot()
  return new PublicGattDatabase(source, snapshot)
}

class PublicGattDatabase implements GattDatabase {
  readonly generation: string
  readonly services: readonly GattService[]
  readonly changed: AsyncIterable<StreamItem<GattDatabaseChangedEvent>>
  private readonly serviceLookup: ReadonlyMap<string, readonly GattService[]>
  private readonly snapshotValue: GattDatabaseSnapshot

  constructor(
    private readonly source: PublicGattDatabaseSource,
    snapshot: PortableGattDatabaseSnapshot
  ) {
    this.generation = snapshot.path.databaseGeneration
    const serviceRecords = recordsWithOccurrence(snapshot.services, record => record.path.serviceUuid)
    const characteristicRecords = recordsWithOccurrence(
      snapshot.characteristics,
      record => record.path.characteristicUuid
    )
    const descriptorRecords = recordsWithOccurrence(snapshot.descriptors, record => record.path.descriptorUuid)
    const serviceObjects = serviceRecords.map(record => {
      const characteristics = recordsWithOccurrence(
        characteristicRecords
          .filter(characteristic => sameService(characteristic.record.path, record.record.path))
          .map(characteristic => characteristic.record),
        characteristic => characteristic.path.characteristicUuid
      )
      const characteristicObjects = characteristics.map(characteristic => {
        const descriptors = recordsWithOccurrence(
          descriptorRecords
            .filter(descriptor => sameCharacteristic(descriptor.record.path, characteristic.record.path))
            .map(descriptor => descriptor.record),
          descriptor => descriptor.path.descriptorUuid
        )
        return { characteristic, descriptors }
      })
      return new PublicGattService(record, source, characteristicObjects)
    })
    this.services = Object.freeze(serviceObjects)
    this.serviceLookup = createLookup(this.services, service => service.uuid)
    this.snapshotValue = Object.freeze({
      generation: this.generation,
      services: this.services,
      characteristics: Object.freeze(this.services.flatMap(service => service.characteristics)),
      descriptors: Object.freeze(
        this.services.flatMap(service => service.characteristics.flatMap(characteristic => characteristic.descriptors))
      )
    })
    this.changed = source.changed ?? emptyChangedStream()
    Object.freeze(this)
  }

  service(uuid: UuidInput, selector: OccurrenceSelector = {}): GattService {
    return selectOne(this.servicesByUuid(uuid), selector, 'public-gatt.database.service')
  }

  servicesByUuid(uuid: UuidInput): readonly GattService[] {
    return this.serviceLookup.get(normalizeUuid(uuid)) ?? Object.freeze([])
  }

  characteristic(
    serviceUuid: UuidInput,
    characteristicUuid: UuidInput,
    selector: GattPathSelector = {}
  ): GattCharacteristic {
    return this.service(serviceUuid, { occurrence: selector.serviceOccurrence }).characteristic(characteristicUuid, {
      occurrence: selector.characteristicOccurrence
    })
  }

  snapshot(): GattDatabaseSnapshot {
    runPublic(() => this.source.assertCurrent?.())
    return this.snapshotValue
  }
}

class PublicGattService implements GattService {
  readonly uuid: string
  readonly occurrence: number
  readonly primary: boolean
  readonly includedServices: readonly GattServiceReference[]
  readonly characteristics: readonly GattCharacteristic[]
  private readonly characteristicLookup: ReadonlyMap<string, readonly GattCharacteristic[]>

  constructor(
    indexedRecord: OccurrenceRecord<PortableGattDatabaseSnapshot['services'][number]>,
    source: PublicGattDatabaseSource,
    characteristics: readonly {
      readonly characteristic: OccurrenceRecord<PortableGattDatabaseSnapshot['characteristics'][number]>
      readonly descriptors: readonly OccurrenceRecord<PortableGattDatabaseSnapshot['descriptors'][number]>[]
    }[]
  ) {
    this.uuid = normalizeUuid(indexedRecord.record.path.serviceUuid)
    this.occurrence = indexedRecord.occurrence
    this.primary = indexedRecord.record.primary ?? true
    this.includedServices = Object.freeze(
      (indexedRecord.record.includedServices ?? []).map(reference =>
        Object.freeze({ uuid: normalizeUuid(reference.uuid), occurrence: occurrenceNumber(reference.occurrence) })
      )
    )
    this.characteristics = Object.freeze(
      characteristics.map(entry => new PublicGattCharacteristic(this, source, entry.characteristic, entry.descriptors))
    )
    this.characteristicLookup = createLookup(this.characteristics, characteristic => characteristic.uuid)
    Object.freeze(this)
  }

  characteristic(uuid: UuidInput, selector: OccurrenceSelector = {}): GattCharacteristic {
    return selectOne(this.characteristicsByUuid(uuid), selector, 'public-gatt.service.characteristic')
  }

  characteristicsByUuid(uuid: UuidInput): readonly GattCharacteristic[] {
    return this.characteristicLookup.get(normalizeUuid(uuid)) ?? Object.freeze([])
  }
}

class PublicGattCharacteristic implements GattCharacteristic {
  readonly uuid: string
  readonly occurrence: number
  readonly service: GattService
  readonly properties: GattCharacteristicProperties
  readonly access: GattAccessRequirements
  readonly descriptors: readonly GattDescriptor[]
  private readonly descriptorLookup: ReadonlyMap<string, readonly GattDescriptor[]>

  constructor(
    service: PublicGattService,
    private readonly source: PublicGattDatabaseSource,
    private readonly indexedRecord: OccurrenceRecord<PortableGattDatabaseSnapshot['characteristics'][number]>,
    descriptorRecords: readonly OccurrenceRecord<PortableGattDatabaseSnapshot['descriptors'][number]>[]
  ) {
    this.service = service
    this.uuid = normalizeUuid(indexedRecord.record.path.characteristicUuid)
    this.occurrence = indexedRecord.occurrence
    this.properties = normalizeCharacteristicProperties(indexedRecord.record)
    this.access = Object.freeze(indexedRecord.record.access ?? { read: 'unknown', write: 'unknown' })
    this.descriptors = Object.freeze(
      descriptorRecords.map(recordValue => new PublicGattDescriptor(this, source, recordValue))
    )
    this.descriptorLookup = createLookup(this.descriptors, descriptor => descriptor.uuid)
    Object.freeze(this)
  }

  read(options: OperationOptions = {}): Promise<Uint8Array> {
    return this.run(() =>
      this.source.read(
        this.indexedRecord.record.path,
        normalizeOperationOptions(options, () => this.source.monotonicNow())
      )
    )
  }

  write(value: Uint8Array, options: GattWriteOptions = {}): Promise<GattWriteReceipt> {
    return this.run(() =>
      this.source.write(this.indexedRecord.record.path, value, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        mode: resolveWriteMode(this.properties, options.response)
      })
    )
  }

  writeLong(value: Uint8Array, options: LongWriteOptions = {}): Promise<GattLongWriteReceipt> {
    return this.run(() =>
      this.source.writeLong(this.indexedRecord.record.path, value, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        mode: resolveWriteMode(this.properties, options.response),
        chunkSize: options.chunkSize
      })
    )
  }

  async subscribe(options: GattSubscribeOptions = {}): Promise<GattSubscription> {
    return this.run(async () => {
      const effectiveDelivery = resolveDelivery(this.properties, options.delivery)
      const budget = resolveStreamPreset({ preset: options.stream ?? 'balanced' })
      const subscription = await this.source.subscribe(this.indexedRecord.record.path, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        delivery: budget,
        deliveryMode: options.delivery
      })
      return Object.freeze({
        requestedDelivery: options.delivery,
        effectiveDelivery,
        values: mapGattValueStream(subscription.values, () => this.source.monotonicNow()),
        remove: () => rehydrateCleanup(subscription.remove())
      })
    })
  }

  descriptor(uuid: UuidInput, selector: OccurrenceSelector = {}): GattDescriptor {
    return selectOne(this.descriptorsByUuid(uuid), selector, 'public-gatt.characteristic.descriptor')
  }

  private descriptorsByUuid(uuid: UuidInput): readonly GattDescriptor[] {
    return this.descriptorLookup.get(normalizeUuid(uuid)) ?? Object.freeze([])
  }

  private async run<Value>(operation: () => Promise<Value>): Promise<Value> {
    try {
      this.source.assertCurrent?.()
      return await operation()
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }
}

class PublicGattDescriptor implements GattDescriptor {
  readonly uuid: string
  readonly occurrence: number
  readonly characteristic: GattCharacteristic
  readonly properties: GattDescriptorProperties

  constructor(
    characteristic: PublicGattCharacteristic,
    private readonly source: PublicGattDatabaseSource,
    private readonly indexedRecord: OccurrenceRecord<PortableGattDatabaseSnapshot['descriptors'][number]>
  ) {
    this.characteristic = characteristic
    this.uuid = normalizeUuid(indexedRecord.record.path.descriptorUuid)
    this.occurrence = indexedRecord.occurrence
    this.properties = Object.freeze(
      indexedRecord.record.properties ?? {
        read: false,
        write: false,
        availability: { read: 'unknown', write: 'unknown' },
        access: { read: 'unknown', write: 'unknown' }
      }
    )
    Object.freeze(this)
  }

  read(options: OperationOptions = {}): Promise<Uint8Array> {
    return this.run(() =>
      this.source.readDescriptor(
        this.indexedRecord.record.path,
        normalizeOperationOptions(options, () => this.source.monotonicNow())
      )
    )
  }

  write(value: Uint8Array, options: DescriptorWriteOptions = {}): Promise<GattWriteReceipt> {
    return this.run(() => {
      if (this.uuid === normalizeUuid('2902')) {
        throw contractError('gatt.cccd-managed', 'gatt', 'public-gatt.descriptor.write-cccd')
      }
      if (!this.properties.write) {
        throw contractError('gatt.property-not-supported', 'gatt', 'public-gatt.descriptor.write')
      }
      return this.source.writeDescriptor(this.indexedRecord.record.path, value, {
        ...normalizeOperationOptions(options, () => this.source.monotonicNow()),
        mode: 'with-response'
      })
    })
  }

  private async run<Value>(operation: () => Promise<Value>): Promise<Value> {
    try {
      this.source.assertCurrent?.()
      return await operation()
    } catch (error) {
      throw rehydratePublicError(error)
    }
  }
}

function normalizeUuid(value: UuidInput): string {
  try {
    const text = typeof value === 'number' ? value.toString(16) : value
    if (typeof text !== 'string' || text.length === 0) {
      throw new Error('UUID input must be non-empty')
    }
    return String(canonicalUuid(text))
  } catch {
    throw rehydratePublicError(contractError('argument.invalid', 'gatt', 'public-gatt.uuid'))
  }
}

function occurrenceNumber(value: string): number {
  const occurrence = Number(value)
  if (Number.isSafeInteger(occurrence) && occurrence >= 0) return occurrence
  return 0
}

interface OccurrenceRecord<RecordValue> {
  readonly record: RecordValue
  readonly occurrence: number
}

function recordsWithOccurrence<
  RecordValue extends {
    readonly path: {
      readonly serviceUuid?: string
      readonly characteristicUuid?: string
      readonly descriptorUuid?: string
    }
  }
>(
  records: readonly RecordValue[],
  key: (record: RecordValue) => string | undefined
): readonly OccurrenceRecord<RecordValue>[] {
  const counts = new Map<string, number>()
  return records.map(record => {
    const value = key(record) ?? ''
    const count = counts.get(value) ?? 0
    counts.set(value, count + 1)
    return Object.freeze({ record, occurrence: count })
  })
}

function createLookup<Value>(
  values: readonly Value[],
  key: (value: Value) => string
): ReadonlyMap<string, readonly Value[]> {
  const lookup = new Map<string, Value[]>()
  for (const value of values) {
    const entries = lookup.get(key(value)) ?? []
    entries.push(value)
    lookup.set(key(value), entries)
  }
  return new Map([...lookup.entries()].map(([entryKey, entryValues]) => [entryKey, Object.freeze(entryValues)]))
}

function selectOne<Value>(values: readonly Value[], selector: OccurrenceSelector, operation: string): Value {
  if (selector.occurrence !== undefined && (!Number.isSafeInteger(selector.occurrence) || selector.occurrence < 0)) {
    throw rehydratePublicError(contractError('argument.invalid', 'gatt', `${operation}.occurrence`))
  }
  if (values.length === 0 || (selector.occurrence !== undefined && values[selector.occurrence] === undefined)) {
    throw rehydratePublicError(contractError('gatt.not-found', 'gatt', operation))
  }
  if (selector.occurrence !== undefined) {
    const selected = values[selector.occurrence]
    if (selected === undefined) throw rehydratePublicError(contractError('gatt.not-found', 'gatt', operation))
    return selected
  }
  if (values.length !== 1) throw rehydratePublicError(contractError('gatt.ambiguous-path', 'gatt', operation))
  const selected = values[0]
  if (selected === undefined) throw rehydratePublicError(contractError('gatt.not-found', 'gatt', operation))
  return selected
}

function sameService(
  characteristic: PortableCurrentCharacteristicPath,
  service: PortableGattDatabaseSnapshot['services'][number]['path']
): boolean {
  return (
    characteristic.serviceUuid === service.serviceUuid && characteristic.serviceOccurrence === service.serviceOccurrence
  )
}

function sameCharacteristic(
  descriptor: PortableCurrentDescriptorPath,
  characteristic: PortableCurrentCharacteristicPath
): boolean {
  return (
    sameService(descriptor, characteristic) &&
    descriptor.characteristicUuid === characteristic.characteristicUuid &&
    descriptor.characteristicOccurrence === characteristic.characteristicOccurrence
  )
}

function normalizeCharacteristicProperties(
  record: PortableGattDatabaseSnapshot['characteristics'][number]
): GattCharacteristicProperties {
  const availability: GattCharacteristicPropertyAvailability = record.properties.availability ?? {
    broadcast: 'unknown',
    read: 'known',
    writeWithResponse: 'known',
    writeWithoutResponse: 'known',
    authenticatedSignedWrites: 'unknown',
    notify: 'known',
    indicate: record.properties.indicate === undefined ? 'unknown' : 'known',
    extendedProperties: 'unknown',
    reliableWrite: 'unknown',
    writableAuxiliaries: 'unknown'
  }
  return Object.freeze({
    broadcast: record.properties.broadcast ?? false,
    read: record.properties.read,
    writeWithResponse: record.properties.writeWithResponse,
    writeWithoutResponse: record.properties.writeWithoutResponse,
    authenticatedSignedWrites: record.properties.authenticatedSignedWrites ?? false,
    notify: record.properties.notify,
    indicate: record.properties.indicate ?? false,
    extendedProperties: record.properties.extendedProperties ?? false,
    reliableWrite: record.properties.reliableWrite ?? false,
    writableAuxiliaries: record.properties.writableAuxiliaries ?? false,
    availability: Object.freeze(availability)
  })
}

function resolveWriteMode(
  properties: GattCharacteristicProperties,
  response: GattWriteOptions['response'] = 'automatic'
): 'with-response' | 'without-response' {
  if (response === 'required') {
    if (!properties.writeWithResponse)
      throw rehydratePublicError(
        contractError('gatt.property-not-supported', 'gatt', 'public-gatt.characteristic.write-required')
      )
    return 'with-response'
  }
  if (response === 'not-required') {
    if (!properties.writeWithoutResponse)
      throw rehydratePublicError(
        contractError('gatt.property-not-supported', 'gatt', 'public-gatt.characteristic.write-not-required')
      )
    return 'without-response'
  }
  if (properties.writeWithResponse) return 'with-response'
  if (properties.writeWithoutResponse) return 'without-response'
  throw rehydratePublicError(contractError('gatt.property-not-supported', 'gatt', 'public-gatt.characteristic.write'))
}

function resolveDelivery(
  properties: GattCharacteristicProperties,
  requested: GattSubscribeOptions['delivery']
): GattDelivery {
  if (requested === 'require-notification' && !properties.notify) {
    throw rehydratePublicError(
      contractError('gatt.property-not-supported', 'gatt', 'public-gatt.subscribe.notification')
    )
  }
  if (requested === 'require-indication' && !properties.indicate) {
    throw rehydratePublicError(contractError('gatt.property-not-supported', 'gatt', 'public-gatt.subscribe.indication'))
  }
  if (requested === 'require-notification') return 'notification'
  if (requested === 'require-indication') return 'indication'
  if (requested === 'prefer-indication' && properties.indicate) return 'indication'
  if (properties.notify) return 'notification'
  if (properties.indicate) return 'indication'
  throw rehydratePublicError(contractError('gatt.property-not-supported', 'gatt', 'public-gatt.subscribe'))
}

function mapGattValueStream(
  source: import('../manager/consumer-handles').PortableBoundedAsyncStream<
    import('../manager/consumer-handles').PortableNotificationValue
  >,
  now: () => number
): GattValueStream {
  let sequence = 1
  return {
    limits: source.limits,
    overflowPolicy: source.overflowPolicy,
    [Symbol.asyncIterator]() {
      const iterator = source[Symbol.asyncIterator]()
      return {
        async next() {
          return mapStreamItem(await iterator.next(), now, () => sequence++)
        },
        return: async () => {
          await iterator.return()
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    },
    close: () => source.close()
  }
}

function mapStreamItem(
  result: IteratorResult<
    import('../manager/consumer-handles').PortableStreamItem<
      import('../manager/consumer-handles').PortableNotificationValue
    >,
    undefined
  >,
  now: () => number,
  nextSequence: () => number
): IteratorResult<StreamItem<GattValueEvent>, undefined> {
  if (result.done) return result
  if (result.value.kind === 'overflow') {
    return {
      done: false,
      value: {
        kind: 'overflow',
        policy: result.value.policy,
        droppedItems: resourceCount(result.value.droppedItems),
        droppedBytes: resourceCount(result.value.droppedBytes),
        replacedItems: resourceCount(result.value.replacedItems)
      }
    }
  }
  if (result.value.kind === 'terminal') {
    return {
      done: false,
      value: {
        kind: 'terminal',
        reason: result.value.reason,
        droppedItems: resourceCount(result.value.droppedItems),
        droppedBytes: resourceCount(result.value.droppedBytes),
        replacedItems: resourceCount(result.value.replacedItems)
      }
    }
  }
  const value = result.value.value
  return {
    done: false,
    value: {
      kind: 'value',
      value: Object.freeze({
        value: new Uint8Array(value.value),
        delivery: value.indication ? 'indication' : 'notification',
        observedAtMonotonicMs: now(),
        sequence: nextSequence()
      })
    }
  }
}

function emptyChangedStream(): AsyncIterable<StreamItem<GattDatabaseChangedEvent>> {
  return {
    [Symbol.asyncIterator]() {
      return {
        async next() {
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    }
  }
}

function rehydrateCleanup(
  operation: Promise<import('../manager/consumer-handles').PortableCleanupRecord>
): Promise<import('../manager/consumer-handles').PortableCleanupRecord> {
  return operation.catch(error => {
    throw rehydratePublicError(error)
  })
}

function runPublic<Value>(operation: () => Value): Value {
  try {
    return operation()
  } catch (error) {
    throw rehydratePublicError(error)
  }
}
