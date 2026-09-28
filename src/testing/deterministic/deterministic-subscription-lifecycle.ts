// src/testing/deterministic/deterministic-subscription-lifecycle.ts

import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { CleanupRecord } from '../../backend-contract/errors'
import type { OperationOptions, OperationTerminalRecord } from '../../backend-contract/operations'
import type { DeterministicOperationRuntime } from './deterministic-operation-runtime'
import type { DeterministicSubscription, PhysicalSubscription } from './deterministic-test-backend-handles'
import { noOperationOptions, releasedCleanup, takePeripheralFailure } from './deterministic-test-backend-handles'
import type { VirtualPeripheral } from './virtual-peripheral'

interface DeterministicSubscriptionResources {
  readonly operations: DeterministicOperationRuntime
  readonly peripheral: VirtualPeripheral
  readonly physicalSubscriptions: Map<string, PhysicalSubscription>
}

export async function unsubscribeManagedDeterministicSubscription<Operation extends string>(
  resources: DeterministicSubscriptionResources & {
    readonly subscriptionsById: Map<string, DeterministicSubscription>
    readonly managed: DeterministicSubscription
    readonly operation: OperationOptions<string, Operation>
  }
): Promise<OperationTerminalRecord<string, string>> {
  const managed = resources.managed
  const subscriptionId = String(managed.subscriptionId)
  if (resources.subscriptionsById.get(subscriptionId) !== managed) {
    throw contractError('gatt.stale-handle', 'gatt', 'gatt.unsubscribe')
  }
  const physical = resources.physicalSubscriptions.get(managed.physicalKey)
  const result = await resources.operations.run(
    'unsubscribe',
    resources.operation,
    resources.operation.correlation,
    false,
    () => {
      // Cleanup uses the admitted identity, not the database's usability.
      // Invalidation may have already confirmed this original physical scope's
      // release while its managed handle still awaits local retirement. Never
      // disable a replacement scope or another consumer in that case.
      if (
        physical !== undefined &&
        resources.physicalSubscriptions.get(managed.physicalKey) === physical &&
        physical.consumers.has(managed)
      ) {
        const ownsPhysicalDisable = physical.consumers.size === 1
        if (ownsPhysicalDisable) {
          takePeripheralFailure(resources.peripheral, 'unsubscribe', 'gatt.subscribe-failed')
        }
        physical.consumers.delete(managed)
        if (ownsPhysicalDisable) {
          physical.state = 'removing'
          resources.physicalSubscriptions.delete(physical.key)
        }
      }
      managed.closeForRemoval()
      if (resources.subscriptionsById.get(subscriptionId) === managed)
        resources.subscriptionsById.delete(subscriptionId)
      return undefined
    },
    null,
    null,
    String(resources.managed.path.connectionId)
  )
  return result.terminal
}

export async function disableDeterministicPhysicalSubscription(
  resources: DeterministicSubscriptionResources & {
    readonly physical: PhysicalSubscription
    readonly recordFailure: (cause: import('../../backend-contract/errors').BleErrorCode) => void
  }
): Promise<CleanupRecord> {
  if (resources.physical.enablePromise !== null) {
    try {
      await resources.physical.enablePromise
    } catch (error) {
      resources.recordFailure(error instanceof BackendContractError ? error.normalized.code : 'platform.failure')
      return releasedCleanup
    }
  }
  try {
    await resources.operations.run(
      'unsubscribe',
      noOperationOptions(),
      null,
      false,
      () => {
        takePeripheralFailure(resources.peripheral, 'unsubscribe', 'gatt.subscribe-failed')
        resources.physical.state = 'removing'
        resources.physicalSubscriptions.delete(resources.physical.key)
        return undefined
      },
      null,
      null,
      String(resources.physical.database.path.connectionId),
      true
    )
  } catch (error) {
    const normalized =
      error instanceof BackendContractError
        ? error.normalized
        : contractError('platform.failure', 'cleanup', 'deterministic.unsubscribe').normalized
    return { state: 'release-failed', failures: [{ resourceKind: 'subscription', error: normalized }] }
  }
  return releasedCleanup
}
