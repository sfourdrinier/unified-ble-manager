'use strict'

const fs = require('node:fs')
const path = require('node:path')

// Current 5.x releases: stable SemVer or numbered release candidates. Historical
// tags keep their own immutable publisher source; this is not a 4.x publisher.
function classifyReleaseVersion(version) {
  const match =
    typeof version === 'string' ? /^5\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-rc\.(0|[1-9]\d*))?$/u.exec(version) : null
  if (
    !match ||
    match[0] !== version ||
    match.slice(1).some(value => value !== undefined && !Number.isSafeInteger(Number(value)))
  ) {
    throw new Error(
      `Unauthorized release version ${String(version)}: expected stable 5.x.y or 5.x.y-rc.N without build metadata or leading zeroes`
    )
  }
  const isStable = match[3] === undefined
  return { npmDistTag: isStable ? 'latest' : 'next', isStable }
}

function validateReleaseIdentity(manifest, tag) {
  if (manifest.name !== 'unified-ble-manager')
    throw new Error('Release requires the canonical package unified-ble-manager')
  const channel = classifyReleaseVersion(manifest.version)
  if (tag !== `v${manifest.version}`)
    throw new Error(`Tag ${String(tag)} does not match package version ${manifest.version}`)
  return channel
}

if (require.main === module) {
  const manifest = JSON.parse(fs.readFileSync(path.resolve(__dirname, '../../package.json'), 'utf8'))
  const channel = validateReleaseIdentity(manifest, process.env.GITHUB_REF_NAME)
  const envFile = process.env.GITHUB_ENV
  const outputFile = process.env.GITHUB_OUTPUT
  if (!envFile || !outputFile) throw new Error('GitHub publication environment/output paths are required')
  fs.appendFileSync(envFile, `NPM_DIST_TAG=${channel.npmDistTag}\n`)
  fs.appendFileSync(outputFile, `is_stable=${channel.isStable}\n`)
  console.log(`Publishing ${manifest.name}@${manifest.version} from tag ${process.env.GITHUB_REF_NAME}`)
}

module.exports = { classifyReleaseVersion, validateReleaseIdentity }
