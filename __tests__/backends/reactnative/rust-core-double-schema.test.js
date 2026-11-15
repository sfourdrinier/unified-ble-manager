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

const ordered = golden.invokes.filter(vector => vector.op.startsWith('gatt.ordered.'))
const vectorFor = op => ordered.find(vector => vector.op === op)
const invoke = async (op, args) => {
  const native = new DeterministicRustCoreNative({ platform: 'android' })
  await native.openSession('schema-test', WIRE_REVISION)
  return JSON.parse(await native.invoke('1', op, JSON.stringify(args)))
}

test.each(ordered.filter(vector => vector.expect === 'failure' &&
  (vector.op.startsWith('gatt.ordered.marker.') || vector.op.startsWith('gatt.ordered.recording-reclaim.')))
  .map(vector => [vector.name, vector]))('the unsupported radio reports the Rust failure for %s', async (_name, vector) => {
  expect(await invoke(vector.op, JSON.parse(vector.args))).toEqual(JSON.parse(vector.envelope))
})

test.each([...new Map(ordered.map(vector => [vector.op, vector])).values()].map(vector => [vector.op, vector]))(
  'ordered operation %s rejects unknown fields', async (op, vector) => {
    const result = await invoke(op, { ...JSON.parse(vector.args), unexpected: true })
    expect(result.ok).toBe(false)
    expect(result.error.code).toBe('argument.invalid')
  }
)

test.each([...new Map(ordered.map(vector => [vector.op, vector])).values()]
  .filter(vector => Object.keys(JSON.parse(vector.args)).some(key => key !== 'admission'))
  .map(vector => [vector.op, vector]))('%s rejects a missing required field', async (op, vector) => {
  const args = JSON.parse(vector.args)
  // Optional fields have dedicated tests; choose a field required in every
  // non-empty ordered operation represented by these Rust vectors.
  delete args.peerId
  expect((await invoke(op, args)).error.code).toBe('argument.invalid')
})

test.each([
  ['gatt.ordered.status', 'collectionId', '01'],
  ['gatt.ordered.status', 'collectionId', '18446744073709551616'],
  ['gatt.ordered.ack-delivery', 'deliveryId', 1],
  ['gatt.ordered.marker.status', 'handle', '-1'],
  ['gatt.ordered.recording-reclaim.start', 'requestId', '+1'],
  ['gatt.ordered.reserve', 'peerId', ''],
  ['gatt.ordered.open', 'maxItems', -1],
  ['gatt.ordered.open', 'maxControls', 4294967296],
  ['gatt.ordered.open', 'members', [null]],
  ['gatt.ordered.marker.start', 'valueB64', 'AB=='],
  ['gatt.ordered.marker.start', 'mode', 'invalid']
])('%s rejects malformed %s', async (op, field, value) => {
  const args = JSON.parse(vectorFor(op).args)
  args[field] = value
  const result = await invoke(op, args)
  expect(result.error.code).toBe('argument.invalid')
})

test('ordered open validates nested member and selector fields before refusing the operation', async () => {
  const args = JSON.parse(vectorFor('gatt.ordered.open').args)
  args.members[0].selector.extra = 1
  expect((await invoke('gatt.ordered.open', args)).error.code).toBe('argument.invalid')
  delete args.members[0].selector.extra
  args.members[0].requestedDelivery = 'anything'
  expect((await invoke('gatt.ordered.open', args)).error.code).toBe('argument.invalid')
})

test('ordered open rejects incompatible wire versions', async () => {
  const args = JSON.parse(vectorFor('gatt.ordered.open').args)
  args.wireVersion = 0
  expect((await invoke('gatt.ordered.open', args)).error.code).toBe('protocol.incompatible')
  args.wireVersion = 2
  expect((await invoke('gatt.ordered.open', args)).error.code).toBe('ownership.denied')
  args.failureMetadataVersion = 2
  expect((await invoke('gatt.ordered.open', args)).error.code).toBe('protocol.incompatible')
})

test('ordered capacity increase rejects legacy revisions and malformed source generations', async () => {
  const args = {
    peerId: 'peer',
    collectionId: '1',
    sourceGeneration: { kind: 'counter', value: '7' },
    maxBytes: 256,
    wireVersion: 1
  }
  const legacy = await invoke('gatt.ordered.increase-capacity', { ...args, wireVersion: 2 })
  expect(legacy.error.code).toBe('argument.invalid')
  const malformed = await invoke('gatt.ordered.increase-capacity', {
    ...args,
    sourceGeneration: { kind: 'counter', value: '07' }
  })
  expect(malformed.error.code).toBe('argument.invalid')
})

test('the double reports ordered wire support without fabricating an ordered group', async () => {
  expect(await invoke('gatt.ordered.version', {})).toEqual({ ok: true, value: { wireVersion: 2 } })
  expect(await invoke('gatt.ordered.failure-metadata.version', {})).toEqual({ ok: true, value: { versions: [1] } })
  const result = await invoke('gatt.ordered.reserve', { peerId: 'peer' })
  expect(result.ok).toBe(false)
  expect(result.error.code).toBe('ownership.denied')
})

test('an existing connection still cannot fabricate an ordered radio group', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'android' })
  await native.openSession('schema-test', WIRE_REVISION)
  const connect = golden.invokes.find(vector => vector.op === 'connection.connect')
  expect(JSON.parse(await native.invoke('1', connect.op, connect.args)).ok).toBe(true)
  const peerId = JSON.parse(connect.args).peerId
  const result = JSON.parse(await native.invoke('1', 'gatt.ordered.reserve', JSON.stringify({ peerId })))
  expect(result.error.code).toBe('capability.unsupported')
  expect(result.ok).toBe(false)
})

test('ordered decimal IDs accept the full u64 range and optional nulls', async () => {
  const args = JSON.parse(vectorFor('gatt.ordered.recording-reclaim.next').args)
  args.collectionId = '18446744073709551615'
  args.callerKey = null
  expect((await invoke('gatt.ordered.recording-reclaim.next', args)).error.code).toBe('capability.unsupported')
})

test('ordered parsing rejects float spellings and oversized wire text', async () => {
  const native = new DeterministicRustCoreNative({ platform: 'android' })
  await native.openSession('schema-test', WIRE_REVISION)
  const open = vectorFor('gatt.ordered.open').args.replace('"maxItems":8', '"maxItems":8.0')
  expect(JSON.parse(await native.invoke('1', 'gatt.ordered.open', open)).error.code).toBe('argument.invalid')
  const oversized = JSON.stringify({ peerId: 'x'.repeat(1048576) })
  expect(JSON.parse(await native.invoke('1', 'gatt.ordered.reserve', oversized)).error.code).toBe('bytes.too-large')
})
