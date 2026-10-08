'use strict'

// Load the sealed desktop Node-API addon under Bun and run the hardware-free
// synthetic central. This is the Bun host gate. It does not qualify a
// physical radio; pass --list-adapters for that separate check.
//
//   bun scripts/ci/bun-desktop-host-smoke.js
//   bun scripts/ci/bun-desktop-host-smoke.js --list-adapters

const fs = require('node:fs')
const path = require('node:path')

const ROOT = path.join(__dirname, '..', '..')
const MINIMUM_BUN = [1, 4, 2]
const IDENTITY_SOURCE = path.join(ROOT, 'src', 'generated', 'native-build-identity.ts')

function fail(message) {
  console.error(`bun-desktop-host: FAIL ${message}`)
  process.exit(1)
}

function versionBelow(actual, minimum) {
  const parts = String(actual)
    .split('.')
    .map(part => Number.parseInt(part, 10))
  for (let index = 0; index < minimum.length; index += 1) {
    const left = parts[index] ?? 0
    const right = minimum[index]
    if (left < right) return true
    if (left > right) return false
  }
  return false
}

function assertBun() {
  const version = process.versions.bun
  if (typeof version !== 'string' || version.length === 0) {
    fail('this smoke runs under Bun, not Node')
  }
  if (versionBelow(version, MINIMUM_BUN)) {
    fail(`Bun ${version} is older than ${MINIMUM_BUN.join('.')}`)
  }
  if (typeof process.versions.napi !== 'string') {
    fail('Bun did not report a Node-API version')
  }
  return version
}

async function openSynthetic(UbmCentral) {
  const central = await UbmCentral.openSynthetic('bun-desktop-host', { platform: 'bluez' })
  const states = await central.runtimeCapabilityStates()
  if (!Array.isArray(states) || states.length === 0) {
    fail('synthetic central returned no runtime capability states')
  }
  let wakes = 0
  central.setEventWaker(() => {
    wakes += 1
  })
  if (central.eventWakeFailures() !== 0) {
    fail(`event waker installed with ${central.eventWakeFailures()} failures`)
  }
  const closed = await central.close()
  if (!closed || closed.state !== 'released') {
    fail(`synthetic central close did not release: ${JSON.stringify(closed)}`)
  }
  return { states: states.length, wakes }
}

async function listAdapters(UbmCentral) {
  const listing = await UbmCentral.listAdapters()
  if (!Array.isArray(listing) || listing.length === 0) {
    fail(`adapter listing was empty: ${JSON.stringify(listing)}`)
  }
  return listing.map(entry => ({
    index: entry.index,
    label: entry.label ?? null,
    error: entry.error ?? null
  }))
}

async function main() {
  const bun = assertBun()
  const listOnly = process.argv.includes('--list-adapters')
  const { loadDesktopCore } = require(path.join(ROOT, 'native', 'desktop-core', 'index.js'))
  let loaded
  try {
    loaded = loadDesktopCore()
  } catch (error) {
    fail(`${error.code ?? 'load-failed'}: ${error.message}`)
  }
  if (
    typeof loaded.module.nativeBuildIdentity !== 'function' ||
    typeof loaded.module.UbmCentral?.openSynthetic !== 'function'
  ) {
    fail('addon is missing nativeBuildIdentity or UbmCentral.openSynthetic')
  }
  const identity = JSON.parse(loaded.module.nativeBuildIdentity())
  const expected = fs.readFileSync(IDENTITY_SOURCE, 'utf8')
  if (identity.binding !== 'napi') {
    fail(`addon is not an napi build: ${JSON.stringify(identity)}`)
  }
  // A packaged prebuild is a sealed release. A debug binary is accepted only
  // from an explicit UBM_NAPI_ADDON source build, matching the desktop loader.
  const allowedProfile = loaded.mode === 'source' ? ['release', 'debug'] : ['release']
  if (!allowedProfile.includes(identity.profile)) {
    fail(`addon profile ${identity.profile} is not valid for ${loaded.mode} mode`)
  }
  if (!expected.includes(identity.sourceDigest) || !expected.includes(identity.bindingSchema)) {
    fail('addon identity does not match src/generated/native-build-identity.ts')
  }
  const UbmCentral = loaded.module.UbmCentral
  const synthetic = listOnly ? null : await openSynthetic(UbmCentral)
  const adapters = listOnly ? await listAdapters(UbmCentral) : undefined
  process.stdout.write(
    `${JSON.stringify({
      ok: true,
      bun,
      napi: process.versions.napi,
      node: process.versions.node,
      modules: process.versions.modules,
      target: identity.target,
      path: loaded.path,
      synthetic,
      adapters
    })}\n`
  )
}

main().catch(error => {
  fail(error && error.message ? error.message : String(error))
})
