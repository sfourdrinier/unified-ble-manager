// src/backends/corebluetooth/corebluetooth-gatt-operations.ts

import type { BackendConnection, BackendSubscription } from '../../backend-contract/backend'
import { contractError, type CleanupRecord } from '../../backend-contract/errors'
import { mapCoreBluetoothNativeFailure } from './corebluetooth-read-notify-provenance'
import type { CharacteristicPath, DatabasePath, DescriptorPath, GattDatabase } from '../../backend-contract/gatt'
import type {
  BackendOperationDispatch,
  OperationOptions,
  OperationTerminalRecord,
  PublicOperationOptions,
  ReadRequest,
  ReadResult,
  SubscribeRequest,
  WriteRequest,
  WriteResult
} from '../../backend-contract/operations'
import { byteLimit, opaqueId, ownBytes, type OwnedBytes } from '../../backend-contract/primitives'
import type { CoreBluetoothCharacteristicAddress, CoreBluetoothDescriptorAddress } from './corebluetooth-boundary'
import { withCoreBluetoothCleanupTimeout } from './corebluetooth-cleanup'
import { CoreBoundedStream } from '../../core/bounded-stream'
import {
  addressKey,
  cleanupFailure,
  CoreBluetoothBackendSubscription,
  CoreBluetoothGattDatabase,
  releasedCleanup,
  successfulTerminal
} from './corebluetooth-handles'
import type { CoreBluetoothBackend, PhysicalSubscription } from './corebluetooth-backend'

const maximumValueBytes = byteLimit(512 * 1024)

type DescriptorBoundary = {
  readonly readDescriptor: (address: CoreBluetoothDescriptorAddress) => Promise<Uint8Array>
  readonly writeDescriptor: (address: CoreBluetoothDescriptorAddress, bytes: Uint8Array) => Promise<void>
}

export class CoreBluetoothGattOperations {
  constructor(private readonly backend: CoreBluetoothBackend) {}

  async discover(
    connection: BackendConnection<string, string>,
    options: PublicOperationOptions
  ): Promise<GattDatabase<string, string, string>> {
    this.backend.assertOperational('direct-gatt.gatt.discover')
    this.backend.operationLifecycle.assertAdmission(options, 'direct-gatt.gatt.discover')
    const record = this.backend.requireConnection(connection, 'direct-gatt.gatt.discover')
    let snapshot: unknown
    try {
      snapshot = await this.backend.operationLifecycle.awaitBoundaryOperation(
        options,
        'direct-gatt.gatt.discover',
        () => this.backend.boundary.discover(record.nativePeerId),
        undefined,
        undefined,
        String(record.connectionId)
      )
    } catch (error) {
      throw this.backend.operationLifecycle.platformError(
        'gatt.read-failed',
        'gatt',
        'direct-gatt.gatt.discover',
        error
      )
    }
    this.backend.assertGattSnapshot(snapshot)
    if (record.state !== 'connected') {
      throw contractError('operation.disconnected', 'connection', 'direct-gatt.gatt.discover.connection')
    }
    record.database?.invalidate()
    const identifiers = this.backend.identifiers()
    const path: DatabasePath<string, string, string> = Object.freeze({
      attachment: this.backend.attachment(),
      attachmentId: this.backend.attachment().attachmentId,
      peerId: record.peerId,
      connectionId: record.connectionId,
      ownerLeaseId: record.ownerLeaseId,
      connectionGeneration: record.connectionGeneration,
      databaseId: identifiers.databaseId(`corebluetooth-database-${this.backend.nextDatabase}`),
      databaseGeneration: opaqueId(
        `corebluetooth-database-generation-${this.backend.nextDatabase}`,
        'database-generation',
        'corebluetooth'
      )
    })
    this.backend.nextDatabase += 1
    const database = new CoreBluetoothGattDatabase(this.backend, record, path, snapshot)
    record.database = database
    return database
  }

