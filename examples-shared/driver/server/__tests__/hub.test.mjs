import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createHub } from '../hub.mjs'
import { connectControl, runOnHosts } from '../client.mjs'
import { runSequence, compareSummary, formatComparison, validateSequence } from '../sequence.mjs'
import { mismatches } from '../match.mjs'
import { startNodeHost, waitFor } from './node-host.mjs'

async function withHub(run, options = {}) {
  const records = []
  const hub = createHub({ port: 0, host: '127.0.0.1', onRecord: record => records.push(record), ...options })
  const { port } = await hub.listen()
  const hosts = []
  const client = await connectControl(`ws://127.0.0.1:${port}/control`)
  try {
    await run({ hub, port, records, client, hosts })
  } finally {
    for (const host of hosts) host.channel.stop()
    await client.close()
    await hub.close()
  }
}

test('hosts register through hello, get a welcome id, and are listed', async () => {
  await withHub(async ({ hub, port, client, hosts }) => {
    hosts.push(startNodeHost({ port, platform: 'android', model: 'Pixel 9' }))
    hosts.push(startNodeHost({ port, platform: 'ios', model: 'iPhone' }))
    hosts.push(startNodeHost({ port, platform: 'ios', model: 'iPhone' }))
    await waitFor(() => hub.hosts().length === 3)
    const { hosts: listed } = await client.request({ type: 'list' })
    assert.deepEqual(listed.map(host => host.hostId).sort(), ['expo-android-pixel-9', 'expo-ios-iphone', 'expo-ios-iphone-2'])
    assert.deepEqual(listed[0].scenarios.map(scenario => scenario.id), ['demo'])
    await waitFor(() => hosts.every(host => host.channel.state().hostId !== null))
  })
})

test('run dispatches to all targeted hosts through the app registry and collects each answer', async () => {
  await withHub(async ({ hub, port, client, hosts, records }) => {
    hosts.push(startNodeHost({ port, platform: 'android', model: 'A' }), startNodeHost({ port, platform: 'ios', model: 'I' }))
    await waitFor(() => hub.hosts().length === 2)
    const outcomes = await runOnHosts(client, { target: 'all', scenario: 'demo', command: 'start', args: { ticks: 2 }, timeoutMs: 2_000 })
    assert.deepEqual(outcomes.map(outcome => [outcome.platform, outcome.ok, outcome.result]).sort(), [
      ['android', true, { ticks: 2 }],
      ['ios', true, { ticks: 2 }]
    ])
    const ticks = records.filter(record => record.message?.type === 'event' && record.message.event.kind === 'tick')
    assert.equal(ticks.length, 4)
    assert.ok(ticks.every(record => typeof record.message.event.atMs === 'number'))
  })
})

test('errors keep their typed code; unknown targets and timeouts are explicit', async () => {
  await withHub(async ({ hub, port, client, hosts }) => {
    hosts.push(startNodeHost({ port, platform: 'ios', model: 'I' }))
    await waitFor(() => hub.hosts().length === 1)
    const [unsupported] = await runOnHosts(client, { target: 'ios', scenario: 'demo', command: 'unsupported' })
    assert.deepEqual([unsupported.ok, unsupported.error.code], [false, 'capability.unsupported'])
    const [unknown] = await runOnHosts(client, { target: 'ios', scenario: 'missing', command: 'x' })
    assert.equal(unknown.error.code, 'scenario.unknown')
    const [hung] = await runOnHosts(client, { target: 'ios', scenario: 'demo', command: 'hang', timeoutMs: 50 })
    assert.equal(hung.error.code, 'driver.command-timeout')
    await assert.rejects(runOnHosts(client, { target: 'android', scenario: 'demo', command: 'start' }), { code: 'driver.no-matching-host' })
  })
})

test('a host that disconnects with a command pending answers driver.host-disconnected', async () => {
  await withHub(async ({ hub, port, client, hosts }) => {
    const host = startNodeHost({ port, platform: 'android', model: 'A', reconnect: { initialMs: 10_000, maxMs: 10_000 } })
    hosts.push(host)
    await waitFor(() => hub.hosts().length === 1)
    const pending = runOnHosts(client, { target: 'android', scenario: 'demo', command: 'hang', timeoutMs: 2_000 })
    await new Promise(resolve => setTimeout(resolve, 50))
    host.channel.stop()
    const [outcome] = await pending
    assert.equal(outcome.error.code, 'driver.host-disconnected')
  })
})

