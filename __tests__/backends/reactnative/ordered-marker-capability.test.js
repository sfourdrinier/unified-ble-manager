// __tests__/backends/reactnative/ordered-marker-capability.test.js
const { rustCoreHarness, environment } = require('../../../test-support/react-native/rust-core-harness')
const { createReactNativeManagerHost } = require('../../../src/react-native-manager')

test.each([0, 1])('manager validates marker version %i during session initialization', async version => {
  const harness = rustCoreHarness()
  const original = harness.native.invoke.bind(harness.native)
  const calls = []
  harness.native.invoke = async (session, op, args) => {
    if (op === 'gatt.ordered.marker.version') { calls.push(op); return JSON.stringify({ ok: true, value: { wireVersion: version } }) }
    return original(session, op, args)
  }
  const host = await createReactNativeManagerHost(environment(harness))
  expect(calls.length).toBeGreaterThan(0)
  expect(host.services.reserveOrderedRecordingReclaim).toBeUndefined()
  await host.manager.destroy()
})

test('manager rejects incompatible marker wire before exposing a session', async () => {
  const harness = rustCoreHarness()
  const original = harness.native.invoke.bind(harness.native)
  harness.native.invoke = async (session, op, args) => op === 'gatt.ordered.marker.version'
    ? JSON.stringify({ ok: true, value: { wireVersion: 2 } }) : original(session, op, args)
  await expect(createReactNativeManagerHost(environment(harness))).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
})

test('manager permits a structured unsupported marker capability', async () => {
  const harness = rustCoreHarness()
  const original = harness.native.invoke.bind(harness.native)
  harness.native.invoke = async (session, op, args) => op === 'gatt.ordered.marker.version'
    ? JSON.stringify({ ok: false, error: { code: 'capability.unsupported', domain: 'capability', operation: op, detail: null }, commit: null, retryability: 'never' }) : original(session, op, args)
  const host = await createReactNativeManagerHost(environment(harness))
  await host.manager.destroy()
})


test('manager rejects malformed marker capability response', async () => {
  const harness = rustCoreHarness()
  const original = harness.native.invoke.bind(harness.native)
  harness.native.invoke = async (session, op, args) => op === 'gatt.ordered.marker.version'
    ? JSON.stringify({ ok: true, value: { wireVersion: '1' } }) : original(session, op, args)
  await expect(createReactNativeManagerHost(environment(harness))).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})

test('recorder start version alone does not advertise recorder reclaim', async () => {
  const harness = rustCoreHarness()
  const native = harness.native
  native.orderedRecordingVersion = jest.fn(async () => 1)
  for (const name of ['orderedRecordingReserve', 'orderedRecordingStartStatus', 'orderedRecordingRetire', 'orderedRecordingStart', 'orderedRecordingContext', 'orderedRecordingStop']) native[name] = jest.fn()
  const original = native.invoke.bind(native)
  native.invoke = async (session, op, args) => op === 'gatt.ordered.recording-reclaim.version'
    ? JSON.stringify({ ok: true, value: { wireVersion: 0 } }) : original(session, op, args)
  harness.binding = require('../../../src/backends/reactnative/react-native-rust-core-binding').createReactNativeRustCoreBinding({ platform: 'android', native })
  const host = await createReactNativeManagerHost(environment(harness))
  expect(host.services.reserveOrderedRecordingReclaim).toBeUndefined()
  await host.manager.destroy()
})
