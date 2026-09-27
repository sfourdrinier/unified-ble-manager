'use strict'

// Runs the real packaged renderer/preload/main boundary without acquiring BLE.
// Build dist first, then: pnpm exec electron example-electron/driver/smoke-no-radio.cjs
const assert = require('node:assert/strict')
const { app } = require('electron')

const deadline = setTimeout(() => {
  console.error('electron-no-radio-smoke: startup or shutdown timed out')
  app.exit(1)
}, 30000)
deadline.unref()

app.once('browser-window-created', (_event, window) => {
  window.webContents.once('did-finish-load', async () => {
    try {
      const preferences = window.webContents.getLastWebPreferences()
      assert.equal(preferences.sandbox, true)
      assert.equal(preferences.contextIsolation, true)
      assert.equal(preferences.nodeIntegration, false)
      const result = await window.webContents.executeJavaScript(`(async () => {
        const control = window.ubmProcessControl
        const status = JSON.parse(await control.describeBacklog())
        const claim = JSON.parse(await control.prepareClaim(1, 1024))
        let unknownTokenRejected = false
        try { await control.acknowledgeClaim('no-owned-prefix') }
        catch { unknownTokenRejected = true }
        return { status, claim, unknownTokenRejected,
          hasNodeRequire: typeof window.require === 'function',
          controlKeys: Object.keys(control).sort() }
      })()`)
      assert.deepEqual(result.status, { ok: true, value: null })
      assert.equal(result.claim.ok, true)
      assert.equal(result.claim.value.disposed, false)
      assert.equal(result.claim.value.consumerCount, 0)
      assert.deepEqual(result.claim.value.batches, [])
      assert.equal(result.unknownTokenRejected, true)
      assert.equal(result.hasNodeRequire, false)
      assert.deepEqual(result.controlKeys, ['acknowledgeClaim', 'describeBacklog', 'execute', 'prepareClaim', 'recordings'])
      console.log('electron-no-radio-smoke: renderer/preload/main checks passed; requesting normal Quit')
      app.quit()
    } catch (error) {
      console.error('electron-no-radio-smoke: failed', error)
      app.exit(1)
    }
  })
})

// No control server is started; destination port zero prevents attaching this
// smoke to another operator's driver session or queued radio commands.
process.argv.push('--driver-url', 'ws://127.0.0.1:0/host')
require('./main.cjs')
