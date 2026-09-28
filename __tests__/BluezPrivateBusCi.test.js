const fs = require('node:fs')
const path = require('node:path')
const os = require('node:os')
const { spawnSync } = require('node:child_process')

test.each([null, 'C:\\ubm-fixture\\scripts\\ci\\test-bluez-daemon-extension.js'])(
  'the Rust-only daemon gate loads without package dependencies (%s)',
  requestOverride => {
    const script = path.resolve(__dirname, '../scripts/ci/test-bluez-daemon-extension.js')
    const requested = requestOverride ?? script
    const result = spawnSync(
      process.execPath,
      [
        '-e',
        `
    const Module = require('node:module')
    const path = require('node:path')
    const load = Module._load
    Module._load = function (id, ...args) {
      if (!id.startsWith('.') && !path.isAbsolute(id) && !path.win32.isAbsolute(id) && !Module.isBuiltin(id)) {
        throw new Error('Rust-only lane cannot load package dependency: ' + id)
      }
      return load.call(this, id === ${JSON.stringify(requested)} ? ${JSON.stringify(script)} : id, ...args)
    }
    require(${JSON.stringify(requested)})
  `
      ],
      { encoding: 'utf8' }
    )
    expect(result.error).toBeUndefined()
    expect(result.stderr).toBe('')
    expect(result.status).toBe(0)
  }
)

test('CI and clean preflight share the complete private-bus regression gate', () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows/ci.yml'), 'utf8')
  const job = workflow.split('\n  rust-5-0:\n')[1].split('\n  rust-parity-5-0:\n')[0]
  expect(job).toContain("if: runner.os == 'Linux'\n        run: bash scripts/ci/test-bluez-private-bus.sh")
  expect(job).toContain('bash scripts/ci/install-linux-native-system-dependencies.sh bluez')
  const runner = fs.readFileSync(path.join(__dirname, '../scripts/ci/test-bluez-private-bus.sh'), 'utf8')
  expect(runner).toContain('set -euo pipefail')
  expect(runner).toContain(
    'UBM_BLUEZ_PRIVATE_BUS_TEST=1 dbus-run-session -- cargo test --locked -p ubm-desktop "$@" -- --ignored --test-threads=1'
  )
  expect(runner).toContain('run --test bluez_private_bus')
  expect(runner).toContain('run --lib private_bus_')
  expect(runner).toContain('run --test bluez_bearer_scope')
  expect(runner).toContain('cargo test --locked -p btleplug --lib le_gatt_tests')
  expect(runner).toContain('node scripts/ci/test-bluez-daemon-extension.js')
  const preflight = fs.readFileSync(path.join(__dirname, '../scripts/ci/preflight.sh'), 'utf8')
  expect(preflight).toContain('bash scripts/ci/test-bluez-private-bus.sh')
  expect(job).not.toContain('dbus-run-session -- cargo')
  const release = fs.readFileSync(path.join(__dirname, '../RELEASE.md'), 'utf8')
  expect(release).toContain('bash scripts/ci/test-bluez-private-bus.sh')
  expect(release).toMatch(/does not\s+qualify physical-radio behavior/)
})

test.each([undefined, 'bluez_private_bus', 'private_bus_', 'le_gatt_tests', 'daemon-extension'])(
  'the shared runner isolates every suite and stops on failure (%s)',
  failTarget => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-bluez-gate-'))
    const log = path.join(directory, 'calls.jsonl')
    try {
      fs.writeFileSync(
        path.join(directory, 'dbus-run-session'),
        `#!/usr/bin/env node
const fs = require('node:fs')
const args = process.argv.slice(2)
fs.appendFileSync(process.env.UBM_GATE_LOG, JSON.stringify({ args, marker: process.env.UBM_BLUEZ_PRIVATE_BUS_TEST }) + '\\n')
if (process.env.UBM_GATE_FAIL && args.includes(process.env.UBM_GATE_FAIL)) process.exit(23)
`,
        { mode: 0o700 }
      )
      const daemonMock = path.join(directory, 'daemon-mock.cjs')
      fs.writeFileSync(
        daemonMock,
        `const fs=require('node:fs')
fs.appendFileSync(process.env.UBM_GATE_LOG, JSON.stringify({args:['daemon-extension']})+'\\n')
if(process.env.UBM_GATE_FAIL==='daemon-extension')process.exit(23)
`
      )
      fs.writeFileSync(
        path.join(directory, 'node'),
        `#!/bin/sh
if [ "$1" = "scripts/ci/test-bluez-daemon-extension.js" ]; then
  exec '${process.execPath}' '${daemonMock}'
fi
exec '${process.execPath}' "$@"
`,
        { mode: 0o700 }
      )
      const result = spawnSync('bash', [path.join(__dirname, '../scripts/ci/test-bluez-private-bus.sh')], {
        encoding: 'utf8',
        env: {
          ...process.env,
          PATH: `${directory}${path.delimiter}${process.env.PATH}`,
          UBM_GATE_LOG: log,
          UBM_GATE_FAIL: failTarget ?? ''
        }
      })
      expect(result.error).toBeUndefined()
      expect(result.status).toBe(failTarget === undefined ? 0 : 23)
      const calls = fs
        .readFileSync(log, 'utf8')
        .trim()
        .split('\n')
        .map(line => JSON.parse(line))
      const targets = ['bluez_private_bus', 'private_bus_', 'bluez_bearer_scope', 'le_gatt_tests', 'daemon-extension']
      expect(calls).toHaveLength(failTarget === undefined ? targets.length : targets.indexOf(failTarget) + 1)
      calls.forEach((call, index) => {
        if (index === 4) {
          expect(call.args).toEqual(['daemon-extension'])
          return
        }
        expect(call.marker).toBe('1')
        expect(call.args).toEqual([
          '--',
          'cargo',
          'test',
          '--locked',
          '-p',
          index === 3 ? 'btleplug' : 'ubm-desktop',
          index === 1 || index === 3 ? '--lib' : '--test',
          targets[index],
          '--',
          '--ignored',
          '--test-threads=1'
        ])
      })
    } finally {
      fs.rmSync(directory, { recursive: true, force: true })
    }
  }
)
