'use strict'
const { test } = require('node:test')
const assert = require('node:assert/strict')
const { spawnSync } = require('node:child_process')
const path = require('node:path')
const root = path.resolve(__dirname, '../../..')
function run(...args) {
  const result = spawnSync('cargo', ['run', '--locked', '--quiet', '--manifest-path', 'tool/h10-sim/Cargo.toml', '--', ...args], { cwd: root, encoding: 'utf8', timeout: 120000 })
  assert.ifError(result.error)
  return result
}
test('comparison CLI refuses every non-finite tolerance before opening a radio', () => {
  for (const flag of ['--tolerance-p50', '--tolerance-ms']) {
    for (const value of ['NaN', 'inf', '-inf', '-1']) {
      const result = run(flag, value, '--emit-driver-hello')
      assert.equal(result.status, 2, result.stderr)
      assert.match(result.stderr, /must be a number >= 0/)
    }
  }
})
test('scenario CLI and committed profiles accept only supported scenarios and positive BPM', () => {
  for (const scenario of ['steady', 'breathing', 'exercise-recovery', 'abrupt']) {
    for (const args of [['--scenario', scenario], ['--profile', `tool/h10-sim/profiles/synthetic-${scenario}.json`]]) {
      const result = run(...args, '--emit-driver-hello')
      assert.equal(result.status, 0, result.stderr)
      assert.doesNotThrow(() => JSON.parse(result.stdout))
    }
  }
  assert.equal(run('--scenario', 'invalid', '--emit-driver-hello').status, 2)
  assert.equal(run('--bpm', '0', '--emit-driver-hello').status, 2)
})
