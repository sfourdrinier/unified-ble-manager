// src/backends/corebluetooth/corebluetooth-connection-controls.ts

import type { BackendConnection } from '../../backend-contract/backend'
import { BUILT_IN_FEATURE_IDS } from '../../backend-contract/capabilities'
import {
  MAXIMUM_REQUESTED_ATT_MTU,
  MINIMUM_ATT_MTU,
  type BlePhy,
  type ConnectionMaximumWriteLengthMeasurement,
  type ConnectionMaximumWriteLengthRequest,
  type ConnectionPhyObservation,
  type ConnectionPhyRequest,
  type ConnectionPriorityRequest,
  type ConnectionWriteReadinessObservation,
  type ConnectionWriteReadinessWatch,
  type RequestPriorityRequest,
  type ReadPhyRequest,
  type RequestPhyRequest,
  type MtuNegotiation,
  type ReadRssiRequest,
  type RequestMtuRequest,
  type RssiMeasurement
} from '../../backend-contract/connection-controls'
import { contractError } from '../../backend-contract/errors'
import type { CleanupRecord } from '../../backend-contract/errors'
import type { BackendOperationDispatch } from '../../backend-contract/operations'
import { capacity } from '../../backend-contract/primitives'
import { CoreBoundedStream } from '../../core/bounded-stream'
import type { CoreBluetoothWriteReadinessEvent } from './corebluetooth-boundary'
import { successfulTerminal } from './corebluetooth-handles'
import type { CoreBluetoothBackend } from './corebluetooth-backend'

/** Bridges optional direct-boundary connection controls into the canonical operation dispatcher. */
export class CoreBluetoothConnectionControls {
  constructor(private readonly backend: CoreBluetoothBackend) {}

