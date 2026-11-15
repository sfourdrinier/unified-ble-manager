const { ElectronMainBleRouter } = require('../../src/electron/main-router')
const { DesktopRustCoreBackend } = require('../../src/backends/desktop/desktop-rust-core-provider')
const { IPC_CLIENT_COMPATIBILITY_OFFER } = require('../../src/ipc/protocol')
const { version, versionRange, opaqueId, monotonicTimestamp } = require('../../src/backend-contract/primitives')

function fixture(options = {}) {
  const axis = name => {
    const selected = version(name, 1)
    const range = versionRange(selected, selected)
    return { axis: name, selected, localRange: range, remoteRange: range }
  }
  const generation = opaqueId('generation', 'terminal-evidence-generation', 'test')
  const attachment = {
    attachmentId: opaqueId('attachment', 'terminal-evidence-attachment', 'test'),
    backendInstanceId: opaqueId('backend', 'terminal-evidence-backend', 'test'),
    backendGeneration: generation,
    adapter: {
      adapterId: opaqueId('adapter', 'terminal-evidence-adapter', 'test'),
      displayName: null,
      state: { availability: 'available', authorization: 'granted', power: 'on', heard: null, backendGeneration: generation, updatedAt: monotonicTimestamp(1), safeReason: null },
      adapterGeneration: opaqueId('adapter-generation', 'terminal-evidence-adapter-generation', 'test'),
      limitations: []
    }
  }
  const records = new Map()
  const reservations = new Map()
  const releases = []
  let serial = 0
  const source = {
    reserveTerminalEvidence: async request => {
      if (reservations.size !== 0) throw new Error('runtime reservation remains busy')
      const reservationId = `reservation-${++serial}`
      reservations.set(reservationId, request)
      return {
        maximumBytes: request.maximumBytes,
        commit: async record => {
          const ref = `ref-${serial}`
          records.set(ref, { bytes: new Uint8Array(record.canonicalBytes), digest: record.digest })
          return { producer: request.producer, ref, byteLength: record.canonicalBytes.byteLength, digest: record.digest, formatVersion: 1, durability: 'committed', scope: request.scope }
        },
        release: async () => {
          releases.push(reservationId)
          reservations.delete(reservationId)
        }
      }
    },
    readTerminalEvidence: async (ref, maximumBytes) => {
      const record = records.get(ref)
      if (!record) return { kind: 'missing' }
      if (record.bytes.byteLength > maximumBytes) return { kind: 'unresolved' }
      return { kind: 'committed', canonicalBytes: new Uint8Array(record.bytes), digest: record.digest }
    },
    releaseTerminalEvidence: async ref => { records.delete(ref) }
  }
  const backend = options.desktopBackend ?? { scanner: source, gatt: source }
  backend.scanner ??= source
  backend.gatt ??= source
  const manager = { attachedBackend: { attachment: { attachment }, backend }, identity: { versions: { backendContract: axis('backend-contract'), capabilitySchema: axis('capability-schema'), eventSchema: axis('event-schema'), traceFormat: axis('trace-format') } }, capabilities: () => [], onAttachmentAdvanced: () => () => {}, monotonicNow: () => 1 }
  const router = new ElectronMainBleRouter({ manager, maximumMessageBytes: 512, maximumOutstandingOperations: 8, maximumRetainedBytes: 4096, publish: async () => 'delivered' })
  return { router, records, reservations, releases }
}

function offer() {
  const selected = version('ipc-protocol', 11)
  return { ...IPC_CLIENT_COMPATIBILITY_OFFER, ipcProtocol: versionRange(selected, selected) }
}

async function route(router, sender) {
  const boot = (await router.dispatch(sender, { kind: 'bootstrap', offer: offer() })).bootstrap
  let sequence = 0
  return { boot, route: async (command, payload, binaryPayload = null) => (await router.dispatch(sender, { kind: 'route', envelope: { versions: boot.versions, attachment: boot.attachment, attachmentId: boot.attachmentId, renderer: boot.renderer, rendererLease: boot.rendererLease, correlation: opaqueId(String(++sequence), 'ipc-operation', 'test'), dispatchEpoch: opaqueId(String(sequence), 'ipc-dispatch-epoch', 'test'), command, payload, binaryPayload } })).payload }
}

