import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  TEST_DRIVER_PROTOCOL,
  decodeAppMessage,
  decodeServerMessage,
  describeError,
  encodeMessage,
  toJsonValue
} from '../protocol.ts'

test('command round-trips and defaults missing args to {}', () => {
  const text = encodeMessage({ type: 'command', id: 'c1', scenario: 'h10-stream', command: 'start', args: { autoReconnect: true } })
  assert.deepEqual(decodeServerMessage(text), {
    ok: true,
    message: { type: 'command', id: 'c1', scenario: 'h10-stream', command: 'start', args: { autoReconnect: true } }
  })
  const bare = decodeServerMessage(JSON.stringify({ type: 'command', id: 'c2', scenario: 's', command: 'stop' }))
  assert.equal(bare.ok, true)
  assert.deepEqual(bare.message.args, {})
})

test('server decode reports invalid json, unknown types, bad args and version mismatch with codes', () => {
  assert.equal(decodeServerMessage('{nope').error.code, 'protocol.invalid-json')
  assert.equal(decodeServerMessage('[]').error.code, 'protocol.invalid-message')
  assert.equal(decodeServerMessage('{"type":"reboot"}').error.code, 'protocol.invalid-message')
  assert.equal(
    decodeServerMessage(JSON.stringify({ type: 'command', id: 'x', scenario: 's', command: 'c', args: [1] })).error.code,
    'protocol.invalid-message'
  )
  assert.equal(decodeServerMessage(JSON.stringify({ type: 'command', id: '', scenario: 's', command: 'c' })).ok, false)
  assert.equal(
    decodeServerMessage(JSON.stringify({ type: 'welcome', protocol: 'ubm-phone-driver/1', hostId: 'p' })).error.code,
    'protocol.version-mismatch'
  )
  assert.deepEqual(decodeServerMessage(JSON.stringify({ type: 'welcome', protocol: TEST_DRIVER_PROTOCOL, hostId: 'expo-ios-1' })), {
    ok: true,
    message: { type: 'welcome', protocol: TEST_DRIVER_PROTOCOL, hostId: 'expo-ios-1' }
  })
})

test('every app message type round-trips through encode/decode', () => {
  const hello = {
    type: 'hello',
    protocol: TEST_DRIVER_PROTOCOL,
    host: 'expo',
    platform: 'android',
    backend: 'expo/android',
    model: 'Pixel 9',
    osVersion: '16',
    appBuild: { ubmVersion: '5.0.0' },
    scenarios: [{ id: 's', title: 'S', description: 'd', commands: [{ name: 'go', label: 'Go', description: '', presets: [], acceptsDevice: true }] }]
  }
  const messages = [
    hello,
    { type: 'snapshot', scenario: 's', atMs: 1, host: 'expo/ios', snapshot: { phase: 'idle' } },
    { type: 'event', event: { scenario: 's', seq: 1, atMs: 2, host: 'expo/ios', kind: 'value', data: { bpm: 60 } } },
    { type: 'result', id: 'c1', scenario: 's', command: 'go', atMs: 3, result: null },
    { type: 'error', id: null, scenario: null, command: null, atMs: 4, error: { code: 'x', message: 'y', detail: null } },
    { type: 'notice', atMs: 5, code: 'outbox-overflow', message: 'dropped', detail: { dropped: 3 } }
  ]
  for (const message of messages) {
    assert.deepEqual(decodeAppMessage(encodeMessage(message)), { ok: true, message })
  }
})

test('the protocol is host-neutral and versioned: ubm-test-driver/1', () => {
  assert.equal(TEST_DRIVER_PROTOCOL, 'ubm-test-driver/1')
})

test('app decode fails closed on an unknown host kind or a hello without backend', () => {
  const base = { type: 'hello', protocol: TEST_DRIVER_PROTOCOL, host: 'expo', platform: 'ios', backend: 'expo/ios', model: 'm', osVersion: '1', appBuild: {}, scenarios: [] }
  assert.equal(decodeAppMessage(JSON.stringify(base)).ok, true)
  for (const host of ['web', 'tauri', 'electron', 'node']) assert.equal(decodeAppMessage(JSON.stringify({ ...base, host })).ok, true)
  assert.equal(decodeAppMessage(JSON.stringify({ ...base, host: 'toaster' })).error.code, 'protocol.invalid-message')
  const { backend: _backend, ...withoutBackend } = base
  assert.equal(decodeAppMessage(JSON.stringify(withoutBackend)).error.code, 'protocol.invalid-message')
  const legacyEvent = { type: 'event', event: { scenario: 's', seq: 1, atMs: 2, platform: 'ios', kind: 'value', data: {} } }
  assert.equal(decodeAppMessage(JSON.stringify(legacyEvent)).error.code, 'protocol.invalid-message')
})

test('app decode rejects a hello from another protocol version', () => {
  const result = decodeAppMessage(
    JSON.stringify({ type: 'hello', protocol: 'ubm-test-driver/2', host: 'expo', platform: 'a', backend: 'x', model: 'b', osVersion: 'c', appBuild: {}, scenarios: [] })
  )
  assert.equal(result.error.code, 'protocol.version-mismatch')
})

test('toJsonValue keeps bytes, bigints, non-finite numbers and drops undefined fields', () => {
  assert.deepEqual(toJsonValue({ a: new Uint8Array([0, 15, 255]), b: 10n, c: Number.NaN, d: undefined, e: [1, undefined] }), {
    a: '000fff',
    b: '10',
    c: 'NaN',
    e: [1, null]
  })
})

test('describeError keeps the typed code and structured detail of a coded error', () => {
  const error = Object.assign(new Error('link lost'), { code: 'connection.lost', domain: 'connection', operation: 'x.y' })
  assert.deepEqual(describeError(error), {
    code: 'connection.lost',
    message: 'link lost',
    detail: { domain: 'connection', operation: 'x.y' }
  })
  assert.deepEqual(describeError(new TypeError('bad')), { code: 'TypeError', message: 'bad', detail: null })
  assert.deepEqual(describeError('plain'), { code: 'non-error-thrown', message: 'plain', detail: null })
  const aggregate = describeError(new AggregateError([new Error('a')], 'both failed'))
  assert.deepEqual(aggregate.detail.errors, [{ code: 'Error', message: 'a', detail: null }])
})

test('a hello whose commands do not say whether they take a device is refused', () => {
  const command = { name: 'go', label: 'Go', description: '', presets: [] }
  const hello = acceptsDevice => ({
    type: 'hello',
    protocol: TEST_DRIVER_PROTOCOL,
    host: 'expo',
    platform: 'ios',
    backend: 'expo/ios',
    model: 'm',
    osVersion: '1',
    appBuild: {},
    scenarios: [{ id: 's', title: 'S', description: 'd', commands: [acceptsDevice === undefined ? command : { ...command, acceptsDevice }] }]
  })
  assert.equal(decodeAppMessage(JSON.stringify(hello(false))).ok, true)
  assert.equal(decodeAppMessage(JSON.stringify(hello(undefined))).error.code, 'protocol.invalid-message')
  assert.equal(decodeAppMessage(JSON.stringify(hello('yes'))).error.code, 'protocol.invalid-message')
})
