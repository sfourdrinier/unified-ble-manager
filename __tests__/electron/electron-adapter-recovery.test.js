// __tests__/electron/electron-adapter-recovery.test.js
//
// Adapter off then on under a supervised connection across the Electron
// main <-> renderer IPC (protocol 4), over the real N-API addon's synthetic
// radio and the real main-process manager. After the loss, main rebinds the
// renderer to the backend's new attachment and announces it on the
// `attachment` stream (`backend-restarted`); the renderer adopts it (it can
// never pick one itself), and the supervisor reconnects when the adapter
// returns, including after more than the 10 s readiness window.
//
// Finding 196: every wait here is signal-driven. Supervisor progress is
// awaited on `supervisor.events` state transitions (never polled), and the
// readiness-window expiry is observed as the renderer's next
// `waitUntilReady` call (the expired window re-parking) with an assertion
// that no connect attempt escapes while parked — no fixed sleeps, no
// Date.now polling. A fail-safe timeout only converts a genuine stall into
// an explicit failure; it never synchronizes the happy path.

const h = require('../helpers/desktop-rust-core-harness')
const { ElectronMainBleBinding, ElectronMainBleRouter } = require('../../src/electron-main')
const { createElectronRendererBleManager } = require('../../src/electron-renderer')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../src/node-host-manager')
const { createConnectionSupervisor } = require('../../src/public/connection-supervisor')
const { IPC_PROTOCOL_VERSION, IPC_ATTACHMENT_STREAM_ID } = require('../../src/ipc/protocol')
const { opaqueId } = require('../../src/backend-contract/primitives')

jest.setTimeout(30_000)

const RETRY = Object.freeze({ initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 })
const WAIT_FAIL_SAFE_MS = 15000

async function flushMicrotasks(rounds = 16) {
  for (let attempt = 0; attempt < rounds; attempt += 1) {
    await new Promise(resolve => setImmediate(resolve))
  }
}

// One iterator over supervisor.events serves every wait. The underlying
// stream load-balances values across concurrent iterators instead of
// broadcasting, so two iterators would steal each other's transitions;
// with one tap, registration and the snapshot check are synchronous, so a
// transition that lands in between cannot be missed, and every state change
// publishes exactly one event, so no wait can resolve early or late.
function createSupervisorEventTap(supervisor, seen) {
  const iterator = supervisor.events[Symbol.asyncIterator]()
  const waiters = []
  let pumpDone = false
  const pump = async () => {
    try {
      while (true) {
        const next = await iterator.next()
        if (next.done) {
          pumpDone = true
          for (const waiter of waiters.splice(0, waiters.length)) {
            waiter.reject(new Error('supervisor events ended while waiting'))
          }
          return
        }
        if (next.value.kind === 'value') {
          seen.push(next.value.value)
          for (let index = waiters.length - 1; index >= 0; index -= 1) {
            if (waiters[index].matches(next.value.value)) {
              waiters.splice(index, 1)[0].resolve()
            }
          }
        }
      }
    } catch (error) {
      for (const waiter of waiters.splice(0, waiters.length)) {
        waiter.reject(error)
      }
    }
  }
  pump()
  return {
    waitFor(matches, describe, what) {
      if (matches.snapshot(supervisor.snapshot)) return Promise.resolve()
      if (pumpDone) return Promise.reject(new Error(`supervisor events ended while waiting for ${what}`))
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          const index = waiters.indexOf(waiter)
          if (index >= 0) waiters.splice(index, 1)
          reject(new Error(`timed out waiting for ${what}: ${JSON.stringify(describe())}`))
        }, WAIT_FAIL_SAFE_MS)
        const waiter = {
          matches: matches.event,
          resolve: () => {
            clearTimeout(timer)
            resolve()
          },
          reject: error => {
            clearTimeout(timer)
            reject(error)
          }
        }
        waiters.push(waiter)
        if (matches.snapshot(supervisor.snapshot)) {
          waiters.splice(waiters.indexOf(waiter), 1)
          clearTimeout(timer)
          resolve()
        }
      })
    },
    close() {
      iterator.return?.().catch(() => undefined)
    }
  }
}

function waitForSupervisorEvent(tap, supervisor, matches, describe, what) {
  return tap.waitFor(matches, describe, what)
}

function stateMatches(state, generation) {
  if (generation === undefined) return snapshot => snapshot.state === state
  return snapshot => snapshot.state === state && snapshot.connectionGeneration === generation
}

function eventMatches(state, generation) {
  if (generation === undefined) return value => value.kind === 'state' && value.state === state
  return value =>
    value.kind === 'state' && value.state === state && value.connectionGeneration === generation
}