const sender = client => ({ authenticatedClientId: client, authenticatedWindowScope: 'window', authenticatedSessionScope: 'session' })

test('Electron binds terminal evidence to its authenticated owner before publishing the reservation', async () => {
  const bindings = []
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  backend.observeOrderedRecordingFinished = () => () => undefined
  backend.hasTerminalEvidenceRecovery = () => false
  backend.bindTerminalEvidenceRecipientForRecovery = async (...binding) => { bindings.push(binding) }
  const { router } = fixture({ desktopBackend: backend })
  const owner = sender('authenticated-client')
  const client = await route(router, owner)
  const scope = { managerId: 'manager', operationId: 'operation', incarnation: 'incarnation' }

  await expect(client.route('terminalEvidence.reserve', {
    version: 1,
    producer: 'scan',
    maximumBytes: 32,
    formatVersion: 1,
    scope,
    recipient: 'renderer-supplied-recipient-must-not-reach-native'
  })).resolves.toMatchObject({ kind: 'reserved' })

  expect(bindings).toEqual([[
    'scan',
    scope,
    JSON.stringify(['authenticated-client', 'window', 'session'])
  ]])
})

test('Electron releases an unpublished terminal reservation when trusted recipient binding fails', async () => {
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  backend.observeOrderedRecordingFinished = () => () => undefined
  backend.hasTerminalEvidenceRecovery = () => false
  backend.bindTerminalEvidenceRecipientForRecovery = async () => {
    throw new Error('recipient binding unavailable')
  }
  const { router, reservations } = fixture({ desktopBackend: backend })
  const client = await route(router, sender('owner'))

  await expect(client.route('terminalEvidence.reserve', {
    version: 1,
    producer: 'scan',
    maximumBytes: 32,
    formatVersion: 1,
    scope: { managerId: 'manager', operationId: 'operation', incarnation: 'incarnation' }
  })).rejects.toThrow('recipient binding unavailable')

  expect(reservations).toEqual(new Map())
})

test('Electron releases a provisionally bound reservation when its renderer closes during binding', async () => {
  let completeBinding
  let bindingStarted
  const bindingStartedPromise = new Promise(resolve => { bindingStarted = resolve })
  const bindingCompletion = new Promise(resolve => { completeBinding = resolve })
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  backend.observeOrderedRecordingFinished = () => () => undefined
  backend.hasTerminalEvidenceRecovery = () => false
  backend.bindTerminalEvidenceRecipientForRecovery = async () => {
    bindingStarted()
    await bindingCompletion
  }
  const { router, reservations, releases } = fixture({ desktopBackend: backend })
  const identity = sender('owner')
  const client = await route(router, identity)
  const reserve = client.route('terminalEvidence.reserve', {
    version: 1,
    producer: 'scan',
    maximumBytes: 32,
    formatVersion: 1,
    scope: { managerId: 'manager', operationId: 'operation', incarnation: 'incarnation' }
  })

  await bindingStartedPromise
  const release = router.releaseRenderer(identity, client.boot.rendererLease)
  // The arbiter marks the resource lifecycle in its scheduled release turn;
  // it then waits this in-flight route. Do not await `release` before the
  // deferred native bind is allowed to settle.
  await Promise.resolve()
  completeBinding()

  await expect(reserve).rejects.toMatchObject({ normalized: { code: 'ownership.denied' } })
  await expect(release).resolves.toEqual({ state: 'released', failures: [] })
  expect(reservations).toEqual(new Map())
  expect(releases).toEqual(['reservation-1'])
})

