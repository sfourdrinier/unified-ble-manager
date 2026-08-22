// src/backends/corebluetooth/corebluetooth-connection-controls.ts

import type { BackendConnection } from '../../backend-contract/backend'
import { BUILT_IN_FEATURE_IDS } from '../../backend-contract/capabilities'
import {
  MAXIMUM_REQUESTED_ATT_MTU,
  MINIMUM_ATT_MTU,
  type ConnectionMaximumWriteLengthMeasurement,
  type ConnectionMaximumWriteLengthRequest,
  type ConnectionPriorityRequest,
  type RequestPriorityRequest,
  type MtuNegotiation,
  type ReadRssiRequest,
  type RequestMtuRequest,
  type RssiMeasurement
} from '../../backend-contract/connection-controls'
import { contractError } from '../../backend-contract/errors'
import type { BackendOperationDispatch } from '../../backend-contract/operations'
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
