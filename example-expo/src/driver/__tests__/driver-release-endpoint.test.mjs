import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import * as urls from '../../../../examples-shared/driver/driver-url.ts'
import { RemoteDriverChannel } from '../../../../examples-shared/driver/remote-channel.ts'
import { ScenarioRegistry } from '../../../../examples-shared/driver/scenario-core.ts'
import { createFakeRuntime } from '../../../../examples-shared/driver/__tests__/fake-runtime.mjs'

function resolve(development, explicitUrl, scriptUrl = 'http://localhost:8081/index.bundle') {
  let scriptReads = 0
  const resolution = urls.resolvePhoneDriverUrl({
    development,
    explicitUrl,
    source: 'EXPO_PUBLIC_UBM_DRIVER_URL',
    readScriptUrl: () => {
      scriptReads += 1
      return scriptUrl
    }
  })
  const sockets = []
  if (resolution !== null) {
    const runtime = createFakeRuntime('expo/android')
    const channel = new RemoteDriverChannel({
      url: resolution.url,
      noHostReason: resolution.reason,
      registry: new ScenarioRegistry([]),
      runtime,
      identity: {
        host: 'expo',
        platform: 'android',
        backend: 'expo/android',
        model: 'phone',
        osVersion: '16',
        appBuild: { dev: development }
      },
      createSocket(url) {
        sockets.push(url)
        return { send() {}, close() {} }
      }
    })
    channel.start()
    channel.stop()
  }
  return { resolution, sockets, scriptReads }
}

test('Release defaults off even with a Metro-looking source URL', () => {
  for (const explicitUrl of [undefined, null, '']) {
    assert.deepEqual(resolve(false, explicitUrl), { resolution: null, sockets: [], scriptReads: 0 })
  }
})

test('Release explicit endpoint enables the actual channel without reading Metro', () => {
  const actual = resolve(false, 'ws://127.0.0.1:8795/host')
  assert.equal(actual.resolution.url, 'ws://127.0.0.1:8795/host')
  assert.deepEqual(actual.sockets, ['ws://127.0.0.1:8795/host'])
  assert.equal(actual.scriptReads, 0)
})

test('explicit off or invalid URL never creates a socket or falls back in either build mode', () => {
  for (const development of [false, true]) {
    for (const explicitUrl of [
      'off',
      'http://localhost:8795/host',
      'ws://localhost:bad/host',
      'ws://localhost:8795/host\n',
      ' ws://localhost:8795/host'
    ]) {
      const actual = resolve(development, explicitUrl)
      assert.equal(actual.resolution.url, null)
      assert.match(actual.resolution.reason, /EXPO_PUBLIC_UBM_DRIVER_URL/)
      assert.deepEqual(actual.sockets, [])
      assert.equal(actual.scriptReads, 0)
    }
  }
})

for (const development of [false, true]) {
  for (const fragment of ['#fragment', '#']) {
    test(`${development ? 'development' : 'Release'} refuses WebSocket fragment ${JSON.stringify(fragment)} without opening a socket`, () => {
      const actual = resolve(development, `ws://localhost:8795/host${fragment}`)
      assert.deepEqual(actual.sockets, [])
      assert.equal(actual.resolution.url, null)
      assert.equal(actual.scriptReads, 0)
    })
  }
}

test('development still automatically discovers Metro and embedded bundles report no host', () => {
  const metro = resolve(true, undefined)
  assert.equal(metro.scriptReads, 1)
  assert.deepEqual(metro.sockets, ['ws://localhost:8795/host'])
  const embedded = resolve(true, undefined, 'file:///main.jsbundle')
  assert.equal(embedded.scriptReads, 1)
  assert.equal(embedded.resolution.url, null)
  assert.deepEqual(embedded.sockets, [])
  assert.match(embedded.resolution.reason, /not served by Metro/)
})

test('Expo reference adapter routes explicit configuration through shared authority before creating the channel', () => {
  const source = readFileSync(new URL('../app-driver.ts', import.meta.url), 'utf8')
  assert.match(
    source,
    /resolvePhoneDriverUrl\(\{[\s\S]*development: __DEV__[\s\S]*explicitUrl: process\.env\.EXPO_PUBLIC_UBM_DRIVER_URL/
  )
  assert.match(source, /if \(resolution === null\) return null/)
  assert.doesNotMatch(source, /if \(!__DEV__\) return null/)
  assert.doesNotMatch(source, /explicitDriverUrl\(/)
})

test('reference guidance documents Release opt-in and remote BLE command risk', () => {
  for (const path of ['../../../../example-expo/README.md', '../../../../examples-shared/driver/README.md']) {
    const doc = readFileSync(new URL(path, import.meta.url), 'utf8')
    assert.match(doc, /Release[^\n]*default[^\n]*off/)
    assert.match(doc, /EXPO_PUBLIC_UBM_DRIVER_URL=ws:\/\/127\.0\.0\.1:8795\/host/)
    assert.match(doc, /remote commands\s+control BLE/)
    assert.match(doc, /build[^\n]*bundle/i)
    assert.doesNotMatch(doc, /Release intentionally\s+disables that development control surface/)
  }
})

test('copied-package refresh guidance requires owned Metro restart, full reload and loaded identity verification', () => {
  const doc = readFileSync(new URL('../../../README.md', import.meta.url), 'utf8')
  assert.match(doc, /pnpm --dir example-expo start --clear --port 8082/)
  assert.match(doc, /full app Reload/)
  assert.match(doc, /actually loaded bundle/)
  assert.match(doc, /Do not weaken\s+identity or protocol guards/)
})
