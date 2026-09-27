// Node desktop host adapter: explicit backend selection (never a silent
// fallback), the identity it announces, and its WebSocket against the real hub.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHub } from '../../examples-shared/driver/server/hub.mjs'
import { createRemoteDriver, createScenarioRegistry } from '../../examples-shared/driver/index.ts'
import { createNodeDriverHost, nodeIdentity, nodeSocket, parseBackend, nodeManagerOptions } from '../host.ts'
import { desktopRustCoreAdapterId } from 'unified-ble-manager/node/bluez'

test('the backend is explicit: per-OS default only when none is named, anything unknown is refused', () => {
  assert.equal(parseBackend(undefined, 'darwin'), 'corebluetooth')
  assert.equal(parseBackend(undefined, 'win32'), 'winrt')
  assert.equal(parseBackend(undefined, 'linux'), 'bluez')
  assert.equal(parseBackend('bluez', 'darwin'), 'bluez')
  assert.throws(() => parseBackend('noble', 'darwin'), /must be one of corebluetooth \| winrt \| bluez/)
  assert.throws(() => parseBackend(undefined, 'aix'), /no desktop backend for aix/)
})

test('the identity names the node host, the OS and the explicit entrypoint', () => {
  const identity = nodeIdentity('winrt')
  assert.equal(identity.host, 'node')
  assert.equal(identity.backend, 'node/winrt')
  assert.equal(typeof identity.appBuild.ubmVersion, 'string')
  const host = createNodeDriverHost('corebluetooth')
  assert.equal(host.appState, null)
  assert.equal(host.userGesture, null)
  assert.equal(host.runtime.host, `node/${identity.platform}`)
})

test('the two-adapter client keeps its exact explicit adapter and rejects an empty selection', () => {
  const adapterId = desktopRustCoreAdapterId('bluez', 'hci1')
  assert.equal(adapterId, '/org/bluez/hci1')
  assert.deepEqual(nodeManagerOptions(adapterId), { adapterId: '/org/bluez/hci1' })
  assert.deepEqual(nodeManagerOptions(undefined), {})
  assert.throws(() => createNodeDriverHost('bluez', ''), /adapter/)
  assert.throws(() => nodeManagerOptions('   '), /adapter/)
})

test('the actual CLI rejects misspelled adapter flags instead of silently using the default radio', () => {
  const result = spawnSync(process.execPath, ['example-node/driver.ts', 'list', '--adpater', 'hci1'], {
    cwd: new URL('../../', import.meta.url),
    encoding: 'utf8'
  })
  assert.equal(result.status, 2)
  assert.match(result.stderr, /unknown option --adpater/)
  assert.equal(result.stdout, '')
})

test('the actual CLI accepts trusted BlueZ owner but rejects it for a different backend without radio', () => {
  for (const [backend, status] of [
    ['bluez', 0],
    ['corebluetooth', 1]
  ]) {
    const result = spawnSync(
      process.execPath,
      ['example-node/driver.ts', 'list', '--backend', backend, '--bluez-daemon-owner', ':1.42'],
      {
        cwd: new URL('../../', import.meta.url),
        encoding: 'utf8',
        env: { ...process.env, UBM_BLUEZ_DAEMON_OWNER: '' }
      }
    )
    assert.equal(result.status, status, result.stderr)
    if (status === 0) assert.ok(Array.isArray(JSON.parse(result.stdout)))
    else assert.match(result.stderr, /only valid with the bluez backend/)
  }
})

test('the node host registers with the hub over its WebSocket without touching the radio', async () => {
  const hub = createHub({ port: 0, host: '127.0.0.1' })
  const { port } = await hub.listen()
  const host = createNodeDriverHost('corebluetooth')
  const registry = createScenarioRegistry(host)
  const remote = createRemoteDriver(host, registry, {
    url: `ws://127.0.0.1:${port}/host`,
    reason: 'test',
    createSocket: nodeSocket
  })
  try {
    remote.start()
    for (let attempt = 0; attempt < 100 && remote.state().hostId === null; attempt += 1)
      await new Promise(resolve => setTimeout(resolve, 10))
    const [listed] = hub.hosts()
    assert.equal(listed.host, 'node')
    assert.equal(listed.backend, 'node/corebluetooth')
    assert.deepEqual(listed.scenarios, registry.describe())
    assert.equal(remote.state().hostId, listed.hostId)
  } finally {
    remote.stop()
    await hub.close()
  }
})
