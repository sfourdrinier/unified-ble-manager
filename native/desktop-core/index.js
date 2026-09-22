// native/desktop-core/index.js
//
// Loader for the shared desktop Rust core (the `ubm5_napi_echo` N-API
// cdylib, shipped as `ubm_desktop_core.node`). CommonJS so `__dirname`
// anchors every path to this file's own location: the addon is found from
// where the package is installed, never from the process cwd, never from
// another platform's or architecture's binary.
//
//   prebuilt mode (default): exactly prebuilds/<platform>-<arch>/ubm_desktop_core.node,
//     hash-checked against its sidecar ubm_desktop_core.identity.json.
//   source mode: UBM_NAPI_ADDON names an absolute path to a checkout build;
//     it is used exclusively (a relative path is refused).
//
// Bundlers must treat this directory as external (see docs/ELECTRON.md).

'use strict'

const crypto = require('crypto')
const fs = require('fs')
const path = require('path')
const { exactPrebuildPath, loadExactPrebuild, loadExplicitAddon } = require('../load-node-api-addon')

const ADDON_NAME = 'ubm_desktop_core'
const SOURCE_ADDON_ENV = 'UBM_NAPI_ADDON'
const SIDECAR_SCHEMA = 'ubm-desktop-core-prebuild/1'

function identitySidecarPath(addonPath) {
  return path.join(path.dirname(addonPath), `${ADDON_NAME}.identity.json`)
}

function sidecarError(code, message, addonPath) {
  const error = new Error(message)
  error.code = code
  error.path = addonPath
  return error
}

/**
 * The staged sidecar for a prebuilt addon: `{ schema, sha256, identity }`,
 * where `sha256` is the addon file's digest and `identity` the
 * `nativeBuildIdentity()` record the builder read from the binary. A
 * prebuild without it, or whose bytes no longer match it, is refused.
 */
function readPrebuildSidecar(addonPath) {
  const sidecarPath = identitySidecarPath(addonPath)
  if (!fs.existsSync(sidecarPath)) {
    throw sidecarError('prebuild-sidecar-missing', `prebuild identity sidecar missing: ${sidecarPath}`, addonPath)
  }
  let sidecar
  try {
    sidecar = JSON.parse(fs.readFileSync(sidecarPath, 'utf8'))
  } catch (cause) {
    throw sidecarError('prebuild-sidecar-malformed', `unreadable prebuild sidecar ${sidecarPath}: ${cause.message}`, addonPath)
  }
  if (sidecar === null || typeof sidecar !== 'object' || sidecar.schema !== SIDECAR_SCHEMA || typeof sidecar.sha256 !== 'string') {
    throw sidecarError('prebuild-sidecar-malformed', `prebuild sidecar ${sidecarPath} is not ${SIDECAR_SCHEMA}`, addonPath)
  }
  const actual = crypto.createHash('sha256').update(fs.readFileSync(addonPath)).digest('hex')
  if (actual !== sidecar.sha256) {
    throw sidecarError(
      'prebuild-digest-mismatch',
      `prebuild ${addonPath} sha256 ${actual} does not match its sidecar ${sidecar.sha256}`,
      addonPath
    )
  }
  return Object.freeze({ path: sidecarPath, sha256: actual, identity: sidecar.identity ?? null })
}

/**
 * Load the desktop core for this process. Returns
 * `{ module, path, mode: 'prebuilt' | 'source', sidecar }` or throws a
 * typed error (`no-prebuilt-for-target`, `load-failed`, `argument-invalid`,
 * `prebuild-sidecar-missing`, `prebuild-sidecar-malformed`,
 * `prebuild-digest-mismatch`). Never falls back to another binary.
 */
function loadDesktopCore(environment = process.env) {
  const explicit = environment[SOURCE_ADDON_ENV]
  if (typeof explicit === 'string' && explicit.length > 0) {
    const loaded = loadExplicitAddon(explicit, SOURCE_ADDON_ENV)
    return Object.freeze({ ...loaded, mode: 'source', sidecar: null })
  }
  const addonPath = exactPrebuildPath({ moduleDirectory: __dirname, addonName: ADDON_NAME })
  // Hash-check before the dynamic loader maps the file: a substituted or
  // truncated prebuild is refused without executing its initializers.
  const sidecar = fs.existsSync(addonPath) ? readPrebuildSidecar(addonPath) : null
  const loaded = loadExactPrebuild({ moduleDirectory: __dirname, addonName: ADDON_NAME })
  return Object.freeze({ ...loaded, mode: 'prebuilt', sidecar })
}

module.exports = Object.freeze({
  ADDON_NAME,
  SIDECAR_SCHEMA,
  SOURCE_ADDON_ENV,
  identitySidecarPath,
  loadDesktopCore,
  readPrebuildSidecar
})
