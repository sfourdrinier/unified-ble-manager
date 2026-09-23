'use strict'

const fs = require('node:fs')
const path = require('node:path')

const root = path.resolve(__dirname, '../..')
const intermediatesRoot = path.join(root, 'example/android/manifest-consumer/build/intermediates')
const presencePermission = 'android.permission.REQUEST_OBSERVE_COMPANION_DEVICE_PRESENCE'

function mergedManifest(variant) {
  const variantRoot = ['merged_manifest', 'merged_manifests']
    .map(directory => path.join(intermediatesRoot, directory, variant))
    .find(directory => fs.existsSync(directory))
  const candidates = []
  function visit(directory) {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const absolute = path.join(directory, entry.name)
      if (entry.isDirectory()) visit(absolute)
      else if (entry.name === 'AndroidManifest.xml') candidates.push(absolute)
    }
  }
  if (variantRoot === undefined) throw new Error(`missing merged-manifest output for ${variant}`)
  visit(variantRoot)
  if (candidates.length !== 1) {
    throw new Error(`expected one merged manifest for ${variant}, found ${candidates.length}: ${candidates.join(', ')}`)
  }
  return fs.readFileSync(candidates[0], 'utf8')
}

function scanPermission(manifest) {
  const match = manifest.match(
    /<uses-permission\b[^>]*android:name="android\.permission\.BLUETOOTH_SCAN"[^>]*\/?\s*>/
  )
  if (match === null) throw new Error('merged manifest has no BLUETOOTH_SCAN permission')
  return match[0]
}

function assert(condition, message) {
  if (!condition) throw new Error(message)
}

for (const variant of ['neverTrueDebug', 'neverFalseDebug', 'neverTrueAgainDebug', 'legacyApi30Debug']) {
  const manifest = mergedManifest(variant)
  assert(manifest.includes(presencePermission), `${variant} did not inherit companion-presence permission`)
}

assert(
  scanPermission(mergedManifest('neverTrueDebug')).includes('android:usesPermissionFlags="neverForLocation"'),
  'neverForLocation=true did not survive the real manifest merge'
)
assert(
  !scanPermission(mergedManifest('neverFalseDebug')).includes('android:usesPermissionFlags='),
  'neverForLocation=false retained the library attribute after the real manifest merge'
)
assert(
  scanPermission(mergedManifest('neverTrueAgainDebug')).includes('android:usesPermissionFlags="neverForLocation"'),
  'neverForLocation=true was not restored after the false configuration'
)

const legacy = mergedManifest('legacyApi30Debug')
for (const permission of ['ACCESS_COARSE_LOCATION', 'ACCESS_FINE_LOCATION']) {
  assert(legacy.includes(`android.permission.${permission}`), `API 30 consumer is missing ${permission}`)
}

process.stdout.write('Android merged-manifest consumer proof passed (presence, true -> false -> true, API 30 location).\n')
