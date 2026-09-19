// Tauri host adapter: the webview bundle resolves `unified-ble-manager` from
// the example's own sources and from the shared driver to the same file, so
// the page holds one package instance.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

const configFile = fileURLToPath(new URL('../vite.config.mts', import.meta.url))
const exampleSource = fileURLToPath(new URL('../src/driver.ts', import.meta.url))
const sharedSource = fileURLToPath(new URL('../../examples-shared/driver/scenarios/heart-rate.ts', import.meta.url))

test('example and shared imports of unified-ble-manager resolve to one checkout package', async () => {
  const server = await createServer({ configFile, logLevel: 'silent', server: { middlewareMode: true, hmr: false, watch: null } })
  try {
    for (const id of ['unified-ble-manager', 'unified-ble-manager/tauri', 'unified-ble-manager/profiles/heart-rate']) {
      const fromExample = await server.pluginContainer.resolveId(id, exampleSource)
      const fromShared = await server.pluginContainer.resolveId(id, sharedSource)
      assert.ok(fromExample !== null, `${id} resolves from example-tauri/src`)
      assert.equal(fromExample.id, fromShared.id, id)
      assert.match(fromExample.id, /\/lib\/module\//)
    }
  } finally {
    await server.close()
  }
})
