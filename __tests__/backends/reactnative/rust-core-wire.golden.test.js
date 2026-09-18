// __tests__/backends/reactnative/rust-core-wire.golden.test.js
//
// PR210-12: golden vectors bind the two halves of `ubm-mobile-wire/1`.
// `crates/ubm-mobile/golden/wire-vectors.json` holds envelope and drain
// text produced by the Rust owner itself (crates/ubm-mobile/tests/golden.rs,
// checked by `cargo test -p ubm-mobile`); every vector must parse through
// the TS codec without Buffer/atob, byte-exactly where bytes are known.

const fs = require('node:fs')
const path = require('node:path')

const removed = {}
for (const name of ['Buffer', 'atob', 'btoa']) {
  removed[name] = Object.getOwnPropertyDescriptor(globalThis, name)
  delete globalThis[name]
}

const wire = require('../../../src/backends/reactnative/rust-core-wire')

afterAll(() => {
  for (const [name, descriptor] of Object.entries(removed)) {
    if (descriptor !== undefined) Object.defineProperty(globalThis, name, descriptor)
  }
})

const golden = JSON.parse(
  fs.readFileSync(path.join(__dirname, '../../../crates/ubm-mobile/golden/wire-vectors.json'), 'utf8')
)

function ok(result, what) {
  if (!result.ok) {
    throw new Error(`${what}: ${result.error.normalized.code} at ${result.error.normalized.operation}`)
  }
  return result.value
}

