'use strict'

// Actual executable ingress checks. All but the explicitly requested missing
// controller case stop before opening a radio. That case only lists adapters.
const assert = require('node:assert/strict')
const { spawnSync } = require('node:child_process')

const binary = process.argv[2]
if (!binary) throw new Error('usage: node tests/adapter-cli.cjs /absolute/h10-sim')
const run = args => {
  const answer = spawnSync(binary, args, { encoding: 'utf8', timeout: 10000 })
  if (answer.error) throw answer.error
  return answer
}
for (const [args, expected] of [
  [['--adapter'], '--adapter requires a value'],
  [['--adapter', '../hci0', '--emit-test-vectors'], 'is not hciN']
]) {
  const answer = run(args)
  assert.equal(answer.status, 2)
  assert.ok(answer.stderr.includes(expected), answer.stderr)
}
const implicit = run(['--emit-test-vectors'])
assert.equal(implicit.status, 0, implicit.stderr)
const explicit = run(['--adapter', 'hci1', '--emit-test-vectors'])
assert.equal(explicit.status, 0, explicit.stderr)
assert.deepEqual(JSON.parse(explicit.stdout), JSON.parse(implicit.stdout))
const missing = run(['--adapter', 'hci65535'])
assert.equal(missing.status, 1)
assert.ok(missing.stderr.includes('requested adapter "hci65535" unavailable'), missing.stderr)
assert.equal(missing.stdout.includes('"kind":"powered-on"'), false)
assert.equal(missing.stdout.includes('"kind":"advertising-started"'), false)
console.log('SIM adapter CLI: missing value, invalid name, omitted/explicit vectors and unavailable controller PASS')
