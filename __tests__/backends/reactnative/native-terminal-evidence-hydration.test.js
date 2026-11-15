// __tests__/backends/reactnative/native-terminal-evidence-hydration.test.js
const {
  rustCoreHarness,
  environment,
  settle,
  scanOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const {
  createBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../../src/manager/ble-manager')
const { opaqueId } = require('../../../src/backend-contract/primitives')

async function setup(scanEndV2 = false) {
  const harness = rustCoreHarness()
  if (scanEndV2) harness.native.negotiateScanEndVersion = async () => JSON.stringify({ version: 2 })
  const internal = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const attached = internal.attachedBackend
  const core = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('hydration-client', 'client', 'test'),
      managerId: opaqueId('hydration-manager', 'manager', 'test'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attached),
    {
      ...DEFAULT_BLE_MANAGER_OPTIONS,
      now: () => 1000,
      terminalEvidence: { maximumCanonicalBytes: 4096 }
    }
  )
  return { ...harness, internal, core }
}

test('shared core hydrates unchanged native owner and joined stop evidence', async () => {
  const h = await setup()
  const owner = await h.core.scan(scanOptions({ sharing: { mode: 'owner', allowSharing: true } }))
  const joined = await h.core.scan(
    scanOptions({ sharing: { mode: 'join', sharedLeaseId: owner.leaseId, token: owner.shareToken } })
  )
  const pendingStop = owner.stop()
  const ownerTerminal = await owner.observations[Symbol.asyncIterator]().next()
  const joinedTerminal = await joined.observations[Symbol.asyncIterator]().next()
  expect(ownerTerminal.value).toMatchObject({
    kind: 'terminal',
    reason: 'owner-released',
    evidence: { producer: 'scan', durability: 'committed' }
  })
  expect(joinedTerminal.value).toMatchObject({
    kind: 'terminal',
    reason: 'owner-released',
    evidence: { producer: 'scan', durability: 'committed' }
  })
  expect(ownerTerminal.value.evidence.scope).not.toEqual(joinedTerminal.value.evidence.scope)
  const raw = h.native.terminalEvidenceCommitted.get(joinedTerminal.value.evidence.ref)
  expect(JSON.parse(Buffer.from(raw.canonicalBytes).toString('utf8'))).toMatchObject({
    t: 'scan-end',
    reason: 'owner-released'
  })
  await pendingStop
  await settle(80)
  await h.core.destroy()
  await h.internal.destroy()
})

test('shared core hydrates the exact native scan failure and preserves its error provenance', async () => {
  const h = await setup(true)
  const owner = await h.core.scan(scanOptions())
  const session = h.native.liveSessions()[0]
  const membership = [...session.scans.keys()][0]
  const startOperationId = h.native.opsInvoked('scan.start')[0].operationId
  const scope = h.native.opsInvoked('terminal-evidence.reserve')[0].scope
  const [reservationId] = [...h.native.terminalEvidenceReservations.keys()]
  const raw = {
    t: 'scan-end',
    operationId: membership,
    startOperationId,
    reason: 'source-failed',
    version: 2,
    source: 'adapter',
    generation: '7',
    error: {
      code: 'adapter.powered-off',
      domain: 'adapter',
      operation: 'scan.start',
      retryability: 'never',
      detail: 'native detail',
      platform: null
    }
  }
  const canonicalBytes = Buffer.from(JSON.stringify(raw))
  const digest = require('node:crypto').createHash('sha256').update(canonicalBytes).digest('hex')
  const committed = JSON.parse(
    await h.native.invoke(
      session.id,
      'terminal-evidence.commit',
      JSON.stringify({ reservationId, canonicalBytesBase64: canonicalBytes.toString('base64'), digest })
    )
  )
  session.scans.delete(membership)
  h.native.push(session, { ...raw, evidence: { producer: 'scan', ...committed.value, scope } })
  const terminal = await owner.observations[Symbol.asyncIterator]().next()
  expect(terminal.value).toMatchObject({
    kind: 'terminal',
    reason: 'source-failed',
    error: {
      code: 'adapter.powered-off',
      domain: 'adapter',
      operation: 'scan.start',
      retryability: 'never',
      platform: {
        metadataEntries: expect.arrayContaining([
          { key: 'ubm.scan.detail', value: { kind: 'text', value: 'native detail' } }
        ])
      }
    }
  })
  expect(h.native.opsInvoked('terminal-evidence.read').length).toBeGreaterThan(0)
  await settle(80)
  await h.core.destroy()
  await h.internal.destroy()
})

test('native decoder rejects forged producer and malformed terminal shape', () => {
  const { decodeNativeTerminalEvidence } = require('../../../src/backends/reactnative/native-terminal-evidence')
  const receipt = { producer: 'scan', scope: { managerId: 'manager', operationId: 'operation', incarnation: '1' } }
  const notice = { kind: 'terminal', reason: 'owner-released', droppedItems: 0, droppedBytes: 0, replacedItems: 0 }
  const decode = value =>
    decodeNativeTerminalEvidence(
      Buffer.from(JSON.stringify(value)),
      receipt,
      () => {
        throw new Error('unused')
      },
      notice
    )
  expect(() => decode({ t: 'stream-end', consumer: 'x', reason: 'closed', droppedItems: 0, droppedBytes: 0 })).toThrow()
  expect(() => decode({ t: 'scan-end', operationId: 'x', reason: 'owner-released', surprise: true })).toThrow()
  expect(() =>
    decode({ t: 'scan-end', operationId: 'x', reason: 'overflow', droppedItems: 1, droppedBytes: -1, replacedItems: 0 })
  ).toThrow()
})

test('native decoder rejects incomplete and extra local quota error fields', () => {
  const { decodeNativeTerminalEvidence } = require('../../../src/backends/reactnative/native-terminal-evidence')
  const receipt = { producer: 'scan', scope: { managerId: 'manager', operationId: 'operation', incarnation: '1' } }
  const notice = { kind: 'terminal', reason: 'source-failed', droppedItems: 0, droppedBytes: 0, replacedItems: 0 }
  const raw = {
    t: 'scan-end',
    operationId: 'native',
    reason: 'source-failed',
    droppedItems: 0,
    droppedBytes: 0,
    replacedItems: 0,
    sourceLoss: { scope: 'host-global', source: 'advertisement-ingress', lostRecords: 3 },
    error: {
      code: 'stream.quota',
      domain: 'stream',
      operation: 'bounded-stream.source-loss-control',
      retryability: 'never',
      surprise: null
    }
  }
  expect(() =>
    decodeNativeTerminalEvidence(
      Buffer.from(JSON.stringify(raw)),
      receipt,
      () => {
        throw new Error('unused')
      },
      notice
    )
  ).toThrow()
})

test('native subscription decoder restores a structured source error without replacing aggregate counts', () => {
  const { decodeNativeTerminalEvidence } = require('../../../src/backends/reactnative/native-terminal-evidence')
  const receipt = {
    producer: 'subscription',
    scope: { managerId: 'manager', operationId: 'operation', incarnation: '1' }
  }
  const notice = { kind: 'terminal', reason: 'source-failed', droppedItems: 5, droppedBytes: 80, replacedItems: 0 }
  const record = {
    t: 'stream-end',
    consumer: 'consumer',
    reason: 'source-failed',
    droppedItems: 2,
    droppedBytes: 32,
    error: {
      code: 'platform.failure',
      domain: 'platform',
      operation: 'gatt.subscribe',
      detail: 'native notification failure',
      platform: null
    }
  }
  const decoded = decodeNativeTerminalEvidence(
    Buffer.from(JSON.stringify(record)),
    receipt,
    () => {
      throw new Error('unused')
    },
    notice
  )
  expect(decoded.terminal).toMatchObject({
    reason: 'source-failed',
    droppedItems: 5,
    droppedBytes: 80,
    error: {
      code: 'platform.failure',
      domain: 'platform',
      operation: 'gatt.subscribe',
      platform: { domain: 'ubm-mobile', safeMessage: 'native notification failure' }
    }
  })
})

test('native decoder retains the bound UTF-16 scope exactly', () => {
  const { decodeNativeTerminalEvidence } = require('../../../src/backends/reactnative/native-terminal-evidence')
  const scope = { managerId: 'manager\ud800', operationId: 'operation', incarnation: '1' }
  const receipt = { producer: 'scan', scope }
  const notice = { kind: 'terminal', reason: 'owner-released', droppedItems: 0, droppedBytes: 0, replacedItems: 0 }
  const decoded = decodeNativeTerminalEvidence(
    Buffer.from(JSON.stringify({ t: 'scan-end', operationId: 'native', reason: 'owner-released' })),
    receipt,
    () => {
      throw new Error('unused')
    },
    notice
  )
  expect(decoded.scope).toEqual(scope)
})

test('shared core retains a full-queue ingress-loss quota terminal with its distinct global fact', async () => {
  const h = await setup()
  const delivery = { itemCapacity: 2, byteCapacity: 200, reservedControlCapacity: 4, overflowPolicy: 'drop-oldest' }
  const owner = await h.core.scan(scanOptions({ delivery }))
  const events = h.internal.attachedBackend.backend.events()[Symbol.asyncIterator]()
  h.native.emitAdvertisement(undefined, { localName: 'first' })
  h.native.emitAdvertisement(undefined, { localName: 'second' })
  h.native.ingressDrop('advertisement', 7)
  await settle(100)
  const reader = owner.observations[Symbol.asyncIterator]()
  const observed = []
  for (let count = 0; count < 4; count += 1) {
    const next = await reader.next()
    observed.push(next.value)
    if (next.value.kind === 'terminal') break
  }
  const terminal = observed.find(item => item.kind === 'terminal')
  expect(terminal).toMatchObject({
    reason: 'source-failed',
    droppedItems: 0,
    droppedBytes: 0,
    error: { code: 'stream.quota', domain: 'stream', operation: 'bounded-stream.source-loss-control' },
    evidence: { producer: 'scan', durability: 'committed' }
  })
  expect(observed.some(item => item.kind === 'value')).toBe(true)
  const raw = h.native.terminalEvidenceCommitted.get(terminal.evidence.ref)
  expect(JSON.parse(Buffer.from(raw.canonicalBytes).toString('utf8'))).toMatchObject({
    reason: 'source-failed',
    droppedItems: 0,
    droppedBytes: 0,
    sourceLoss: { scope: 'host-global', source: 'advertisement-ingress', lostRecords: 7 }
  })
  const diagnostic = await events.next()
  expect(diagnostic.value).toMatchObject({
    kind: 'value',
    value: { kind: 'diagnostic-warning', code: 'native-ingress-drop', detail: { class: 'advertisement', count: 7 } }
  })
  await events.return()
  await settle(80)
  await h.core.destroy()
  await h.internal.destroy()
})

test('shared core hydrates separate owner and joined proofs for one host-global loss', async () => {
  const h = await setup()
  const owner = await h.core.scan(scanOptions({ sharing: { mode: 'owner', allowSharing: true } }))
  const joined = await h.core.scan(
    scanOptions({ sharing: { mode: 'join', sharedLeaseId: owner.leaseId, token: owner.shareToken } })
  )
  h.native.ingressDrop('advertisement', 11)
  const ownerTerminal = (await owner.observations[Symbol.asyncIterator]().next()).value
  const joinedTerminal = (await joined.observations[Symbol.asyncIterator]().next()).value
  for (const terminal of [ownerTerminal, joinedTerminal]) {
    expect(terminal).toMatchObject({
      kind: 'terminal',
      reason: 'overflow',
      droppedItems: 0,
      droppedBytes: 0,
      evidence: { producer: 'scan', durability: 'committed' }
    })
    const raw = h.native.terminalEvidenceCommitted.get(terminal.evidence.ref)
    expect(JSON.parse(Buffer.from(raw.canonicalBytes).toString('utf8'))).toMatchObject({
      sourceLoss: { scope: 'host-global', source: 'advertisement-ingress', lostRecords: 11 },
      droppedItems: 0
    })
  }
  expect(ownerTerminal.evidence.scope).not.toEqual(joinedTerminal.evidence.scope)
  await settle(80)
  await h.core.destroy()
  await h.internal.destroy()
})
