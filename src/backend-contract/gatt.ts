// src/backend-contract/gatt.ts

import { contractError } from './errors'
import type { CleanupRecord } from './errors'
import { attachmentRecordsEqual, type AttachmentRecord } from './identity'
import type {
  AttachmentId,
  BorrowedBytes,
  ConnectionId,
  GattDatabaseId,
  GenerationId,
  LeaseId,
  OwnedBytes,
  PeerId,
  SubscriptionId,
  Uuid
} from './primitives'
import type {
  CharacteristicRead,
  PublicOperationOptions,
  SubscriptionOptions,
  WriteMode,
  WritePolicy,
  WriteReceipt
} from './operations'
import type { BoundedAsyncStream } from './streams'

/** An explicit acquired transport; ordinary writes are never its fallback. */
export interface AcquiredGattWriter<Attachment extends string> {
  readonly mtuBytes: number
  write(value: BorrowedBytes, options: PublicOperationOptions): Promise<WriteReceipt<Attachment, string>>
  close(): Promise<CleanupRecord>
}
export interface AcquiredGattNotifications {
  readonly mtuBytes: number
  readonly values: BoundedAsyncStream<NotificationValue>
  close(): Promise<CleanupRecord>
}

export type PathValidity = 'current' | 'stale'
export interface GattDatabaseChangedEvent {
  readonly previousGeneration: string
  readonly reason: 'service-changed' | 'reconnect' | 'backend-reset' | 'manual-rediscovery'
  readonly affectedHandleRange: { readonly start: number; readonly end: number } | null
}
export interface DevicePath<Attachment extends string> {
  readonly attachment: AttachmentRecord<Attachment>
  readonly attachmentId: AttachmentId<Attachment>
  readonly peerId: PeerId<Attachment>
}
export interface ConnectionPath<Attachment extends string, Connection extends string> extends DevicePath<Attachment> {
  readonly connectionId: ConnectionId<Attachment, Connection>
  readonly ownerLeaseId: LeaseId<Attachment, Connection>
  readonly connectionGeneration: GenerationId<'connection-generation', Connection>
}
export interface DatabasePath<Attachment extends string, Connection extends string, Database extends string>
  extends ConnectionPath<Attachment, Connection> {
  readonly databaseId: GattDatabaseId<Attachment, Connection, Database>
  readonly databaseGeneration: GenerationId<'database-generation', Database>
}
export interface ServicePath<
  Attachment extends string,
  Connection extends string,
  Database extends string,
  ServiceScope extends string
> extends DatabasePath<Attachment, Connection, Database> {
  readonly serviceUuid: Uuid
  readonly serviceOccurrence: GenerationId<'service-occurrence', ServiceScope>
}
export interface CharacteristicPath<
  Attachment extends string,
  Connection extends string,
  Database extends string,
  ServiceScope extends string,
  CharacteristicScope extends string,
  Validity extends PathValidity = 'current'
> extends ServicePath<Attachment, Connection, Database, ServiceScope> {
  readonly characteristicUuid: Uuid
  readonly characteristicOccurrence: GenerationId<'characteristic-occurrence', CharacteristicScope>
  readonly validity: Validity
}
export interface DescriptorPath<
  Attachment extends string,
  Connection extends string,
  Database extends string,
  ServiceScope extends string,
  CharacteristicScope extends string,
  DescriptorScope extends string,
  Validity extends PathValidity = 'current'
> extends CharacteristicPath<Attachment, Connection, Database, ServiceScope, CharacteristicScope, Validity> {
  readonly descriptorUuid: Uuid
  readonly descriptorOccurrence: GenerationId<'descriptor-occurrence', DescriptorScope>
}
/** A discovered service whose characteristics could not be listed. */
export interface GattServiceRestriction {
  readonly state: 'restricted'
  readonly reason: 'os-reserved' | 'access-denied'
  readonly gattStatus: 'access-denied' | null
  readonly attError: null
}

