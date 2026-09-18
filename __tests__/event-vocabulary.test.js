// __tests__/event-vocabulary.test.js
//
// One name per physical event on every backend, and one supervisor decision
// per event (owner decision, 5.0). The table lives in
// src/backend-contract/event-vocabulary.ts; this test pins its two copies
// (docs/UNIFIED_SEMANTICS.md, the Rust fixture) and feeds every backend's
// names through the shared connection supervisor.
//
// Regenerate the pinned copies: UBM_WRITE_EVENT_VOCABULARY=1 pnpm exec jest --config jest.package.config.js __tests__/event-vocabulary.test.js

const fs = require('node:fs')
const path = require('node:path')

const {
  EVENT_VOCABULARY,
  VOCABULARY_BACKENDS,
  eventNamesFor,
  eventVocabularyFixture,
  renderEventVocabularyTable
} = require('../src/backend-contract/event-vocabulary')
const { createConnectionSupervisor } = require('../src/public/connection-supervisor')
const { BleError } = require('../src/public/errors')

const ROOT = path.join(__dirname, '..')
const DOC = path.join(ROOT, 'docs', 'UNIFIED_SEMANTICS.md')
const FIXTURE = path.join(ROOT, 'crates', 'ubm-desktop', 'tests', 'fixtures', 'event-vocabulary.json')
const BEGIN = '<!-- EVENT-VOCABULARY:BEGIN (generated from src/backend-contract/event-vocabulary.ts) -->'
const END = '<!-- EVENT-VOCABULARY:END -->'
const WRITE = process.env.UBM_WRITE_EVENT_VOCABULARY === '1'

function docBlock() {
  return `${BEGIN}\n${renderEventVocabularyTable()}\n${END}`
}

describe('the event vocabulary cannot drift', () => {
  test('docs/UNIFIED_SEMANTICS.md embeds the table', () => {
    const doc = fs.readFileSync(DOC, 'utf8')
    const start = doc.indexOf(BEGIN)
    const end = doc.indexOf(END)
    if (WRITE && start >= 0 && end > start) {
      fs.writeFileSync(DOC, `${doc.slice(0, start)}${docBlock()}${doc.slice(end + END.length)}`)
      return
    }
    expect(start).toBeGreaterThanOrEqual(0)
    expect(doc.slice(start, end + END.length)).toBe(docBlock())
  })

  test('the Rust mapping tests read the same table', () => {
    if (WRITE) fs.writeFileSync(FIXTURE, eventVocabularyFixture())
    expect(fs.readFileSync(FIXTURE, 'utf8')).toBe(eventVocabularyFixture())
  })

  test('every backend differs only with a stated reason', () => {
    for (const entry of EVENT_VOCABULARY) {
      for (const [backend, exception] of Object.entries(entry.differs)) {
        expect(VOCABULARY_BACKENDS).toContain(backend)
        expect(exception.why.length).toBeGreaterThan(20)
      }
    }
  })
})

function domainOf(code) {
  const prefix = code.split('.')[0]
  return {
    connection: 'connection',
    operation: 'connection',
    peer: 'connection',
    platform: 'platform',
    gatt: 'gatt',
    adapter: 'adapter'
  }[prefix]
}

function bleError(names, operation) {
  return new BleError(names.error, domainOf(names.error), operation, { retryability: names.retryability })
}

function wait(milliseconds = 5) {
  return new Promise(resolve => setTimeout(resolve, milliseconds))
}

/** A lifecycle stream that emits `connected`, then (when told) the event's transition. */
function lifecycleStream() {
  const queue = [
    { kind: 'connection-lifecycle', previous: 'connecting', current: 'connected', cause: 'connected', sequence: 1 }
  ]
  let wake = null
  let closed = false
  return {
    push(event) {
      queue.push(event)
      if (wake !== null) wake()
    },
    [Symbol.asyncIterator]() {
      return {
        next: async () => {
          while (queue.length === 0 && !closed) {
            await new Promise(resolve => {
              wake = resolve
            })
            wake = null
          }
          if (queue.length > 0) return { done: false, value: queue.shift() }
          return { done: true, value: undefined }
        },
        return: async () => {
          closed = true
          if (wake !== null) wake()
          return { done: true, value: undefined }
        },
        [Symbol.asyncIterator]() {
          return this
        }
      }
    }
  }
}