  readRssi<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ReadRssiRequest<string, Operation>
  ): BackendOperationDispatch<string, RssiMeasurement<string, Operation>> {
    if (this.backend.boundary.connectionControlCapabilities?.rssi === 'unavailable') {
      return this.unsupported(request.operation, 'corebluetooth.connection.read-rssi')
    }
    const readRssi = this.backend.boundary.readRssi?.bind(this.backend.boundary)
    if (readRssi === undefined) {
      return this.unsupported(request.operation, 'corebluetooth.connection.read-rssi')
    }
    this.backend.assertOperational('corebluetooth.connection.read-rssi')
    const record = this.backend.requireConnection(connection, 'corebluetooth.connection.read-rssi')
    return this.backend.dispatcher.dispatch(
      request.operation,
      'corebluetooth.connection.read-rssi',
      async () => {
        const rssi = await readRssi(record.nativePeerId)
        if (!Number.isSafeInteger(rssi)) {
          throw contractError('protocol.malformed', 'connection', 'corebluetooth.connection.read-rssi.result')
        }
        return Object.freeze({
          rssi,
          observedAtMonotonicMs: this.backend.monotonicNow(),
          terminal: successfulTerminal(request.operation)
        })
      },
      String(connection.connectionId)
    )
  }

  requestMtu<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: RequestMtuRequest<string, Operation>
  ): BackendOperationDispatch<string, MtuNegotiation<string, Operation>> {
    if (!this.requestMtuFeatureIsCallable()) {
      return this.unsupported(request.operation, 'corebluetooth.connection.request-mtu')
    }
    if (this.backend.boundary.connectionControlCapabilities?.requestMtu === 'unavailable') {
      return this.unsupported(request.operation, 'corebluetooth.connection.request-mtu')
    }
    const requestMtu = this.backend.boundary.requestMtu?.bind(this.backend.boundary)
    if (requestMtu === undefined) {
      return this.unsupported(request.operation, 'corebluetooth.connection.request-mtu')
    }
    this.backend.assertOperational('corebluetooth.connection.request-mtu')
    const record = this.backend.requireConnection(connection, 'corebluetooth.connection.request-mtu')
    return this.backend.dispatcher.dispatch(
      request.operation,
      'corebluetooth.connection.request-mtu',
      async () => {
        const negotiatedMtu = await requestMtu(record.nativePeerId, request.requestedMtu)
        if (
          !Number.isSafeInteger(negotiatedMtu) ||
          negotiatedMtu < MINIMUM_ATT_MTU ||
          negotiatedMtu > MAXIMUM_REQUESTED_ATT_MTU
        ) {
          throw contractError('protocol.malformed', 'connection', 'corebluetooth.connection.request-mtu.result')
        }
        return Object.freeze({
          requestedMtu: request.requestedMtu,
          negotiatedMtu,
          observedAtMonotonicMs: this.backend.monotonicNow(),
          terminal: successfulTerminal(request.operation)
        })
      },
      String(connection.connectionId)
    )
  }

  requestPriority<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: RequestPriorityRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionPriorityRequest<string, Operation>> {
    if (this.backend.boundary.connectionControlCapabilities?.priority !== 'available') {
      return this.unsupported(request.operation, 'corebluetooth.connection.request-priority')
    }
    const requestPriority = this.backend.boundary.requestPriority?.bind(this.backend.boundary)
    if (requestPriority === undefined) {
      return this.unsupported(request.operation, 'corebluetooth.connection.request-priority')
    }
    this.backend.assertOperational('corebluetooth.connection.request-priority')
    const record = this.backend.requireConnection(connection, 'corebluetooth.connection.request-priority')
    return this.backend.dispatcher.dispatch(
      request.operation,
      'corebluetooth.connection.request-priority',
      async () => {
        const accepted = await requestPriority(record.nativePeerId, request.priority)
        if (typeof accepted !== 'boolean') {
          throw contractError('protocol.malformed', 'connection', 'corebluetooth.connection.request-priority.result')
        }
        return Object.freeze({
          requested: request.priority,
          accepted,
          observedAtMonotonicMs: this.backend.monotonicNow(),
          terminal: successfulTerminal(request.operation)
        })
      },
      String(connection.connectionId)
    )
  }

  readPhy<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ReadPhyRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionPhyObservation<string, Operation>> {
    if (this.backend.boundary.connectionControlCapabilities?.phy !== 'available') {
      return this.unsupported(request.operation, 'corebluetooth.connection.read-phy')
    }
    const readPhy = this.backend.boundary.readPhy?.bind(this.backend.boundary)
    if (readPhy === undefined) {
      return this.unsupported(request.operation, 'corebluetooth.connection.read-phy')
    }
    this.backend.assertOperational('corebluetooth.connection.read-phy')
    const record = this.backend.requireConnection(connection, 'corebluetooth.connection.read-phy')
    return this.backend.dispatcher.dispatch(
      request.operation,
      'corebluetooth.connection.read-phy',
      async () => {
        const value = await readPhy(record.nativePeerId)
        if (!isBlePhy(value.txPhy) || !isBlePhy(value.rxPhy)) {
          throw contractError('protocol.malformed', 'connection', 'corebluetooth.connection.read-phy.result')
        }
        return Object.freeze({
          txPhy: value.txPhy,
          rxPhy: value.rxPhy,
          observedAtMonotonicMs: this.backend.monotonicNow(),
          terminal: successfulTerminal(request.operation)
        })
      },
      String(connection.connectionId)
    )
  }

  requestPhy<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: RequestPhyRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionPhyRequest<string, Operation>> {
    if (this.backend.boundary.connectionControlCapabilities?.phy !== 'available') {
      return this.unsupported(request.operation, 'corebluetooth.connection.request-phy')
    }
    const requestPhy = this.backend.boundary.requestPhy?.bind(this.backend.boundary)
    if (requestPhy === undefined) {
      return this.unsupported(request.operation, 'corebluetooth.connection.request-phy')
    }
    this.backend.assertOperational('corebluetooth.connection.request-phy')
    const record = this.backend.requireConnection(connection, 'corebluetooth.connection.request-phy')
    return this.backend.dispatcher.dispatch(
      request.operation,
      'corebluetooth.connection.request-phy',
      async () => {
        const value = await requestPhy(record.nativePeerId, request.preference)
        if (typeof value.accepted !== 'boolean') {
          throw contractError('protocol.malformed', 'connection', 'corebluetooth.connection.request-phy.result')
        }
        const observation = value.observation
        if (value.accepted !== (observation !== null)) {
          throw contractError('protocol.malformed', 'connection', 'corebluetooth.connection.request-phy.observation')
        }
        const terminal = successfulTerminal(request.operation)
        return Object.freeze({
          requested: request.preference,
          accepted: value.accepted,
          observation:
            observation === null
              ? null
              : Object.freeze({
                  txPhy: requireBlePhy(observation.txPhy, 'corebluetooth.connection.request-phy.tx'),
                  rxPhy: requireBlePhy(observation.rxPhy, 'corebluetooth.connection.request-phy.rx'),
                  observedAtMonotonicMs: this.backend.monotonicNow(),
                  terminal
                }),
          observedAtMonotonicMs: this.backend.monotonicNow(),
          terminal
        })
      },
      String(connection.connectionId)
    )
  }

  async writeWithoutResponseReadiness(
    connection: BackendConnection<string, string>
  ): Promise<ConnectionWriteReadinessWatch<string>> {
    const probe = this.backend.boundary.canSendWriteWithoutResponse?.bind(this.backend.boundary)
    const onReadiness = this.backend.boundary.onWriteWithoutResponseReadiness
    if (probe === undefined || onReadiness === undefined) {
      throw contractError('capability.unsupported', 'connection', 'corebluetooth.connection.write-readiness')
    }
    this.backend.assertOperational('corebluetooth.connection.write-readiness')
    const record = this.backend.requireConnection(connection, 'corebluetooth.connection.write-readiness')
    const stream = new CoreBoundedStream<ConnectionWriteReadinessObservation<string>>(
      { itemCapacity: capacity(64), byteCapacity: capacity(16 * 1024), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    let closed = false
    let initialized = false
    let lastOrdinal = 0
    let closePromise: Promise<CleanupRecord> | null = null
    let unsubscribeDisconnect: (() => void) | null = null
    let unregisterWatch: (() => void) | null = null
    const buffered: CoreBluetoothWriteReadinessEvent[] = []
    const emit = (event: CoreBluetoothWriteReadinessEvent): void => {
      if (
        closed ||
        event.nativePeerId !== record.nativePeerId ||
        event.connectionGeneration !== String(record.connectionGeneration) ||
        !Number.isSafeInteger(event.ordinal) ||
        event.ordinal <= lastOrdinal
      ) {
        return
      }
      const value = Object.freeze({
        connectionId: record.connectionId,
        connectionGeneration: record.connectionGeneration,
        ready: event.ready,
        observedAtMonotonicMs: this.backend.monotonicNow(),
        ordinal: event.ordinal
      })
      lastOrdinal = event.ordinal
      stream.emit(value, 128)
    }
    const unsubscribe = onReadiness(event => {
      if (!initialized) {
        buffered.push(event)
        return
      }
      emit(event)
    })
    const close = (reason: 'owner-released' | 'connection-lost' | 'source-failed'): Promise<CleanupRecord> => {
      if (closePromise !== null) return closePromise
      closed = true
      unsubscribe()
      unsubscribeDisconnect?.()
      unregisterWatch?.()
      stream.closeWithReason(reason)
      closePromise = Promise.resolve({ state: 'released', failures: [] })
      return closePromise
    }
    unsubscribeDisconnect = this.backend.boundary.onDisconnect(nativePeerId => {
      if (nativePeerId === record.nativePeerId) {
        close('connection-lost').catch(() => undefined)
      }
    })
    unregisterWatch = this.backend.registerReadinessWatch(() => close('owner-released').then(() => undefined))
    try {
      const snapshot = await probe(record.nativePeerId)
      if (
        snapshot.nativePeerId !== record.nativePeerId ||
        snapshot.connectionGeneration !== String(record.connectionGeneration) ||
        typeof snapshot.ready !== 'boolean' ||
        !Number.isSafeInteger(snapshot.ordinal)
      ) {
        throw contractError('protocol.malformed', 'connection', 'corebluetooth.connection.write-readiness.snapshot')
      }
      lastOrdinal = snapshot.ordinal
      stream.emit(
        Object.freeze({
          connectionId: record.connectionId,
          connectionGeneration: record.connectionGeneration,
          ready: snapshot.ready,
          observedAtMonotonicMs: this.backend.monotonicNow(),
          ordinal: snapshot.ordinal
        }),
        128
      )
      initialized = true
      buffered.sort((left, right) => left.ordinal - right.ordinal)
      for (const event of buffered) emit(event)
      buffered.length = 0
      return {
        events: stream,
        close: () => close('owner-released')
      }
    } catch (error) {
      await close('source-failed')
      throw error
    }
  }

  maximumWriteLength<Operation extends string>(
    connection: BackendConnection<string, string>,
    request: ConnectionMaximumWriteLengthRequest<string, Operation>
  ): BackendOperationDispatch<string, ConnectionMaximumWriteLengthMeasurement<string, Operation>> {
    const maximumWriteValueLength = this.backend.boundary.maximumWriteValueLength?.bind(this.backend.boundary)
    if (maximumWriteValueLength === undefined) {
      return this.unsupported(request.operation, 'corebluetooth.connection.maximum-write-length')
    }
    this.backend.assertOperational('corebluetooth.connection.maximum-write-length')
    const record = this.backend.requireConnection(connection, 'corebluetooth.connection.maximum-write-length')
    return this.backend.dispatcher.dispatch(
      request.operation,
      'corebluetooth.connection.maximum-write-length',
      async () => {
        const maximumWriteLength = await maximumWriteValueLength(record.nativePeerId, request.mode === 'with-response')
        if (!Number.isSafeInteger(maximumWriteLength) || maximumWriteLength < 1) {
          throw contractError(
            'protocol.malformed',
            'connection',
            'corebluetooth.connection.maximum-write-length.result'
          )
        }
        return Object.freeze({
          connectionId: record.connectionId,
          connectionGeneration: record.connectionGeneration,
          mode: request.mode,
          maximumWriteLength,
          observedAtMonotonicMs: this.backend.monotonicNow(),
          terminal: successfulTerminal(request.operation)
        })
      },
      String(connection.connectionId)
    )
  }

  private unsupported<Operation extends string, Result>(
    operation: ReadRssiRequest<string, Operation>['operation'],
    operationName: string
  ): BackendOperationDispatch<string, Result> {
    return this.backend.dispatcher.dispatch(operation, operationName, async () => {
      throw contractError('capability.unsupported', 'connection', operationName)
    })
  }

  private requestMtuFeatureIsCallable(): boolean {
    const registration = this.backend.features.registrations.find(
      candidate => candidate.id === BUILT_IN_FEATURE_IDS.connectionRequestMtu
    )
    return registration?.state === 'supported' || registration?.state === 'limited'
  }
}

function isBlePhy(value: string): value is BlePhy {
  return value === 'le-1m' || value === 'le-2m' || value === 'le-coded'
}

function requireBlePhy(value: string, operation: string): BlePhy {
  if (!isBlePhy(value)) {
    throw contractError('protocol.malformed', 'connection', operation)
  }
  return value
}
