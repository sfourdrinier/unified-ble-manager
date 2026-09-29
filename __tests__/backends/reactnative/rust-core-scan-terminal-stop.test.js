const {
  rustCoreHarness,
  environment,
  scanOptions,
  settle
} = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeBleManagerWithEnvironment } = require('../../../src/react-native-manager')
const { opaqueId } = require('../../../src/backend-contract/primitives')
const { createPublicBleManager } = require('../../../src/public/ble-manager')
const { DEFAULT_PEER } = require('../../../test-support/react-native/deterministic-rust-core-native')

const refusedReceipt = {
  state: 'release-failed',
  failures: [
    {
      resourceKind: 'scan',
      code: 'platform.failure',
      domain: 'platform',
      operation: 'scan.stop',
      detail: 'held stop refused',
      platform: {
        domain: 'android-test',
        code: 'stop-refused',
        message: 'native stop refusal',
        metadata: { attempt: 1 }
      }
    }
  ]
}

test.each([undefined, refusedReceipt, { state: 'released', failures: [] }])(
  'confirmed native membership end wins over late stop outcome %j',
  async lateOutcome => {
    const harness = rustCoreHarness()
    const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const { native } = harness
    const backend = manager.attachedBackend.backend
    const events = []
    const collecting = (async () => {
      for await (const event of backend.events()) {
        if (event.kind === 'value') events.push(event.value)
      }
    })()
    const client = opaqueId('client', 'client', 'test')
    const scan = await backend.scanner.start(scanOptions(), client)
    native.hold('scan.stop')
    const stopping = scan.stop()
    await settle(60)
    expect(native.opsInvoked('scan.stop')).toHaveLength(1)
    native.endScans('operation-timed-out')
    await settle(60)
    native.release('scan.stop', lateOutcome)
    const outcome = await stopping
    const repeated = await scan.stop()
    const stopCalls = native.opsInvoked('scan.stop').length
    const replacement = await backend.scanner.start(scanOptions(), client)
    const replacementAdmissionStopCalls = native.opsInvoked('scan.stop').length
    await replacement.stop()
    await manager.destroy()
    await collecting
    expect(outcome).toEqual({ state: 'released', failures: [] })
    expect(repeated).toEqual({ state: 'released', failures: [] })
    expect(stopCalls).toBe(1)
    expect(replacementAdmissionStopCalls).toBe(1)
    const diagnostics = events.filter(event => event.code === 'scan-stop-after-native-end')
    if (lateOutcome?.state === 'released') {
      expect(diagnostics).toEqual([])
    } else {
      expect(diagnostics).toHaveLength(1)
      expect(diagnostics[0]).toMatchObject({
        kind: 'diagnostic-warning',
        detail: {
          scanSessionId: expect.any(String),
          failures: [
            {
              resourceKind: 'scan',
              error:
                lateOutcome === undefined
                  ? {
                      code: 'lifecycle.invalid-state',
                      domain: 'scan',
                      operation: 'scan.stop',
                      platform: { safeMessage: 'scan-not-active' }
                    }
                  : {
                      code: 'platform.failure',
                      domain: 'platform',
                      operation: 'scan.stop',
                      platform: {
                        domain: 'android-test',
                        code: 'stop-refused',
                        safeMessage: 'native stop refusal',
                        metadata: { attempt: 1 }
                      }
                    }
            }
          ]
        }
      })
    }
  }
)

test.each([false, true])(
  'unconfirmed membership remains owned and retryable (unrelated terminal: %s)',
  async unrelatedTerminal => {
    const harness = rustCoreHarness()
    const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
    const { native } = harness
    const backend = manager.attachedBackend.backend
    const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
    native.hold('scan.stop')
    const stopping = scan.stop()
    await settle(60)
    if (unrelatedTerminal) {
      native.push([...native.sessions.values()][0], {
        t: 'scan-end',
        operationId: 'unrelated-membership',
        reason: 'operation-timed-out'
      })
      await settle(60)
    }
    native.release('scan.stop', refusedReceipt)
    expect(await stopping).toMatchObject({
      state: 'release-failed',
      failures: [{ error: { code: 'platform.failure' } }]
    })
    expect([...native.sessions.values()][0].scans.size).toBe(1)
    expect(await scan.stop()).toEqual({ state: 'released', failures: [] })
    expect(native.opsInvoked('scan.stop')).toHaveLength(2)
    await manager.destroy()
  }
)

