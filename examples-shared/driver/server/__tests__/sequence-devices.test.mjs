import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createHub } from '../hub.mjs'
import { connectControl } from '../client.mjs'
import { formatComparison, resolveDevice, runSequence, validateSequence } from '../sequence.mjs'
import { matchesTarget } from '../targets.mjs'
import { startNodeHost, waitFor } from './node-host.mjs'

const STRAP_A = 'Polar H10 E997042F'
const STRAP_B = 'Polar H10 E9B93D29'

async function withHosts(specs, run) {
  const hub = createHub({ port: 0, host: '127.0.0.1', onRecord: () => {} })
  const { port } = await hub.listen()
  const client = await connectControl(`ws://127.0.0.1:${port}/control`)
  const hosts = specs.map(spec => startNodeHost({ port, ...spec }))
  try {
    await waitFor(() => hub.hosts().length === specs.length)
    await run(client)
  } finally {
    for (const host of hosts) host.channel.stop()
    await client.close()
    await hub.close()
  }
}

const android = { hostId: 'expo-android-pixel', host: 'expo', platform: 'android' }
const iphone = { hostId: 'expo-ios-iphone', host: 'expo', platform: 'ios' }
const mac = { hostId: 'node-macos-mac', host: 'node', platform: 'macos' }

test('a device binding resolves by host id, then host kind, then platform', () => {
  const devices = { android: 'by-platform', expo: 'by-kind', 'expo-android-pixel': 'by-id' }
  assert.equal(resolveDevice(devices, android), 'by-id')
  assert.equal(resolveDevice(devices, iphone), 'by-kind')
  assert.equal(resolveDevice({ android: 'by-platform', node: 'by-kind' }, android), 'by-platform')
  assert.equal(resolveDevice({ ios: STRAP_B }, android), null)
  assert.equal(resolveDevice(undefined, android), null)
  assert.equal(resolveDevice({}, { hostId: 'x', host: 'y', platform: 'toString' }), null, 'inherited keys are not bindings')
})

test('sequence devices must map targets to non-empty device names; target may list several', () => {
  const steps = [{ note: 'n' }]
  assert.throws(() => validateSequence({ name: 'x', devices: ['a'], steps }), /devices must be an object/)
  assert.throws(() => validateSequence({ name: 'x', devices: { android: 3 }, steps }), /devices\.android must be a non-empty device name/)
  assert.throws(() => validateSequence({ name: 'x', devices: { android: '' }, steps }), /devices\.android must be a non-empty device name/)
  assert.throws(() => validateSequence({ name: 'x', target: [], steps }), /target must be/)
  assert.ok(validateSequence({ name: 'x', devices: { android: STRAP_A, ios: STRAP_B }, target: ['android', 'ios'], steps }))
  assert.ok(matchesTarget(android, ['ios', 'android']))
  assert.ok(!matchesTarget(mac, ['ios', 'android']))
})

test('each host runs on its own strap: device injected per target into device-taking commands only, a step device wins', async () => {
  await withHosts([{ platform: 'android', model: 'Pixel' }, { platform: 'ios', model: 'iPhone' }], async client => {
    const spec = validateSequence({
      name: 'two straps',
      defaultTimeoutMs: 2_000,
      devices: { android: STRAP_A, ios: STRAP_B },
      steps: [
        { run: 'demo', command: 'pick' },
        { run: 'demo', command: 'pick', args: { device: 'Polar H10 override' }, hosts: ['ios'] },
        { run: 'demo', command: 'stop' }
      ]
    })
    const summary = await runSequence(client, spec)
    const byPlatform = Object.fromEntries(summary.hosts.map(host => [host.platform, host]))
    assert.equal(byPlatform.android.device, STRAP_A)
    assert.equal(byPlatform.ios.device, STRAP_B)
    assert.equal(byPlatform.android.steps[0].detail.result.peer.name, STRAP_A)
    assert.equal(byPlatform.ios.steps[0].detail.result.peer.name, STRAP_B)
    assert.equal(byPlatform.ios.steps[1].detail.result.peer.name, 'Polar H10 override')
    assert.deepEqual(summary.hosts.map(host => host.steps[2].status), ['passed', 'passed'], 'stop takes no device and is not given one')
    const table = formatComparison(summary)
    assert.match(table, /device/)
    assert.ok(table.includes(STRAP_A) && table.includes(STRAP_B) && table.includes('Polar H10 override'))
  })
})

test('a sequence that binds devices refuses to run a selected host that has no binding', async () => {
  await withHosts([{ platform: 'android', model: 'Pixel' }, { host: 'node', platform: 'macos', model: 'Mac' }], async client => {
    const spec = validateSequence({ name: 'strict', devices: { android: STRAP_A }, steps: [{ run: 'demo', command: 'pick' }] })
    await assert.rejects(runSequence(client, spec), error => {
      assert.equal(error.code, 'sequence.device-unbound')
      assert.match(error.message, /node-macos-mac/)
      return true
    })
    const summary = await runSequence(client, spec, { target: ['android'] })
    assert.deepEqual(summary.hosts.map(host => [host.platform, host.device, host.passed]), [['android', STRAP_A, true]])
  })
})

test('without devices every host keeps the default device and nothing is injected', async () => {
  await withHosts([{ platform: 'android', model: 'Pixel' }], async client => {
    const summary = await runSequence(client, validateSequence({ name: 'default', steps: [{ run: 'demo', command: 'pick' }] }))
    assert.equal(summary.hosts[0].device, null)
    assert.equal(summary.hosts[0].steps[0].detail.result.peer.name, 'Polar H10 0000')
  })
})

test('parallel-two-straps.json binds one strap per host and runs the core scenarios', () => {
  const spec = validateSequence(JSON.parse(readFileSync(new URL('../sequences/parallel-two-straps.json', import.meta.url), 'utf8')))
  assert.deepEqual(Object.values(spec.devices).sort(), [STRAP_A, STRAP_B])
  const scenarios = new Set(spec.steps.map(step => step.run).filter(Boolean))
  assert.deepEqual([...scenarios].sort(), ['device-info', 'ecg', 'h10-stream', 'link-loss', 'mtu'])
  assert.ok(spec.steps.every(step => step.args?.device === undefined), 'devices come from the sequence map, not the steps')
})
