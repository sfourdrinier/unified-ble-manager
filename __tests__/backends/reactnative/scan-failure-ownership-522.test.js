const {
  rustCoreHarness,
  environment,
  settle,
  scanOptions
} = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const {
  createBleManager,
  createManagerOwnershipAuthority,
  DEFAULT_BLE_MANAGER_OPTIONS
} = require('../../../src/manager/ble-manager')
const { opaqueId } = require('../../../src/backend-contract/primitives')

const cause = {
  code: 'adapter.powered-off',
  domain: 'adapter',
  operation: 'scan.start',
  retryability: 'never',
  detail: 'native detail',
  platform: { domain: 'radio', code: 'off', message: 'off', metadata: {} }
}
async function setup() {
  const h = rustCoreHarness()
  h.native.negotiateScanEndVersion = async () => JSON.stringify({ version: 2 })
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h))
  const manager = await createPublicBleManager(internal, () => 1000)
  return { ...h, internal, manager }
}
async function withSharedCore(h, options = {}) {
  const attached = h.internal.attachedBackend
  const coreManager = await createBleManager(
    {
      attachedBackend: attached,
      clientId: opaqueId('core-test-client', 'client', 'core-test'),
      managerId: opaqueId('core-test-manager', 'manager', 'core-test'),
      ownerMode: 'owning'
    },
    createManagerOwnershipAuthority(attached),
    { ...DEFAULT_BLE_MANAGER_OPTIONS, now: () => 1000, ...options }
  )
  return { coreManager, publicCore: await createPublicBleManager(coreManager, () => 1000) }
}
function end(native, membership, start, error = cause) {
  native.liveSessions()[0].scans.delete(membership)
  native.push(native.liveSessions()[0], {
    t: 'scan-end',
    operationId: membership,
    startOperationId: start,
    reason: 'source-failed',
    version: 2,
    source: 'adapter',
    generation: '42',
    error
  })
}
function holdStartResult(native) {
  const invoke = native.invoke.bind(native)
  let release
  const gate = new Promise(resolve => {
    release = resolve
  })
  native.invoke = async (...args) => {
    const result = await invoke(...args)
    if (args[1] === 'scan.start') await gate
    return result
  }
  return release
}
async function nextWarning(iterator) {
  for (;;) {
    const item = await iterator.next()
    if (item.done || item.value.kind === 'terminal') throw new Error('backend event stream ended')
    if (item.value.kind === 'value' && item.value.value.code === 'unmatched-scan-end') return item.value.value
  }
}

test.each(['react-native', 'shared-core'])(
  '%s public scan retains accepted advertisement before terminal',
  async route => {
    const h = await setup()
    const { native } = h
    const core = route === 'shared-core' ? await withSharedCore(h) : null
    const manager = core?.publicCore ?? h.manager
    const release = holdStartResult(native)
    const starting = manager.scan({ duplicates: 'all' })
    await settle(80)
    native.emitAdvertisement()
    const membership = [...native.liveSessions()[0].scans.keys()][0]
    end(native, membership, native.opsInvoked('scan.start')[0].operationId)
    await settle(100)
    release()
    const scan = await starting
    await settle(100)
    const iterator = scan.observations[Symbol.asyncIterator]()
    expect((await iterator.next()).value).toMatchObject({ kind: 'value' })
    expect((await iterator.next()).value).toMatchObject({
      kind: 'terminal',
      reason: 'source-failed',
      error: { code: cause.code },
      droppedItems: 0
    })
    await manager.destroy()
    if (core !== null) await h.manager.destroy()
  }
)

test('aborted admission retains early failure as an unmatched backend diagnostic', async () => {
  const h = await setup()
  const { native, internal } = h
  const { publicCore: manager } = await withSharedCore(h)
  const eventStream = internal.attachedBackend.backend.events()
  const events = eventStream[Symbol.asyncIterator]()
  const release = holdStartResult(native)
  const controller = new AbortController()
  const start = manager.scan({ signal: controller.signal }).catch(e => e)
  await settle(80)
  const membership = [...native.liveSessions()[0].scans.keys()][0]
  const operation = native.opsInvoked('scan.start')[0].operationId
  controller.abort()
  expect(await start).toMatchObject({ code: 'operation.aborted' })
  end(native, membership, operation)
  await settle(60)
  release()
  await settle(100)
  const warning = await nextWarning(events)
  expect(JSON.parse(warning.detail.failure)).toMatchObject({ code: cause.code, platform: { code: 'off' } })
  await manager.destroy()
  await h.manager.destroy()
}, 4000)

