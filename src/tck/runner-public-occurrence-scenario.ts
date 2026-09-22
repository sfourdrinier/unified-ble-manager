// src/tck/runner-public-occurrence-scenario.ts

import type { BleCentralBackend } from '../backend-contract/backend'
import type { BackendIdentity } from '../backend-contract/identity'
import type { BackendTckFixture, TckFact, TckScenarioDefinition } from './contracts'
import { TckAssertionError } from './contracts'
import {
  ambiguousNotifiableCharacteristics,
  inspectOccurrenceIndexing,
  occurrenceIndexingDetail,
  occurrenceWorldIsComplete,
  routingTargetsCoverBothLevels
} from './runner-public-occurrence-support'
import {
  assertCleanupReleased,
  connectAndDiscover,
  emptyInput,
  fact,
  notificationInput,
  operationOptions,
  subscriptionOptions
} from './runner-public-scenario-support'
import type { PublicManager } from './runner-public-scenarios'

/** Real-time bound on one routed delivery: a value that never arrives fails the scenario, never hangs it. */
const DELIVERY_LIMIT_MS = 5_000
const FIRST_ROUTED_BYTE = 0x80
const ROUTED_BYTES_PER_PASS = 0x40

/**
 * Duplicate UUIDs at every level (docs/UNIFIED_SEMANTICS.md §9): discovery
 * returns every occurrence, numbered per UUID under its parent in discovery
 * order, and a notification addressed to one instance reaches only the
 * subscription on that instance's complete path.
 */
export async function executeDuplicateUuidOccurrenceScenario<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  manager: PublicManager<Attachment, Identity>,
  fixture: BackendTckFixture<Attachment, Identity, Backend>,
  definition: TckScenarioDefinition
): Promise<readonly TckFact[]> {
  const connected = await connectToScenarioPeer(manager, fixture, definition)
  const indexing = inspectOccurrenceIndexing(connected.snapshot)
  if (!occurrenceWorldIsComplete(indexing)) {
    throw new TckAssertionError(
      definition.id,
      'the fixture world lacks a second service UUID or a same-UUID occurrence ≥ 1 at the service, characteristic or descriptor level'
    )
  }
  const targets = ambiguousNotifiableCharacteristics(connected.snapshot.characteristics)
  if (!routingTargetsCoverBothLevels(targets)) {
    throw new TckAssertionError(
      definition.id,
      'the fixture world lacks notifiable same-UUID characteristics under one service and across service occurrences'
    )
  }
  if (targets.length > ROUTED_BYTES_PER_PASS) {
    throw new TckAssertionError(definition.id, 'the fixture world has more routing targets than distinct routed values')
  }
  const subscriptions = []
  try {
    for (const target of targets) {
      const subscription = await fixture.controller.settle(
        connected.database.subscribe(target.path, subscriptionOptions('drop-oldest', 4, 128))
      )
      subscriptions.push({ subscription, iterator: subscription.values[Symbol.asyncIterator]() })
    }
    // Forward then backward: a value misrouted to a sibling is queued there
    // ahead of the sibling's own value, whichever instance was addressed.
    const indices = targets.map((_, index) => index)
    const passes = [indices, [...indices].reverse()]
    const delivered: boolean[] = []
    for (const [pass, order] of passes.entries()) {
      for (const index of order) {
        const target = targets[index]
        const entry = subscriptions[index]
        if (target === undefined || entry === undefined) {
          throw new TckAssertionError(definition.id, 'a routing target has no subscription')
        }
        const expected = FIRST_ROUTED_BYTE + pass * ROUTED_BYTES_PER_PASS + index
        const next = entry.iterator.next()
        await fixture.controller.perform(
          'emit-notification',
          notificationInput(target.path, new Uint8Array([expected]))
        )
        const item = await withinDeliveryLimit(fixture.controller.settle(next), definition)
        delivered.push(
          !item.done &&
            item.value.kind === 'value' &&
            item.value.value.value.byteLength === 1 &&
            item.value.value.value[0] === expected
        )
      }
    }
    const routedExactly = delivered.length === targets.length * passes.length && delivered.every(Boolean)
    for (const entry of subscriptions.splice(0)) {
      assertCleanupReleased(definition, await fixture.controller.settle(entry.subscription.remove()), 'subscription')
    }
    assertCleanupReleased(definition, await fixture.controller.settle(connected.connection.release()), 'connection')
    return [
      fact(
        'gatt-duplicate-uuid-occurrences-are-indexed-per-parent',
        indexing.pathsUnique && indexing.parentsResolve && indexing.occurrencesExact,
        occurrenceIndexingDetail(indexing)
      ),
      fact('gatt-duplicate-uuid-notifications-route-to-exact-instance', routedExactly, {
        routedTargets: targets.length,
        routedExactly
      })
    ]
  } finally {
    for (const entry of subscriptions) {
      await fixture.controller.settle(entry.subscription.remove())
    }
  }
}

/**
 * Reach the fixture's peer through the host's own discovery surface: a
 * browser chooser where the host has one, otherwise a scan.
 */
async function connectToScenarioPeer<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  manager: PublicManager<Attachment, Identity>,
  fixture: BackendTckFixture<Attachment, Identity, Backend>,
  definition: TckScenarioDefinition
) {
  const chooser = fixture.featureScenarioAdapters?.webChooser
  if (chooser === undefined) {
    if (!fixture.controller.availableActions.includes('queue-advertisement')) {
      throw new TckAssertionError(definition.id, 'the fixture exposes neither a scan peer nor a browser chooser')
    }
    return connectAndDiscover(manager, fixture, definition)
  }
  if (!fixture.controller.availableActions.includes('resolve-chooser')) {
    throw new TckAssertionError(definition.id, 'the browser chooser fixture cannot resolve its chooser')
  }
  const choosing = chooser.chooser.choose(chooser.request, operationOptions)
  await fixture.controller.flush()
  await fixture.controller.perform('resolve-chooser', emptyInput)
  const selection = await fixture.controller.settle(choosing)
  const connection = await fixture.controller.settle(manager.connect(selection.peerId, operationOptions))
  const database = await fixture.controller.settle(connection.discover(operationOptions))
  const snapshot = await fixture.controller.settle(database.snapshot())
  return { connection, database, snapshot }
}

async function withinDeliveryLimit<Value>(delivery: Promise<Value>, definition: TckScenarioDefinition): Promise<Value> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const limit = new Promise<never>((_, reject) => {
    timer = setTimeout(
      () => reject(new TckAssertionError(definition.id, 'a notification addressed to one instance never arrived')),
      DELIVERY_LIMIT_MS
    )
  })
  try {
    return await Promise.race([delivery, limit])
  } finally {
    clearTimeout(timer)
  }
}
