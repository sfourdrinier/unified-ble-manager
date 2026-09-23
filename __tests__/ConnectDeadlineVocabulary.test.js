// __tests__/ConnectDeadlineVocabulary.test.js
//
// Finding 161, cross-host pin: a dispatched connect whose deadline expires
// before any link came up is the peer not answering — `connection.failed`
// (`caller-decides`) on every host path. The native core already reports that
// name (pinned by `a_connect_deadline_without_a_link_is_connection_failed`
// in `crates/ubm-desktop` and by `__tests__/event-vocabulary.test.js` across
// all six backends); this file pins the TS-enforced side — the shared IPC
// deadline (Tauri + Electron renderer), where the timer wins the race against
// the native core — and the Web backend, which enforces its own deadline.
// Every other operation keeps `operation.timed-out`; a caller AbortSignal
// abort stays `operation.aborted`.

'use strict'

const { BUILT_IN_FEATURE_IDS } = require('../src/backend-contract/capabilities')

// -- Tauri fixture (mirrors __tests__/TauriManager.test.js) ------------------

class FakeChannel {
  constructor() {
    this.onmessage = null
    FakeChannel.current = this
  }

  emit(message) {
    this.onmessage?.(message)
  }
}

function negotiated(axis) {
  const selected = { axis, value: axis === 'ipc-protocol' ? 4 : 1 }
  const range = { axis, minimum: selected, maximum: selected }
  return { axis, selected, localRange: range, remoteRange: range }
}

function tauriCapabilityDescriptor(id, scenario, state = 'limited') {
  const limitation = {
    code: state === 'limited' ? 'deterministic-only' : 'not-implemented',
    explanation:
      state === 'limited'
        ? 'The fixture exposes deterministic host evidence only.'
        : 'The fixture does not implement this capability.',
    affectedGuarantee: state === 'limited' ? 'Physical-radio qualification is not claimed.' : 'support'
  }
  const schemaRange = {
    axis: 'capability-schema',
    minimum: { axis: 'capability-schema', value: 1 },
    maximum: { axis: 'capability-schema', value: 1 }
  }
  return {
    id,
    state,
    selectedSchemaRange: schemaRange,
    implementationOrigin: 'backend-native',
    tck: { suiteId: 'capability.catalog-v2', requiredScenarioIds: [scenario], contractRange: schemaRange },
    evidence: {
      receiptId: `fixture-${id}`,
      evidenceLevel: state === 'limited' ? 'deterministic' : 'blocked',
      implementationVersion: 'fixture-v2',
      sourceDigest: `fixture-${id}`,
      scenarioIds: [scenario],
      limitations: [limitation]
    },
    limitations: [limitation],
    limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
  }
}

function tauriBootstrap() {
  const backendGeneration = 'backend-generation-1'
  const attachment = {
    attachmentId: 'tauri-attachment-1',
    backendInstanceId: 'tauri-btleplug-1',
    backendGeneration,
    adapter: {
      adapterId: 'tauri-adapter-1',
      displayName: 'Bluetooth',
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        heard: null,
        backendGeneration,
        updatedAt: 1,
        safeReason: null
      },
      adapterGeneration: 'adapter-generation-1',
      limitations: []
    }
  }
  const entries = [
    ['discovery:continuous-scan', 'scan.owner-join-authority-and-signature'],
    ['connection:direct', 'connection.lease-joins-borrowing-transfer-and-revocation'],
    ['connection:rssi', 'connection.rssi-and-att-mtu-capability-contract'],
    ['gatt:descriptors', 'gatt.descriptor-discovery-read-write'],
    ['gatt:indications', 'gatt.reads-descriptors-write-policy-and-dispatched-cancellation']
  ]
  const metadata = new Map(entries)
  return {
    attachment,
    attachmentId: attachment.attachmentId,
    versions: {
      backendContract: negotiated('backend-contract'),
      capabilitySchema: negotiated('capability-schema'),
      eventSchema: negotiated('event-schema'),
      traceFormat: negotiated('trace-format'),
      ipcProtocol: negotiated('ipc-protocol')
    },
    capabilities: {
      schemaVersion: 2,
      backendGeneration,
      descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => {
        const scenario = metadata.get(id)
        return tauriCapabilityDescriptor(
          id,
          scenario ?? 'capability.truth-limits-evidence-and-binding',
          scenario === undefined ? 'unsupported' : 'limited'
        )
      })
    },
    core: {
      contractRevision: 'C-UBM.0.1.2-DRAFT',
      implementationVersion: '5.0.0-rc.5'
    },
    renderer: {
      clientId: 'tauri-client-1',
      windowScope: 'main',
      sessionScope: 'session-1'
    },
    rendererLease: { leaseId: 'tauri-lease-1', generation: 'tauri-lease-generation-1' }
  }
}