function waitForSupervisorState(tap, supervisor, state, describe, generation = undefined) {
  return waitForSupervisorEvent(
    tap,
    supervisor,
    { snapshot: stateMatches(state, generation), event: eventMatches(state, generation) },
    describe,
    `supervisor state ${state}`
  )
}

function waitForSupervisorConnectedGeneration(tap, supervisor, excludedGeneration, describe) {
  return waitForSupervisorEvent(
    tap,
    supervisor,
    {
      snapshot: snapshot =>
        snapshot.state === 'connected' && snapshot.connectionGeneration !== excludedGeneration,
      event: value =>
        value.kind === 'state' &&
        value.state === 'connected' &&
        value.connectionGeneration !== excludedGeneration
    },
    describe,
    'supervisor reconnection on a new generation'
  )
}

/** One trusted renderer frame wired to the main binding in-process. */
function createRendererPort(binding) {
  const listeners = new Set()
  const sendWaiters = new Set()
  const mainFrame = Object.freeze({ processId: 10, routingId: 20 })
  const sent = []
  const sender = {
    mainFrame,
    trusted: {
      authenticatedClientId: opaqueId('renderer-client', 'client', 'electron:renderer'),
      authenticatedWindowScope: 'renderer-window',
      authenticatedSessionScope: 'renderer-session'
    },
    isDestroyed: () => false,
    once: () => undefined,
    on: () => undefined,
    removeListener: () => undefined,
    send(_channel, event) {
      sent.push(event)
      for (const listener of [...listeners]) listener(event)
      for (const waiter of [...sendWaiters]) waiter(event)
    }
  }
  const handle = request =>
    binding.port.handler({ sender, frameId: mainFrame.routingId, processId: mainFrame.processId }, request)
  // Resolve when main pushes a matching event to this renderer. Already-pushed
  // events are checked first, so a push that lands before subscribing cannot
  // be missed — same no-miss pattern as the supervisor event tap.
  function waitForPush(matches, what) {
    const found = sent.find(matches)
    if (found !== undefined) return Promise.resolve(found)
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        sendWaiters.delete(waiter)
        reject(new Error(`timed out waiting for renderer push ${what}`))
      }, WAIT_FAIL_SAFE_MS)
      const waiter = event => {
        if (matches(event)) {
          clearTimeout(timer)
          sendWaiters.delete(waiter)
          resolve(event)
        }
      }
      sendWaiters.add(waiter)
      const late = sent.find(matches)
      if (late !== undefined) {
        clearTimeout(timer)
        sendWaiters.delete(waiter)
        resolve(late)
      }
    })
  }
  return {
    sent,
    waitForPush,
    transport: {
      invoke: request => handle(request),
      subscribe(listener) {
        listeners.add(listener)
        return () => listeners.delete(listener)
      },
      acknowledge: (rendererLease, eventId) => handle({ kind: 'event.ack', rendererLease, eventId })
    }
  }
}

async function open(platform) {
  const harness = h.realBinding(platform)
  const provider = createTestDesktopRustCoreBackendProvider({
    platform,
    owner: `electron-recovery-${platform}`,
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: h.HOST_PLATFORM[platform]
  })
  const manager = await createNodeBleManagerFromProvider(provider, DESKTOP_RUST_CORE_PROFILES[platform].compatibility, {
    now: () => performance.now()
  })
  const stage = harness.opened[harness.opened.length - 1]
  const router = new ElectronMainBleRouter({
    manager,
    maximumMessageBytes: 262144,
    maximumOutstandingOperations: 16,
    maximumRetainedBytes: 1048576,
    publish: async () => 'terminalized'
  })
  const port = {
    handler: null,
    handle(_channel, handler) {
      this.handler = handler
    },
    removeHandler: () => undefined
  }
  const binding = new ElectronMainBleBinding({ router, port, authenticate: event => event.sender.trusted })
  binding.install()
  binding.port = port
  const renderer = createRendererPort(binding)
  const publicManager = await createElectronRendererBleManager({ transport: renderer.transport })
  await stage.stageServices('peer-1', h.hrmServices())
  const scan = await manager.scan(h.scanOptions())
  const iterator = scan.observations[Symbol.asyncIterator]()
  await stage.stageAdvertisement({ peerId: 'peer-1', rssi: -60, localName: 'Polar H10' })
  const observation = await h.nextValue(iterator, 5000)
  await iterator.return?.()
  await scan.stop()
  return { manager, stage, renderer, publicManager, binding, peerId: String(observation.device.id) }
}

