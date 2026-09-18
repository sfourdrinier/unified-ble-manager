// __tests__/electron/electron-adapter-recovery.test.js
//
// Adapter off then on under a supervised connection across the Electron
// main <-> renderer IPC (protocol 4), over the real N-API addon's synthetic
// radio and the real main-process manager. After the loss, main rebinds the
// renderer to the backend's new attachment and announces it on the
// `attachment` stream (`backend-restarted`); the renderer adopts it (it can
// never pick one itself), and the supervisor reconnects when the adapter
// returns, including after more than the 10 s readiness window.

const h = require('../helpers/desktop-rust-core-harness')
const { ElectronMainBleBinding, ElectronMainBleRouter } = require('../../src/electron-main')
const { createElectronRendererBleManager } = require('../../src/electron-renderer')
const {
  createDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../src/node-host-manager')
const { createConnectionSupervisor } = require('../../src/public/connection-supervisor')
const { IPC_PROTOCOL_VERSION, IPC_ATTACHMENT_STREAM_ID } = require('../../src/ipc/protocol')
const { opaqueId } = require('../../src/backend-contract/primitives')

jest.setTimeout(30_000)

const RETRY = Object.freeze({ initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1, jitter: 0 })

async function eventually(what, predicate, describe = () => null, timeoutMs = 8000) {
  const started = Date.now()
  while (!predicate()) {
    if (Date.now() - started > timeoutMs)
      throw new Error(`timed out waiting for ${what}: ${JSON.stringify(describe())}`)
    await new Promise(resolve => setTimeout(resolve, 10))
  }
}

/** One trusted renderer frame wired to the main binding in-process. */
function createRendererPort(binding) {
  const listeners = new Set()
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
  const provider = createDesktopRustCoreBackendProvider({
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
    const supervisor = createConnectionSupervisor(publicManager, peerId, { retry: RETRY })
    const seen = []
    ;(async () => {
      for await (const item of supervisor.events) if (item.kind === 'value') seen.push(item.value)
    })()
    const describe = () => ({
      snapshot: supervisor.snapshot,
      cleanups: seen.filter(e => e.cleanup).map(e => e.cleanup)
    })
    supervisor.start()
    await eventually('the first connection', () => supervisor.snapshot.state === 'connected', describe)
    const first = supervisor.snapshot.connectionGeneration

    // The renderer's readiness clock runs 11 s ahead once the adapter is off.
    let offset = 0
    const base = performance.now()
    const started = process.hrtime.bigint()
    const clock = jest
      .spyOn(performance, 'now')
      .mockImplementation(() => base + Number(process.hrtime.bigint() - started) / 1e6 + offset)
    try {
      await stage.stageAdapterState('powered-off', true)
      await eventually('the adapter wait', () => supervisor.snapshot.state === 'waiting-for-gate', describe)
      // Main announced the rebind on the attachment stream, for this lease only.
      const announced = renderer.sent.filter(event => event.streamId === IPC_ATTACHMENT_STREAM_ID)
      expect(announced).toHaveLength(1)
      expect(announced[0].item).toMatchObject({ kind: 'value', value: { kind: 'backend-restarted' } })
      const rebound = announced[0].item.value
      expect(rebound.attachmentId).toBe(rebound.attachment.attachmentId)
      expect(rebound.previousAttachmentId).not.toBe(rebound.attachmentId)
      // The adapter stays off past the readiness window (10 s).
      offset += 11_000
      await stage.stageAdapterState('powered-off', true)
      await new Promise(resolve => setTimeout(resolve, 100))
      expect(supervisor.snapshot.state).toBe('waiting-for-gate')

      await stage.stageAdapterState('powered-on', true)
      await eventually('the reconnection', () => supervisor.snapshot.state === 'connected', describe)
      expect(supervisor.snapshot.connectionGeneration).not.toBe(first)

      // Main and the renderer follow the new generation: a later link loss on
      // the new connection is observed and the supervisor reconnects again.
      const recovered = supervisor.snapshot.connectionGeneration
      await stage.stageLinkLoss('peer-1')
      await eventually(
        'the second reconnection',
        () => supervisor.snapshot.state === 'connected' && supervisor.snapshot.connectionGeneration !== recovered,
        describe
      )
    } finally {
      clock.mockRestore()
    }
    expect((await supervisor.stop()).state).toBe('released')
    expect(await publicManager.destroy()).toEqual({ state: 'released', failures: [] })
    await binding.dispose?.()
  })
})
