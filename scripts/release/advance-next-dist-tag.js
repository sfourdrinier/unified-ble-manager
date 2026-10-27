#!/usr/bin/env node
// scripts/release/advance-next-dist-tag.js

'use strict'

// npm `next` is the release-candidate channel, so `npm install pkg@next` must
// never resolve older than `latest`. After a stable release, `next` converges
// on the highest published version: the new stable, or a newer candidate.
// `npm dist-tag add` is an unconditional write and other release tags publish
// concurrently, so the target is recomputed from a fresh registry read after
// every write until `next` matches it; a candidate published mid-step is
// found by that read, and one published later sets `next` itself.

const fs = require('node:fs')
const path = require('node:path')
const { spawnSync } = require('node:child_process')
const semver = require('semver')
const { classifyReleaseVersion } = require('./release-version-policy')

const PACKAGE = 'unified-ble-manager'

function nextDistTagDecision(version, { distTags, versions }) {
  if (!classifyReleaseVersion(version).isStable)
    return { advance: false, target: undefined, reason: `release candidate ${version} publishes to next itself` }
  const target = semver.rsort([...new Set([...versions, version])])[0]
  const next = distTags.next
  if (next === target) return { advance: false, target, reason: `next already resolves to ${target}` }
  if (next === undefined) return { advance: true, target, reason: `npm has no next tag; highest is ${target}` }
  if (semver.valid(next) === null)
    throw new Error(`npm next holds ${next}, which is not a version this release can order`)
  return semver.lt(next, target)
    ? { advance: true, target, reason: `next ${next} is older than the highest published ${target}` }
    : { advance: false, target, reason: `next ${next} is not older than the highest published ${target}` }
}

function advanceNextDistTag({ version, viewRegistry, addDistTag, sleep, log, attempts = 24, delayMs = 5_000 }) {
  let written
  let state
  for (let attempt = 1; attempt <= attempts; attempt++) {
    state = viewRegistry()
    const decision = nextDistTagDecision(version, state)
    if (!decision.advance) {
      log(`npm next resolves to ${state.distTags.next}: ${decision.reason}`)
      return { advanced: written !== undefined, next: state.distTags.next, reason: decision.reason }
    }
    if (decision.target !== written) {
      try {
        addDistTag(decision.target)
      } catch (error) {
        throw new Error(
          `${PACKAGE}@${version} is published; npm next was not advanced (${error.message}). ` +
            'Enable "Allow npm dist-tag" on the package\'s trusted publisher for publish.yml, then rerun this failed job.'
        )
      }
      written = decision.target
      log(`Set npm next to ${written}: ${decision.reason}`)
    }
    if (attempt < attempts) sleep(delayMs)
  }
  throw new Error(`npm next still resolves to ${state.distTags.next} after adding ${written}`)
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
    viewRegistry: () => {
      const view = JSON.parse(npm(['view', PACKAGE, 'dist-tags', 'versions', '--json']))
      return { distTags: view['dist-tags'], versions: view.versions }
    },
    addDistTag: target => npm(['dist-tag', 'add', `${PACKAGE}@${target}`, 'next']),
    sleep: ms => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms),
    log: line => console.log(line)
  })
}

module.exports = { advanceNextDistTag, nextDistTagDecision }
