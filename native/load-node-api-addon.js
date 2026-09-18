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

/** A typed loader failure: `code` names what happened, `path` where. */
function addonLoadError(code, message, details) {
  const error = new Error(message)
  error.code = code
  Object.assign(error, details)
  return error
}

/**
 * The C library the running Node links against: `glibc-<version>` from the
 * process report, or `musl`/`unknown`. Reported with a missing prebuild so a
 * musl host sees why no binary matches, never a generic failure.
 */
function detectLibc(platform = process.platform) {
  if (platform !== 'linux') return null
  const report = typeof process.report?.getReport === 'function' ? process.report.getReport() : null
  const glibc = report?.header?.glibcVersionRuntime
  if (typeof glibc === 'string' && glibc.length > 0) return `glibc-${glibc}`
  const shared = Array.isArray(report?.sharedObjects) ? report.sharedObjects : []
  return shared.some(object => typeof object === 'string' && object.includes('musl')) ? 'musl' : 'unknown'
}

/** The one prebuild path for this exact platform and architecture. */
function exactPrebuildPath({ moduleDirectory, addonName, platform = process.platform, arch = process.arch }) {
  if (typeof moduleDirectory !== 'string' || !path.isAbsolute(moduleDirectory)) {
    throw new TypeError('moduleDirectory must be an absolute path (anchor it to __dirname)')
  }
  if (typeof addonName !== 'string' || !/^[a-z0-9_]+$/u.test(addonName)) {
    throw new TypeError('addonName must contain only lowercase letters, digits, and underscores')
  }
  return path.join(moduleDirectory, 'prebuilds', `${platform}-${arch}`, `${addonName}.node`)
}

/**
 * Load exactly `prebuilds/<platform>-<arch>/<addonName>.node` under
 * `moduleDirectory`: no build/ fallback, no other architecture, no cwd.
 * Returns `{ module, path }` or throws a typed error:
 *   `no-prebuilt-for-target` - no file for this target (with `libc` on Linux);
 *   `load-failed`            - the file exists but the runtime refused it
 *                              (the dlopen text rides in `cause`, unchanged).
 */
function loadExactPrebuild({ moduleDirectory, addonName, platform = process.platform, arch = process.arch }) {
  const candidate = exactPrebuildPath({ moduleDirectory, addonName, platform, arch })
  if (!fs.existsSync(candidate)) {
    const libc = detectLibc(platform)
    throw addonLoadError(
      'no-prebuilt-for-target',
      `no ${addonName} prebuild for ${platform}-${arch}${libc === null ? '' : ` (${libc})`}: ${candidate}`,
      { path: candidate, platform, arch, libc }
    )
  }
  return Object.freeze({ module: requireAddonFile(candidate), path: candidate })
}

/**
 * Load an explicitly named addon (source mode). The path must be absolute:
 * a relative path would resolve against whatever the process cwd happens to
 * be, so it is refused (`argument-invalid`), never guessed.
 */
function loadExplicitAddon(file, variable) {
  if (typeof file !== 'string' || file.length === 0 || !path.isAbsolute(file)) {
    throw addonLoadError('argument-invalid', `${variable} must be an absolute path to a built addon, got ${JSON.stringify(file)}`, {
      path: file
    })
  }
  if (!fs.existsSync(file)) {
    throw addonLoadError('no-prebuilt-for-target', `${variable} names a missing addon: ${file}`, { path: file })
  }
  return Object.freeze({ module: requireAddonFile(file), path: file })
}

function requireAddonFile(file) {
  try {
    return require(file)
  } catch (cause) {
    throw addonLoadError('load-failed', `the runtime refused to load ${file}: ${cause?.message ?? String(cause)}`, {
      path: file,
      cause
    })
  }
}

module.exports = Object.freeze({
  SOURCE_BUILD_ENV,
  detectLibc,
  exactPrebuildPath,
  loadExactPrebuild,
  loadExplicitAddon,
  loadNodeApiAddon,
  nodeApiAddonCandidates
})
