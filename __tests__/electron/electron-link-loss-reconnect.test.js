'use strict'

// Finding 211: after a simulator drop-link, the Electron supervisor must
// reconnect like every other host. Main auto-removes subscriptions when
// their source ends terminal (link loss), so the supervisor's explicit
// release was denied `ownership.denied/subscription-ownership` and the
// supervisor parked in `cleanup-failed` instead of backing off and
// reconnecting.

const h = require('../helpers/desktop-rust-core-harness')
const { ElectronMainBleBinding, ElectronMainBleRouter } = require('../../src/electron-main')
const { createElectronRendererBleManager } = require('../../src/electron-renderer')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../src/node-host-manager')
const { createConnectionSupervisor } = require('../../src/public/connection-supervisor')
const { opaqueId } = require('../../src/backend-contract/primitives')
const { randomBytes: nodeRandomBytes } = require('node:crypto')

jest.setTimeout(60000)

const RETRY = Object.freeze({ initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 })
const WAIT_FAIL_SAFE_MS = 20000

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
  return tap.waitFor(
    { snapshot: stateMatches(state, generation), event: eventMatches(state, generation) },
    describe,
    `supervisor state ${state}`
  )
}

function waitForSupervisorConnectedGeneration(tap, supervisor, excludedGeneration, describe) {
  return tap.waitFor(
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
  return {
    sent,
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
    owner: `electron-link-loss-${platform}`,
    now: () => performance.now(),
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: h.HOST_PLATFORM[platform]
  })
  // The jest sandbox has no WebCrypto: supply the host CSPRNG explicitly,
  // as production hosts without it must.
  const manager = await createNodeBleManagerFromProvider(provider, DESKTOP_RUST_CORE_PROFILES[platform].compatibility, {
    now: () => performance.now(),
    randomBytes: length => nodeRandomBytes(length)
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

describe('Finding 211: Electron supervisor reconnects after a drop-link', () => {
  test('a subscribed supervisor survives link loss and reconnects on a new generation', async () => {
    const { stage, publicManager, binding, peerId } = await open('corebluetooth')
    const abort = new AbortController()
    const supervisor = createConnectionSupervisor(publicManager, peerId, {
      retry: RETRY,
      configure: async connection => {
        const database = await connection.discover({ signal: abort.signal, timeoutMs: 10000 })
        const subscription = await database
          .characteristic(h.HRM_SERVICE, h.HRM_MEASUREMENT)
          .subscribe({ signal: abort.signal, timeoutMs: 10000 })
        return { subscription }
      },
      disposeSession: async session => {
        await session.subscription.remove()
      }
    })
    const seen = []
    const tap = createSupervisorEventTap(supervisor, seen)
    const describe = () => ({
      snapshot: supervisor.snapshot,
      cleanups: seen.filter(event => event.cleanup !== undefined && event.cleanup !== null)
    })
    supervisor.start()
    await waitForSupervisorState(tap, supervisor, 'connected', describe)
    const first = supervisor.snapshot.connectionGeneration
    expect(first).toEqual(expect.any(String))

    await stage.stageLinkLoss('peer-1')
    await waitForSupervisorConnectedGeneration(tap, supervisor, first, describe)
    expect(supervisor.snapshot.connectionGeneration).not.toBe(first)

    expect((await supervisor.stop()).state).toBe('released')
    tap.close()
    abort.abort()
    expect(await publicManager.destroy()).toEqual({ state: 'released', failures: [] })
    await binding.dispose?.()
  })
})