test('ambiguous early terminal reports its original cause and rejects admission', async () => {
  const { native, manager, internal } = await setup()
  const events = internal.attachedBackend.backend.events()[Symbol.asyncIterator]()
  const release = holdStartResult(native)
  const start = manager.scan().catch(e => e)
  await settle(80)
  end(native, 'different-membership', native.opsInvoked('scan.start')[0].operationId)
  await settle(60)
  release()
  expect(await start).toMatchObject({ code: 'protocol.violation' })
  const warning = await nextWarning(events)
  expect(JSON.parse(warning.detail.failure)).toMatchObject({ code: cause.code, platform: { code: 'off' } })
  await manager.destroy()
}, 4000)

test('public terminal accepts maximum metadata entries and long detail without losing either', async () => {
  const { native, manager } = await setup()
  const scan = await manager.scan()
  const iterator = scan.observations[Symbol.asyncIterator]()
  const metadataEntries = Array.from({ length: 1024 }, (_, i) => ({
    key: `k${i}`,
    value: { kind: 'text', value: 'x' }
  }))
  const detail = 'd'.repeat(65537)
  end(native, [...native.liveSessions()[0].scans.keys()][0], native.opsInvoked('scan.start')[0].operationId, {
    ...cause,
    detail,
    platform: { ...cause.platform, metadataEntries }
  })
  const terminal = (await iterator.next()).value
  expect(terminal).toMatchObject({
    kind: 'terminal',
    reason: 'source-failed',
    error: { code: cause.code, platform: { metadataEntries } }
  })
  expect(JSON.stringify(terminal.error.platform)).toContain(detail)
  await manager.destroy()
})

test('a large unmatched failure remains observable without terminating the backend event stream', async () => {
  const { native, manager, internal } = await setup()
  const stream = internal.attachedBackend.backend.events()
  const events = stream[Symbol.asyncIterator]()
  const detail = 'z'.repeat(300000)
  end(native, 'first', 'first')
  end(native, 'unknown', 'unknown', { ...cause, detail })
  await settle(100)
  expect(JSON.parse((await nextWarning(events)).detail.failure).code).toBe(cause.code)
  const warning = await nextWarning(events)
  expect(JSON.parse(warning.detail.failure)).toMatchObject({ code: cause.code, platform: { code: 'off' } })
  expect(warning.detail.failure).toContain(detail)
  expect(stream.isTerminal()).toBe(false)
  await manager.destroy()
})

test('legacy metadata too large for ordered entries remains intact on the public failure', async () => {
  const { native, manager } = await setup()
  const scan = await manager.scan()
  const text = 'm'.repeat(65537)
  end(native, [...native.liveSessions()[0].scans.keys()][0], native.opsInvoked('scan.start')[0].operationId, {
    ...cause,
    platform: { ...cause.platform, metadata: { legacy: text } }
  })
  const terminal = (await scan.observations[Symbol.asyncIterator]().next()).value
  expect(terminal.error.platform.metadata).toMatchObject({ legacy: text, 'ubm.scan.source': 'adapter' })
  await manager.destroy()
})

test('shared core continues charging retained scan values until they drain', async () => {
  const h = await setup()
  const { coreManager, publicCore } = await withSharedCore(h)
  const release = holdStartResult(h.native)
  const starting = publicCore.scan({ duplicates: 'all', observation: { includeRawAdvertisement: true } })
  await settle(80)
  h.native.emitAdvertisement(undefined, { rawRecordB64: 'AQIDBA==' })
  end(h.native, [...h.native.liveSessions()[0].scans.keys()][0], h.native.opsInvoked('scan.start')[0].operationId)
  await settle(60)
  release()
  const scan = await starting
  await settle(100)
  expect(coreManager.core.aggregateQuota.retainedPayloadBytes()).toBeGreaterThan(0)
  const iterator = scan.observations[Symbol.asyncIterator]()
  expect((await iterator.next()).value.kind).toBe('value')
  expect(coreManager.core.aggregateQuota.retainedPayloadBytes()).toBe(0)
  await publicCore.destroy()
  await h.manager.destroy()
})

