// src/tck/deterministic/deterministic-tck-occurrences.ts

import type { DeterministicBackendFixture } from '../../testing/deterministic/deterministic-test-backend'
import {
  ambiguousNotifiableCharacteristics,
  inspectOccurrenceIndexing,
  occurrenceIndexingDetail,
  occurrenceWorldIsComplete,
  routingTargetsCoverBothLevels
} from '../runner-public-occurrence-support'
import {
  characteristicAddress,
  connectAndDiscover,
  drainVirtualClock,
  fact,
  nextValue,
  releaseConnection,
  subscriptionOptions,
  type FactObservation
} from './deterministic-tck-scenario-helpers'

const FIRST_ROUTED_BYTE = 0x80

/**
 * Deterministic evidence for `gatt.duplicate-uuid-occurrences-route-exactly`:
 * the virtual peripheral's duplicate-UUID world is discovered with per-parent
 * occurrence indices, and a notification addressed to one instance reaches
 * only the consumer on that instance's path.
 */
export async function deterministicDuplicateUuidOccurrenceFacts(
  fixture: DeterministicBackendFixture
): Promise<readonly FactObservation[]> {
  const connected = await connectAndDiscover(fixture, 'gatt-duplicate-uuid-occurrences')
  const indexing = inspectOccurrenceIndexing(connected.snapshot)
  const targets = ambiguousNotifiableCharacteristics(connected.snapshot.characteristics)
  const worldComplete = occurrenceWorldIsComplete(indexing) && routingTargetsCoverBothLevels(targets)
  const subscriptions = []
  for (const target of targets) {
    const subscribing = connected.database.subscribe(target.path, subscriptionOptions('drop-oldest', 4, 128))
    await drainVirtualClock(fixture)
    subscriptions.push(await subscribing)
  }
  const delivered: boolean[] = []
  for (const [index, target] of targets.entries()) {
    const subscription = subscriptions[index]
    if (subscription === undefined) {
      throw new Error('deterministic TCK routing target has no subscription')
    }
    fixture.controller.emitNotification(characteristicAddress(target.path), new Uint8Array([FIRST_ROUTED_BYTE + index]))
    const value = await nextValue(subscription.values)
    delivered.push(value !== null && value.value.byteLength === 1 && value.value[0] === FIRST_ROUTED_BYTE + index)
  }
  for (const subscription of subscriptions) {
    const removal = subscription.remove()
    await drainVirtualClock(fixture)
    await removal
  }
  await releaseConnection(fixture, connected.lease)
  const routedExactly = worldComplete && delivered.length === targets.length && delivered.every(Boolean)
  return [
    fact(
      'gatt-duplicate-uuid-occurrences-are-indexed-per-parent',
      worldComplete && indexing.pathsUnique && indexing.parentsResolve && indexing.occurrencesExact,
      occurrenceIndexingDetail(indexing)
    ),
    fact('gatt-duplicate-uuid-notifications-route-to-exact-instance', routedExactly, {
      routedTargets: targets.length,
      routedExactly
    })
  ]
}
