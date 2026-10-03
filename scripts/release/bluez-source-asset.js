// Shared source-only manifest authority; also runs in Rust CI without pnpm.
const crypto = require('node:crypto')
const fs = require('node:fs')
const path = require('node:path')
const { deploymentRelease } = require('../../vendor/bluez/deployment/identity.cjs')

function readBluezSourceAsset(directory) {
  const manifest = JSON.parse(fs.readFileSync(path.join(directory, 'source-asset-manifest.json'), 'utf8'))
  if (
    manifest.schemaVersion !== 1 ||
    manifest.name !== 'bluez' ||
    manifest.version !== '5.87' ||
    manifest.licenseExpression !== 'GPL-2.0-or-later AND LGPL-2.1-or-later'
  )
    throw new Error('Unreviewed BlueZ source-asset identity or license')
  const distribution = manifest.distribution
  if (
    !distribution ||
    distribution.kind !== 'source-only-daemon-extension' ||
    distribution.packageIncludesDaemonBinary !== false ||
    distribution.linkedIntoPackageNativeLibraries !== false ||
    distribution.automaticInstallOrLaunch !== false ||
    distribution.deployment !== 'external-explicit-host-action'
  )
    throw new Error('BlueZ source-asset distribution requires a new review')
  if (distribution.linuxAuthorityContract !== undefined) deploymentRelease(distribution)
  const digest = /^[a-f0-9]{64}$/
  if (
    !manifest.upstream ||
    manifest.upstream.url !== 'https://www.kernel.org/pub/linux/bluetooth/bluez-5.87.tar.xz' ||
    manifest.upstream.archive !== 'bluez-5.87.tar.xz' ||
    !digest.test(manifest.upstream.sha256)
  )
    throw new Error('Invalid BlueZ source provenance')
  const verifyFile = evidence => {
    if (
      !evidence ||
      typeof evidence.file !== 'string' ||
      path.basename(evidence.file) !== evidence.file ||
      evidence.file === '.' ||
      evidence.file === '..' ||
      !digest.test(evidence.sha256)
    )
      throw new Error('Invalid BlueZ source evidence path or hash')
    const file = path.join(directory, evidence.file)
    if (
      !fs.lstatSync(file).isFile() ||
      crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex') !== evidence.sha256
    ) {
      throw new Error(`BlueZ source evidence changed: ${evidence.file}`)
    }
  }
  verifyFile(manifest.patch)
  if (
    manifest.patch.file !== 'ubm-le-gatt-5.87.patch' ||
    !Array.isArray(manifest.licenseFiles) ||
    manifest.licenseFiles.length !== 2
  ) {
    throw new Error('Invalid BlueZ patch/license evidence set')
  }
  for (const [index, [file, license]] of [
    ['COPYING', 'GPL-2.0-or-later'],
    ['COPYING.LIB', 'LGPL-2.1-or-later']
  ].entries()) {
    const evidence = manifest.licenseFiles[index]
    if (evidence.file !== file || evidence.license !== license) throw new Error('Unreviewed BlueZ license evidence')
    verifyFile(evidence)
  }
  return manifest
}

module.exports = { readBluezSourceAsset }
