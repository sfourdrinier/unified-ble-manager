'use strict'

// scripts/native/ensure-native.js — F9: the one guard every native consumer
// calls before it builds or launches (hosts.sh up, build-tv.sh build, the
// phone Expo builds). Refreshes exactly the named groups (a no-op when
// fresh); UBM_NATIVE_REFRESH=off switches to check-only. Any failure aborts
// the caller — nothing continues on a stale artifact.
//
// Usage:
//   node scripts/native/ensure-native.js <group...>   (android | apple | desktop)

const { spawnSync } = require('node:child_process')
const path = require('node:path')

const status = require('./native-status')

const ROOT = path.resolve(__dirname, '..', '..')

function main(argv, env = process.env) {
  const groups = argv
  if (groups.length === 0)
    throw new Error('usage: node scripts/native/ensure-native.js <group...> (android | apple | desktop)')
  for (const group of groups) {
    if (!status.GROUPS.includes(group)) throw new Error(`unknown native artifact group '${group}'`)
  }
  const only = groups.join(',')
  const checkOnly = env.UBM_NATIVE_REFRESH === 'off'
  const script = checkOnly ? 'native:status' : 'native:refresh'
  // Windows executes pnpm as pnpm.cmd, which needs cmd.exe: without a shell
  // the spawn fails and no consumer guard can ever observe the stub or the
  // real shim. shell:true is portable (same command on every platform) and
  // matches native-refresh.js, which already runs its builders through one.
  const result = spawnSync('pnpm', ['--dir', ROOT, script, '--only', only], { stdio: 'inherit', shell: true })
  if (result.error !== undefined) {
    throw new Error(`could not run pnpm ${script} --only ${only}: ${result.error.message}`)
  }
  if (result.status !== 0) {
    if (checkOnly) {
      throw new Error(
        `UBM_NATIVE_REFRESH=off (check-only): ${only} is stale or missing — refusing to continue (run: pnpm native:refresh --only ${only})`
      )
    }
    throw new Error(
      `refresh failed for ${only} — refusing to continue on a stale artifact (run: pnpm native:refresh --only ${only})`
    )
  }
}

if (require.main === module) {
  try {
    main(process.argv.slice(2))
  } catch (error) {
    process.stderr.write(`ensure-native: ${String(error.message).split('\n')[0]}\n`)
    process.exitCode = 1
  }
}

module.exports = { main }
