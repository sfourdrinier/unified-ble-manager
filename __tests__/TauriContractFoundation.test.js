'use strict'

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')
const read = relative => fs.readFileSync(path.join(root, relative), 'utf8')

describe('framework-neutral desktop IPC contract', () => {
  test('desktop host identity describes execution placement rather than a framework brand', () => {
    const identity = read('src/backend-contract/identity.ts')

    expect(identity).toContain("'desktop-native'")
    expect(identity).toContain("'desktop-webview'")
    expect(identity).not.toMatch(/HostKind\s*=.*'electron-main'/)
    expect(identity).not.toMatch(/HostKind\s*=.*'electron-renderer'/)
  })

  test('generic IPC authority is available without importing an Electron entrypoint', () => {
    const ipc = read('src/backend-contract/ipc.ts')
    const contract = read('src/backend-contract/index.ts')

    expect(ipc).toContain('IpcClientIdentity')
    expect(ipc).toContain('IpcClientLeaseIdentity')
    expect(ipc).toContain('TrustedIpcCaller')
    expect(ipc).toContain('IpcArbiterContext')
    expect(contract).toContain("from './ipc'")
  })

  test('Electron keeps compatibility aliases over the generic IPC authority', () => {
    const electron = read('src/backend-contract/electron.ts')

    expect(electron).toContain("from './ipc'")
    expect(electron).toContain('IpcArbiterContext as ElectronMainArbiterContext')
  })
})