describe('ubm-mobile-wire/1 golden vectors (Rust-generated)', () => {
  it('speaks the same wire revision', () => {
    expect(golden.wireRevision).toBe(wire.WIRE_REVISION)
  })

  it('covers every op of the TS op table', () => {
    const covered = new Set(golden.invokes.map(vector => vector.op))
    expect(wire.WIRE_OPS.filter(op => !covered.has(op))).toEqual([])
  })

  const known = golden.invokes.filter(vector => wire.WIRE_OPS.includes(vector.op))
  it.each(known.map(vector => [vector.name, vector]))('parses invoke: %s', (_name, vector) => {
    const envelope = ok(wire.parseInvokeEnvelope(vector.envelope, vector.op), `${vector.name} envelope`)
    if (vector.expect === 'value') {
      expect(envelope.kind).toBe('value')
      ok(wire.parseOpValue(vector.op, envelope.value), `${vector.name} value`)
    } else {
      expect(envelope.kind).toBe('failure')
      expect(wire.remoteFailureError(envelope.failure).normalized.code).toBe(envelope.failure.code)
    }
  })

  it('rejects an unknown op before anything reaches the owner, as the owner does', () => {
    for (const vector of golden.invokes.filter(entry => !wire.WIRE_OPS.includes(entry.op))) {
      expect(wire.parseInvokeEnvelope(vector.envelope, vector.op).ok).toBe(false)
      expect(JSON.parse(vector.envelope)).toMatchObject({ ok: false, error: { code: 'argument.invalid' } })
    }
  })

  it('carries write commit states the parser accepts', () => {
    const byName = Object.fromEntries(golden.invokes.map(vector => [vector.name, vector]))
    const confirmed = ok(wire.parseInvokeEnvelope(byName['write with response'].envelope, 'gatt.write'), 'w1')
    expect(ok(wire.parseOpValue('gatt.write', confirmed.value), 'w1 value')).toEqual({ commitState: 'confirmed' })
    const unconfirmed = ok(wire.parseInvokeEnvelope(byName['write without response'].envelope, 'gatt.write'), 'w2')
    const receipt = ok(wire.parseOpValue('gatt.write', unconfirmed.value), 'w2 value')
    expect(ok(wire.checkWriteReceipt(receipt, 'without-response'), 'w2 receipt')).toEqual({ commitState: 'unknown' })
    const malformed = ok(wire.parseInvokeEnvelope(byName['write malformed base64'].envelope, 'gatt.write'), 'w3')
    expect(malformed).toMatchObject({ kind: 'failure', commit: 'not-dispatched' })
    const refused = ok(wire.parseInvokeEnvelope(byName['write refused before sending'].envelope, 'gatt.write'), 'w4')
    expect(refused).toMatchObject({ kind: 'failure', commit: 'not-dispatched' })
  })

  // Owner decision (5.0): every failure envelope carries the owner's own
  // retryability, and the contract error reports it instead of re-deriving
  // it from the code. A connect whose link the platform could not establish
  // (Android GATT 133) is `caller-decides` with the platform's answer kept.
  it('reports the owner retryability of every failure envelope', () => {
    const byName = Object.fromEntries(golden.invokes.map(vector => [vector.name, vector]))
    for (const vector of known.filter(entry => entry.expect !== 'value')) {
      const envelope = ok(wire.parseInvokeEnvelope(vector.envelope, vector.op), vector.name)
      expect(envelope.retryability).toBe(JSON.parse(vector.envelope).retryability)
    }
    const transient = byName['connect link not established (android gatt 133)']
    const envelope = ok(wire.parseInvokeEnvelope(transient.envelope, 'connection.connect'), 'transient connect')
    const error = wire.failureEnvelopeError(envelope)
    expect(error.normalized).toMatchObject({
      code: 'connection.failed',
      operation: 'connection.connect',
      retryability: 'caller-decides',
      platform: { domain: 'android', code: 'connectionFailed', metadata: { androidGattStatus: 133 } }
    })
    const refusedWrite = ok(
      wire.parseInvokeEnvelope(byName['write refused by the peer (android gatt status)'].envelope, 'gatt.write'),
      'refused write'
    )
    expect(wire.failureEnvelopeError(refusedWrite).normalized).toMatchObject({
      retryability: 'never',
      commit: 'uncertain'
    })
    const missing = JSON.parse(transient.envelope)
    delete missing.retryability
    expect(wire.parseInvokeEnvelope(JSON.stringify(missing), 'connection.connect').ok).toBe(false)
  })

  it('names no desktop host anywhere on the mobile wire', () => {
    const texts = [...golden.invokes.map(vector => vector.envelope), ...golden.drains.map(batch => batch.text)]
    expect(texts.filter(text => text.toLowerCase().includes('desktop'))).toEqual([])
  })

  it('reports the legacy React Native generations ("1", corebluetooth-attachment-lifecycle.ts on origin/main)', () => {
    const byName = Object.fromEntries(golden.invokes.map(vector => [vector.name, vector]))
    const envelope = ok(wire.parseInvokeEnvelope(byName['adapter state'].envelope, 'adapter.state'), 'adapter state')
    const state = ok(wire.parseOpValue('adapter.state', envelope.value), 'adapter state value')
    expect(state).toMatchObject({ backendGeneration: '1', adapterGeneration: '1' })
    const adapters = golden.drains
      .flatMap(batch => ok(wire.parseDrainText(batch.text, batch.lastOrdinal), batch.name).records)
      .filter(record => record.t === 'adapter')
    expect(adapters.length).toBeGreaterThan(0)
    for (const record of adapters) {
      expect(record.state).toMatchObject({ backendGeneration: '1', adapterGeneration: '1' })
    }
  })

  it('parses every drain batch in order, byte-exactly', () => {
    const batches = golden.drains.map(batch =>
      ok(wire.parseDrainText(batch.text, batch.lastOrdinal), `drain ${batch.name}`)
    )
    const records = batches.flatMap(batch => batch.records)
    const adv = records.find(record => record.t === 'adv' && record.manufacturerData !== null)
    expect(Array.from(adv.manufacturerData[0].payload)).toEqual([0x00, 0x80, 0xff])
    expect(Array.from(adv.serviceData[0].payload)).toEqual([])
    const values = records.filter(record => record.t === 'value').map(record => Array.from(record.value))
    expect(values).toEqual([
      [0x00, 0x55],
      [0x10, 0x55, 0x20, 0x03]
    ])
    const kinds = records.map(record => record.t)
    expect(kinds.indexOf('link')).toBeLessThan(kinds.indexOf('stream-end'))
    for (const t of ['adv', 'value', 'adapter', 'security', 'restored', 'ingress-drop', 'link', 'stream-end']) {
      expect(kinds).toContain(t)
    }
  })
})
