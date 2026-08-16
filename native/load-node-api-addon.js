'use strict'

const fs = require('fs')
const path = require('path')

const SOURCE_BUILD_ENV = 'UNIFIED_BLE_MANAGER_NATIVE_SOURCE'

function nodeApiAddonCandidates({
  moduleDirectory,
  addonName,
  platform = process.platform,
  arch = process.arch,
  preferSourceBuild = process.env[SOURCE_BUILD_ENV] === '1'
}) {
  if (typeof moduleDirectory !== 'string' || moduleDirectory.length === 0) {
    throw new TypeError('moduleDirectory must be a non-empty string')
  }
  if (typeof addonName !== 'string' || !/^[a-z0-9_]+$/u.test(addonName)) {
    throw new TypeError('addonName must contain only lowercase letters, digits, and underscores')
  }
  if (typeof platform !== 'string' || typeof arch !== 'string' || platform.length === 0 || arch.length === 0) {
    throw new TypeError('platform and arch must be non-empty strings')
  }
  if (typeof preferSourceBuild !== 'boolean') {
    throw new TypeError('preferSourceBuild must be a boolean')
  }

  const filename = `${addonName}.node`
  const prebuildCandidate = path.join(moduleDirectory, 'prebuilds', `${platform}-${arch}`, filename)
  const sourceBuildCandidates = [
    path.join(moduleDirectory, 'build', 'Release', filename),
    path.join(moduleDirectory, 'build', 'Debug', filename)
  ]
  return Object.freeze(
    preferSourceBuild ? [...sourceBuildCandidates, prebuildCandidate] : [prebuildCandidate, ...sourceBuildCandidates]
  )
}

function loadNodeApiAddon(options) {
  const candidates = nodeApiAddonCandidates(options)
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      // If a selected binary exists but cannot be loaded, propagate that failure.
      // Silently trying another ABI would hide corrupt or mispackaged releases.
      // eslint-disable-next-line import/no-dynamic-require, global-require
      return require(candidate)
    }
  }
  return null
}

module.exports = Object.freeze({
  SOURCE_BUILD_ENV,
  loadNodeApiAddon,
  nodeApiAddonCandidates
})