/** Map a native service-access label onto the public restriction. */
export function serviceAccessRestriction(access: string | null | undefined): GattServiceRestriction | undefined {
  if (access === 'os-reserved') {
    return Object.freeze({ state: 'restricted', reason: 'os-reserved', gattStatus: null, attError: null })
  }
  if (access === 'access-denied') {
    return Object.freeze({
      state: 'restricted',
      reason: 'access-denied',
      gattStatus: 'access-denied',
      attError: null
    })
  }
  return undefined
}

export interface Service<
  Attachment extends string,
  Connection extends string,
  Database extends string,
  Occurrence extends string
> {
  readonly path: ServicePath<Attachment, Connection, Database, Occurrence>
  /** null means the platform did not provide primary/secondary status. */
  readonly primary: boolean | null
  /** null means inclusion discovery was unavailable; [] is an observed empty list. */
  readonly includedServices: readonly GattServiceReference[] | null
  readonly restriction?: GattServiceRestriction
}
export interface Characteristic<
  Attachment extends string,
  Connection extends string,
  Database extends string,
  ServiceOccurrence extends string,
  Occurrence extends string
> {
  readonly path: CharacteristicPath<Attachment, Connection, Database, ServiceOccurrence, Occurrence>
  readonly properties: CharacteristicProperties
  readonly access: GattAccessRequirements
}
/** Complete normalized operation metadata captured at GATT discovery time. */
export interface CharacteristicProperties {
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
export interface GattCharacteristicPropertyAvailability {
  readonly broadcast: 'known' | 'unknown'
  readonly read: 'known' | 'unknown'
  readonly writeWithResponse: 'known' | 'unknown'
  readonly writeWithoutResponse: 'known' | 'unknown'
  readonly authenticatedSignedWrites: 'known' | 'unknown'
  readonly notify: 'known' | 'unknown'
  readonly indicate: 'known' | 'unknown'
  readonly extendedProperties: 'known' | 'unknown'
  readonly reliableWrite: 'known' | 'unknown'
  readonly writableAuxiliaries: 'known' | 'unknown'
}
export interface GattAccessRequirements {
  readonly read: 'none' | 'encrypted' | 'authenticated' | 'authorized' | 'unknown'
  readonly write: 'none' | 'encrypted' | 'authenticated' | 'authorized' | 'unknown'
}
export interface GattDescriptorProperties {
  readonly read: boolean
  readonly write: boolean
  readonly availability: {
    readonly read: 'known' | 'unknown'
    readonly write: 'known' | 'unknown'
  }
  readonly access: GattAccessRequirements
}
export interface GattServiceReference {
  readonly uuid: Uuid
  readonly occurrence: string
}

export interface GattCharacteristicPropertyInput {
  readonly read: boolean
  readonly writeWithResponse: boolean
  readonly writeWithoutResponse: boolean
  readonly notify: boolean
  readonly indicate?: boolean
  readonly broadcast?: boolean
  readonly authenticatedSignedWrites?: boolean
  readonly extendedProperties?: boolean
  readonly reliableWrite?: boolean
  readonly writableAuxiliaries?: boolean
  readonly availability?: Partial<GattCharacteristicPropertyAvailability>
}

export function createGattCharacteristicProperties(input: GattCharacteristicPropertyInput): CharacteristicProperties {
  const optionalAvailability = input.availability ?? {}
  return Object.freeze({
    broadcast: input.broadcast ?? false,
    read: input.read,
    writeWithResponse: input.writeWithResponse,
    writeWithoutResponse: input.writeWithoutResponse,
    authenticatedSignedWrites: input.authenticatedSignedWrites ?? false,
    notify: input.notify,
    indicate: input.indicate ?? false,
    extendedProperties: input.extendedProperties ?? false,
    reliableWrite: input.reliableWrite ?? false,
    writableAuxiliaries: input.writableAuxiliaries ?? false,
    availability: Object.freeze({
      broadcast: optionalAvailability.broadcast ?? (input.broadcast === undefined ? 'unknown' : 'known'),
      read: optionalAvailability.read ?? 'known',
      writeWithResponse: optionalAvailability.writeWithResponse ?? 'known',
      writeWithoutResponse: optionalAvailability.writeWithoutResponse ?? 'known',
      authenticatedSignedWrites:
        optionalAvailability.authenticatedSignedWrites ??
        (input.authenticatedSignedWrites === undefined ? 'unknown' : 'known'),
      notify: optionalAvailability.notify ?? 'known',
      indicate: optionalAvailability.indicate ?? (input.indicate === undefined ? 'unknown' : 'known'),
      extendedProperties:
        optionalAvailability.extendedProperties ?? (input.extendedProperties === undefined ? 'unknown' : 'known'),
      reliableWrite: optionalAvailability.reliableWrite ?? (input.reliableWrite === undefined ? 'unknown' : 'known'),
      writableAuxiliaries:
        optionalAvailability.writableAuxiliaries ?? (input.writableAuxiliaries === undefined ? 'unknown' : 'known')
    })
  })
}

export function createGattDescriptorProperties(
  read: boolean,
  write: boolean,
  availability: GattDescriptorProperties['availability'],
  access: GattAccessRequirements
): GattDescriptorProperties {
  return Object.freeze({
    read,
    write,
    availability: Object.freeze({ ...availability }),
    access: Object.freeze({ ...access })
  })
}
export interface Descriptor<
  Attachment extends string,
  Connection extends string,
  Database extends string,
  ServiceOccurrence extends string,
  CharacteristicOccurrence extends string,
  Occurrence extends string
> {
  readonly path: DescriptorPath<
    Attachment,
    Connection,
    Database,
    ServiceOccurrence,
    CharacteristicOccurrence,
    Occurrence
  >
  readonly properties: GattDescriptorProperties
}
export interface GattDatabaseSnapshot<Attachment extends string, Connection extends string, Database extends string> {
  readonly path: DatabasePath<Attachment, Connection, Database>
  readonly services: readonly Service<Attachment, Connection, Database, string>[]
  readonly characteristics: readonly Characteristic<Attachment, Connection, Database, string, string>[]
  readonly descriptors: readonly Descriptor<Attachment, Connection, Database, string, string, string>[]
}
export interface GattDatabase<Attachment extends string, Connection extends string, Database extends string> {
  readonly path: DatabasePath<Attachment, Connection, Database>
  snapshot(): Promise<GattDatabaseSnapshot<Attachment, Connection, Database>>
  acquireWrite?<ServiceOccurrence extends string, CharacteristicOccurrence extends string>(
    path: CharacteristicPath<Attachment, Connection, Database, ServiceOccurrence, CharacteristicOccurrence, 'current'>,
    options: PublicOperationOptions
  ): Promise<AcquiredGattWriter<Attachment>>
  acquireNotifications?<ServiceOccurrence extends string, CharacteristicOccurrence extends string>(
    path: CharacteristicPath<Attachment, Connection, Database, ServiceOccurrence, CharacteristicOccurrence, 'current'>,
    options: SubscriptionOptions
  ): Promise<AcquiredGattNotifications>
  /** Reads one characteristic; the answer carries the platform's own provenance. */
  read<ServiceOccurrence extends string, CharacteristicOccurrence extends string>(
    path: CharacteristicPath<Attachment, Connection, Database, ServiceOccurrence, CharacteristicOccurrence, 'current'>,
    options: PublicOperationOptions
  ): Promise<CharacteristicRead>
  write<ServiceOccurrence extends string, CharacteristicOccurrence extends string>(
    path: CharacteristicPath<Attachment, Connection, Database, ServiceOccurrence, CharacteristicOccurrence, 'current'>,
    value: BorrowedBytes,
    options: WritePolicy
  ): Promise<WriteReceipt<Attachment, string>>
  /** Native-owned FIFO readiness wait using the original write admission. */
  writeWhenReady?<ServiceOccurrence extends string, CharacteristicOccurrence extends string>(
    path: CharacteristicPath<Attachment, Connection, Database, ServiceOccurrence, CharacteristicOccurrence, 'current'>,
    value: BorrowedBytes,
    options: WritePolicy
  ): Promise<WriteReceipt<Attachment, string>>
  readDescriptor<
    ServiceOccurrence extends string,
    CharacteristicOccurrence extends string,
    DescriptorOccurrence extends string
  >(
    path: DescriptorPath<
      Attachment,
      Connection,
      Database,
      ServiceOccurrence,
      CharacteristicOccurrence,
      DescriptorOccurrence,
      'current'
    >,
    options: PublicOperationOptions
  ): Promise<OwnedBytes>
  writeDescriptor<
    ServiceOccurrence extends string,
    CharacteristicOccurrence extends string,
    DescriptorOccurrence extends string
  >(
    path: DescriptorPath<
      Attachment,
      Connection,
      Database,
      ServiceOccurrence,
      CharacteristicOccurrence,
      DescriptorOccurrence,
      'current'
    >,
    value: BorrowedBytes,
    options: WritePolicy
  ): Promise<WriteReceipt<Attachment, string>>
  subscribe<ServiceOccurrence extends string, CharacteristicOccurrence extends string>(
    path: CharacteristicPath<Attachment, Connection, Database, ServiceOccurrence, CharacteristicOccurrence, 'current'>,
    options: SubscriptionOptions
  ): Promise<Subscription<Attachment, Connection, Database, string, string, string>>
}
/** Current backend observation used by the portable chunking policy. */
export interface MaximumWriteLengthObservation<Attachment extends string> {
  readonly connectionId: ConnectionId<Attachment, string>
  readonly connectionGeneration: GenerationId<'connection-generation', string>
  readonly mode: WriteMode
  readonly maximumWriteLength: number
  readonly observedAtMonotonicMs: number
}
/**
 * How a characteristic value reached the central. `unknown` is the answer of
 * a platform that does not report it (CoreBluetooth, BlueZ): a backend never
 * infers a delivery the platform did not report.
 */
export type NotificationDelivery = 'notification' | 'indication' | 'unknown'

export const NOTIFICATION_DELIVERIES: readonly NotificationDelivery[] = Object.freeze([
  'notification',
  'indication',
  'unknown'
])

export interface NotificationValue {
  readonly value: OwnedBytes
  readonly delivery: NotificationDelivery
}
export interface Subscription<
  Attachment extends string = string,
  Connection extends string = string,
  Database extends string = string,
  ServiceOccurrence extends string = string,
  CharacteristicOccurrence extends string = string,
  SubscriptionScope extends string = string
> {
  readonly subscriptionId: SubscriptionId<
    Attachment,
    Connection,
    Database,
    ServiceOccurrence,
    CharacteristicOccurrence,
    SubscriptionScope
  >
  readonly path: CharacteristicPath<
    Attachment,
    Connection,
    Database,
    ServiceOccurrence,
    CharacteristicOccurrence,
    'current'
  >
  readonly observedDelivery?: NotificationDelivery
  readonly values: BoundedAsyncStream<NotificationValue>
  remove(): Promise<CleanupRecord>
}
export function assertCurrentPath<
  Attachment extends string,
  Connection extends string,
  Database extends string,
  ServiceOccurrence extends string,
  CharacteristicOccurrence extends string
>(
  path: CharacteristicPath<Attachment, Connection, Database, ServiceOccurrence, CharacteristicOccurrence>
): asserts path is CharacteristicPath<
  Attachment,
  Connection,
  Database,
  ServiceOccurrence,
  CharacteristicOccurrence,
  'current'
> {
  if (path.validity !== 'current') {
    throw contractError('gatt.stale-handle', 'gatt', 'gatt.assert-current-path')
  }
}
export function assertPathMatchesAttachment<Attachment extends string>(
  path: DevicePath<Attachment>,
  attachment: AttachmentRecord<Attachment>
): void {
  if (path.attachmentId !== attachment.attachmentId || !attachmentRecordsEqual(path.attachment, attachment)) {
    throw contractError('gatt.stale-handle', 'gatt', 'gatt.assert-path-matches-attachment')
  }
}
