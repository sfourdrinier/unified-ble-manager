// src/core/core-connection-controls.ts

import type { BleCentralBackend } from '../backend-contract/backend'
import {
  MAXIMUM_REQUESTED_ATT_MTU,
  MINIMUM_ATT_MTU,
  type ConnectionMaximumWriteLengthMeasurement,
  type ConnectionPhyObservation,
  type ConnectionPhyRequest,
  type ConnectionPriorityRequest,
  type ConnectionWriteReadinessWatch,
  type EffectiveMtuMeasurement,
  type MtuNegotiation,
  type RssiMeasurement,
  type ConnectionPriority,
  type BlePhy,
  type PhyPreference
} from '../backend-contract/connection-controls'
import { contractError } from '../backend-contract/errors'
import type { BackendIdentity } from '../backend-contract/identity'
import type { PublicOperationOptions, WriteMode } from '../backend-contract/operations'
import type { CoreConnection } from './core-gatt-handles'
import type { CoreOperationCoordinator } from './operation-coordinator'
import { coreDispatch, requireOperationValue } from './unified-ble-core-helpers'

export interface CoreConnectionControls<Attachment extends string, Identity extends BackendIdentity<Attachment>> {
  readRssi(
    connection: CoreConnection<Attachment, Identity>,
    options: PublicOperationOptions
  ): Promise<RssiMeasurement<Attachment, string>>
  requestMtu(
    connection: CoreConnection<Attachment, Identity>,
    requestedMtu: number,
    options: PublicOperationOptions
  ): Promise<MtuNegotiation<Attachment, string>>
  effectiveMtu(
    connection: CoreConnection<Attachment, Identity>,
    options: PublicOperationOptions
  ): Promise<EffectiveMtuMeasurement<Attachment, string>>
  requestPriority(
    connection: CoreConnection<Attachment, Identity>,
    priority: ConnectionPriority,
    options: PublicOperationOptions
  ): Promise<ConnectionPriorityRequest<Attachment, string>>
  readPhy(
    connection: CoreConnection<Attachment, Identity>,
    options: PublicOperationOptions
  ): Promise<ConnectionPhyObservation<Attachment, string>>
  requestPhy(
    connection: CoreConnection<Attachment, Identity>,
    preference: PhyPreference,
    options: PublicOperationOptions
  ): Promise<ConnectionPhyRequest<Attachment, string>>
  maximumWriteLength(
    connection: CoreConnection<Attachment, Identity>,
    mode: WriteMode,
    options: PublicOperationOptions
  ): Promise<ConnectionMaximumWriteLengthMeasurement<Attachment, string>>
  writeWithoutResponseReadiness(
    connection: CoreConnection<Attachment, Identity>
  ): Promise<ConnectionWriteReadinessWatch<Attachment>>
}

export function createCoreConnectionControls<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  assertReady: (operation: string) => void
): CoreConnectionControls<Attachment, Identity> {
  return Object.freeze({
    readRssi: (connection: CoreConnection<Attachment, Identity>, options: PublicOperationOptions) => {
      assertReady('read-rssi')
      return readCoreRssi(backend, operationCoordinator, connection, options)
    },
    requestMtu: (
      connection: CoreConnection<Attachment, Identity>,
      requestedMtu: number,
      options: PublicOperationOptions
    ) => {
      assertReady('request-mtu')
      return requestCoreMtu(backend, operationCoordinator, connection, requestedMtu, options)
    },
    effectiveMtu: (connection: CoreConnection<Attachment, Identity>, options: PublicOperationOptions) => {
      assertReady('effective-mtu')
      return observeCoreEffectiveMtu(backend, operationCoordinator, connection, options)
    },
    requestPriority: (
      connection: CoreConnection<Attachment, Identity>,
      priority: ConnectionPriority,
      options: PublicOperationOptions
    ) => {
      assertReady('request-priority')
      return requestCorePriority(backend, operationCoordinator, connection, priority, options)
    },
    readPhy: (connection: CoreConnection<Attachment, Identity>, options: PublicOperationOptions) =>
      readCorePhy(backend, operationCoordinator, connection, options),
    requestPhy: (
      connection: CoreConnection<Attachment, Identity>,
      preference: PhyPreference,
      options: PublicOperationOptions
    ) => requestCorePhy(backend, operationCoordinator, connection, preference, options),
    maximumWriteLength: (
      connection: CoreConnection<Attachment, Identity>,
      mode: WriteMode,
      options: PublicOperationOptions
    ) => {
      assertReady('maximum-write-length')
      return observeCoreMaximumWriteLength(backend, operationCoordinator, connection, mode, options)
    },
    writeWithoutResponseReadiness: (connection: CoreConnection<Attachment, Identity>) => {
      assertReady('write-readiness')
      return observeCoreWriteReadiness(backend, connection)
    }
  })
}

