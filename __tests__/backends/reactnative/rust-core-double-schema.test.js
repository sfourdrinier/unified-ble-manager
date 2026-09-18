// __tests__/backends/reactnative/rust-core-double-schema.test.js
//
// The deterministic `UnifiedBleRustCore` module enforces the Rust owner's
// argument key sets (crates/ubm-mobile/src/session.rs). This pins the double
// to the owner: every argument text the Rust owner itself produced for the
// golden vectors is accepted by the double, so a provider that passes the
// double also sends what the real owner admits.

const fs = require('node:fs')
const path = require('node:path')
const {
  DeterministicRustCoreNative,
  WIRE_REVISION
} = require('../../../test-support/react-native/deterministic-rust-core-native')

const golden = JSON.parse(
  fs.readFileSync(path.join(__dirname, '../../../crates/ubm-mobile/golden/wire-vectors.json'), 'utf8')
)

test('the double speaks the golden wire revision', () => {
  expect(golden.wireRevision).toBe(WIRE_REVISION)
})

test.each(golden.invokes.filter(vector => vector.expect === 'value').map(vector => [vector.name, vector]))(
  'the double admits the owner’s own args for %s',
  async (_name, vector) => {
    const native = new DeterministicRustCoreNative({ platform: 'android' })
    await native.openSession('schema-test', WIRE_REVISION)
    const envelope = JSON.parse(await native.invoke('1', vector.op, vector.args))
    if (!envelope.ok) expect(envelope.error.code).not.toBe('argument.invalid')
  }
)
