#!/usr/bin/env node
// scripts/ci/install-android-ndk.js

'use strict'

// Installs the NDK android/build.gradle pins. sdkmanager neither retries nor
// verifies a download: a truncated archive fails as "Error reading Zip
// content", and one such runner fetch failed a whole release gate. Each failed
// attempt discards its partial state before the next; success means the
// installed source.properties reports the pinned revision.

const fs = require('node:fs')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

const root = path.resolve(__dirname, '../..')

function pinnedNdkVersion(gradle = fs.readFileSync(path.join(root, 'android', 'build.gradle'), 'utf8')) {
  const pin = /ndkVersion\s*=.*"(\d+\.\d+\.\d+)"/.exec(gradle)
  if (pin === null) throw new Error('android/build.gradle declares no default ndkVersion')
  return pin[1]
}

function installedNdkRevision(ndkDir) {
  let properties
  try {
    properties = fs.readFileSync(path.join(ndkDir, 'source.properties'), 'utf8')
  } catch (error) {
    if (error.code === 'ENOENT') return null
    throw error
  }
  return /^Pkg\.Revision\s*=\s*(\S+)/m.exec(properties)?.[1] ?? null
}

function installNdk({ sdkRoot, version, install, attempts = 4, delayMs = 20_000, sleep, log }) {
  const ndkDir = path.join(sdkRoot, 'ndk', version)
  if (installedNdkRevision(ndkDir) === version) {
    log(`NDK ${version} is already installed at ${ndkDir}`)
    return 0
  }
  const partialState = [ndkDir, path.join(sdkRoot, '.temp'), path.join(sdkRoot, '.downloadIntermediates')]
  for (let attempt = 1; ; attempt++) {
    const status = install(`ndk;${version}`)
    const revision = installedNdkRevision(ndkDir)
    if (status === 0 && revision === version) {
      log(`Installed NDK ${version} at ${ndkDir} on attempt ${attempt} of ${attempts}`)
      return attempt
    }
    const reason = status === 0 ? `installed revision is ${revision ?? 'missing'}` : `sdkmanager exited ${status}`
    if (attempt >= attempts)
      throw new Error(`NDK ${version} did not install after ${attempts} attempts (last: ${reason})`)
    log(
      `::warning::NDK ${version} install attempt ${attempt} of ${attempts} failed (${reason}); discarding partial download state and retrying`
    )
    for (const dir of partialState) fs.rmSync(dir, { recursive: true, force: true })
    sleep(delayMs * attempt)
  }
}

function sdkmanagerInstall(pkg) {
  const result = spawnSync('sdkmanager', ['--install', pkg], { stdio: 'inherit' })
  if (result.error) throw result.error
  return result.status ?? `on signal ${result.signal}`
}

if (require.main === module) {
  const sdkRoot = process.env.ANDROID_SDK_ROOT || process.env.ANDROID_HOME
  if (!sdkRoot) throw new Error('ANDROID_SDK_ROOT or ANDROID_HOME must name the Android SDK')
  installNdk({
    sdkRoot,
    version: pinnedNdkVersion(),
    install: sdkmanagerInstall,
    sleep: ms => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms),
    log: line => console.log(line)
  })
}

module.exports = { installNdk, installedNdkRevision, pinnedNdkVersion }
