import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { test } from 'node:test'

const config = JSON.parse(readFileSync(new URL('../../../example-tauri/src-tauri/tauri.conf.json', import.meta.url), 'utf8'))

// Finding 212: WKWebView suspends an occluded window's page while its socket
// keeps answering pings, so a backgrounded Tauri driver host looked connected
// yet never ran a command. The host window must opt out of that throttling.
test('the Tauri driver host window keeps running while occluded', () => {
  const main = config.app.windows.find(window => window.label === 'main')
  assert.ok(main, 'tauri.conf.json has no main window')
  assert.equal(main.backgroundThrottling, 'disabled')
})