function connection() {
  return {
    peer: { id: 'peer-v', name: null, rssi: null, reference: null, sources: [], lastAdvertisement: null },
    lifecycleEvents: lifecycleStream(),
    release: jest.fn(async () => ({ state: 'released', failures: [] })),
    disconnect: jest.fn(async () => ({ state: 'released', failures: [] }))
  }
}

/**
 * Drive the shared supervisor through one physical event as `backend` names
 * it, and observe what it did: stopped, reconnected, or waited for the
 * adapter first. An adapter event also turns the fake adapter off until the
 * supervisor waits for it, as the platform would.
 */
async function observeDecision(entry, backend) {
  const names = eventNamesFor(entry.event, backend)
  const adapterEvent = entry.event.startsWith('adapter-loss')
  let adapterOn = true
  const adapterState = jest.fn(async () => ({
    availability: 'available',
    authorization: 'granted',
    power: adapterOn ? 'on' : 'off',
    backendGeneration: '1',
    updatedAt: 1,
    safeReason: null
  }))
  const waitUntilReady = jest.fn(async () => {
    adapterOn = true
    return adapterState()
  })
  const connections = []
  let connectCalls = 0
  const ble = {
    adapter: { state: adapterState, waitUntilReady },
    capabilities: { supports: () => false },
    peers: { resolve: jest.fn(async () => null) },
    connect: jest.fn(async () => {
      connectCalls += 1
      if (!adapterOn) throw new BleError('adapter.powered-off', 'adapter', 'connection.connect')
      if (entry.supervisor.context === 'connect' && connectCalls === 1) {
        throw bleError(names, 'connection.connect')
      }
      const created = connection()
      connections.push(created)
      return created
    })
  }
  let configureCalls = 0
  const supervisor = createConnectionSupervisor(ble, `peer-${entry.event}-${backend}`, {
    retry: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 },
    configure: async () => {
      configureCalls += 1
      if (entry.supervisor.context === 'configure' && configureCalls === 1) {
        if (adapterEvent) adapterOn = false
        throw bleError(names, 'gatt.discover')
      }
      return `session-${configureCalls}`
    }
  })
  supervisor.start()
  for (let turn = 0; turn < 100 && supervisor.snapshot.state !== 'connected'; turn += 1) await wait()
  if (entry.supervisor.context === 'lifecycle') {
    if (adapterEvent) adapterOn = false
    connections[0].lifecycleEvents.push({
      kind: 'connection-lifecycle',
      previous: 'connected',
      current: names.lifecycle.current,
      cause: names.lifecycle.cause,
      sequence: 2
    })
    for (let turn = 0; turn < 100 && connections.length < 2; turn += 1) await wait()
  }
  for (let turn = 0; turn < 100; turn += 1) {
    const state = supervisor.snapshot.state
    if (state === 'stopped' || (state === 'connected' && ble.connect.mock.calls.length >= 2)) break
    await wait()
  }
  const state = supervisor.snapshot.state
  const decision =
    state === 'stopped'
      ? 'stop'
      : state === 'connected' && ble.connect.mock.calls.length >= 2
        ? waitUntilReady.mock.calls.length > 0
          ? 'wait-for-adapter'
          : 'reconnect'
        : `unsettled:${state}`
  if (state !== 'stopped') await supervisor.stop()
  return decision
}

describe('the connection supervisor decides the same for the same event on every backend', () => {
  const cases = EVENT_VOCABULARY.flatMap(entry => VOCABULARY_BACKENDS.map(backend => [entry.event, backend, entry]))
  test.each(cases)('%s on %s', async (_event, backend, entry) => {
    await expect(observeDecision(entry, backend)).resolves.toBe(entry.supervisor.decision)
  })
})
