// src/backends/reactnative/react-native-ordered-setup.ts
import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { NormalizedBleError } from '../../backend-contract/errors'
import type { CharacteristicPath } from '../../backend-contract/gatt'
import type { SubscriptionOptions } from '../../backend-contract/operations'
import { OrderedNativeStream } from '../../core/ordered-native-stream'
import type { OrderedNotificationSetup } from '../../core/ordered-notification-contract'
import { createReactNativeOrderedTransport } from './react-native-ordered-transport'
import type { OrderedSessionInvoke } from './react-native-ordered-transport'
import type { WireJsonObject } from './rust-core-wire'
import type { ReactNativeOrderedRecoveryHandle } from './react-native-ordered-recovery'

export interface ReactNativeOrderedMember {
  readonly memberId: string
  readonly path: CharacteristicPath<string, string, string, string, string, 'current'>
  readonly deliveryMode: NonNullable<SubscriptionOptions['deliveryMode']>
  readonly selector: WireJsonObject
}

/** Admission checks and characteristic resolution are completed by the provider first. */
export async function setupReactNativeOrdered(input: {
  readonly invoke: OrderedSessionInvoke
  readonly open: (args: WireJsonObject) => Promise<void>
  readonly operationId: () => string
  readonly validate: () => void
  readonly peerId: string
  readonly connectionGeneration: string
  readonly databaseGeneration: string
  readonly publicConnectionGeneration: string
  readonly publicDatabaseGeneration: string
  readonly members: readonly ReactNativeOrderedMember[]
  readonly options: SubscriptionOptions
  readonly retain: (stream: OrderedNativeStream, handle: ReactNativeOrderedRecoveryHandle) => void
}): Promise<OrderedNotificationSetup> {
  const version = await input.invoke('gatt.ordered.version', {})
  if (version.wireVersion !== 2)
    throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered.version')
  const metadata = await input.invoke('gatt.ordered.failure-metadata.version', {})
  if (!metadata.versions?.includes(1))
    throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered.failure-metadata.version')
  if (
    input.members.length === 0 ||
    new Set(input.members.map(m => m.memberId)).size !== input.members.length ||
    input.members.some(m => m.memberId.length === 0)
  )
    throw contractError('argument.invalid', 'gatt', 'react-native.ordered.members')
  const reserved = await input.invoke('gatt.ordered.reserve', { peerId: input.peerId })
  const transport = createReactNativeOrderedTransport(
    input.invoke,
    input.peerId,
    reserved.collectionId,
    reserved.reclaimCapability,
    input.members.map(m => m.memberId),
    input.operationId
  )
  const stream = new OrderedNativeStream(
    input.options.delivery,
    transport,
    input.peerId,
    reserved.collectionId,
    input.publicConnectionGeneration,
    input.publicDatabaseGeneration,
    input.members.map(m => m.memberId),
    1
  )
  input.retain(
    stream,
    Object.freeze({
      peerId: input.peerId,
      collectionId: reserved.collectionId,
      reclaimCapability: reserved.reclaimCapability
    })
  )
  const members = input.members.map((member, index) => {
    const requirement =
      member.deliveryMode === 'require-notification'
        ? 'notification'
        : member.deliveryMode === 'require-indication'
          ? 'indication'
          : undefined
    const preference =
      member.deliveryMode === 'prefer-notification'
        ? 'notification'
        : member.deliveryMode === 'prefer-indication'
          ? 'indication'
          : undefined
    return {
      memberId: String(index),
      publicMemberId: member.memberId,
      requestedDelivery: member.deliveryMode,
      selector: member.selector,
      ...(requirement === undefined ? {} : { deliveryMode: requirement }),
      ...(preference === undefined ? {} : { preferredDeliveryMode: preference })
    }
  })
  try {
    await input.open({
      wireVersion: 2,
      failureMetadataVersion: 1,
      peerId: input.peerId,
      collectionId: reserved.collectionId,
      connectionGeneration: input.connectionGeneration,
      databaseGeneration: input.databaseGeneration,
      publicConnectionGeneration: input.publicConnectionGeneration,
      publicDatabaseGeneration: input.publicDatabaseGeneration,
      members,
      maxItems: input.options.delivery.itemCapacity,
      maxBytes: input.options.delivery.byteCapacity,
      maxControls: input.options.delivery.reservedControlCapacity
    })
    const status = await transport.status()
    if (status.setupError !== null) throw new BackendContractError(status.setupError)
    input.validate()
    if (input.options.signal?.aborted) throw contractError('operation.aborted', 'gatt', 'react-native.ordered.setup')
    if (status.members.some(member => member.acquisition !== 'acquired'))
      throw contractError('protocol.malformed', 'boundary', 'react-native.ordered.setup-status')
    stream.setSourceGeneration(status.sourceGeneration)
    return {
      state: 'ready',
      sourceGeneration: status.sourceGeneration,
      members: input.members.map((member, index) => {
        const current = status.members[index]
        if (current === undefined)
          throw contractError('protocol.malformed', 'boundary', 'react-native.ordered.setup-member')
        return {
          memberId: member.memberId,
          path: member.path,
          requestedDelivery: member.deliveryMode,
          effectiveDelivery: current.effectiveDelivery
        }
      }),
      stream
    }
  } catch (error) {
    const primary: NormalizedBleError =
      error instanceof BackendContractError
        ? error.normalized
        : contractError('platform.failure', 'boundary', 'react-native.ordered.setup').normalized
    let sourceGeneration: import('../../core/ordered-notification-contract').OrderedSourceGeneration | null = null
    let acquisitions: {
      readonly memberId: string
      readonly acquisition: 'not-started' | 'acquired' | 'indeterminate'
    }[] = input.members.map(m => ({ memberId: m.memberId, acquisition: 'indeterminate' }))
    const failures: import('../../backend-contract/errors').CleanupFailure[] = []
    try {
      const status = await transport.status()
      sourceGeneration = status.sourceGeneration
      stream.setSourceGeneration(sourceGeneration)
      acquisitions = status.members.map(m => ({ memberId: m.memberId, acquisition: m.acquisition }))
    } catch (statusError) {
      failures.push({
        resourceKind: 'ordered-status',
        error: transport.normalizeError(statusError, 'react-native.ordered.status').normalized
      })
    }
    let cleanup: import('../../core/ordered-notification-contract').OrderedNotificationCleanup
    try {
      cleanup = await stream.close()
    } catch (cleanupError) {
      const pending: import('../../backend-contract/errors').CleanupRecord = {
        state: 'release-failed',
        failures: [
          {
            resourceKind: 'ordered-group',
            error: transport.normalizeError(cleanupError, 'react-native.ordered.cleanup').normalized
          }
        ]
      }
      cleanup = { group: pending, members: input.members.map(m => ({ memberId: m.memberId, cleanup: pending })) }
    }
    const unopenedReleased = transport.inventoryEmpty() && cleanup.group.state === 'released'
    if (unopenedReleased)
      acquisitions = input.members.map(member => ({ memberId: member.memberId, acquisition: 'not-started' }))
    let observations: readonly (
      | import('../../core/ordered-notification-contract').OrderedNotificationValue
      | import('../../core/ordered-notification-contract').OrderedNotificationLoss
      | import('../../core/ordered-notification-contract').OrderedNotificationMarker
    )[]
    try {
      const collected = unopenedReleased ? { observations: [], error: null } : await stream.setupObservations()
      observations = collected.observations
      if (collected.error !== null) failures.push({ resourceKind: 'ordered-observations', error: collected.error })
    } catch (observationError) {
      failures.push({
        resourceKind: 'ordered-observations',
        error: transport.normalizeError(observationError, 'react-native.ordered.observations').normalized
      })
      observations = []
    }
    if (failures.length > 0)
      cleanup = { ...cleanup, group: { state: 'release-failed', failures: [...cleanup.group.failures, ...failures] } }
    return {
      state: 'failed',
      sourceGeneration,
      error: primary,
      members: acquisitions,
      observations,
      uncertainTail: true,
      cleanup,
      stream,
      close: () => stream.close()
    }
  }
}
