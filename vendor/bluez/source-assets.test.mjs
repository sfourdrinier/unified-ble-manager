import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

const directory = new URL('./', import.meta.url)
const digest = bytes => createHash('sha256').update(bytes).digest('hex')

test('source patch owner refuses foreign archive before changing either asset', () => {
  const manifestPath = new URL('source-asset-manifest.json', directory)
  const manifestBefore = readFileSync(manifestPath)
  const manifest = JSON.parse(manifestBefore)
  const patchPath = new URL(manifest.patch.file, directory)
  const patchBefore = readFileSync(patchPath)
  const result = spawnSync(
    process.execPath,
    [
      fileURLToPath(new URL('regenerate-source-patch.js', directory)),
      fileURLToPath(new URL('COPYING', directory)),
      fileURLToPath(directory)
    ],
    { encoding: 'utf8' }
  )
  assert.notEqual(result.status, 0)
  assert.match(result.stderr, /archive hash mismatch; no source assets changed/)
  assert.deepEqual(readFileSync(manifestPath), manifestBefore)
  assert.deepEqual(readFileSync(patchPath), patchBefore)
})

test('BlueZ derivative source assets retain exact upstream provenance and licenses', () => {
  const manifest = JSON.parse(readFileSync(new URL('source-asset-manifest.json', directory), 'utf8'))
  assert.equal(manifest.schemaVersion, 1)
  assert.equal(manifest.name, 'bluez')
  assert.equal(manifest.version, '5.87')
  assert.equal(manifest.upstream.url, 'https://www.kernel.org/pub/linux/bluetooth/bluez-5.87.tar.xz')
  assert.equal(manifest.upstream.sha256, '26bdcf2cebd7310c6f598850606b037ef0c515fe6608ebc54d22c50c4c32b35f')
  assert.equal(manifest.licenseExpression, 'GPL-2.0-or-later AND LGPL-2.1-or-later')
  assert.deepEqual(
    manifest.licenseFiles.map(file => [file.file, file.license, file.sha256]),
    [
      ['COPYING', 'GPL-2.0-or-later', 'b499eddebda05a8859e32b820a64577d91f1de2b52efa2a1575a2cb4000bc259'],
      ['COPYING.LIB', 'LGPL-2.1-or-later', 'ec60b993835e2c6b79e6d9226345f4e614e686eb57dc13b6420c15a33a8996e5']
    ]
  )
  for (const asset of [manifest.patch, ...manifest.licenseFiles]) {
    assert.match(asset.file, /^[A-Za-z0-9_.-]+$/)
    assert.match(asset.sha256, /^[a-f0-9]{64}$/)
    assert.equal(digest(readFileSync(new URL(asset.file, directory))), asset.sha256)
  }
  assert.deepEqual(manifest.distribution, {
    kind: 'source-only-daemon-extension',
    packageIncludesDaemonBinary: false,
    linkedIntoPackageNativeLibraries: false,
    deployment: 'external-explicit-host-action',
    automaticInstallOrLaunch: false,
    linuxAuthorityContract: [1, 2, 1],
    release: '5.87-ubm.3'
  })
  const patch = readFileSync(new URL(manifest.patch.file, directory), 'utf8')
  for (const path of ['src/ubm-le-lease.c', 'src/ubm-le-lease.h', 'unit/test-ubm-le-lease.c', 'unit/test-ubm-scan-filter.c']) {
    assert.ok(patch.includes(`+++ b/${path}`), `missing production authority source: ${path}`)
  }
  assert.match(patch, /protocol = 1, lease = 2, gatt = 1/)
  assert.match(patch, /GDBUS_METHOD\("RecoverLease"/)
  assert.match(patch, /GDBUS_METHOD\("AckLease"/)
  assert.match(patch, /"reason", "y"/)
})