export async function requestCorePriority<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  connection: CoreConnection<Attachment, Identity>,
  priority: ConnectionPriority,
  options: PublicOperationOptions
): Promise<ConnectionPriorityRequest<Attachment, string>> {
  const requestPriority = backend.connections.requestPriority
  if (requestPriority === undefined) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.request-priority')
  }
  connection.assertCurrent()
  const result = await operationCoordinator.run({
    queueKey: String(connection.resource.connectionId),
    fairnessKey: 'control',
    options,
    mayCommit: false,
    dispatch: correlation => {
      connection.assertCurrent()
      const dispatch = requestPriority(connection.resource, {
        operation: { ...options, correlation },
        priority
      })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.request-priority')
}

export async function readCorePhy<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  connection: CoreConnection<Attachment, Identity>,
  options: PublicOperationOptions
): Promise<ConnectionPhyObservation<Attachment, string>> {
  const readPhy = backend.connections.readPhy
  if (readPhy === undefined) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.read-phy')
  }
  connection.assertCurrent()
  const result = await operationCoordinator.run({
    queueKey: String(connection.resource.connectionId),
    fairnessKey: 'control',
    options,
    mayCommit: false,
    dispatch: correlation => {
      connection.assertCurrent()
      const dispatch = readPhy(connection.resource, { operation: { ...options, correlation } })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.read-phy')
}

export async function requestCorePhy<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  connection: CoreConnection<Attachment, Identity>,
  preference: PhyPreference,
  options: PublicOperationOptions
): Promise<ConnectionPhyRequest<Attachment, string>> {
  if (
    (preference.tx === undefined && preference.rx === undefined) ||
    (preference.tx !== undefined && !isPhy(preference.tx)) ||
    (preference.rx !== undefined && !isPhy(preference.rx))
  ) {
    throw contractError('argument.invalid', 'connection', 'unified-core.request-phy')
  }
  const requestPhy = backend.connections.requestPhy
  if (requestPhy === undefined) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.request-phy')
  }
  connection.assertCurrent()
  const result = await operationCoordinator.run({
    queueKey: String(connection.resource.connectionId),
    fairnessKey: 'control',
    options,
    mayCommit: false,
    dispatch: correlation => {
      connection.assertCurrent()
      const dispatch = requestPhy(connection.resource, {
        operation: { ...options, correlation },
        preference
      })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.request-phy')
}

export async function observeCoreWriteReadiness<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>
>(
  backend: BleCentralBackend<Attachment, Identity>,
  connection: CoreConnection<Attachment, Identity>
): Promise<ConnectionWriteReadinessWatch<Attachment>> {
  const observe = backend.connections.writeWithoutResponseReadiness
  if (observe === undefined) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.write-readiness')
  }
  connection.assertCurrent()
  return observe(connection.resource)
}

function isPhy(value: string): value is BlePhy {
  return value === 'le-1m' || value === 'le-2m' || value === 'le-coded'
}

export async function readCoreRssi<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  connection: CoreConnection<Attachment, Identity>,
  options: PublicOperationOptions
): Promise<RssiMeasurement<Attachment, string>> {
  const readRssi = backend.connections.readRssi
  if (readRssi === undefined) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.read-rssi')
  }
  connection.assertCurrent()
  const result = await operationCoordinator.run({
    queueKey: String(connection.resource.connectionId),
    fairnessKey: 'control',
    options,
    mayCommit: false,
    dispatch: correlation => {
      connection.assertCurrent()
      const dispatch = readRssi(connection.resource, { operation: { ...options, correlation } })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.read-rssi')
}

export async function requestCoreMtu<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  connection: CoreConnection<Attachment, Identity>,
  requestedMtu: number,
  options: PublicOperationOptions
): Promise<MtuNegotiation<Attachment, string>> {
  if (
    !Number.isSafeInteger(requestedMtu) ||
    requestedMtu < MINIMUM_ATT_MTU ||
    requestedMtu > MAXIMUM_REQUESTED_ATT_MTU
  ) {
    throw contractError('argument.invalid', 'connection', 'unified-core.request-mtu')
  }
  const requestMtu = backend.connections.requestMtu
  if (requestMtu === undefined) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.request-mtu')
  }
  connection.assertCurrent()
  const result = await operationCoordinator.run({
    queueKey: String(connection.resource.connectionId),
    fairnessKey: 'control',
    options,
    mayCommit: true,
    dispatch: correlation => {
      connection.assertCurrent()
      const dispatch = requestMtu(connection.resource, {
        operation: { ...options, correlation },
        requestedMtu
      })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.request-mtu')
}

export async function observeCoreEffectiveMtu<Attachment extends string, Identity extends BackendIdentity<Attachment>>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  connection: CoreConnection<Attachment, Identity>,
  options: PublicOperationOptions
): Promise<EffectiveMtuMeasurement<Attachment, string>> {
  const effectiveMtu = backend.connections.effectiveMtu
  if (effectiveMtu === undefined) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.effective-mtu')
  }
  connection.assertCurrent()
  const result = await operationCoordinator.run({
    queueKey: String(connection.resource.connectionId),
    fairnessKey: 'control',
    options,
    mayCommit: false,
    dispatch: correlation => {
      connection.assertCurrent()
      const dispatch = effectiveMtu(connection.resource, { operation: { ...options, correlation } })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.effective-mtu')
}

export async function observeCoreMaximumWriteLength<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>
>(
  backend: BleCentralBackend<Attachment, Identity>,
  operationCoordinator: CoreOperationCoordinator<Attachment>,
  connection: CoreConnection<Attachment, Identity>,
  mode: WriteMode,
  options: PublicOperationOptions
): Promise<ConnectionMaximumWriteLengthMeasurement<Attachment, string>> {
  const maximumWriteLength = backend.connections.maximumWriteLength
  if (maximumWriteLength === undefined) {
    throw contractError('capability.unsupported', 'connection', 'unified-core.maximum-write-length')
  }
  connection.assertCurrent()
  const result = await operationCoordinator.run({
    queueKey: String(connection.resource.connectionId),
    fairnessKey: 'control',
    options,
    mayCommit: false,
    dispatch: correlation => {
      connection.assertCurrent()
      const dispatch = maximumWriteLength(connection.resource, {
        operation: { ...options, correlation },
        mode
      })
      return coreDispatch(dispatch, correlation, value => value.terminal)
    }
  })
  return requireOperationValue(result, 'unified-core.maximum-write-length')
}