  read(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>
  ): BackendOperationDispatch<string, ReadResult<string, string>> {
    this.backend.assertOperational('direct-gatt.gatt.read')
    const database = this.backend.databaseForPath(path, 'direct-gatt.gatt.read')
    const address = database.addressFor(path, 'direct-gatt.gatt.read')
    return this.backend.dispatcher.dispatch(
      request.operation,
      'direct-gatt.gatt.read',
      async () => {
        try {
          return {
            value: ownBytes(await this.backend.boundary.read(address), maximumValueBytes),
            terminal: successfulTerminal(request.operation)
          }
        } catch (error) {
          throw mapCoreBluetoothNativeFailure(error, 'direct-gatt.gatt.read', 'gatt.read-failed')
        }
      },
      String(path.connectionId)
    )
  }

  write(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    this.backend.assertOperational('direct-gatt.gatt.write')
    const database = this.backend.databaseForPath(path, 'direct-gatt.gatt.write')
    const address = database.addressFor(path, 'direct-gatt.gatt.write')
    const copied = new Uint8Array(request.bytes)
    return this.backend.dispatcher.dispatch(
      request.operation,
      'direct-gatt.gatt.write',
      async () => {
        await this.backend.boundary.write(address, copied, request.mode === 'with-response')
        return Object.freeze({ terminal: successfulTerminal(request.operation), commitState: 'confirmed' })
      },
      String(path.connectionId)
    )
  }

  readDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: ReadRequest<string, string>
  ): BackendOperationDispatch<string, ReadResult<string, string>> {
    this.backend.assertOperational('direct-gatt.gatt.read-descriptor')
    const boundary = this.descriptorBoundary('direct-gatt.gatt.read-descriptor')
    const database = this.backend.databaseForPath(path, 'direct-gatt.gatt.read-descriptor')
    const address = database.descriptorAddressFor(path, 'direct-gatt.gatt.read-descriptor')
    return this.backend.dispatcher.dispatch(
      request.operation,
      'direct-gatt.gatt.read-descriptor',
      async () => ({
        value: ownBytes(await boundary.readDescriptor(address), maximumValueBytes),
        terminal: successfulTerminal(request.operation)
      }),
      String(path.connectionId)
    )
  }

  writeDescriptor(
    path: DescriptorPath<string, string, string, string, string, string, 'current'>,
    request: WriteRequest<string, string>
  ): BackendOperationDispatch<string, WriteResult<string, string>> {
    this.backend.assertOperational('direct-gatt.gatt.write-descriptor')
    const boundary = this.descriptorBoundary('direct-gatt.gatt.write-descriptor')
    const database = this.backend.databaseForPath(path, 'direct-gatt.gatt.write-descriptor')
    const address = database.descriptorAddressFor(path, 'direct-gatt.gatt.write-descriptor')
    const copied = new Uint8Array(request.bytes)
    return this.backend.dispatcher.dispatch(
      request.operation,
      'direct-gatt.gatt.write-descriptor',
      async () => {
        await boundary.writeDescriptor(address, copied)
        return Object.freeze({ terminal: successfulTerminal(request.operation), commitState: 'confirmed' })
      },
      String(path.connectionId)
    )
  }

  subscribe(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    request: SubscribeRequest<string, string>
  ): BackendOperationDispatch<string, BackendSubscription<string, string, string, string, string>> {
    this.backend.assertOperational('direct-gatt.gatt.subscribe')
    const database = this.backend.databaseForPath(path, 'direct-gatt.gatt.subscribe')
    const address = database.addressFor(path, 'direct-gatt.gatt.subscribe')
    const mode = database.notificationDeliveryModeFor(path, 'direct-gatt.gatt.subscribe', request.options.deliveryMode)
    return this.backend.dispatcher.dispatch(
      request.operation,
      'direct-gatt.gatt.subscribe',
      async execution => {
        const key = `${addressKey(address)}|${mode}`
        let physical = this.backend.subscriptions.get(key)
        if (physical?.state === 'removing') {
          if (physical.removal === null) {
            throw contractError('lifecycle.invariant-violation', 'gatt', 'direct-gatt.gatt.subscribe.removal')
          }
          const cleanup = await physical.removal
          if (cleanup.state === 'release-failed') {
            throw new Error('CoreBluetooth notification cleanup must be retried before a new subscription')
          }
          physical = this.backend.subscriptions.get(key)
        }
        if (physical !== undefined && physical.consumers.size === 0) {
          const cleanup = await this.stopPhysicalSubscription(physical)
          if (cleanup.state === 'release-failed') {
            throw new Error('CoreBluetooth notification cleanup must be retried before a new subscription')
          }
          physical = this.backend.subscriptions.get(key)
        }
        const identifiers = this.backend.identifiers()
        const subscriptionId = identifiers.subscriptionId(`corebluetooth-subscription-${this.backend.nextSubscription}`)
        this.backend.nextSubscription += 1
        if (physical === undefined) {
          physical = {
            key,
            address,
            mode,
            consumers: new Set(),
            state: 'enabling',
            nativeStart: null,
            removalBeforeNativeStart: false,
            removal: null,
            nativeRemoval: null
          }
          this.backend.subscriptions.set(key, physical)
          const enabling = physical
          const subscription = new CoreBluetoothBackendSubscription(
            this.backend,
            enabling,
            path,
            subscriptionId,
            successfulTerminal(request.operation),
            new CoreBoundedStream(request.options.delivery, request.options.delivery.overflowPolicy)
          )
          enabling.consumers.add(subscription)
          let nativeStart: Promise<void> | null = null
          try {
            const onValue = (bytes: Uint8Array): void => this.emitNotification(enabling, bytes)
            nativeStart =
              this.backend.boundary.startNotifyWithMode === undefined
                ? this.backend.boundary.startNotify(address, onValue)
                : this.backend.boundary.startNotifyWithMode(address, enabling.mode, onValue)
            enabling.nativeStart = nativeStart
            await nativeStart
            if (enabling.nativeStart === nativeStart) {
              enabling.nativeStart = null
            }
          } catch (error) {
            if (nativeStart !== null && enabling.nativeStart === nativeStart) {
              enabling.nativeStart = null
            }
            enabling.consumers.delete(subscription)
            subscription.stream.closeWithReason('source-failed')
            if (this.backend.subscriptions.get(key) === enabling) {
              this.backend.subscriptions.delete(key)
            }
            throw mapCoreBluetoothNativeFailure(error, 'direct-gatt.gatt.subscribe', 'gatt.subscribe-failed')
          }
          if (
            this.backend.subscriptions.get(key) !== enabling ||
            enabling.state !== 'enabling' ||
            subscription.removed ||
            execution.isPublicSettled()
          ) {
            const cleanup = await this.removeSubscription(subscription)
            const postStartCleanup =
              cleanup.state === 'released' && enabling.removalBeforeNativeStart
                ? await this.stopPhysicalSubscription(enabling)
                : cleanup
            if (postStartCleanup.state === 'release-failed') {
              console.error(
                '[CoreBluetoothGattOperations.subscribe] Cancelled notification cleanup failed:',
                postStartCleanup.failures
              )
            }
            throw contractError('operation.cancelled-by-destroy', 'gatt', 'direct-gatt.gatt.subscribe.cancelled')
          }
          enabling.state = 'ready'
          return subscription
        }
        const subscription = new CoreBluetoothBackendSubscription(
          this.backend,
          physical,
          path,
          subscriptionId,
          successfulTerminal(request.operation),
          new CoreBoundedStream(request.options.delivery, request.options.delivery.overflowPolicy)
        )
        physical.consumers.add(subscription)
        if (execution.isPublicSettled()) {
          const cleanup = await this.removeSubscription(subscription)
          if (cleanup.state === 'release-failed') {
            console.error(
              '[CoreBluetoothGattOperations.subscribe] Cancelled notification cleanup failed:',
              cleanup.failures
            )
          }
          throw contractError('operation.cancelled-by-destroy', 'gatt', 'direct-gatt.gatt.subscribe.cancelled')
        }
        return subscription
      },
      String(path.connectionId)
    )
  }

  unsubscribe(
    subscription: BackendSubscription<string, string, string, string, string>,
    operation: OperationOptions<string, string>
  ): BackendOperationDispatch<string, OperationTerminalRecord<string, string>> {
    if (!(subscription instanceof CoreBluetoothBackendSubscription) || !subscription.isOwnedBy(this.backend)) {
      throw contractError('ownership.denied', 'gatt', 'direct-gatt.gatt.unsubscribe.subscription')
    }
    return this.backend.dispatcher.dispatch(
      operation,
      'direct-gatt.gatt.unsubscribe',
      async () => {
        const cleanup = await this.removeSubscription(subscription)
        if (cleanup.state === 'release-failed') {
          throw new Error('CoreBluetooth notification cleanup requires retry')
        }
        return successfulTerminal(operation)
      },
      String(subscription.path.connectionId)
    )
  }

  async removeSubscription(subscription: CoreBluetoothBackendSubscription): Promise<CleanupRecord> {
    const physical = subscription.physical
    if (subscription.removed && physical.consumers.size > 0) {
      return releasedCleanup
    }
    if (subscription.removed) {
      return this.stopPhysicalSubscription(physical)
    }
    subscription.stream.closeWithReason('owner-released')
    physical.consumers.delete(subscription)
    subscription.removed = true
    if (physical.consumers.size > 0) {
      return releasedCleanup
    }
    return this.stopPhysicalSubscription(physical)
  }

  stopPhysicalSubscription(physical: PhysicalSubscription): Promise<CleanupRecord> {
    if (physical.state === 'released') {
      return Promise.resolve(releasedCleanup)
    }
    if (physical.removal !== null) {
      return physical.removal
    }
    if (physical.nativeRemoval !== null) {
      return Promise.resolve(
        cleanupFailure(
          'subscription',
          'direct-gatt.gatt.stop-notify',
          new Error('CoreBluetooth notification cleanup remains in flight')
        )
      )
    }
    physical.state = 'removing'
    physical.removalBeforeNativeStart = physical.nativeStart !== null
    let nativeRemoval: Promise<void>
    try {
      nativeRemoval = this.backend.boundary.stopNotify(physical.address)
    } catch (error) {
      nativeRemoval = Promise.reject(error)
    }
    physical.nativeRemoval = nativeRemoval
    const nativeCompletion = nativeRemoval.then(
      () => {
        if (physical.removalBeforeNativeStart) {
          physical.state = 'removing'
        } else {
          physical.state = 'released'
        }
        physical.nativeRemoval = null
        if (!physical.removalBeforeNativeStart && this.backend.subscriptions.get(physical.key) === physical) {
          this.backend.subscriptions.delete(physical.key)
        }
        return releasedCleanup
      },
      error => {
        physical.nativeRemoval = null
        physical.state = 'cleanup-failed'
        return cleanupFailure('subscription', 'direct-gatt.gatt.stop-notify', error)
      }
    )
    const removal = withCoreBluetoothCleanupTimeout(() => nativeCompletion, 'direct-gatt.gatt.stop-notify').catch(
      error => {
        physical.state = 'cleanup-failed'
        return cleanupFailure('subscription', 'direct-gatt.gatt.stop-notify', error)
      }
    )
    physical.removal = removal
    nativeCompletion.then(() => {
      if (physical.removal === removal) {
        physical.removal = null
      }
    })
    return removal
  }

  unsupportedDispatch<Result>(
    operation: OperationOptions<string, string>,
    name: string
  ): BackendOperationDispatch<string, Result> {
    return this.backend.dispatcher.dispatch(operation, name, async () => {
      throw contractError('capability.unsupported', 'gatt', name)
    })
  }

  async subscribeFromDatabase(
    path: CharacteristicPath<string, string, string, string, string, 'current'>,
    options: import('../../backend-contract/operations').SubscriptionOptions
  ): Promise<CoreBluetoothBackendSubscription> {
    const correlation = this.backend
      .identifiers()
      .operationCorrelation(`corebluetooth-database-subscribe-${this.backend.nextSubscription}`)
    const dispatch = this.subscribe(path, { operation: { ...options, correlation }, options })
    const subscription = await dispatch.completion
    if (!(subscription instanceof CoreBluetoothBackendSubscription)) {
      throw contractError('protocol.violation', 'gatt', 'direct-gatt.gatt.database-subscribe.subscription')
    }
    return subscription
  }

  async readFromDatabase(
    address: CoreBluetoothCharacteristicAddress,
    options: PublicOperationOptions,
    connectionSerializationKey: string
  ): Promise<OwnedBytes> {
    this.backend.assertOperational('direct-gatt.gatt.database-read')
    const dispatch = this.backend.dispatcher.dispatch(
      options,
      'direct-gatt.gatt.database-read',
      async () => ownBytes(await this.backend.boundary.read(address), maximumValueBytes),
      connectionSerializationKey
    )
    return dispatch.completion
  }

  async writeFromDatabase(
    address: CoreBluetoothCharacteristicAddress,
    value: Uint8Array,
    withResponse: boolean,
    options: PublicOperationOptions,
    connectionSerializationKey: string
  ): Promise<void> {
    this.backend.assertOperational('direct-gatt.gatt.database-write')
    const copied = new Uint8Array(value)
    const dispatch = this.backend.dispatcher.dispatch(
      options,
      'direct-gatt.gatt.database-write',
      async () => this.backend.boundary.write(address, copied, withResponse),
      connectionSerializationKey
    )
    await dispatch.completion
  }

  async readDescriptorFromDatabase(
    address: CoreBluetoothDescriptorAddress,
    options: PublicOperationOptions,
    connectionSerializationKey: string
  ): Promise<OwnedBytes> {
    this.backend.assertOperational('direct-gatt.gatt.database-read-descriptor')
    const boundary = this.descriptorBoundary('direct-gatt.gatt.database-read-descriptor')
    const dispatch = this.backend.dispatcher.dispatch(
      options,
      'direct-gatt.gatt.database-read-descriptor',
      async () => ownBytes(await boundary.readDescriptor(address), maximumValueBytes),
      connectionSerializationKey
    )
    return dispatch.completion
  }

  async writeDescriptorFromDatabase(
    address: CoreBluetoothDescriptorAddress,
    value: Uint8Array,
    options: PublicOperationOptions,
    connectionSerializationKey: string
  ): Promise<void> {
    this.backend.assertOperational('direct-gatt.gatt.database-write-descriptor')
    const boundary = this.descriptorBoundary('direct-gatt.gatt.database-write-descriptor')
    const copied = new Uint8Array(value)
    const dispatch = this.backend.dispatcher.dispatch(
      options,
      'direct-gatt.gatt.database-write-descriptor',
      async () => boundary.writeDescriptor(address, copied),
      connectionSerializationKey
    )
    await dispatch.completion
  }

  private descriptorBoundary(operation: string): DescriptorBoundary {
    const boundary = this.backend.boundary
    if (
      boundary.descriptorOperationsAvailable !== true ||
      boundary.readDescriptor === undefined ||
      boundary.writeDescriptor === undefined
    ) {
      throw contractError('capability.unsupported', 'gatt', operation)
    }
    const readDescriptor = boundary.readDescriptor
    const writeDescriptor = boundary.writeDescriptor
    return Object.freeze({
      readDescriptor: address => readDescriptor.call(boundary, address),
      writeDescriptor: (address, bytes) => writeDescriptor.call(boundary, address, bytes)
    })
  }

  emitNotification(physical: PhysicalSubscription, source: Uint8Array): void {
    if (physical.state === 'removing' || physical.state === 'cleanup-failed' || physical.state === 'released') {
      return
    }
    for (const consumer of [...physical.consumers]) {
      if (consumer.removed || consumer.stream.isTerminal()) {
        continue
      }
      const copied = ownBytes(source, maximumValueBytes)
      const push = consumer.stream.emit(
        Object.freeze({ value: ownBytes(copied, maximumValueBytes), indication: false }),
        copied.byteLength
      )
      if (push.terminated) {
        this.removeSubscription(consumer)
          .then(result => {
            if (result.state === 'release-failed') {
              console.error(
                '[CoreBluetoothGattOperations.emitNotification] Overflow notify cleanup requires retry:',
                result.failures
              )
            }
          })
          .catch(error => {
            console.error('[CoreBluetoothGattOperations.emitNotification] Overflow notify cleanup rejected:', error)
          })
      }
    }
  }
}
