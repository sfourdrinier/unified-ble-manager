// scripts/ci/build-package.js

'use strict'

const { spawnSync } = require('child_process')
const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '../..')
const output = path.join(root, 'lib')
const pnpm = process.platform === 'win32' ? 'pnpm.cmd' : 'pnpm'

function run(command, args) {
  const result = spawnSync(command, args, {
    cwd: root,
    stdio: 'inherit',
    // Windows command shims (`pnpm.cmd`) must be launched through cmd.exe.
    shell: process.platform === 'win32'
  })
  if (result.error) {
    throw result.error
  }
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} failed with exit code ${String(result.status)}`)
  }
}

fs.rmSync(output, { recursive: true, force: true })
run(pnpm, ['run', 'clean:plugin'])
run(pnpm, ['run', 'native-protocol:check'])
run(pnpm, ['run', 'validate:evidence'])
// PR210-18: the expected native build identity compiled into lib/ must match
// the Rust sources and binding schema; a stale module fails here with the
// --write command instead of shipping a runtime check that rejects every
// correctly built binary (or accepts a stale one).
run(process.execPath, ['scripts/release/native-build-identity.js', '--check'])
run(pnpm, ['run', 'build:plugin'])
run(pnpm, ['exec', 'bob', 'build'])
// F23: seal the exact input set that produced lib/ before anything verifies
// it. verify-package-artifacts expects the seal as a build artifact.
run(process.execPath, ['scripts/release/generate-build-fingerprint.js'])
run(pnpm, ['run', 'docs:check'])
run(process.execPath, ['scripts/ci/verify-package-artifacts.js'])