test('a public scan cannot acquire new undrainable membership after fatal native drain failure', async () => {
  const harness = rustCoreHarness()
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const publicManager = await createPublicBleManager(manager, () => 1000)
  const first = await publicManager.scan()
  harness.native.emitAdvertisement(DEFAULT_PEER, { rssi: 1.5 })
  await settle(80)
  await first.stop()
  const before = harness.native.opsInvoked('scan.start').length
  let outcome
  try {
    const replacement = await publicManager.scan()
    outcome = { admitted: true }
    await replacement.stop()
  } catch (error) {
    outcome = { admitted: false, error }
  }
  const after = harness.native.opsInvoked('scan.start').length
  await publicManager.destroy()
  expect(outcome).toMatchObject({ admitted: false, error: { code: 'lifecycle.destroyed' } })
  expect(after).toBe(before)
})

test('reconciled native absence settles the exact membership while stop is held', async () => {
  const harness = rustCoreHarness()
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const backend = manager.attachedBackend.backend
  const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
  harness.native.hold('scan.stop')
  const stopping = scan.stop()
  await settle(60)
  // Model an authoritative snapshot after native expiry, whose end event was lost.
  ;[...harness.native.sessions.values()][0].scans.clear()
  await backend.rereadAfterControlLoss()
  harness.native.release('scan.stop', refusedReceipt)
  const outcome = await stopping
  await manager.destroy()
  expect(outcome).toEqual({ state: 'released', failures: [] })
})

test('confirmed parent disposal retires cleanup after its held stop is cancelled', async () => {
  const harness = rustCoreHarness()
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const backend = manager.attachedBackend.backend
  const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
  harness.native.hold('scan.stop')
  const stopping = scan.stop()
  await settle(60)
  const disposal = await backend.destroy()
  harness.native.release('scan.stop', refusedReceipt)
  const outcome = await stopping
  const retry = await scan.stop()
  await manager.destroy()
  expect(disposal).toEqual({ state: 'released', failures: [] })
  expect(outcome).toMatchObject({
    state: 'release-failed',
    failures: [{ error: { code: 'operation.aborted', domain: 'core', operation: 'scan.stop' } }]
  })
  expect(retry).toEqual({ state: 'released', failures: [] })
  expect(harness.native.opsInvoked('scan.stop')).toHaveLength(1)
})

test('an older reconciliation snapshot cannot retire a newly admitted membership', async () => {
  const harness = rustCoreHarness()
  const manager = await createReactNativeBleManagerWithEnvironment(environment(harness))
  const backend = manager.attachedBackend.backend
  const session = [...harness.native.sessions.values()][0]
  const oldSnapshot = await harness.native.run(session, 'session.reconcile', {})
  harness.native.hold('session.reconcile')
  const reconciling = backend.rereadAfterControlLoss()
  await settle(60)
  const scan = await backend.scanner.start(scanOptions(), opaqueId('client', 'client', 'test'))
  harness.native.release('session.reconcile', oldSnapshot)
  await reconciling
  harness.native.emitAdvertisement(DEFAULT_PEER, { localName: 'after stale snapshot' })
  await settle(60)
  const terminal = scan.observations.isTerminal()
  const value = terminal ? null : await scan.observations[Symbol.asyncIterator]().next()
  const callsBeforeStop = harness.native.opsInvoked('scan.stop').length
  await scan.stop()
  const callsAfterStop = harness.native.opsInvoked('scan.stop').length
  await manager.destroy()
  expect(terminal).toBe(false)
  expect(value).toMatchObject({
    done: false,
    value: { kind: 'value', value: { localName: { state: 'present', value: 'after stale snapshot' } } }
  })
  expect(callsAfterStop).toBe(callsBeforeStop + 1)
})