describe('Electron main <-> renderer: adapter off and on', () => {
  test('speaks IPC protocol 4', () => {
    expect(IPC_PROTOCOL_VERSION).toBe(4)
    expect(IPC_ATTACHMENT_STREAM_ID).toBe('attachment')
  })

  test('main rebinds the renderer and the supervisor reconnects after a >10 s outage', async () => {
    const { stage, renderer, publicManager, binding, peerId } = await open('corebluetooth')
    const connectSpy = jest.spyOn(publicManager, 'connect')
    const supervisor = createConnectionSupervisor(publicManager, peerId, { retry: RETRY })
    const seen = []
    const tap = createSupervisorEventTap(supervisor, seen)
    const describe = () => ({
      snapshot: supervisor.snapshot,
      cleanups: seen.filter(e => e.cleanup).map(e => e.cleanup)
    })
    supervisor.start()
    await waitForSupervisorState(tap, supervisor, 'connected', describe)
    const first = supervisor.snapshot.connectionGeneration

    // The renderer's readiness clock runs 11 s ahead once the adapter is off.
    // A plain assignment (not jest.spyOn): the mocked clock is read on every
    // control-plane tick, and a jest mock object carries resettable state
    // that anything sharing the registry can disturb mid-test. A closure has
    // no such state to disturb.
    let offset = 0
    const base = performance.now()
    const started = process.hrtime.bigint()
    const realNow = performance.now
    const hadOwnNow = Object.prototype.hasOwnProperty.call(performance, 'now')
    performance.now = () => base + Number(process.hrtime.bigint() - started) / 1e6 + offset
    try {
      await stage.stageAdapterState('powered-off', true)
      await waitForSupervisorState(tap, supervisor, 'waiting-for-gate', describe)
      // Main announced the rebind on the attachment stream, for this lease only.
      // The gate parking and the rebind push are independent signals — the
      // supervisor can park before the push lands — so await the push itself.
      await renderer.waitForPush(
        event =>
          event.streamId === IPC_ATTACHMENT_STREAM_ID &&
          event.item.kind === 'value' &&
          event.item.value.kind === 'backend-restarted',
        'attachment backend-restarted'
      )
      const announced = renderer.sent.filter(event => event.streamId === IPC_ATTACHMENT_STREAM_ID)
      expect(announced).toHaveLength(1)
      expect(announced[0].item).toMatchObject({ kind: 'value', value: { kind: 'backend-restarted' } })
      const rebound = announced[0].item.value
      expect(rebound.attachmentId).toBe(rebound.attachment.attachmentId)
      expect(rebound.previousAttachmentId).not.toBe(rebound.attachmentId)
      // The adapter stays off past the readiness window (10 s): the in-flight
      // readiness wait expires and the gate re-parks instead of failing. The
      // re-park is the next waitUntilReady call; no connect may escape while
      // parked.
      const originalWaitUntilReady = publicManager.adapter.waitUntilReady.bind(publicManager.adapter)
      const waitUntilReadySpy = jest.spyOn(publicManager.adapter, 'waitUntilReady')
      // The spy is installed after the in-flight wait started, so it observes
      // only the re-park below: exactly one more call, no connect escaping.
      const waitsBefore = waitUntilReadySpy.mock.calls.length
      const connectsBefore = connectSpy.mock.calls.length
      let rearmResolve = null
      const rearmed = new Promise(resolve => {
        rearmResolve = resolve
      })
      waitUntilReadySpy.mockImplementation((...args) => {
        if (rearmResolve !== null) {
          const resolve = rearmResolve
          rearmResolve = null
          resolve()
        }
        return originalWaitUntilReady(...args)
      })
      offset += 11_000
      await stage.stageAdapterState('powered-off', true)
      await rearmed
      await flushMicrotasks()
      expect(waitUntilReadySpy.mock.calls.length).toBe(waitsBefore + 1)
      expect(connectSpy.mock.calls.length).toBe(connectsBefore)
      expect(supervisor.snapshot.state).toBe('waiting-for-gate')

      await stage.stageAdapterState('powered-on', true)
      await waitForSupervisorState(tap, supervisor, 'connected', describe)
      expect(supervisor.snapshot.connectionGeneration).not.toBe(first)

      // Main and the renderer follow the new generation: a later link loss on
      // the new connection is observed and the supervisor reconnects again.
      const recovered = supervisor.snapshot.connectionGeneration
      await stage.stageLinkLoss('peer-1')
      await waitForSupervisorConnectedGeneration(tap, supervisor, recovered, describe)
    } finally {
      if (hadOwnNow) {
        performance.now = realNow
      } else {
        delete performance.now
      }
    }
    expect((await supervisor.stop()).state).toBe('released')
    tap.close()
    expect(await publicManager.destroy()).toEqual({ state: 'released', failures: [] })
    await binding.dispose?.()
  })
})
