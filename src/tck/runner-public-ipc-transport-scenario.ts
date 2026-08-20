// src/tck/runner-public-ipc-transport-scenario.ts

import type { BleCentralBackend } from '../backend-contract/backend'
import type { BackendIdentity } from '../backend-contract/identity'
import type { IpcBleEvent } from '../ipc/protocol'
import type {
  BackendTckFixture,
  TckFact,
  TckIpcTransportScenarioAdapter,
  TckScenarioController,
  TckScenarioDefinition
} from './contracts'
import { TckAssertionError } from './contracts'
import { emptyInput, fact } from './runner-public-scenario-support'

/**
 * Proves the desktop webview event-sink lifetime documented on `IpcClientTransport`: the sink is
 * bound once when the client attaches, and the request/response path never disturbs it. The runner
 * emits one host event, replays the adapter's route requests, acknowledges the first event, and
 * requires a further host event to arrive after each of those phases.
 *
 * A transport that re-sends or rebinds its sink outside attach fails here instead of failing
 * silently in production: on Tauri, re-sending the sink unregisters the one shared host callback,
 * so lifecycle, advertisement, and notification streams stop with no rejection anywhere.
 */
export async function executePublicIpcTransportScenario<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: BackendTckFixture<Attachment, Identity, Backend>,
  definition: TckScenarioDefinition
): Promise<readonly TckFact[]> {
  const adapter = requireIpcTransportAdapter(fixture, definition)
  const transport = adapter.transport
  if (
    typeof transport.invoke !== 'function' ||
    typeof transport.subscribe !== 'function' ||
    typeof transport.acknowledge !== 'function'
  ) {
    throw new TckAssertionError(
      definition.id,
      'IPC transport adapter does not implement invoke, subscribe, and acknowledge'
    )
  }
  if (adapter.routeRequests.length < 2) {
    throw new TckAssertionError(
      definition.id,
      `IPC transport adapter supplied ${adapter.routeRequests.length} route requests; a sequence of at least two is required`
    )
  }
  for (const [index, request] of adapter.routeRequests.entries()) {
    if (request.kind !== 'route') {
      throw new TckAssertionError(
        definition.id,
        `IPC transport adapter route request ${index} has kind ${String(request.kind)}; only route requests may be replayed against a bound event sink`
      )
    }
  }

  const observed: IpcBleEvent[] = []
  const unsubscribe = transport.subscribe(event => {
    observed.push(event)
  })
  if (typeof unsubscribe !== 'function') {
    throw new TckAssertionError(definition.id, 'IPC transport subscribe did not return an unsubscribe function')
  }
  try {
    const attached = await deliverOneEvent(
      fixture.controller,
      definition,
      observed,
      'before any request/response traffic, so this fixture never established a live event sink for the scenario to observe'
    )

    for (const [index, request] of adapter.routeRequests.entries()) {
      const response = await fixture.controller.settle(transport.invoke(request))
      if (response.kind === 'failure') {
        throw new TckAssertionError(
          definition.id,
          `IPC transport route request ${index} was answered with failure ${response.error.code}`
        )
      }
      if (response.kind !== 'route') {
        throw new TckAssertionError(
          definition.id,
          `IPC transport route request ${index} was answered with a ${response.kind} response`
        )
      }
    }
    const afterInvokes = await deliverOneEvent(
      fixture.controller,
      definition,
      observed,
      `after ${adapter.routeRequests.length} request/response invoke calls; invoke must never carry or rebind the event sink, because rebinding it unregisters the one host callback and silently stops every stream`
    )

    const acknowledgement = await fixture.controller.settle(
      transport.acknowledge(attached.rendererLease, attached.eventId)
    )
    if (acknowledgement.kind !== 'event.ack') {
      throw new TckAssertionError(
        definition.id,
        `IPC transport acknowledge was answered with a ${acknowledgement.kind} response`
      )
    }
    const afterAcknowledge = await deliverOneEvent(
      fixture.controller,
      definition,
      observed,
      'after the event acknowledgement; acknowledge is request/response only and must never disturb the event sink'
    )

    assertSameClientLease(definition, attached, afterInvokes, 'the replayed route requests')
    assertSameClientLease(definition, attached, afterAcknowledge, 'the event acknowledgement')

    return [
      fact('ipc-event-sink-survives-request-response-traffic', true, {
        replayedRouteRequestCount: adapter.routeRequests.length,
        acknowledgedEventCount: 1,
        deliveredEventCount: observed.length,
        clientLeaseUnchanged: true
      })
    ]
  } finally {
    unsubscribe()
  }
}

async function deliverOneEvent(
  controller: TckScenarioController,
  definition: TckScenarioDefinition,
  observed: readonly IpcBleEvent[],
  phase: string
): Promise<IpcBleEvent> {
  const before = observed.length
  await controller.perform('emit-ipc-event', emptyInput)
  await controller.flush()
  const delivered = observed.length - before
  if (delivered === 0) {
    throw new TckAssertionError(definition.id, `the transport event sink delivered no event ${phase}`)
  }
  if (delivered !== 1) {
    throw new TckAssertionError(
      definition.id,
      `the transport event sink delivered ${delivered} events for one emitted event ${phase}`
    )
  }
  const event = observed[before]
  if (event === undefined) {
    throw new TckAssertionError(definition.id, `the transport event sink reported a delivery without an event ${phase}`)
  }
  return event
}

function assertSameClientLease(
  definition: TckScenarioDefinition,
  attached: IpcBleEvent,
  observed: IpcBleEvent,
  cause: string
): void {
  if (
    String(observed.rendererLease.leaseId) !== String(attached.rendererLease.leaseId) ||
    String(observed.rendererLease.generation) !== String(attached.rendererLease.generation)
  ) {
    throw new TckAssertionError(
      definition.id,
      `the transport event sink was rebound to a different client lease by ${cause}; only re-attaching may rebind it`
    )
  }
}

function requireIpcTransportAdapter<
  Attachment extends string,
  Identity extends BackendIdentity<Attachment>,
  Backend extends BleCentralBackend<Attachment, Identity>
>(
  fixture: BackendTckFixture<Attachment, Identity, Backend>,
  definition: TckScenarioDefinition
): TckIpcTransportScenarioAdapter<Attachment> {
  const adapter = fixture.featureScenarioAdapters?.ipcTransport
  if (adapter === undefined) {
    throw new TckAssertionError(definition.id, 'fixture lacks an IPC transport scenario adapter')
  }
  return adapter
}
