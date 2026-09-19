'use strict'

// scripts/native/native-refresh.js — F9: rebuild only what is stale, with
// the existing canonical builders (their logic is never duplicated here),
// then re-run the status check and fail while anything is still stale.
//
// Usage:
//   node scripts/native/native-refresh.js [--root <dir>] [--only android,apple,desktop]
//
// Builders (overridable per command for CI hermeticity, never in production):
//   UBM_NATIVE_BUILDER_ANDROID / _APPLE / _DESKTOP (a shell command each).
// A rebuild prints one line naming what was rebuilt and why (old/new
// digest); fresh groups are a silent no-op. A builder failure aborts the
// run with the builder's error — nothing continues on a stale artifact.

const { execFileSync } = require('node:child_process')
const path = require('node:path')

const status = require('./native-status')

const ROOT = path.resolve(__dirname, '..', '..')

const BUILDER_ENV = Object.freeze({
  android: 'UBM_NATIVE_BUILDER_ANDROID',
  apple: 'UBM_NATIVE_BUILDER_APPLE',
  desktop: 'UBM_NATIVE_BUILDER_DESKTOP'
})

function firstLine(message) {
  return String(message).split('\n')[0]
}

function defaultBuilders({ dir }) {
  return {
    android: process.env.UBM_NATIVE_BUILDER_ANDROID ?? status.defaultRefreshCommand('android'),
    apple: process.env.UBM_NATIVE_BUILDER_APPLE ?? status.defaultRefreshCommand('apple'),
    desktop: process.env.UBM_NATIVE_BUILDER_DESKTOP ?? status.defaultRefreshCommand('desktop', { dir })
  }
}

function applicableResult(group, results) {
  if (group === 'desktop') return results.find(entry => entry.name === 'desktop')
  return results.find(entry => entry.group === group)
}

function digestLabel(digests) {
  return digests === null ? 'missing' : digests.sourceDigest
}

function runRefresh({
  groups = status.GROUPS,
  root = ROOT,
  identity,
  platform = process.platform,
  arch = process.arch,
  builders = null,
  runCommand = null
}) {
  const resolved = builders ?? defaultBuilders({ dir: `${platform}-${arch}` })
  const run = runCommand ?? (command => execFileSync(command, { cwd: root, stdio: 'inherit', shell: true }))
  const output = []
  for (const group of groups) {
    if (!status.GROUPS.includes(group)) throw new Error(`unknown native artifact group '${group}'`)
    const before = applicableResult(group, status.classifyAll({ root, identity, platform, arch, only: [group] }))
    if (before === undefined || before.state === 'not-applicable' || before.state === 'fresh') continue
    const command = resolved[group]
    if (typeof command !== 'string' || command.length === 0) {
      return { exitCode: 1, output, error: `native:refresh: no builder for '${group}'` }
    }
    try {
      run(command)
    } catch (error) {
      return {
        exitCode: 1,
        output,
        error: `native:refresh: ${group} refresh failed (${command}): ${firstLine(error.message)}; refusing to continue on a stale artifact`
      }
    }
    const after = applicableResult(group, status.classifyAll({ root, identity, platform, arch, only: [group] }))
    if (after === undefined || after.state !== 'fresh') {
      return {
        exitCode: 1,
        output,
        error:
          `native:refresh: ${group} still stale after ${command}` +
          `${after?.detail ? `: ${after.detail}` : ''}; refusing to continue on a stale artifact`
      }
    }
    output.push(
      `native:refresh: ${group} rebuilt (sourceDigest ${digestLabel(before.staged)} -> ${after.staged.sourceDigest}; ` +
        `bindingSchema ${before.staged === null ? 'missing' : before.staged.bindingSchema} -> ${after.staged.bindingSchema})`
    )
  }
  return { exitCode: 0, output, error: null }
}

function parseArguments(argv) {
  const options = { root: process.env.UBM_NATIVE_ROOT ?? ROOT, only: status.GROUPS }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--root' || argument === '--only') {
      const value = argv[index + 1]
      if (value === undefined || value.startsWith('--')) throw new Error(`${argument} needs a value`)
      options[argument.slice(2)] =
        argument === '--only'
          ? value
              .split(',')
              .map(entry => entry.trim())
              .filter(Boolean)
          : value
      index += 1
      continue
    }
    throw new Error(`unknown argument ${argument} (expected --root <dir>, --only android,apple,desktop)`)
  }
  return options
}

if (require.main === module) {
  try {
    const options = parseArguments(process.argv.slice(2))
    const identity = require(
      process.env.UBM_NATIVE_IDENTITY_MODULE ?? path.join(ROOT, 'scripts', 'release', 'native-build-identity.js')
    )
    const outcome = runRefresh({ groups: options.only, root: path.resolve(options.root), identity })
    for (const line of outcome.output) process.stdout.write(`${line}\n`)
    if (outcome.error !== null) process.stderr.write(`${outcome.error}\n`)
    process.exitCode = outcome.exitCode
  } catch (error) {
    process.stderr.write(`native:refresh: ${firstLine(error.message)}\n`)
    process.exitCode = 2
  }
}

module.exports = { BUILDER_ENV, defaultBuilders, runRefresh }