test('direct backend shared consumer retains its terminal and preceding value after stop', async () => {
  const h = await setup()
  const backend = h.internal.attachedBackend.backend
  const client = opaqueId('direct-client', 'client', 'direct')
  const owner = await backend.scanner.start(scanOptions({ sharing: { mode: 'owner', allowSharing: true } }), client)
  const joined = await backend.scanner.join(owner.leaseId, owner.shareToken, client)
  h.native.emitAdvertisement()
  end(h.native, [...h.native.liveSessions()[0].scans.keys()][0], h.native.opsInvoked('scan.start')[0].operationId)
  await settle(100)
  await joined.stop()
  const iterator = joined.observations[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'value' })
  expect((await iterator.next()).value).toMatchObject({
    kind: 'terminal',
    reason: 'source-failed',
    error: { code: cause.code }
  })
  await h.manager.destroy()
})

test('direct backend preserves pending observations if a subsequent drain fails', async () => {
  const h = await setup()
  const release = holdStartResult(h.native)
  const backend = h.internal.attachedBackend.backend
  const starting = backend.scanner.start(scanOptions(), opaqueId('direct-client', 'client', 'direct'))
  await settle(80)
  h.native.emitAdvertisement()
  await settle(60)
  h.native.push(h.native.liveSessions()[0], { t: 'invalid-record' })
  await settle(80)
  release()
  const lease = await starting
  const iterator = lease.observations[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'value' })
  expect((await iterator.next()).value).toMatchObject({
    kind: 'terminal',
    reason: 'source-failed',
    error: { code: 'protocol.malformed' }
  })
  await h.manager.destroy()
})

test('core cancellation during post-admission refresh reports an already received source failure', async () => {
  const h = await setup()
  const { publicCore } = await withSharedCore(h)
  const events = h.internal.attachedBackend.backend.events()[Symbol.asyncIterator]()
  h.native.hold('counters.describe')
  const controller = new AbortController()
  const starting = publicCore.scan({ signal: controller.signal }).catch(error => error)
  await settle(100)
  end(h.native, [...h.native.liveSessions()[0].scans.keys()][0], h.native.opsInvoked('scan.start')[0].operationId)
  await settle(60)
  controller.abort()
  expect(await starting).toMatchObject({ code: 'operation.aborted' })
  h.native.release('counters.describe')
  await settle(100)
  const result = await nextWarning(events)
  expect(JSON.parse(result.detail.failure)).toMatchObject({ code: cause.code })
  await publicCore.destroy()
  await h.manager.destroy()
})

test.each(['react-native', 'shared-core'])('%s scan cancellation retains accepted observations', async route => {
  const h = await setup()
  const core = route === 'shared-core' ? await withSharedCore(h) : null
  const manager = core?.publicCore ?? h.manager
  const controller = new AbortController()
  const scan = await manager.scan({ signal: controller.signal, duplicates: 'all' })
  h.native.emitAdvertisement()
  await settle(80)
  controller.abort()
  await settle(100)
  const iterator = scan.observations[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'value' })
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
  await manager.destroy()
  if (core !== null) await h.manager.destroy()
})

test('core drains a backend-accepted observation when cancellation wins before forwarding', async () => {
  const h = await setup()
  const backend = h.internal.attachedBackend.backend
  const scanner = backend.scanner
  let releaseRead
  const readGate = new Promise(resolve => {
    releaseRead = resolve
  })
  backend.scanner = {
    ...scanner,
    start: async (...args) => {
      const lease = await scanner.start(...args)
      return {
        ...lease,
        observations: {
          ...lease.observations,
          [Symbol.asyncIterator]() {
            const source = lease.observations[Symbol.asyncIterator]()
            return {
              async next() {
                await readGate
                return source.next()
              },
              return: () => source.return(),
              [Symbol.asyncIterator]() {
                return this
              }
            }
          }
        }
      }
    }
  }
  const { publicCore } = await withSharedCore(h)
  const controller = new AbortController()
  const scan = await publicCore.scan({ signal: controller.signal, duplicates: 'all' })
  h.native.emitAdvertisement()
  await settle(80)
  controller.abort()
  await settle(80)
  releaseRead()
  await settle(80)
  const iterator = scan.observations[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'value' })
  expect((await iterator.next()).value).toMatchObject({ kind: 'terminal', reason: 'owner-released' })
  await publicCore.destroy()
  await h.manager.destroy()
})

