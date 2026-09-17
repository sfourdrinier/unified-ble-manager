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
  // Watch the probe consumer plus the library's TS sources (273 files, far
  // under the inotify budget). Watching the whole worktree (repo root)
  // exhausts the host inotify budget (ENOSPC): it drags in every
  // node_modules tree plus example/android .cxx build dirs. The symlinked
  // `unified-ble-manager` package still resolves (see nodeModulesPaths /
  // extraNodeModules below). The R01 producer alias resolves source files
  // under <repo>/src, and Metro refuses to hash files outside watchFolders,
  // so src/ must be watched for the RUSTCORE leg to bundle.
  watchFolders: [__dirname, path.join(root, 'src')],

  resolver: {
    blockList: exclusionList(modules.map(m => new RegExp(`^${escape(path.join(root, 'node_modules', m))}\\/.*$`))),
    disableHierarchicalLookup: true,

    // R01 producer probe alias (probe-only scaffolding, never shipped):
    // resolves the pre-R01 production binding source directly. The binding
    // has no package export yet (R01 owns the entrypoint flip); the alias
    // keeps the probe independent of packaging decisions.
    resolveRequest: (context, moduleName, platform) => {
      if (moduleName === '@ubm-rustcore-producer') {
        return {
          filePath: path.join(root, 'src', 'backends', 'reactnative', 'react-native-rust-core-binding.ts'),
          type: 'sourceFile'
        }
      }
      return context.resolveRequest(context, moduleName, platform)
    },

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
