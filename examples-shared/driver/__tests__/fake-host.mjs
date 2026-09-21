// A scripted DriverHost for scenario tests: a manager double that records
// every public call in order, so tests assert what the shared scenarios ask
// of the library on every host (scan vs chooser, gesture gating, ordering).

import { createFakeRuntime } from './fake-runtime.mjs'

export function stream() {
  const items = []
  let wake = null
  let ended = false
  return {
    push(item) {
      items.push(item)
      wake?.()
    },
    end() {
      ended = true
      wake?.()
    },
    async *[Symbol.asyncIterator]() {
      for (;;) {
        if (items.length > 0) {
          yield items.shift()
          continue
        }
        if (ended) return
        await new Promise(resolve => (wake = resolve))
        wake = null
      }
    }
  }
}

const released = () => ({ state: 'released', failures: [] })

const SCANNING = { 'discovery:continuous-scan': 'supported', 'discovery:system-chooser': 'unsupported' }
const CHOOSING = { 'discovery:continuous-scan': 'unsupported', 'discovery:system-chooser': 'supported' }

export function createFakeManager({
  discovery = 'continuous-scan',
  capabilities = {},
  reads = {},
  connectFailures = [],
  peerName = 'Polar H10 1234',
  releaseError = null,
  managerId = 'fake-manager',
  // Finding 185: a process-shared stuck-scan registry modelling the process
  // host's scan lease across scenario managers (every scenario run creates
  // its own manager). The doubles model the fixed provider contract —
  // `find` heals a retained membership before starting, `destroy` clears
  // the owner's stuck scan like the fixed dispose — while the provider
  // tests prove the provider honors it. `failFindStop` leaves the run's
  // scan open while `find` still reports its cleanup failure loudly.
  sharedScans = null,
  presence = 'supported',
  restored = 'supported',
  restoredPeers = null,
  // The host continuation API: null (absent, like a plain BleManager host),
  // 'unsupported' (present but refused by the platform), or { status, claim }.
  continuation = null,
  failFindStop = 0,
  // Models a dispose that also failed (finding 185 residual): the stuck
  // scan survives destroy with visible debt, and the next find heals it.
  failDispose = 0
} = {}) {
  let pendingFindStopFailures = failFindStop
  let pendingDisposeFailures = failDispose
  const pendingConnectFailures = [...connectFailures]
  const reported = { ...(discovery === 'system-chooser' ? CHOOSING : SCANNING), ...capabilities }
  const calls = []
  const subscriptions = []
  const lifecycle = stream()
  const characteristic = (service, uuid) => ({
    async read() {
      calls.push(`read ${uuid}`)
      const bytes = reads[uuid]
      if (bytes === undefined) throw Object.assign(new Error(`no ${uuid}`), { code: 'gatt.attribute-not-found' })
      return bytes
    },
    async write(bytes) {
      calls.push(`write ${uuid} ${[...bytes].map(byte => byte.toString(16).padStart(2, '0')).join('')}`)
      return { state: 'written' }
    },
    async subscribe(options) {
      calls.push(`subscribe ${uuid}`)
      const values = stream()
      const subscription = {
        uuid,
        values,
        requestedDelivery: options.delivery ?? null,
        effectiveDelivery: 'notification',
        async remove() {
          calls.push(`unsubscribe ${uuid}`)
          values.end()
          return released()
        }
      }
      subscriptions.push(subscription)
      return subscription
    }
  })
  const connection = {
    connectionGeneration: 'gen-1',
    lifecycleEvents: lifecycle,
    controls: {},
    async discover() {
      calls.push('discover')
      return { generation: 'db-1', services: [{ uuid: '180d' }], characteristic }
    },
    async release() {
      calls.push('connection.release')
      lifecycle.end()
      if (releaseError !== null) throw releaseError
      return released()
    }
  }
  const peer = { id: 'peer-h10', name: peerName, rssi: -50, reference: null, sources: ['test'] }
  const unsupportedPresence = (peerId, operation) =>
    Object.assign(new Error(`presence observation is not supported on this platform (peer ${peerId})`), {
      code: 'capability.unsupported',
      operation,
      platform: 'fake-april'
    })
  const presenceApi =
    presence === 'absent'
      ? null
      : {
          async observe({ peerId }) {
            calls.push(`observe-presence ${peerId}`)
            if (presence === 'unsupported') throw unsupportedPresence(peerId, 'fake.presence.observe')
            return { state: 'observing' }
          },
          async unobserve({ peerId }) {
            calls.push(`unobserve-presence ${peerId}`)
            if (presence === 'unsupported') throw unsupportedPresence(peerId, 'fake.presence.unobserve')
            return { state: 'idle' }
          }
        }
  const unsupportedRestored = operation =>
    Object.assign(new Error('restored peers are not supported on this platform'), {
      code: 'capability.unsupported',
      operation,
      platform: 'fake-april'
    })
  const peersApi = {
    async restored() {
      calls.push('peers.restored')
      if (restored === 'unsupported') throw unsupportedRestored('fake.peers.restored')
      return restoredPeers ?? [{ ...peer }]
    }
  }
  const unsupportedContinuation = operation =>
    Object.assign(new Error('continuation is not supported on this platform'), {
      code: 'capability.unsupported',
      operation,
      platform: 'fake-april'
    })
  const continuationApi =
    continuation === null
      ? null
      : continuation === 'unsupported'
        ? {
            async status() {
              calls.push('continuation.status')
              throw unsupportedContinuation('fake.continuation.status')
            },
            async claim() {
              calls.push('continuation.claim')
              throw unsupportedContinuation('fake.continuation.claim')
            }
          }
        : {
            async status() {
              calls.push('continuation.status')
              return continuation.status
            },
            async claim() {
              calls.push('continuation.claim')
              return continuation.claim
            }
          }
  const manager = {
    ...(presenceApi === null ? {} : { presence: presenceApi }),
    ...(continuationApi === null ? {} : { continuation: continuationApi }),
    peers: peersApi,
    discovery: { kind: discovery },
    adapter: {
      async state() {
        return { availability: 'available', power: 'on', authorization: 'granted' }
      },
      async waitUntilReady(options) {
        calls.push(`waitUntilReady ${options.operation}`)
        return { availability: 'available', power: 'on', authorization: 'granted' }
      }
    },
    capabilities: {
      supports(id) {
        const state = reported[id] ?? 'unsupported'
        return state === 'supported' || state === 'limited'
      },
      get(id) {
        const state = reported[id]
        return state === undefined ? undefined : { id, state, limitations: state === 'supported' ? [] : [{ code: `${id}-${state}` }] }
      },
      require(id) {
        const state = reported[id] ?? 'unsupported'
        if (state === 'unsupported') throw Object.assign(new Error(`${id} unsupported`), { code: 'capability.unsupported' })
        return { id, state, limitations: [] }
      },
      list: () => []
    },
    async find(options) {
      calls.push(`find ${JSON.stringify(options.query)}`)
      if (sharedScans !== null && sharedScans.has('stuck')) {
        calls.push('heal stuck scan')
        sharedScans.delete('stuck')
      }
      if (pendingFindStopFailures > 0) {
        pendingFindStopFailures -= 1
        sharedScans?.set('stuck', { owner: managerId })
        throw Object.assign(new Error('find cleanup: scan stop failed'), { code: 'platform.failure' })
      }
      return peer
    },
    async choose(options) {
      calls.push(`choose ${JSON.stringify({ filters: options.filters, optionalServices: options.optionalServices })}`)
      return peer
    },
    async connect(target, options) {
      calls.push(`connect ${options.intent}`)
      const failure = pendingConnectFailures.shift()
      if (failure !== undefined) throw failure
      return connection
    },
    async destroy() {
      calls.push('manager.destroy')
      if (pendingDisposeFailures > 0) {
        pendingDisposeFailures -= 1
        return {
          state: 'release-failed',
          failures: [{ resourceKind: 'scan', error: { code: 'platform.failure' } }]
        }
      }
      if (sharedScans?.get('stuck')?.owner === managerId) sharedScans.delete('stuck')
      return released()
    }
  }
  return { manager, calls, subscriptions }
}

export function createFakeHost({ manager, userGesture = null, appState = null, host = 'web', platform = 'macos', backgroundFeature = 'background:desktop-maintain-connection', adapterHostManager }) {
  const identity = { host, platform, backend: `${host}/fake`, model: 'fake', osVersion: '0', appBuild: {} }
  const runtime = createFakeRuntime(`${host}/${platform}`)
  return {
    identity,
    runtime,
    appState,
    userGesture,
    createManager: async () => adapterHostManager(manager, backgroundFeature)
  }
}

export const settle = async (turns = 20) => {
  for (let turn = 0; turn < turns; turn += 1) await new Promise(resolve => setImmediate(resolve))
}
