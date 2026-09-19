// Host facts shared by the Web, Tauri and Electron-renderer adapters.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { browserSocket, documentAppState, engineFromUserAgent, platformFromUserAgent } from '../browser/host-facts.ts'
import { createRemoteDriver, createScenarioRegistry } from '../create-driver.ts'
import { createHub } from '../server/hub.mjs'
import { adapterHostManager } from '../host.ts'
import { createFakeHost, createFakeManager } from './fake-host.mjs'

const CHROME_MAC = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36'
const ELECTRON_WIN = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) ubm/1.0 Chrome/140.0.0.0 Electron/43.2.0 Safari/537.36'
const CHROME_ANDROID = 'Mozilla/5.0 (Linux; Android 16; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Mobile Safari/537.36'

test('platform and engine come from the user agent, unknown stays unknown', () => {
  assert.equal(platformFromUserAgent(CHROME_MAC), 'macos')
  assert.equal(platformFromUserAgent(ELECTRON_WIN), 'windows')
  assert.equal(platformFromUserAgent(CHROME_ANDROID), 'android')
  assert.equal(platformFromUserAgent('curl/8'), 'unknown')
  assert.equal(engineFromUserAgent(CHROME_MAC), 'Chrome 140.0.0.0')
  assert.equal(engineFromUserAgent(ELECTRON_WIN), 'Electron 43.2.0')
  assert.equal(engineFromUserAgent('curl/8'), 'unknown-engine')
})

test('page visibility is the app state: visible is foreground, hidden is not', () => {
  const listeners = new Set()
  const document = {
    visibilityState: 'visible',
    addEventListener: (type, listener) => type === 'visibilitychange' && listeners.add(listener),
    removeEventListener: (type, listener) => listeners.delete(listener)
  }
  const source = documentAppState(document)
  assert.deepEqual(source.current(), { state: 'visible', foreground: true })
  const seen = []
  const unsubscribe = source.subscribe(reading => seen.push(reading))
  document.visibilityState = 'hidden'
  for (const listener of listeners) listener()
  assert.deepEqual(seen, [{ state: 'hidden', foreground: false }])
  unsubscribe()
  assert.equal(listeners.size, 0)
})

test('the browser socket speaks to the real hub (standard WebSocket API)', async () => {
  const hub = createHub({ port: 0, host: '127.0.0.1' })
  const { port } = await hub.listen()
  const { manager } = createFakeManager()
  const host = createFakeHost({ manager, host: 'tauri', adapterHostManager })
  const remote = createRemoteDriver(host, createScenarioRegistry(host), { url: `ws://127.0.0.1:${port}/host`, reason: 'test', createSocket: browserSocket })
  try {
    remote.start()
    for (let attempt = 0; attempt < 100 && remote.state().hostId === null; attempt += 1) await new Promise(resolve => setTimeout(resolve, 10))
    assert.equal(remote.state().hostId, 'tauri-macos-fake')
  } finally {
    remote.stop()
    await hub.close()
  }
})
