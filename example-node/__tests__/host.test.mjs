// Node desktop host adapter: explicit backend selection (never a silent
// fallback), the identity it announces, and its WebSocket against the real hub.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createHub } from '../../examples-shared/driver/server/hub.mjs'
import { createRemoteDriver, createScenarioRegistry } from '../../examples-shared/driver/index.ts'
import { createNodeDriverHost, nodeIdentity, nodeSocket, parseBackend } from '../host.ts'

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

test('the node host registers with the hub over its WebSocket without touching the radio', async () => {
  const hub = createHub({ port: 0, host: '127.0.0.1' })
  const { port } = await hub.listen()
  const host = createNodeDriverHost('corebluetooth')
  const remote = createRemoteDriver(host, createScenarioRegistry(host), { url: `ws://127.0.0.1:${port}/host`, reason: 'test', createSocket: nodeSocket })
  try {
    remote.start()
    for (let attempt = 0; attempt < 100 && remote.state().hostId === null; attempt += 1) await new Promise(resolve => setTimeout(resolve, 10))
    const [listed] = hub.hosts()
    assert.equal(listed.host, 'node')
    assert.equal(listed.backend, 'node/corebluetooth')
    assert.equal(listed.scenarios.length, 7)
    assert.equal(remote.state().hostId, listed.hostId)
  } finally {
    remote.stop()
    await hub.close()
  }
})
