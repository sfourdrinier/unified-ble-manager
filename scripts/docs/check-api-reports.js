'use strict'

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..', '..')
const required = {
  'etc/api/root.api.md': ['find(options', 'choose(options', 'withScan', 'PeerReference', 'createConnectionSupervisor'],
  'etc/api/react-native.api.md': ['createReactNativeBleManager(options?', 'application factory and does not accept'],
  'etc/api/electron-renderer.api.md': [
    'createElectronRendererBleManager',
    'createElectronRendererBleManagerWithEnvironment',
    'ElectronRendererBleManagerEnvironment'
  ],
  'etc/api/tauri.api.md': ['createTauriBleManager', 'createTauriBleManagerWithEnvironment']
}

for (const [relativePath, tokens] of Object.entries(required)) {
  const absolutePath = path.join(root, relativePath)
  const document = fs.readFileSync(absolutePath, 'utf8')
  for (const token of tokens) {
    if (!document.includes(token)) {
      throw new Error(`API report ${relativePath} is missing reviewed token: ${token}`)
    }
  }
}

console.log(`API reports checked: ${Object.keys(required).length} entrypoints`)
