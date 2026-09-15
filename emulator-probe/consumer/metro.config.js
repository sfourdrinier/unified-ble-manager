// emulator-probe/consumer/metro.config.js
// Minimal bundler config for the probe consumer. Mirrors the example's
// peer-aliasing so the symlinked `unified-ble-manager` (file:../..) resolves
// to this consumer's node_modules copies.

const { getDefaultConfig, mergeConfig } = require('@react-native/metro-config')
const path = require('path')
const escape = (s) => String(s).replace(/[|\\{}()[\]^$+*?.]/g, '\\$&')
const { default: exclusionList } = require('metro-config/private/defaults/exclusionList')
const pak = require('../../package.json')

const root = path.resolve(__dirname, '..', '..')
const modules = Object.keys({ ...pak.peerDependencies })

const config = {
  // Watch ONLY the probe consumer. Watching the whole worktree (repo root)
  // exhausts the host inotify budget (ENOSPC): it drags in every
  // node_modules tree plus example/android .cxx build dirs. The symlinked
  // `unified-ble-manager` package still resolves (see nodeModulesPaths /
  // extraNodeModules below); only live-reload of lane sources is lost,
  // which this battery does not need.
  watchFolders: [__dirname],

  resolver: {
    blockList: exclusionList(modules.map(m => new RegExp(`^${escape(path.join(root, 'node_modules', m))}\\/.*$`))),
    disableHierarchicalLookup: true,

    nodeModulesPaths: [
      path.join(__dirname, 'node_modules')
    ],

    extraNodeModules: {
      '@babel/runtime': path.join(__dirname, 'node_modules', '@babel/runtime'),
      ...modules.reduce((acc, name) => {
        acc[name] = path.join(__dirname, 'node_modules', name)
        return acc
      }, {})
    }
  },

  transformer: {
    getTransformOptions: async () => ({
      transform: {
        experimentalImportSupport: false,
        inlineRequires: true
      }
    })
  }
}

module.exports = mergeConfig(getDefaultConfig(__dirname), config)