// A native side that never answers a dispatched connect: the TS deadline is
// the only answer, so the test proves which name the TS side reports.
function hangingTauriInvoke(onCancel) {
  let resolveConnect
  let connectDispatched
  const connectSeen = new Promise(resolve => {
    connectDispatched = resolve
  })
  const invoke = jest.fn(async (_command, args) => {
    const request = args.request
    if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: tauriBootstrap() }
    if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
    if (request.kind === 'event.ack') return { kind: 'event.ack' }
    if (request.envelope.command === 'operation.cancel') {
      onCancel?.(request.envelope)
      resolveConnect({
        kind: 'failure',
        error: {
          code: 'operation.aborted',
          domain: 'ipc',
          operation: 'tauri.connect',
          platform: null,
          retryability: 'caller-decides'
        }
      })
      return { kind: 'route', payload: { state: 'cancellation-requested' } }
    }
    connectDispatched(request.envelope)
    return new Promise(resolve => {
      resolveConnect = resolve
    })
  })
  return { invoke, connectSeen }
}

// -- Electron fixture (mirrors __tests__/ElectronPublicManager.test.js) ------

function electronBootstrap() {
  const attachment = {
    attachmentId: 'electron-attachment-1',
    backendInstanceId: 'electron-backend-1',
    backendGeneration: 'electron-generation-1',
    adapter: {
      adapterId: 'electron-adapter-1',
      displayName: 'test',
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        heard: null,
        backendGeneration: 'electron-generation-1',
        updatedAt: 1,
        safeReason: null
      },
      adapterGeneration: 'electron-adapter-generation-1',
      limitations: []
    }
  }
  const schema = {
    axis: 'capability-schema',
    minimum: { axis: 'capability-schema', value: 1 },
    maximum: { axis: 'capability-schema', value: 1 }
  }
  const limitation = {
    code: 'not-implemented',
    explanation: 'fixture capability is not implemented',
    affectedGuarantee: 'support'
  }
  const version = axis => ({
    axis,
    selected: { axis, value: axis === 'ipc-protocol' ? 4 : 1 },
    localRange: {
      axis,
      minimum: { axis, value: axis === 'ipc-protocol' ? 4 : 1 },
      maximum: { axis, value: axis === 'ipc-protocol' ? 4 : 1 }
    },
    remoteRange: {
      axis,
      minimum: { axis, value: axis === 'ipc-protocol' ? 4 : 1 },
      maximum: { axis, value: axis === 'ipc-protocol' ? 4 : 1 }
    }
  })
  return {
    attachment,
    attachmentId: attachment.attachmentId,
    versions: {
      backendContract: version('backend-contract'),
      capabilitySchema: version('capability-schema'),
      eventSchema: version('event-schema'),
      traceFormat: version('trace-format'),
      ipcProtocol: version('ipc-protocol')
    },
    capabilities: {
      schemaVersion: 2,
      backendGeneration: attachment.backendGeneration,
      descriptors: Object.values(BUILT_IN_FEATURE_IDS).map(id => ({
        id,
        state: id === 'connection:direct' || id === 'security:state' ? 'limited' : 'unsupported',
        selectedSchemaRange: schema,
        implementationOrigin: 'backend-native',
        tck: {
          suiteId: 'capability.catalog-v2',
          requiredScenarioIds: ['capability.truth-limits-evidence-and-binding'],
          contractRange: schema
        },
        evidence: {
          receiptId: `test-${id}`,
          evidenceLevel: id === 'connection:direct' || id === 'security:state' ? 'deterministic' : 'blocked',
          implementationVersion: 'test',
          sourceDigest: `test-${id}`,
          scenarioIds: ['capability.truth-limits-evidence-and-binding'],
          limitations: [limitation]
        },
        limitations: [limitation],
        limits: { availability: { maximum: 1, minimum: null, unit: 'boolean' } }
      }))
    },
    renderer: { clientId: 'renderer-client-1', windowScope: 'window-1', sessionScope: 'session-1' },
    rendererLease: { leaseId: 'renderer-lease-1', generation: 'renderer-lease-generation-1' }
  }
}

function hangingElectronTransport() {
  let resolveConnect
  let connectDispatched
  const connectSeen = new Promise(resolve => {
    connectDispatched = resolve
  })
  let cancelRouted
  const cancelSeen = new Promise(resolve => {
    cancelRouted = resolve
  })
  const invoke = jest.fn(async request => {
    if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: electronBootstrap() }
    if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
    if (request.kind === 'event.ack') return { kind: 'event.ack' }
    const envelope = request.envelope
    if (envelope.command === 'operation.cancel') {
      cancelRouted(envelope)
      resolveConnect({
        kind: 'failure',
        error: {
          code: 'operation.aborted',
          domain: 'ipc',
          operation: 'electron.connect',
          platform: null,
          retryability: 'caller-decides'
        }
      })
      return { kind: 'route', payload: { state: 'cancellation-requested' } }
    }
    if (envelope.command === 'connection.connect') {
      connectDispatched(envelope)
      return new Promise(resolve => {
        resolveConnect = resolve
      })
    }
    throw new Error(`unexpected routed command ${envelope.command}`)
  })
  return {
    transport: {
      invoke,
      subscribe: () => () => undefined,
      acknowledge: async () => ({ kind: 'event.ack' })
    },
    connectSeen,
    cancelSeen
  }
}

