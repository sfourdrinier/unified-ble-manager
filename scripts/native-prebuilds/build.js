#!/usr/bin/env node
'use strict'

const fs = require('fs')
const path = require('path')
const { spawnSync } = require('child_process')
const { NODE_API_VERSION, NATIVE_PREBUILD_TARGETS } = require('./targets')

const root = path.resolve(__dirname, '../..')

function parseBackend(argv) {
  const backendIndex = argv.indexOf('--backend')
  if (backendIndex === -1 || backendIndex + 1 >= argv.length || argv.length !== 2) {
    throw new Error('Usage: node scripts/native-prebuilds/build.js --backend <corebluetooth|winrt>')
  }
  return argv[backendIndex + 1]
}

function runNodeGyp(target) {
  const nodeGyp = require.resolve('node-gyp/bin/node-gyp.js', { paths: [root] })
  const moduleDirectory = path.join(root, ...target.moduleDirectory.split('/'))
  const result = spawnSync(process.execPath, [nodeGyp, 'rebuild', '--release', `--arch=${target.arch}`], {
    cwd: moduleDirectory,
    encoding: 'utf8',
    stdio: 'inherit',
    shell: false,
    env: {
      ...process.env,
      npm_config_napi_version: String(NODE_API_VERSION)
    }
  })
  if (result.error) throw result.error
  if (result.status !== 0) {
    throw new Error(`node-gyp failed for ${target.backend}/${target.platform}-${target.arch}`)
  }
  return moduleDirectory
}

function verifyLoad(target, binaryPath) {
  // eslint-disable-next-line import/no-dynamic-require, global-require
  const nativeModule = require(binaryPath)
  if (target.backend === 'corebluetooth' && typeof nativeModule.createNativeRadio !== 'function') {
    throw new Error('CoreBluetooth prebuild does not export createNativeRadio')
  }
  if (
    target.backend === 'winrt' &&
    (nativeModule.boundaryVersion !== 2 || typeof nativeModule.createContractBoundary !== 'function')
  ) {
    throw new Error('WinRT prebuild does not implement native boundary protocol v2')
  }
}

function main(argv) {
  const backend = parseBackend(argv)
  const target = NATIVE_PREBUILD_TARGETS.find(
    candidate => candidate.backend === backend && candidate.platform === process.platform && candidate.arch === process.arch
  )
  if (target === undefined) {
    throw new Error(`No maintained ${backend} prebuild target exists for ${process.platform}-${process.arch}`)
  }

  const moduleDirectory = runNodeGyp(target)
  const source = path.join(moduleDirectory, 'build', 'Release', `${target.addonName}.node`)
  if (!fs.existsSync(source) || fs.statSync(source).size === 0) {
    throw new Error(`node-gyp did not produce a non-empty native addon: ${source}`)
  }

  const destination = path.join(root, ...target.prebuildPath.split('/'))
  fs.mkdirSync(path.dirname(destination), { recursive: true })
  fs.copyFileSync(source, destination)
  verifyLoad(target, destination)

  const staged = path.join(root, '.native-prebuild-artifact', ...target.prebuildPath.split('/'))
  fs.rmSync(path.join(root, '.native-prebuild-artifact'), { recursive: true, force: true })
  fs.mkdirSync(path.dirname(staged), { recursive: true })
  fs.copyFileSync(destination, staged)

  process.stdout.write(
    `${JSON.stringify({
      artifactName: target.artifactName,
      backend: target.backend,
      platform: target.platform,
      arch: target.arch,
      nodeApiVersion: NODE_API_VERSION,
      prebuildPath: target.prebuildPath,
      bytes: fs.statSync(destination).size
    })}\n`
  )
}

main(process.argv.slice(2))
