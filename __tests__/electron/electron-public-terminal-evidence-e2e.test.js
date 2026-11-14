'use strict'

// Native-synthetic Electron end-to-end coverage.  This deliberately uses the
// real first-party N-API addon on its synthetic radio; it is not RF evidence.

const crypto = require('node:crypto')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const h = require('../helpers/desktop-rust-core-harness')
const { ElectronMainBleBinding, ElectronMainBleRouter } = require('../../src/electron-main')
const { createElectronRendererBleManager } = require('../../src/electron-renderer')
const {
  createTestDesktopRustCoreBackendProvider,
  DESKTOP_RUST_CORE_PROFILES
} = require('../../src/backends/desktop/desktop-rust-core-provider')
const { createNodeBleManagerFromProvider } = require('../../src/node-host-manager')
const { opaqueId } = require('../../src/backend-contract/primitives')
const { decodeTerminalEvidence } = require('../../src/core/terminal-evidence-codec')
const { IPC_CLIENT_COMPATIBILITY_OFFER } = require('../../src/ipc/protocol')

jest.setTimeout(30_000)

function rendererPort(
  binding,
  identity = 'electron-terminal-evidence-renderer',
  mutate = request => request,
  routingId = 72
) {
  const listeners = new Set()
  const mainFrame = Object.freeze({ processId: 71, routingId })
  const sender = {
    mainFrame,
    trusted: {
      authenticatedClientId: opaqueId(identity, 'client', 'electron:renderer'),
      authenticatedWindowScope: `${identity}-window`,
      authenticatedSessionScope: `${identity}-session`
    },
    isDestroyed: () => false,
    once: () => undefined,
    on: () => undefined,
    removeListener: () => undefined,
    send(_channel, event) {
      for (const listener of listeners) listener(event)
    }
  }
  const invoke = request =>
    binding.port.handler({ sender, frameId: mainFrame.routingId, processId: mainFrame.processId }, mutate(request))
  let bootstrap = null
  let sequence = 0
  const rawRoute = async (command, payload) => {
    if (bootstrap === null) {
      const response = await invoke({ kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
      if (response.kind !== 'bootstrap') throw new Error('raw Electron bootstrap failed')
      bootstrap = response.bootstrap
    }
    return invoke({
      kind: 'route',
      envelope: {
        versions: bootstrap.versions,
        attachment: bootstrap.attachment,
        attachmentId: bootstrap.attachmentId,
        renderer: bootstrap.renderer,
        rendererLease: bootstrap.rendererLease,
        correlation: opaqueId(String(++sequence), 'ipc-operation', 'electron-terminal-evidence-e2e'),
        dispatchEpoch: opaqueId(String(sequence), 'ipc-dispatch-epoch', 'electron-terminal-evidence-e2e'),
        command,
        payload,
        binaryPayload: null
      }
    })
  }
  return {
    rawRoute,
    transport: {
      invoke,
      subscribe(listener) {
        listeners.add(listener)
        return () => listeners.delete(listener)
      },
      acknowledge: (rendererLease, eventId) => invoke({ kind: 'event.ack', rendererLease, eventId })
    }
  }
}

async function open() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-electron-terminal-evidence-'))
  const harness = h.realBinding('bluez')
  const now = () => performance.now()
  const provider = createTestDesktopRustCoreBackendProvider({
    platform: 'bluez',
    owner: 'electron-terminal-evidence-e2e',
    now,
    radio: 'synthetic',
    binding: harness.binding,
    hostPlatform: 'linux',
    hostKind: 'desktop-native',
    terminalEvidenceStore: { directory, maxBytes: '8192' }
  })
  const manager = await createNodeBleManagerFromProvider(provider, DESKTOP_RUST_CORE_PROFILES.bluez.compatibility, {
    now
  })
  const router = new ElectronMainBleRouter({
    manager,
    maximumMessageBytes: 262144,
    maximumOutstandingOperations: 16,
    maximumRetainedBytes: 1048576,
    publish: async () => 'native-synthetic'
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
  const createRenderer = (identity, mutate, routingId) =>
    createElectronRendererBleManager({
      transport: rendererPort(binding, identity, mutate, routingId).transport,
      terminalEvidence: { maximumCanonicalBytes: 4096, maximumDurableBytes: 8192 }
    })
  let publicManager = await createRenderer()
  return {
    stage: harness.opened.at(-1),
    harness,
    binding,
    get publicManager() {
      return publicManager
    },
    async reload() {
      await publicManager.destroy()
      publicManager = await createRenderer()
      return publicManager
    },
    createRenderer,
    async close() {
      await publicManager.destroy()
      await binding.destroy()
      await manager.destroy()
      fs.rmSync(directory, { recursive: true, force: true })
    }
  }
}

test('authenticated Electron renderer hydrates native scan terminal evidence and retires it only after recovery acknowledgement', async () => {
  const current = await open()
  try {
    const scan = await current.publicManager.scan()
    const events = scan.observations[Symbol.asyncIterator]()
    const terminal = events.next()

    await current.stage.stageScanTerminated(true, 'native synthetic terminal evidence')

    const terminalResult = await terminal
    expect(terminalResult).toMatchObject({
      done: false,
      value: {
        kind: 'terminal',
        reason: 'source-failed',
        error: { code: 'scan.start-failed', platform: { safeMessage: 'native synthetic terminal evidence' } }
      }
    })
    await expect(events.next()).resolves.toMatchObject({ done: true })
    const start = current.harness.calls.find(([name]) => name === 'startScan')
    expect(start).toBeDefined()
    expect(start[1][0].terminalEvidenceScope).toBeDefined()

    await current.reload()
    const [record] = await current.publicManager.terminalEvidenceRecovery.list()
    expect(record).toMatchObject({ producer: 'scan', formatVersion: 1 })
    const recovered = await current.publicManager.terminalEvidenceRecovery.read(record.handle, record.byteLength)
    expect(recovered.digest).toBe(record.digest)
    expect(crypto.createHash('sha256').update(recovered.canonicalBytes).digest('hex')).toBe(record.digest)
    expect(decodeTerminalEvidence(recovered.canonicalBytes)).toMatchObject({
      terminal: {
        kind: 'terminal',
        reason: 'source-failed',
        error: { code: 'scan.start-failed', platform: { safeMessage: 'native synthetic terminal evidence' } }
      }
    })

    await current.publicManager.terminalEvidenceRecovery.acknowledge(record.handle)
    await expect(current.publicManager.terminalEvidenceRecovery.list()).resolves.toEqual([])
  } finally {
    await current.close()
  }
})

test.each([
  ['ordinary metadata', { fixture: 'electron-public-terminal-evidence' }, undefined],
  ['literal byte-tag metadata', { $bytes: '00ff' }, 2]
])(
  'authenticated Electron renderer hydrates native subscription terminal evidence with %s',
  async (_label, metadata, encodingVersion) => {
    const current = await open()
    try {
      await current.stage.stageServices('peer-1', h.hrmServices())
      const scan = await current.publicManager.scan()
      const observations = scan.observations[Symbol.asyncIterator]()
      const observed = h.nextValue(observations, 5000)
      await current.stage.stageAdvertisement({ peerId: 'peer-1', localName: 'native synthetic peer' })
      const peer = (await observed).peer
      await observations.return?.()
      await scan.stop()

      const connection = await current.publicManager.connect(peer)
      const database = await connection.discover()
      const characteristic = database.characteristic('180d', '2a37')
      const subscription = await characteristic.subscribe()
      const subscribe = current.harness.calls.find(([name]) => name === 'subscribe')
      expect(subscribe).toBeDefined()
      expect(subscribe[1][0].terminalEvidenceScope).toBeDefined()
      const values = subscription.values[Symbol.asyncIterator]()
      const terminal = values.next()
      await current.stage.stageNotification({
        peerId: 'peer-1',
        serviceUuid: h.HRM_SERVICE,
        serviceOccurrence: 0,
        characteristicUuid: h.HRM_MEASUREMENT,
        characteristicOccurrence: 0,
        value: Buffer.alloc(0),
        sourceFailure: {
          domain: 'bluez',
          code: 'terminal',
          message: 'native synthetic subscription terminal',
          metadata
        }
      })
      const terminalResult = await terminal
      expect(terminalResult).toMatchObject({
        done: false,
        value: {
          kind: 'terminal',
          reason: 'source-failed',
          error: {
            code: 'platform.failure',
            domain: 'gatt',
            operation: 'bluez.gatt.notification',
            platform: {
              domain: 'bluez',
              code: 'terminal',
              safeMessage: 'native synthetic subscription terminal',
              metadata
            }
          }
        }
      })

      await subscription.remove()
      await connection.release()
      await current.reload()
      const records = await current.publicManager.terminalEvidenceRecovery.list()
      const record = records.find(candidate => candidate.producer === 'subscription')
      expect(record).toBeDefined()
      const recovered = await current.publicManager.terminalEvidenceRecovery.read(record.handle, record.byteLength)
      expect(crypto.createHash('sha256').update(recovered.canonicalBytes).digest('hex')).toBe(record.digest)
      expect(JSON.parse(Buffer.from(recovered.canonicalBytes).toString('utf8')).encodingVersion).toBe(encodingVersion)
      expect(decodeTerminalEvidence(recovered.canonicalBytes)).toMatchObject({
        terminal: {
          kind: 'terminal',
          reason: 'source-failed',
          error: { platform: { safeMessage: 'native synthetic subscription terminal', metadata } }
        }
      })
      await current.publicManager.terminalEvidenceRecovery.acknowledge(record.handle)
    } finally {
      await current.close()
    }
  }
)

test('Electron refuses an unknown renderer terminal-evidence scope before native scan startup and releases its reservation', async () => {
  const current = await open()
  let forged
  try {
    const unknownScope = Object.freeze({
      managerId: 'unknown-manager',
      operationId: 'unknown-operation',
      incarnation: 'unknown-incarnation'
    })
    forged = await current.createRenderer('unknown-scope-renderer', request => {
      if (request.kind !== 'route' || request.envelope.command !== 'scan.start') return request
      return Object.freeze({
        ...request,
        envelope: Object.freeze({
          ...request.envelope,
          payload: Object.freeze({ ...request.envelope.payload, terminalEvidenceScope: unknownScope })
        })
      })
    })
    const starts = current.harness.calls.filter(([name]) => name === 'startScan').length
    const releases = current.harness.calls.filter(([name]) => name === 'releaseTerminalEvidence').length
    await expect(forged.scan()).rejects.toMatchObject({ code: 'ownership.denied' })
    expect(current.harness.calls.filter(([name]) => name === 'startScan')).toHaveLength(starts)
    expect(current.harness.calls.filter(([name]) => name === 'releaseTerminalEvidence')).toHaveLength(releases + 1)
  } finally {
    await forged?.destroy()
    await current.close()
  }
})

test.each([['a different authenticated session', 'scan', 'terminal-owner-a', 'terminal-owner-b']])(
  'Electron refuses %s before native scan startup and cleans both reservations',
  async (_name, producer, owner, forgedOwner) => {
    const current = await open()
    let forged
    const raw = rendererPort(current.binding, owner)
    const scope = Object.freeze({
      managerId: `manager-${producer}`,
      operationId: `operation-${producer}`,
      incarnation: '1'
    })
    try {
      const reserved = await raw.rawRoute('terminalEvidence.reserve', {
        version: 1,
        producer,
        maximumBytes: 4096,
        formatVersion: 1,
        scope
      })
      expect(reserved).toMatchObject({
        kind: 'route',
        payload: { kind: 'reserved', reservationId: expect.any(String) }
      })
      forged = await current.createRenderer(forgedOwner, request => {
        if (request.kind !== 'route' || request.envelope.command !== 'scan.start') return request
        return Object.freeze({
          ...request,
          envelope: Object.freeze({
            ...request.envelope,
            payload: Object.freeze({ ...request.envelope.payload, terminalEvidenceScope: scope })
          })
        })
      })
      const starts = current.harness.calls.filter(([name]) => name === 'startScan').length
      await expect(forged.scan()).rejects.toMatchObject({ code: 'ownership.denied' })
      expect(current.harness.calls.filter(([name]) => name === 'startScan')).toHaveLength(starts)
      await expect(
        raw.rawRoute('terminalEvidence.release', { version: 1, reservationId: reserved.payload.reservationId })
      ).resolves.toMatchObject({ kind: 'route' })
    } finally {
      await forged?.destroy()
      await current.close()
    }
  }
)

test('Electron refuses the same renderer reservation for the wrong producer before source startup', async () => {
  const current = await open()
  let forged
  let changedReservations = 0
  try {
    forged = await current.createRenderer('wrong-producer-renderer', request => {
      if (request.kind !== 'route' || request.envelope.command !== 'terminalEvidence.reserve') return request
      changedReservations += 1
      return Object.freeze({
        ...request,
        envelope: Object.freeze({
          ...request.envelope,
          payload: Object.freeze({ ...request.envelope.payload, producer: 'subscription' })
        })
      })
    })
    const starts = current.harness.calls.filter(([name]) => name === 'startScan').length
    const releases = current.harness.calls.filter(([name]) => name === 'releaseTerminalEvidence').length
    await expect(forged.scan()).rejects.toMatchObject({ code: 'ownership.denied' })
    expect(changedReservations).toBe(1)
    expect(current.harness.calls.filter(([name]) => name === 'startScan')).toHaveLength(starts)
    expect(current.harness.calls.filter(([name]) => name === 'releaseTerminalEvidence')).toHaveLength(releases + 1)
  } finally {
    await forged?.destroy()
    await current.close()
  }
})
