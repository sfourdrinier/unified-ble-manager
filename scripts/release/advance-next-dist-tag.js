#!/usr/bin/env node
// scripts/release/advance-next-dist-tag.js

'use strict'

// npm `next` is the release-candidate channel, so `npm install pkg@next` must
// never resolve older than `latest`. A stable release advances `next` when it
// holds an older version (or none) and leaves a newer candidate in place. The
// outcome is read back from the registry rather than assumed from the write.

const fs = require('node:fs')
const path = require('node:path')
const { spawnSync } = require('node:child_process')
const semver = require('semver')
const { classifyReleaseVersion } = require('./release-version-policy')

const PACKAGE = 'unified-ble-manager'

function nextDistTagDecision(version, distTags) {
  if (!classifyReleaseVersion(version).isStable)
    return { advance: false, reason: `release candidate ${version} publishes to next itself` }
  const next = distTags.next
  if (next === undefined) return { advance: true, reason: `npm has no next tag` }
  if (semver.valid(next) === null)
    throw new Error(`npm next holds ${next}, which is not a version this release can order`)
  return semver.lt(next, version)
    ? { advance: true, reason: `next ${next} is older than stable ${version}` }
    : { advance: false, reason: `next ${next} is not older than stable ${version}` }
}

function advanceNextDistTag({ version, viewDistTags, addDistTag, sleep, log, attempts = 24, delayMs = 5_000 }) {
  const before = viewDistTags()
  const decision = nextDistTagDecision(version, before)
  if (!decision.advance) {
    log(`Leaving npm next unchanged: ${decision.reason}`)
    return { advanced: false, next: before.next, reason: decision.reason }
  }
  try {
    addDistTag(version)
  } catch (error) {
    throw new Error(
      `${PACKAGE}@${version} is published; npm next was not advanced (${error.message}). ` +
        'Enable "Allow npm dist-tag" on the package\'s trusted publisher for publish.yml, then rerun this failed job.'
    )
  }
  let next
  for (let attempt = 1; attempt <= attempts; attempt++) {
    next = viewDistTags().next
    if (next === version) {
      log(`npm next now resolves to ${version}: ${decision.reason}`)
      return { advanced: true, next, reason: decision.reason }
    }
    if (attempt < attempts) sleep(delayMs)
  }
  throw new Error(`npm next still resolves to ${next} after adding ${version}`)
}

function npm(args) {
  const result = spawnSync('npm', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] })
  if (result.error) throw result.error
  if (result.status !== 0) throw new Error(`npm ${args.join(' ')} exited ${result.status ?? result.signal}`)
  return result.stdout
}

if (require.main === module) {
  const { version } = JSON.parse(fs.readFileSync(path.resolve(__dirname, '../../package.json'), 'utf8'))
  advanceNextDistTag({
    version,
    viewDistTags: () => JSON.parse(npm(['view', PACKAGE, 'dist-tags', '--json'])),
    addDistTag: target => npm(['dist-tag', 'add', `${PACKAGE}@${target}`, 'next']),
    sleep: ms => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms),
    log: line => console.log(line)
  })
}

module.exports = { advanceNextDistTag, nextDistTagDecision }
