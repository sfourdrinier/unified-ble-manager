// __tests__/tck/ipc-transport-event-sink-scenario.test.js

const {
  IPC_TRANSPORT_TCK_FEATURE_SUITE,
  IPC_TRANSPORT_TCK_SCENARIO_ID,
  IPC_TRANSPORT_TCK_SUITE_ID,
  TckAssertionError
} = require('../../src/tck/contracts')
const { findTckScenario } = require('../../src/tck/scenarios')
const { executePublicIpcTransportScenario } = require('../../src/tck/runner-public-ipc-transport-scenario')

const definition = findTckScenario(IPC_TRANSPORT_TCK_SCENARIO_ID)

const routeRequests = Object.freeze([
  Object.freeze({ kind: 'route', envelope: Object.freeze({ operation: 'scan.start' }) }),
  Object.freeze({ kind: 'route', envelope: Object.freeze({ operation: 'scan.stop' }) })
])

describe('runner-owned IPC transport event-sink scenario', () => {
  test('registers the transport event-sink case as a feature scenario with one suite authority', () => {
    expect(definition).toEqual({
      id: 'ipc.event-sink-survives-request-response-traffic',
      execution: 'feature',
      requiredFacts: ['ipc-event-sink-survives-request-response-traffic'],
      requiredControllerActions: ['emit-ipc-event']
    })
    expect(IPC_TRANSPORT_TCK_FEATURE_SUITE.suiteId).toBe(IPC_TRANSPORT_TCK_SUITE_ID)
    expect(IPC_TRANSPORT_TCK_FEATURE_SUITE.scenarioIds).toEqual([IPC_TRANSPORT_TCK_SCENARIO_ID])
  })

  test('accepts a transport whose event sink survives invoke and acknowledge traffic', async () => {
    const host = createIpcHostBoundary()
    const transport = createIpcTransport(host, {})

    const facts = await executePublicIpcTransportScenario(createFixture(host, transport), definition)

    expect(facts).toEqual([
      {
        id: 'ipc-event-sink-survives-request-response-traffic',
        holds: true,
        detail: {
          replayedRouteRequestCount: 2,
          acknowledgedEventCount: 1,
          deliveredEventCount: 3,
          clientLeaseUnchanged: true
        }
      }
    ])
    expect(host.boundSinkCount).toBe(1)
    expect(transport.listenerCount()).toBe(0)
  })

  test('fails when invoke re-sends the event sink and unregisters the shared host callback', async () => {
    const host = createIpcHostBoundary()
    const transport = createIpcTransport(host, { resendsEventSinkOnInvoke: true })

    await expect(executePublicIpcTransportScenario(createFixture(host, transport), definition)).rejects.toThrow(
      new TckAssertionError(
        definition.id,
        'the transport event sink delivered no event after 2 request/response invoke calls; invoke must never carry or rebind the event sink, because rebinding it unregisters the one host callback and silently stops every stream'
      )
    )
    expect(transport.listenerCount()).toBe(0)
  })

  test('fails when acknowledging a delivered event tears the event sink down', async () => {
    const host = createIpcHostBoundary()
    const transport = createIpcTransport(host, { resendsEventSinkOnAcknowledge: true })

    await expect(executePublicIpcTransportScenario(createFixture(host, transport), definition)).rejects.toThrow(
      new TckAssertionError(
        definition.id,
        'the transport event sink delivered no event after the event acknowledgement; acknowledge is request/response only and must never disturb the event sink'
      )
    )
  })

  test('fails when request/response traffic rebinds the event sink to a different client lease', async () => {
    const host = createIpcHostBoundary()
    const transport = createIpcTransport(host, { rebindsClientLeaseOnInvoke: true })

    await expect(executePublicIpcTransportScenario(createFixture(host, transport), definition)).rejects.toThrow(
      new TckAssertionError(
        definition.id,
        'the transport event sink was rebound to a different client lease by the replayed route requests; only re-attaching may rebind it'
      )
    )
  })

  test('fails when the fixture never established an event sink to observe', async () => {
    const host = createIpcHostBoundary()
    const transport = createIpcTransport(host, {})
    host.unregisterSharedCallback()

    await expect(executePublicIpcTransportScenario(createFixture(host, transport), definition)).rejects.toThrow(
      new TckAssertionError(
        definition.id,
        'the transport event sink delivered no event before any request/response traffic, so this fixture never established a live event sink for the scenario to observe'
      )
    )
  })

  test('refuses traffic that would legitimately rebind the sink instead of proving the invariant', async () => {
    const host = createIpcHostBoundary()
    const transport = createIpcTransport(host, {})
    const fixture = createFixture(host, transport, [routeRequests[0], { kind: 'bootstrap' }])

    await expect(executePublicIpcTransportScenario(fixture, definition)).rejects.toThrow(
      new TckAssertionError(
        definition.id,
        'IPC transport adapter route request 1 has kind bootstrap; only route requests may be replayed against a bound event sink'
      )
    )
  })

  test('refuses an adapter that supplies no request sequence at all', async () => {
    const host = createIpcHostBoundary()
    const transport = createIpcTransport(host, {})

    await expect(
      executePublicIpcTransportScenario(createFixture(host, transport, [routeRequests[0]]), definition)
    ).rejects.toThrow(
      new TckAssertionError(
        definition.id,
        'IPC transport adapter supplied 1 route requests; a sequence of at least two is required'
      )
    )
  })

  test('refuses a fixture without a transport scenario adapter', async () => {
    const host = createIpcHostBoundary()

    await expect(
      executePublicIpcTransportScenario(
        { backend: {}, controller: createController(host), dispose: async () => cleanupRecord() },
        definition
      )
    ).rejects.toThrow(new TckAssertionError(definition.id, 'fixture lacks an IPC transport scenario adapter'))
  })
})

