import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { decodeAppMessage, TEST_DRIVER_PROTOCOL } from '../protocol.ts'

// Conformance: the Rust simulator's `--emit-driver-hello` output must decode
// as a ubm-test-driver/1 hello. Any drift in field names, the host kind or
// the command table fails here (and the fixture freshness check in CI
// regenerates the file from the binary, so a stale fixture fails there).
const hello = JSON.parse(
  readFileSync(new URL('../../../tool/h10-sim/tests/driver-hello.json', import.meta.url), 'utf8')
)

const EXPECTED_COMMANDS = [
  'set-bpm',
  'set-battery',
  'set-contact',
  'pair-policy',
  'load-profile',
  'set-advertising',
  'drop-link',
  'set-silent',
  'reject-next-pmd',
  'clear-pmd-fault',
  'delay-responses',
  'flap-link',
  'interrupt-next-subscribe',
  'stale-callback',
  'constrain-delivery',
  'set-rates',
  'run-record',
  'get-state',
  'help'
]

test('rust peripheral-sim hello conforms to ubm-test-driver/1', () => {
  assert.equal(TEST_DRIVER_PROTOCOL, 'ubm-test-driver/1')
  const decoded = decodeAppMessage(JSON.stringify(hello))
  assert.equal(decoded.ok, true)
  assert.equal(decoded.message.host, 'peripheral-sim')
  assert.equal(decoded.message.backend, 'tool/h10-sim')
  assert.equal(decoded.message.scenarios.length, 1)
  assert.equal(decoded.message.scenarios[0].id, 'sim-control')
  const commands = decoded.message.scenarios[0].commands
  assert.deepEqual(commands.map(command => command.name), EXPECTED_COMMANDS)
  for (const command of commands) assert.equal(command.acceptsDevice, false)
})
