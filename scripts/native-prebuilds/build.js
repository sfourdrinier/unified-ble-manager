#!/usr/bin/env node
'use strict'

const fs = require('fs')
const path = require('path')
const { spawnSync } = require('child_process')
const { NODE_API_VERSION, NATIVE_PREBUILD_BACKENDS, NATIVE_PREBUILD_TARGETS } = require('./targets')

const root = path.resolve(__dirname, '../..')

function parseBackend(argv) {
  const args = argv.filter(argument => argument !== '--')
  const usage = `Usage: node scripts/native-prebuilds/build.js --backend <${NATIVE_PREBUILD_BACKENDS.join('|')}>`
  const backendIndex = args.indexOf('--backend')
  if (backendIndex === -1 || backendIndex + 1 >= args.length) {
    throw new Error(usage)
  }
  const backend = args[backendIndex + 1]
  if (!NATIVE_PREBUILD_BACKENDS.includes(backend)) {
    throw new Error(usage)
  }
  return backend
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

/**
 * The shared desktop Rust core: a stripped release build for the exact
 * target triple, sealed with the source identity, staged with its sidecar
 * (build-napi-addon.js --out writes both).
 */
function runCargo(target) {
  const destination = path.join(root, ...target.prebuildPath.split('/'))
  const result = spawnSync(
    process.execPath,
    [
      path.join(root, 'scripts', 'ci', 'build-napi-addon.js'),
      '--profile',
      'release',
      '--target',
      target.rustTarget,
      '--out',
      destination
    ],
    { cwd: root, encoding: 'utf8', stdio: 'inherit', shell: false }
  )
  if (result.error) throw result.error
  if (result.status !== 0) {
    throw new Error(`build-napi-addon failed for ${target.backend}/${target.platform}-${target.arch}`)
  }
  return destination
}

function verifyLoad(target, binaryPath) {
  // Dynamic require of the just-built prebuild under test (the retired
  // import/no-dynamic-require suppression was removed: the rule no longer
  // exists in the installed plugin and global-require is not enabled here).
  const nativeModule = require(binaryPath)
  if (target.backend === 'corebluetooth' && typeof nativeModule.createNativeRadio !== 'function') {
    throw new Error('CoreBluetooth prebuild does not export createNativeRadio')
  }
  if (target.backend === 'desktop-core') {
    if (
      typeof nativeModule.nativeBuildIdentity !== 'function' ||
      typeof nativeModule.UbmCentral?.open !== 'function' ||
      typeof nativeModule.UbmCentral?.openSynthetic !== 'function' ||
      typeof nativeModule.UbmCentral?.listAdapters !== 'function'
    ) {
      throw new Error('desktop-core prebuild does not export nativeBuildIdentity + UbmCentral.open/openSynthetic/listAdapters')
    }
    const identity = JSON.parse(nativeModule.nativeBuildIdentity())
    if (identity.profile !== 'release' || identity.target !== target.rustTarget || identity.sourceDigest === 'unsealed') {
      throw new Error(`desktop-core prebuild identity is not a sealed release for ${target.rustTarget}: ${JSON.stringify(identity)}`)
    }
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

  const destination = path.join(root, ...target.prebuildPath.split('/'))
  if (target.builder === 'cargo') {
    runCargo(target)
  } else {
    const moduleDirectory = runNodeGyp(target)
    const source = path.join(moduleDirectory, 'build', 'Release', `${target.addonName}.node`)
    if (!fs.existsSync(source) || fs.statSync(source).size === 0) {
      throw new Error(`node-gyp did not produce a non-empty native addon: ${source}`)
    }
    fs.mkdirSync(path.dirname(destination), { recursive: true })
    fs.copyFileSync(source, destination)
  }
  verifyLoad(target, destination)

  const staged = path.join(root, '.native-prebuild-artifact', ...target.prebuildPath.split('/'))
  fs.rmSync(path.join(root, '.native-prebuild-artifact'), { recursive: true, force: true })
  fs.mkdirSync(path.dirname(staged), { recursive: true })
  fs.copyFileSync(destination, staged)
  if (target.sidecarPath !== null) {
    fs.copyFileSync(
      path.join(root, ...target.sidecarPath.split('/')),
      path.join(root, '.native-prebuild-artifact', ...target.sidecarPath.split('/'))
    )
  }

  process.stdout.write(
    `${JSON.stringify({
      artifactName: target.artifactName,
      backend: target.backend,
      platform: target.platform,
      arch: target.arch,
      nodeApiVersion: NODE_API_VERSION,
      builder: target.builder,
      prebuildPath: target.prebuildPath,
      sidecarPath: target.sidecarPath,
      bytes: fs.statSync(destination).size
    })}\n`
  )
}

module.exports = { parseBackend, main }

if (require.main === module) {
  main(process.argv.slice(2))
}