/**
 * Models a desktop webview host with exactly one shared event callback. Re-sending the sink drops
 * the previously registered callback permanently, which is what Tauri's Channel argument does.
 */
function createIpcHostBoundary() {
  let sink = null
  let boundSinkCount = 0
  let nextEventId = 1
  let lease = clientLease(1)
  return {
    get boundSinkCount() {
      return boundSinkCount
    },
    bindSink(listener) {
      sink = listener
      boundSinkCount += 1
    },
    unregisterSharedCallback() {
      sink = null
    },
    rebindClientLease() {
      lease = clientLease(2)
    },
    emit() {
      if (sink === null) {
        return
      }
      sink({
        rendererLease: lease,
        eventId: `event-${nextEventId++}`,
        streamId: 'connection-events-1',
        item: { kind: 'connection-lifecycle' }
      })
    }
  }
}

function createIpcTransport(host, behaviour) {
  const listeners = new Set()
  host.bindSink(event => {
    for (const listener of [...listeners]) {
      listener(event)
    }
  })
  return {
    listenerCount: () => listeners.size,
    invoke: async request => {
      if (behaviour.resendsEventSinkOnInvoke === true) {
        host.unregisterSharedCallback()
      }
      if (behaviour.rebindsClientLeaseOnInvoke === true) {
        host.rebindClientLease()
      }
      return { kind: request.kind, payload: {} }
    },
    subscribe: listener => {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },
    acknowledge: async () => {
      if (behaviour.resendsEventSinkOnAcknowledge === true) {
        host.unregisterSharedCallback()
      }
      return { kind: 'event.ack' }
    }
  }
}

function createFixture(host, transport, requests = routeRequests) {
  return {
    backend: {},
    controller: createController(host),
    featureScenarioAdapters: { ipcTransport: { transport, routeRequests: requests } },
    dispose: async () => cleanupRecord()
  }
}

function createController(host) {
  return {
    availableActions: ['emit-ipc-event'],
    now: () => 0,
    settle: promise => promise,
    flush: async () => undefined,
    perform: async action => {
      if (action !== 'emit-ipc-event') {
        throw new Error(`deterministic IPC transport boundary received unsupported action ${action}`)
      }
      host.emit()
    }
  }
}

function clientLease(ordinal) {
  return { leaseId: `client-lease-${ordinal}`, generation: `client-lease-generation-${ordinal}` }
}

function cleanupRecord() {
  return { state: 'released', failures: [] }
}
