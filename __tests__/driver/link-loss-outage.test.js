// __tests__/driver/link-loss-outage.test.js
//
// Finding 162: the link-loss outage must complete no matter which event
// carries the reconnect generation first. iOS observes the supervisor
// `connected` event before the first post-reconnect value; on Android the
// first value with the new generation can arrive first (batched drain
// delivery), and a late duplicate loss for the old generation must not
// open a ghost outage. The outage record must fill `reconnectMs` and
// `firstValueAfterReconnectMs` in every ordering.

const { LinkLossScenario } = require('../../examples-shared/driver/scenarios/link-loss.ts')

let nowMs = 0
const host = {
  runtime: {
    host: 'test-harness',
    now: () => nowMs,
    schedule: () => () => {},
    log: () => {}
  },
  identity: { backend: 'test-harness' },
  createManager: async () => {
    throw new Error('no radio in this harness')
  }
}

class ExposedLinkLoss extends LinkLossScenario {
  seed(patch) {
    this.replace({ ...this.snapshot(), ...patch })
  }
  markRunning() {
    this.runAbort = new AbortController()
  }
  lifecycle(event) {
    this.onLifecycle(event)
  }
  lifecycleEnded(generation, error) {
    this.onLifecycleEnded(generation, error)
  }
  supervisor(event) {
    this.onSupervisor(event)
  }
  value(observation) {
    this.onHeartRateValue(observation)
  }
}

function scenario() {
  const next = new ExposedLinkLoss(host)
  next.markRunning()
  next.seed({
    phase: 'streaming',
    connectionGeneration: 'gen-1',
    valueCount: 3,
    lastValueAtMs: 900,
    supervisorAttempt: 1
  })
  return next
}

function lostEvent(generation) {
  return {
    previous: 'connected',
    current: 'lost',
    cause: 'peer-link-loss',
    connectionGeneration: generation,
    sequence: 2
  }
}

function supervisorConnected(generation, attempt = 5) {
  return {
    kind: 'state',
    supervisorId: 'supervisor-1',
    previous: 'configuring',
    state: 'connected',
    attempt,
    connectionGeneration: generation,
    timestamp: nowMs,
    delayMs: null,
    gateDecision: null,
    error: null,
    session: null
  }
}

function valueAt(atMs, generation) {
  return { atMs, gapMs: null, connectionGeneration: generation }
}

describe('link-loss outage record (finding 162)', () => {
  test('supervisor-connected first (iOS ordering) fills reconnectMs and time-to-first-value', () => {
    const next = scenario()
    nowMs = 2000
    next.lifecycle(lostEvent('gen-1'))
    expect(next.snapshot().openOutage).toMatchObject({ generationBefore: 'gen-1' })

    nowMs = 125293
    next.supervisor(supervisorConnected('gen-2'))
    expect(next.snapshot().openOutage).toMatchObject({
      reconnectedAtMs: 125293,
      generationAfter: 'gen-2',
      reconnectMs: 123293
    })

    nowMs = 125442
    next.value(valueAt(125442, 'gen-2'))
    const { openOutage, outages } = next.snapshot()
    expect(openOutage).toBeNull()
    expect(outages).toHaveLength(1)
    expect(outages[0]).toMatchObject({
      reconnectMs: 123293,
      firstValueAtMs: 125442,
      firstValueAfterReconnectMs: 149,
      outageMs: 123442
    })
  })

  test('first value with the new generation wins (Android ordering) still fills the record', () => {
    const next = scenario()
    nowMs = 2000
    next.lifecycle(lostEvent('gen-1'))
    expect(next.snapshot().openOutage).toMatchObject({ generationBefore: 'gen-1' })

    // The first post-reconnect value arrives before the supervisor
    // `connected` state event is observed: the value itself is the
    // reconnect observation, so the record fills from it instead of
    // staying null forever.
    nowMs = 125293
    next.value(valueAt(125293, 'gen-2'))
    const { openOutage, outages } = next.snapshot()
    expect(openOutage).toBeNull()
    expect(outages).toHaveLength(1)
    expect(outages[0]).toMatchObject({
      reconnectedAtMs: 125293,
      generationAfter: 'gen-2',
      reconnectMs: 123293,
      firstValueAtMs: 125293,
      firstValueAfterReconnectMs: 0,
      outageMs: 123293
    })

    // The late supervisor `connected` for the same generation changes nothing.
    nowMs = 125300
    next.supervisor(supervisorConnected('gen-2'))
    expect(next.snapshot().outages).toHaveLength(1)
  })

  test('a lifecycle-connected with the new generation fills the reconnect before values', () => {
    const next = scenario()
    nowMs = 2000
    next.lifecycle(lostEvent('gen-1'))

    nowMs = 125293
    next.lifecycle({
      previous: 'connecting',
      current: 'connected',
      cause: 'connected',
      connectionGeneration: 'gen-2',
      sequence: 9
    })
    expect(next.snapshot().openOutage).toMatchObject({
      reconnectedAtMs: 125293,
      generationAfter: 'gen-2',
      reconnectMs: 123293
    })

    nowMs = 125400
    next.value(valueAt(125400, 'gen-2'))
    expect(next.snapshot().outages[0]).toMatchObject({
      reconnectMs: 123293,
      firstValueAfterReconnectMs: 107
    })
  })

  test('a late duplicate loss for the superseded generation opens no ghost outage', () => {
    const next = scenario()
    nowMs = 2000
    next.lifecycle(lostEvent('gen-1'))
    nowMs = 125293
    next.supervisor(supervisorConnected('gen-2'))
    // The reconnect configures the new generation, as configureLink does.
    next.seed({ connectionGeneration: 'gen-2' })
    nowMs = 125442
    next.value(valueAt(125442, 'gen-2'))
    expect(next.snapshot().outages).toHaveLength(1)

    // A duplicate loss signal for the old generation arrives after the
    // reconnect: it belongs to the completed outage, not a new one.
    nowMs = 130000
    next.lifecycle(lostEvent('gen-1'))
    const { openOutage, outages } = next.snapshot()
    expect(openOutage).toBeNull()
    expect(outages).toHaveLength(1)
  })
})