test('trusted main retains committed terminal evidence across a renderer lease release for same-renderer recovery only', async () => {
  const { router, records, reservations } = fixture()
  const firstSender = sender('one')
  const first = await route(router, firstSender)
  const reservation = await first.route('terminalEvidence.reserve', { version: 1, producer: 'scan', maximumBytes: 32, formatVersion: 1, scope: { managerId: 'manager', operationId: 'operation', incarnation: 'incarnation' } })
  const committed = await first.route('terminalEvidence.commit', { version: 1, reservationId: reservation.reservationId, digest: 'a'.repeat(64) }, new Uint8Array([1, 2, 3]))
  expect(committed.ref).toBeTruthy()
  await router.releaseRenderer(firstSender, first.boot.rendererLease)
  expect(reservations.size).toBe(0)
  const reloaded = await route(router, firstSender)
  const recovery = await reloaded.route('terminalEvidence.recovery.list', { version: 1 })
  expect(recovery).toEqual({ records: [expect.objectContaining({ handle: expect.any(String), digest: 'a'.repeat(64) })] })
  await expect(reloaded.route('terminalEvidence.recovery.read', { version: 1, handle: recovery.records[0].handle, maximumBytes: 3 })).resolves.toEqual({ kind: 'committed', canonicalBytes: new Uint8Array([1, 2, 3]), digest: 'a'.repeat(64) })
  await reloaded.route('terminalEvidence.recovery.ack', { version: 1, handle: recovery.records[0].handle })
  expect(records.has(committed.ref)).toBe(false)
  await expect(reloaded.route('terminalEvidence.reserve', { version: 1, producer: 'scan', maximumBytes: 32, formatVersion: 1, scope: { managerId: 'manager', operationId: 'next-operation', incarnation: 'incarnation' } })).resolves.toMatchObject({ kind: 'reserved' })
  await router.releaseRenderer(firstSender, reloaded.boot.rendererLease)
  const other = await route(router, sender('two'))
  await expect(other.route('terminalEvidence.recovery.list', { version: 1 })).resolves.toEqual({ records: [] })
})

test('stream release abandons only the runtime lease; recovery ACK is the sole retirement', async () => {
  const { router, records } = fixture()
  const firstSender = sender('one')
  const first = await route(router, firstSender)
  const reservation = await first.route('terminalEvidence.reserve', { version: 1, producer: 'scan', maximumBytes: 32, formatVersion: 1, scope: { managerId: 'manager', operationId: 'operation', incarnation: 'incarnation' } })
  const committed = await first.route('terminalEvidence.commit', { version: 1, reservationId: reservation.reservationId, digest: 'b'.repeat(64) }, new Uint8Array([4, 5, 6]))

  await first.route('terminalEvidence.release', { version: 1, reservationId: reservation.reservationId })
  expect(records.has(committed.ref)).toBe(true)
  const recovery = await first.route('terminalEvidence.recovery.list', { version: 1 })
  expect(recovery.records).toHaveLength(1)
  await first.route('terminalEvidence.recovery.ack', { version: 1, handle: recovery.records[0].handle })
  expect(records.has(committed.ref)).toBe(false)
})

test('a stale same-identity renderer cannot acknowledge evidence claimed by a newer private recovery lease', async () => {
  let reference = null
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  backend.observeOrderedRecordingFinished = () => () => undefined
  backend.hasTerminalEvidenceRecovery = () => true
  backend.bindTerminalEvidenceRecipientForRecovery = async () => undefined
  backend.stageTerminalEvidenceForRecovery = async () => undefined
  const recovered = leaseId =>
    reference === null
      ? []
      : [{ producer: 'scan', reservationId: 'native-reservation', reference, digest: 'c'.repeat(64), byteLength: 3, leaseId }]
  backend.recoverTerminalEvidenceForRecipientWithStatus = async (_recipient, leaseId) => ({ records: recovered(leaseId), busyProducers: [] })
  backend.recoverTerminalEvidenceForRecipient = async (_recipient, leaseId) => recovered(leaseId)
  backend.releaseTerminalEvidenceRecoveryLease = async () => undefined
  const { router, records } = fixture({ desktopBackend: backend })
  const firstSender = sender('same-client')
  const first = await route(router, firstSender)
  const reservation = await first.route('terminalEvidence.reserve', { version: 1, producer: 'scan', maximumBytes: 32, formatVersion: 1, scope: { managerId: 'manager', operationId: 'operation', incarnation: 'incarnation' } })
  const committed = await first.route('terminalEvidence.commit', { version: 1, reservationId: reservation.reservationId, digest: 'c'.repeat(64) }, new Uint8Array([7, 8, 9]))
  reference = committed.ref
  await first.route('terminalEvidence.release', { version: 1, reservationId: reservation.reservationId })

  const second = await route(router, firstSender)
  const listed = await second.route('terminalEvidence.recovery.list', { version: 1 })
  expect(listed.records).toHaveLength(1)
  await expect(first.route('terminalEvidence.recovery.ack', { version: 1, handle: listed.records[0].handle })).rejects.toMatchObject({
    normalized: { code: 'ownership.denied' }
  })
  expect(records.has(committed.ref)).toBe(true)
  await second.route('terminalEvidence.recovery.ack', { version: 1, handle: listed.records[0].handle })
  expect(records.has(committed.ref)).toBe(false)
})