// -- Web fixture (trimmed from web-bluetooth-lifecycle-hardening.test.js) -----

const WEB_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'

function webFixture() {
  const timers = new Set()
  const disconnectListeners = new Set()
  const gatt = {
    connected: false,
    connect: async () => {
      // Hangs: the browser never answers, so the backend deadline is the
      // only answer — the same position the Web radio is always in, since
      // Web Bluetooth never fails a pending connect on its own.
      await new Promise(() => undefined)
    },
    disconnect: () => {
      gatt.connected = false
    },
    getPrimaryServices: async () => []
  }
  const device = {
    id: 'browser-secret-device',
    gatt,
    addDisconnectListener: listener => disconnectListeners.add(listener),
    removeDisconnectListener: listener => disconnectListeners.delete(listener)
  }
  const boundary = {
    implementationVersion: 'connect-deadline-vocabulary-test',
    browserEngine: 'test',
    isSecureContext: () => true,
    hasTransientUserActivation: () => true,
    bluetoothAvailable: async () => true,
    requestDevice: async () => ({ device, grantedServices: [WEB_SERVICE] }),
    now: () => 10,
    setTimer: callback => {
      const handle = { callback }
      timers.add(handle)
      return handle
    },
    clearTimer: handle => timers.delete(handle),
    addPageLifecycleListener: () => () => undefined
  }
  return { boundary, device, timers }
}

async function webBackend(testFixture) {
  const { createWebBluetoothProvider } = require('../src/web/web-bluetooth-backend')
  const provider = createWebBluetoothProvider(testFixture.boundary)
  const [adapter] = await provider.listAdapters()
  const backend = await provider.create({ selectedAdapterId: adapter.adapterId })
  await backend.attach({ coreCompatibility: provider.descriptor.compatibility })
  return backend
}

async function webSelectedPeer(backend) {
  return backend.choose(
    {
      filters: [{ serviceUuids: [WEB_SERVICE], manufacturerData: [], localNamePrefix: null }],
      acceptAllDevices: false,
      optionalServices: [WEB_SERVICE]
    },
    { signal: null, deadline: null }
  )
}

