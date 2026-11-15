const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

test('the actual CommonJS package export defers failing storage for both public factories', () => {
  const root = path.resolve(__dirname, '../..')
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-web-export-'))
  try {
    const build = spawnSync(
      process.execPath,
      [
        require.resolve('typescript/bin/tsc'),
        '-p',
        path.join(root, 'tsconfig.build.json'),
        '--rootDir',
        path.join(root, 'src'),
        '--outDir',
        path.join(temporary, 'lib/commonjs'),
        '--module',
        'commonjs',
        '--verbatimModuleSyntax',
        'false',
        '--noEmit',
        'false'
      ],
      { cwd: root, encoding: 'utf8' }
    )
    expect({ status: build.status, output: build.stdout + build.stderr }).toEqual({ status: 0, output: '' })
    fs.copyFileSync(path.join(root, 'package.json'), path.join(temporary, 'package.json'))
    fs.writeFileSync(
      path.join(temporary, 'check.cjs'),
      `
const assert = require('node:assert/strict')
const { webcrypto } = require('node:crypto')
const { createWebBleManager, createWebBleManagerWithEnvironment } = require('unified-ble-manager/web')
const serviceId = '0000180d-0000-1000-8000-00805f9b34fb'
const characteristicId = '00002a37-0000-1000-8000-00805f9b34fb'
let opens = 0, writes = 0, starts = 0, disconnects = 0
const characteristic = {
  uuid: characteristicId, properties: { notify: true, write: true },
  getDescriptors: async () => [],
  writeValueWithResponse: async () => { writes++ },
  startNotifications: async () => { starts++; return characteristic },
  stopNotifications: async () => characteristic,
  addEventListener() {}, removeEventListener() {}
}
const service = { uuid: serviceId, getCharacteristics: async () => [characteristic] }
const listeners = new Set()
const gatt = {
  connected: false,
  async connect() { this.connected = true; return this },
  disconnect() { this.connected = false; disconnects++; for (const listener of listeners) listener() },
  getPrimaryServices: async () => [service]
}
const device = {
  id: 'test-device', gatt,
  addEventListener(_type, listener) { listeners.add(listener) },
  removeEventListener(_type, listener) { listeners.delete(listener) }
}
const bluetooth = { getAvailability: async () => true, requestDevice: async () => device }
const locks = { request() { throw new Error('lock must not be acquired') } }
Object.defineProperty(globalThis, 'crypto', { configurable: true, value: webcrypto })
Object.defineProperty(globalThis, 'indexedDB', { configurable: true, value: {
  open() { opens++; throw new Error('package storage denied') }
} })
Object.defineProperty(globalThis, 'navigator', { configurable: true, value: {
  bluetooth, locks, userActivation: { isActive: true }
} })
Object.defineProperty(globalThis, 'isSecureContext', { configurable: true, value: true })
const environment = {
  implementationVersion: 'test', browserEngine: 'test', bluetooth,
  isSecureContext: () => true, hasTransientUserActivation: () => true,
  now: () => performance.now(), setTimer: (callback, ms) => setTimeout(callback, ms),
  clearTimer: handle => clearTimeout(handle), addPageLifecycleListener: () => () => {}
}
async function run() {
  for (const create of [() => createWebBleManager(), () => createWebBleManagerWithEnvironment({ environment })]) {
    const previousOpens = opens
    const manager = await create()
    try {
      const peer = await manager.choose({ acceptAllDevices: true, optionalServices: [serviceId] })
      const connection = await manager.connect(peer)
      const database = await connection.discover()
      const member = database.characteristic(serviceId, characteristicId)
      await member.write(Uint8Array.of(1))
      assert.equal(opens, previousOpens)
      const handle = database.reserveOrderedRecordingStart()
      const result = await handle.start([{ memberId: 'heart', characteristic: member }], {
        recording: { id: 'package-failure', maxBytes: 1048576, maxRecords: 20 }
      })
      assert.equal(result.kind, 'prepare-failed')
      assert.equal(result.receipt.sourceStarted, false)
      assert.equal(result.receipt.error.source.code, 'capability.unavailable')
      assert.equal(result.receipt.error.source.domain, 'restoration')
      assert.equal(result.receipt.error.source.platformDomain, 'indexeddb')
      assert.equal(result.receipt.error.source.platformCode, 'Error')
      assert.equal(opens, previousOpens + 1)
      await handle.retire()
      await connection.release()
    } finally { await manager.destroy() }
  }
  assert.equal(writes, 2)
  assert.equal(starts, 0)
  assert.equal(disconnects, 2)
  assert.equal(listeners.size, 0)
}
run().catch(error => { console.error(error); process.exitCode = 1 })
`
    )
    const check = spawnSync(process.execPath, [path.join(temporary, 'check.cjs')], {
      cwd: temporary,
      encoding: 'utf8',
      timeout: 15000
    })
    expect({ status: check.status, output: check.stdout + check.stderr }).toEqual({ status: 0, output: '' })
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true })
  }
}, 30000)