test('a second same-identity renderer cannot acknowledge a busy first controller recovery alias', async () => {
  let reference = null
  let claimedLease = null
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  backend.observeOrderedRecordingFinished = () => () => undefined
  backend.hasTerminalEvidenceRecovery = () => true
  backend.bindTerminalEvidenceRecipientForRecovery = async () => undefined
  backend.stageTerminalEvidenceForRecovery = async () => undefined
  const page = leaseId => {
    if (reference === null) return { records: [], busyProducers: [] }
    if (claimedLease === null || claimedLease === leaseId) {
      claimedLease = leaseId
      return { records: [{ producer: 'scan', reservationId: 'native-reservation', reference, digest: 'd'.repeat(64), byteLength: 3 }], busyProducers: [] }
    }
    return { records: [], busyProducers: ['scan'] }
  }
  backend.recoverTerminalEvidenceForRecipientWithStatus = async (_recipient, leaseId) => page(leaseId)
  backend.recoverTerminalEvidenceForRecipient = async (_recipient, leaseId) => {
    const value = page(leaseId)
    if (value.busyProducers.length !== 0) throw new Error('busy')
    return value.records
  }
  backend.releaseTerminalEvidenceRecoveryLease = async leaseId => { if (claimedLease === leaseId) claimedLease = null }
  const { router, records } = fixture({ desktopBackend: backend })
  const firstSender = sender('same-client')
  const first = await route(router, firstSender)
  const reservation = await first.route('terminalEvidence.reserve', { version: 1, producer: 'scan', maximumBytes: 32, formatVersion: 1, scope: { managerId: 'manager', operationId: 'operation', incarnation: 'incarnation' } })
  const committed = await first.route('terminalEvidence.commit', { version: 1, reservationId: reservation.reservationId, digest: 'd'.repeat(64) }, new Uint8Array([3, 2, 1]))
  reference = committed.ref
  await first.route('terminalEvidence.release', { version: 1, reservationId: reservation.reservationId })
  const firstList = await first.route('terminalEvidence.recovery.list', { version: 1 })
  const second = await route(router, firstSender)
  await expect(second.route('terminalEvidence.recovery.ack', { version: 1, handle: firstList.records[0].handle })).rejects.toMatchObject({
    normalized: { code: 'ownership.denied' }
  })
  expect(records.has(committed.ref)).toBe(true)
  await first.route('terminalEvidence.recovery.ack', { version: 1, handle: firstList.records[0].handle })
  expect(records.has(committed.ref)).toBe(false)
})

test('Electron renderer teardown relinquishes its private native recovery lease without acknowledging evidence', async () => {
  const claimed = new Set()
  const released = []
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  backend.observeOrderedRecordingFinished = () => () => undefined
  backend.hasTerminalEvidenceRecovery = () => true
  backend.recoverTerminalEvidenceForRecipientWithStatus = async (_recipient, leaseId) => {
    if (claimed.size !== 0) throw new Error('another recovery controller owns the record')
    claimed.add(leaseId)
    return { records: [], busyProducers: [] }
  }
  backend.releaseTerminalEvidenceRecoveryLease = async leaseId => {
    claimed.delete(leaseId)
    released.push(leaseId)
  }
  const { router } = fixture({ desktopBackend: backend })
  const firstSender = sender('one')
  const first = await route(router, firstSender)
  await router.releaseRenderer(firstSender, first.boot.rendererLease)
  expect(released).toHaveLength(1)
  expect(released[0]).toMatch(/^electron-terminal-evidence-recovery-/)
  expect(claimed).toEqual(new Set())
  const reloaded = await route(router, firstSender)
  await router.releaseRenderer(firstSender, reloaded.boot.rendererLease)
  expect(released).toHaveLength(2)
})