test('a host speaking the retired phone protocol is rejected and recorded', async () => {
  await withHub(async ({ port, records }) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/host`)
    await new Promise(resolve => socket.addEventListener('open', resolve))
    socket.send(JSON.stringify({ type: 'hello', protocol: 'ubm-phone-driver/1', platform: 'ios', model: 'x', osVersion: '1', appBuild: {}, scenarios: [] }))
    const code = await new Promise(resolve => socket.addEventListener('close', event => resolve(event.code)))
    assert.equal(code, 4400)
    assert.equal(records.find(record => record.event === 'host-rejected').detail.code, 'protocol.version-mismatch')
  })
})

test('a sequence runs per host, honours host filters and expectations, and compares outcomes', async () => {
  await withHub(async ({ hub, port, client, hosts }) => {
    hosts.push(startNodeHost({ port, platform: 'android', model: 'A' }), startNodeHost({ port, platform: 'ios', model: 'I' }))
    await waitFor(() => hub.hosts().length === 2)
    const spec = validateSequence({
      name: 'demo',
      defaultTimeoutMs: 2_000,
      steps: [
        { run: 'demo', command: 'start', args: { ticks: 3 }, capture: ['ticks'] },
        { waitForEvent: 'demo', kind: 'tick', count: 3 },
        { waitForSnapshot: 'demo', match: { phase: 'done', ticks: { $gte: 3 } } },
        { run: 'demo', command: 'unsupported', hosts: ['android'], expect: { ok: false, error: { code: 'capability.unsupported' } } },
        { run: 'demo', command: 'unsupported', hosts: ['ios'] },
        { expectSnapshot: 'demo', match: { phase: 'idle' } },
        { note: 'cleanup', always: true }
      ]
    })
    const progress = []
    const summary = await runSequence(client, spec, { onProgress: event => progress.push(event) })
    const byPlatform = Object.fromEntries(summary.hosts.map(host => [host.platform, host.steps.map(step => step.status)]))
    assert.deepEqual(byPlatform.android, ['passed', 'passed', 'passed', 'passed', 'skipped', 'failed', 'passed'])
    assert.deepEqual(byPlatform.ios, ['passed', 'passed', 'passed', 'skipped', 'failed', 'skipped', 'passed'])
    assert.deepEqual(summary.hosts[0].steps[0].detail.captured, { ticks: 3 })
    const rows = compareSummary(summary)
    assert.deepEqual(rows.filter(row => row.differs).map(row => row.index), [3, 4, 5])
    assert.match(formatComparison(summary), /FAILED/)
    assert.ok(progress.some(event => event.type === 'step-end'))
  })
})

test('invalid sequences are refused before anything runs', () => {
  assert.throws(() => validateSequence({ name: 'x', steps: [{ run: 'demo' }] }), /requires "command"/)
  assert.throws(() => validateSequence({ name: 'x', steps: [{ sleep: 1, note: 'both' }] }), /exactly one/)
})

test('matcher supports partial objects and $ operators', () => {
  assert.deepEqual(mismatches({ a: 1, b: { c: 'hello', d: [1, 2] } }, { b: { c: { $contains: 'ell' }, d: { $length: 2 } } }), [])
  assert.deepEqual(mismatches({ n: 3 }, { n: { $gt: 5 } }), ['$.n: expected $gt 5, got 3'])
  assert.deepEqual(mismatches({ n: null }, { n: { $exists: false } }), [])
  assert.deepEqual(mismatches({ s: 'x' }, { s: { $in: ['x', 'y'] } }), [])
  assert.equal(mismatches({ s: 'x' }, { s: { $bogus: 1 } })[0], '$.s: unknown operator $bogus')
})

test('targets select by host kind, platform or id across mixed hosts', async () => {
  await withHub(async ({ hub, port, client, hosts }) => {
    hosts.push(
      startNodeHost({ port, host: 'expo', platform: 'android', model: 'A' }),
      startNodeHost({ port, host: 'node', platform: 'macos', model: 'Mac' }),
      startNodeHost({ port, host: 'tauri', platform: 'macos', model: 'Mac' })
    )
    await waitFor(() => hub.hosts().length === 3)
    const hostsOf = outcomes => outcomes.map(outcome => outcome.hostId).sort()
    assert.deepEqual(hostsOf(await runOnHosts(client, { target: 'macos', scenario: 'demo', command: 'start', args: { ticks: 0 } })), ['node-macos-mac', 'tauri-macos-mac'])
    assert.deepEqual(hostsOf(await runOnHosts(client, { target: 'node', scenario: 'demo', command: 'start', args: { ticks: 0 } })), ['node-macos-mac'])
    assert.deepEqual(hostsOf(await runOnHosts(client, { target: 'expo-android-a', scenario: 'demo', command: 'start', args: { ticks: 0 } })), ['expo-android-a'])
    const [listed] = (await client.request({ type: 'list' })).hosts.filter(host => host.hostId === 'node-macos-mac')
    assert.deepEqual([listed.host, listed.platform, listed.backend], ['node', 'macos', 'node/test'])
  })
})

test('a hello naming an unknown host kind is refused', async () => {
  await withHub(async ({ port, records }) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/host`)
    await new Promise(resolve => socket.addEventListener('open', resolve))
    socket.send(JSON.stringify({ type: 'hello', protocol: 'ubm-test-driver/1', host: 'toaster', platform: 'ios', backend: 'x', model: 'x', osVersion: '1', appBuild: {}, scenarios: [] }))
    const code = await new Promise(resolve => socket.addEventListener('close', event => resolve(event.code)))
    assert.equal(code, 4400)
    assert.equal(records.find(record => record.event === 'host-rejected').detail.code, 'protocol.invalid-message')
  })
})

test('an upgrade on any other path, such as the retired /phone, is refused and recorded', async () => {
  await withHub(async ({ port, records }) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}/phone`)
    await new Promise(resolve => socket.addEventListener('error', resolve))
    await waitFor(() => records.some(record => record.event === 'upgrade-refused'))
    assert.equal(records.find(record => record.event === 'upgrade-refused').detail.path, '/phone')
  })
})

test('sequences written with the retired "platforms" filter are refused', () => {
  assert.throws(() => validateSequence({ name: 'x', steps: [{ note: 'n', platforms: ['ios'] }] }), /renamed to "hosts"/)
})