describe('connect deadline vocabulary across hosts (finding 161)', () => {
  test('Tauri: a connect deadline with no link is connection.failed caller-decides', async () => {
    const { invoke, connectSeen } = hangingTauriInvoke()
    const { createTauriBleManagerWithEnvironment } = require('../src/tauri')
    const manager = await createTauriBleManagerWithEnvironment({ invoke, Channel: FakeChannel })

    jest.useFakeTimers()
    try {
      const connecting = manager.connect('polar-h10', { timeoutMs: 5 })
      connecting.catch(() => undefined)
      await connectSeen
      jest.advanceTimersByTime(6)
      await expect(connecting).rejects.toMatchObject({
        code: 'connection.failed',
        retryability: 'caller-decides'
      })
    } finally {
      jest.useRealTimers()
    }
    await manager.destroy()
  })

  test('Electron renderer: a connect deadline with no link is connection.failed caller-decides', async () => {
    const harness = hangingElectronTransport()
    const { createElectronRendererBleManager } = require('../src/electron-renderer')
    const manager = await createElectronRendererBleManager({ transport: harness.transport })

    jest.useFakeTimers()
    try {
      const connecting = manager.connect('peer-1', { timeoutMs: 5 })
      connecting.catch(() => undefined)
      await harness.connectSeen
      jest.advanceTimersByTime(6)
      await expect(connecting).rejects.toMatchObject({
        code: 'connection.failed',
        retryability: 'caller-decides',
        platform: { domain: 'ipc', code: 'deadline-expired' }
      })
      await harness.cancelSeen
    } finally {
      jest.useRealTimers()
    }
    await manager.destroy()
  })

  test('IPC route: a connect deadline carries the deadline fact, other operations keep operation.timed-out', async () => {
    const seen = {}
    const resolvers = {}
    const waitFor = command =>
      new Promise(resolve => {
        seen[command] = resolve
      })
    const hang = command =>
      new Promise(resolve => {
        resolvers[command] = resolve
      })
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: tauriBootstrap() }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      const envelope = request.envelope
      if (envelope.command === 'operation.cancel') {
        // Both hangs were cancelled by their deadline timers; answer each
        // with the native abort acknowledgement the timers race against.
        for (const command of Object.keys(resolvers)) {
          resolvers[command]?.({
            kind: 'failure',
            error: {
              code: 'operation.aborted',
              domain: 'ipc',
              operation: `tauri.${command}`,
              platform: null,
              retryability: 'caller-decides'
            }
          })
        }
        return { kind: 'route', payload: { state: 'cancellation-requested' } }
      }
      seen[envelope.command]?.(envelope)
      return hang(envelope.command)
    })
    const { IpcBleManager } = require('../src/ipc/manager')
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const ipc = await IpcBleManager.create(new TauriBleIpcTransport({ invoke, Channel: FakeChannel }))

    jest.useFakeTimers()
    try {
      // Both dispatch signals are registered before either route is started:
      // the two dispatches race each other through independent microtask
      // chains, so registering after starting would miss the loser's signal.
      const connectDispatched = waitFor('connection.connect')
      const readDispatched = waitFor('gatt.read')
      const now = globalThis.performance.now()
      const connecting = ipc.route('connection.connect', { peerId: 'peer-1', deadline: now + 50 }, null)
      connecting.catch(() => undefined)
      const reading = ipc.route('gatt.read', { handle: 'characteristic-1', deadline: now + 50 }, null)
      reading.catch(() => undefined)
      await connectDispatched
      await readDispatched
      jest.advanceTimersByTime(51)

      const connectFailure = await connecting.then(
        () => null,
        error => error
      )
      expect(connectFailure?.normalized).toMatchObject({
        code: 'connection.failed',
        domain: 'ipc',
        operation: 'ipc-manager.connection.connect',
        retryability: 'caller-decides',
        platform: { domain: 'ipc', code: 'deadline-expired' }
      })
      expect(Number.isFinite(connectFailure?.normalized?.platform?.metadata?.deadlineMs)).toBe(true)

      const readFailure = await reading.then(
        () => null,
        error => error
      )
      // Other operations keep the native operation identity and only change
      // the code to the expiry it was.
      expect(readFailure?.normalized).toMatchObject({
        code: 'operation.timed-out',
        operation: 'tauri.gatt.read'
      })
    } finally {
      jest.useRealTimers()
    }
    await ipc.destroy()
  })

  test('IPC route: a caller abort of a connect stays operation.aborted', async () => {
    let resolveConnect
    let connectDispatched
    const connectSeen = new Promise(resolve => {
      connectDispatched = resolve
    })
    const invoke = jest.fn(async (_command, args) => {
      const request = args.request
      if (request.kind === 'bootstrap') return { kind: 'bootstrap', bootstrap: tauriBootstrap() }
      if (request.kind === 'release') return { kind: 'release', cleanup: { state: 'released', failures: [] } }
      if (request.kind === 'event.ack') return { kind: 'event.ack' }
      if (request.envelope.command === 'operation.cancel') {
        resolveConnect({
          kind: 'failure',
          error: {
            code: 'operation.aborted',
            domain: 'ipc',
            operation: 'tauri.connect',
            platform: null,
            retryability: 'caller-decides'
          }
        })
        return { kind: 'route', payload: { state: 'cancellation-requested' } }
      }
      connectDispatched(request.envelope)
      return new Promise(resolve => {
        resolveConnect = resolve
      })
    })
    const { IpcBleManager } = require('../src/ipc/manager')
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const ipc = await IpcBleManager.create(new TauriBleIpcTransport({ invoke, Channel: FakeChannel }))

    const controller = new AbortController()
    const connecting = ipc.route(
      'connection.connect',
      { peerId: 'peer-1', deadline: globalThis.performance.now() + 5000 },
      null,
      controller.signal
    )
    connecting.catch(() => undefined)
    await connectSeen
    controller.abort()
    await expect(connecting).rejects.toMatchObject({
      normalized: { code: 'operation.aborted' }
    })
    await ipc.destroy()
  })

  test('Web: a connect deadline with no link is connection.failed caller-decides', async () => {
    const testFixture = webFixture()
    const backend = await webBackend(testFixture)
    try {
      const selected = await webSelectedPeer(backend)
      const connecting = backend.connections.connect(selected.peerId, 'client', {
        signal: null,
        deadline: 20
      })
      connecting.catch(() => undefined)
      for (const timer of [...testFixture.timers]) {
        timer.callback()
      }
      await expect(connecting).rejects.toMatchObject({
        normalized: {
          code: 'connection.failed',
          retryability: 'caller-decides',
          platform: { domain: 'web-bluetooth', code: 'DeadlineExpired' }
        }
      })
      const failure = await connecting.catch(error => error)
      expect(typeof failure?.normalized?.platform?.metadata?.deadlineMs).toBe('number')
    } finally {
      await backend.destroy()
    }
  })
})