test('core deadline during admission retains an early native failure', async () => {
  const h = await setup()
  let fireDeadline
  const timer = {
    scheduleAt: (_at, action) => {
      fireDeadline = action
      return { cancel() {} }
    }
  }
  const { publicCore } = await withSharedCore(h, { timer })
  const events = h.internal.attachedBackend.backend.events()[Symbol.asyncIterator]()
  const release = holdStartResult(h.native)
  const starting = publicCore.scan({ timeoutMs: 100 }).catch(error => error)
  await settle(80)
  fireDeadline()
  expect(await starting).toMatchObject({ code: 'operation.timed-out' })
  end(h.native, [...h.native.liveSessions()[0].scans.keys()][0], h.native.opsInvoked('scan.start')[0].operationId)
  await settle(60)
  release()
  await settle(100)
  expect(JSON.parse((await nextWarning(events)).detail.failure)).toMatchObject({ code: cause.code })
  await publicCore.destroy()
  await h.manager.destroy()
})

test('core keeps a queued backend failure when cancellation arrives before the forwarding iterator', async () => {
  const h = await setup()
  const backend = h.internal.attachedBackend.backend
  const scanner = backend.scanner
  let releaseRead
  const readGate = new Promise(resolve => {
    releaseRead = resolve
  })
  backend.scanner = {
    ...scanner,
    start: async (...args) => {
      const lease = await scanner.start(...args)
      return {
        ...lease,
        observations: {
          ...lease.observations,
          [Symbol.asyncIterator]() {
            const source = lease.observations[Symbol.asyncIterator]()
            return {
              async next() {
                await readGate
                return source.next()
              },
              return: () => source.return(),
              [Symbol.asyncIterator]() {
                return this
              }
            }
          }
        }
      }
    }
  }
  const { publicCore } = await withSharedCore(h)
  const controller = new AbortController()
  const scan = await publicCore.scan({ signal: controller.signal, duplicates: 'all' })
  h.native.emitAdvertisement()
  end(h.native, [...h.native.liveSessions()[0].scans.keys()][0], h.native.opsInvoked('scan.start')[0].operationId)
  await settle(80)
  controller.abort()
  await settle(80)
  releaseRead()
  const iterator = scan.observations[Symbol.asyncIterator]()
  expect((await iterator.next()).value).toMatchObject({ kind: 'value' })
  expect((await iterator.next()).value).toMatchObject({
    kind: 'terminal',
    reason: 'source-failed',
    error: { code: cause.code }
  })
  await publicCore.destroy()
  await h.manager.destroy()
})

test('backend diagnostic overflow preserves accepted warnings and counts the rejected record', async () => {
  const h = await setup()
  const events = h.internal.attachedBackend.backend.events()[Symbol.asyncIterator]()
  for (let i = 0; i < 257; i++) {
    end(h.native, `unknown-${i}`, `unknown-${i}`)
    await settle(20)
  }
  for (let i = 0; i < 256; i++) {
    expect((await events.next()).value).toMatchObject({ kind: 'value', value: { code: 'unmatched-scan-end' } })
  }
  expect((await events.next()).value).toMatchObject({ kind: 'terminal', reason: 'overflow', droppedItems: 1 })
  await h.manager.destroy()
})

test('backend reports a terminal when cancellation reaches an already returned unadopted lease', async () => {
  const h = await setup()
  const backend = h.internal.attachedBackend.backend
  const events = backend.events()[Symbol.asyncIterator]()
  const controller = new AbortController()
  const lease = await backend.scanner.start(
    scanOptions({ signal: controller.signal }),
    opaqueId('direct-client', 'client', 'direct')
  )
  end(h.native, [...h.native.liveSessions()[0].scans.keys()][0], h.native.opsInvoked('scan.start')[0].operationId)
  await settle(80)
  controller.abort()
  await lease.stop()
  const result = await nextWarning(events)
  expect(JSON.parse(result.detail.failure)).toMatchObject({ code: cause.code })
  await h.manager.destroy()
})
