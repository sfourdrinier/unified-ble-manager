'use strict'

const fs = require('fs')
const path = require('path')

function nodeApiAddonCandidates({ moduleDirectory, addonName, platform = process.platform, arch = process.arch }) {
  if (typeof moduleDirectory !== 'string' || moduleDirectory.length === 0) {
    throw new TypeError('moduleDirectory must be a non-empty string')
  }
  if (typeof addonName !== 'string' || !/^[a-z0-9_]+$/u.test(addonName)) {
    throw new TypeError('addonName must contain only lowercase letters, digits, and underscores')
  }
  if (typeof platform !== 'string' || typeof arch !== 'string' || platform.length === 0 || arch.length === 0) {
    throw new TypeError('platform and arch must be non-empty strings')
  }
  const filename = `${addonName}.node`
  return Object.freeze([
    path.join(moduleDirectory, 'prebuilds', `${platform}-${arch}`, filename),
    path.join(moduleDirectory, 'build', 'Release', filename),
    path.join(moduleDirectory, 'build', 'Debug', filename)
  ])
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

module.exports = Object.freeze({ loadNodeApiAddon, nodeApiAddonCandidates })
