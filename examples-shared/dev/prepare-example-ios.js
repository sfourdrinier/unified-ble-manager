// Prepare the exact pnpm file:.. copy that Expo autolinks into the iOS app.
'use strict'

const fs = require('node:fs')
const path = require('node:path')
const { spawnSync } = require('node:child_process')
const { main: ensureNative } = require('../../scripts/native/ensure-native')
const { checkAppleStaging, checkGenerated } = require('../../scripts/release/native-build-identity')
const { inspectExampleLibrary, readRepoFacts, readCopyFacts, describeLibraryOutcome } = require('./verify-example-library')

const ROOT = path.resolve(__dirname, '..', '..')

function runPnpm(args, env = process.env) {
  const result = spawnSync('pnpm', args, { cwd: ROOT, env, stdio: 'inherit' })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) throw new Error(`pnpm ${args.join(' ')} failed (exit ${String(result.status)})`)
}

function prepareExampleIos({ verifyRootIdentity, ensureApple, inspectCopy, verifyCopyApple, refreshCopy }) {
  verifyRootIdentity()
  ensureApple()
  const initial = inspectCopy()
  let refresh = !initial.ok
  if (!refresh) {
    try {
      verifyCopyApple()
    } catch (error) {
      console.log(`prepare-example-ios: copied RustCore needs refresh: ${error.message}`)
      refresh = true
    }
  }
  if (refresh) {
    refreshCopy()
    const final = inspectCopy()
    if (!final.ok) throw new Error(`copied package remains ${final.state}: ${describeLibraryOutcome(final)}`)
    verifyCopyApple()
  }
}

function main(exampleDir = path.join(ROOT, 'example-expo')) {
  const copyRoot = path.join(exampleDir, 'node_modules', 'unified-ble-manager')
  prepareExampleIos({
    verifyRootIdentity: () => checkGenerated(ROOT),
    ensureApple: () => ensureNative(['apple']),
    inspectCopy: () => inspectExampleLibrary({ repo: readRepoFacts(ROOT), copy: readCopyFacts(exampleDir) }),
    verifyCopyApple: () => checkAppleStaging(copyRoot),
    refreshCopy: () => {
      if (process.env.UBM_NATIVE_REFRESH === 'off') {
        throw new Error('UBM_NATIVE_REFRESH=off: Expo package copy is stale; refusing to build')
      }
      console.log('prepare-example-ios: refreshing the Expo file:.. package copy')
      runPnpm(['run', 'prepack'])
      // pnpm --force recopies file:.. dependencies; a normal install may keep
      // its previous store snapshot even when the root RustCore has changed.
      runPnpm(['--dir', exampleDir, 'install', '--force', '--frozen-lockfile'], {
        ...process.env,
        NODE_OPTIONS: `${process.env.NODE_OPTIONS ?? ''} --max-old-space-size=8192`.trim()
      })
      if (!fs.existsSync(path.join(copyRoot, 'package.json'))) {
        throw new Error(`pnpm did not install ${copyRoot}`)
      }
    }
  })
  console.log('prepare-example-ios: installed JavaScript and RustCore match the current sources')
}

if (require.main === module) {
  try {
    main(path.resolve(process.argv[2] ?? path.join(ROOT, 'example-expo')))
  } catch (error) {
    console.error(`prepare-example-ios: ${error.message}`)
    process.exitCode = 1
  }
}

module.exports = { prepareExampleIos, main }
