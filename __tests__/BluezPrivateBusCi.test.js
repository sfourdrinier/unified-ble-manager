const fs = require('node:fs')
const path = require('node:path')
const os = require('node:os')
const { spawnSync } = require('node:child_process')

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
  const preflight = fs.readFileSync(path.join(__dirname, '../scripts/ci/preflight.sh'), 'utf8')
  expect(preflight).toContain('bash scripts/ci/test-bluez-private-bus.sh')
  expect(job).not.toContain('dbus-run-session -- cargo')
  const release = fs.readFileSync(path.join(__dirname, '../RELEASE.md'), 'utf8')
  expect(release).toContain('bash scripts/ci/test-bluez-private-bus.sh')
  expect(release).toMatch(/does not\s+qualify physical-radio behavior/)
})

test.each([undefined, 'bluez_private_bus', 'private_bus_'])(
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
      const targets = ['bluez_private_bus', 'private_bus_', 'bluez_bearer_scope']
      expect(calls).toHaveLength(failTarget === undefined ? 3 : targets.indexOf(failTarget) + 1)
      calls.forEach((call, index) => {
        expect(call.marker).toBe('1')
        expect(call.args).toEqual([
          '--',
          'cargo',
          'test',
          '--locked',
          '-p',
          'ubm-desktop',
          index === 1 ? '--lib' : '--test',
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
