const { rustCoreHarness, environment, settle } = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { createPublicBleManager } = require('../../../src/public/ble-manager')

const failure = {
  code: 'adapter.powered-off',
  domain: 'adapter',
  operation: 'scan.start',
  retryability: 'never',
  detail: 'adapter state changed',
  platform: { domain: 'corebluetooth', code: 'poweredOff', message: 'adapter powered off', metadata: { state: 'off' } }
}
async function setup(version = 2) {
  const h = rustCoreHarness()
  const calls = []
  h.native.negotiateScanEndVersion = async (session, supported) => {
    calls.push([session, supported])
    return JSON.stringify({ version })
  }
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h))
  return { ...h, calls, internal, manager: await createPublicBleManager(internal, () => 1000) }
}
function end(native, membership, startOperationId) {
  const session = native.liveSessions()[0]
  session.scans.delete(membership)
  startOperationId ??= native.opsInvoked('scan.start').at(-1).operationId
  native.push(session, {
    t: 'scan-end',
    operationId: membership,
    reason: 'source-failed',
    startOperationId,
    version: 2,
    source: 'adapter',
    generation: '42',
    error: failure
  })
}

test('negotiated v2 reaches the real public scan event with native cause and source evidence', async () => {
  const { native, manager, calls } = await setup()
  const scan = await manager.scan()
  const next = scan.events[Symbol.asyncIterator]().next()
  const membership = [...native.liveSessions()[0].scans.keys()][0]
  end(native, membership)
  const error = await next.catch(e => e)
  expect(calls).toHaveLength(1)
  expect(JSON.parse(calls[0][1])).toEqual([1, 2])
  expect(error).toMatchObject({
    code: 'adapter.powered-off',
    domain: 'adapter',
    operation: 'scan.start',
    retryability: 'never',
    platform: {
      domain: 'corebluetooth',
      code: 'poweredOff',
      safeMessage: 'adapter powered off',
      metadata: { state: 'off' }
    }
  })
  expect(error.platform.metadataEntries).toEqual(
    expect.arrayContaining([
      { key: 'ubm.scan.source', value: { kind: 'text', value: 'adapter' } },
      { key: 'ubm.scan.generation', value: { kind: 'text', value: '42' } },
      { key: 'ubm.scan.detail', value: { kind: 'text', value: 'adapter state changed' } },
      { key: 'ubm.scan.platform.message', value: { kind: 'text', value: 'adapter powered off' } }
    ])
  )
  await scan.stop()
  await manager.destroy()
})

test('unknown negotiation fails before a scan starts', async () => {
  await expect(setup(3)).rejects.toMatchObject({ code: 'protocol.incompatible' })
})

test('legacy native is never offered v2 and still scans', async () => {
  const h = rustCoreHarness()
  const internal = await createReactNativeBleManagerWithEnvironment(environment(h))
  const manager = await createPublicBleManager(internal, () => 1000)
  const scan = await manager.scan()
  h.native.endScans('source-failed')
  await settle(40)
  await scan.stop()
  await manager.destroy()
})

test('a delayed old source failure cannot terminate a newly starting scan', async () => {
  const { native, manager } = await setup()
  const first = await manager.scan()
  const oldMembership = [...native.liveSessions()[0].scans.keys()][0]
  const oldStart = native.opsInvoked('scan.start')[0].operationId
  await first.stop()
  native.hold('scan.start')
  const starting = manager.scan()
  await settle(40)
  end(native, oldMembership, oldStart)
  await settle(40)
  native.release('scan.start')
  const second = await starting
  expect(native.liveSessions()[0].scans.size).toBe(1)
  await second.stop()
  await manager.destroy()
})

test('an aborted scan remains closed when its delayed failure arrives', async () => {
  const { native, manager, internal } = await setup()
  const diagnostics = internal.attachedBackend.backend.events()[Symbol.asyncIterator]()
  const controller = new AbortController()
  const scan = await manager.scan({ signal: controller.signal })
  const membership = [...native.liveSessions()[0].scans.keys()][0]
  const observations = scan.observations[Symbol.asyncIterator]()
  controller.abort()
  await settle(60)
  end(native, membership)
  await settle(60)
  const terminal = await observations.next()
  let diagnostic
  do {
    diagnostic = await diagnostics.next()
  } while (diagnostic.value.value.code !== 'unmatched-scan-end')
  expect(JSON.parse(diagnostic.value.value.detail.failure)).toMatchObject({
    code: 'adapter.powered-off',
    platform: { code: 'poweredOff' }
  })
  expect(diagnostic.value.value.detail).toMatchObject({ source: 'adapter', generation: '42' })
  expect(terminal.value.reason).not.toBe('source-failed')
  const replacement = await manager.scan()
  await replacement.stop()
  await manager.destroy()
})

test('a v2 failure for the same membership but a different start cannot end the live scan', async () => {
  const { native, internal, manager } = await setup()
  const events = internal.attachedBackend.backend.events()[Symbol.asyncIterator]()
  const scan = await manager.scan()
  const session = native.liveSessions()[0]
  const membership = [...session.scans.keys()][0]
  native.push(session, {
    t: 'scan-end',
    operationId: membership,
    startOperationId: 'stale-start',
    reason: 'source-failed',
    version: 2,
    source: 'radio',
    generation: '41',
    error: failure
  })
  let diagnostic
  do {
    diagnostic = await events.next()
  } while (diagnostic.value.value.code !== 'unmatched-scan-end')
  expect(JSON.parse(diagnostic.value.value.detail.failure)).toMatchObject({ code: 'adapter.powered-off' })
  expect(session.scans.has(membership)).toBe(true)
  expect((await scan.stop()).state).toBe('released')
  await manager.destroy()
})
