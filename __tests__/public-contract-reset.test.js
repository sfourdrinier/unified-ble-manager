// __tests__/public-contract-reset.test.js — PR1 TDD contract
const vectors = require('./fixtures/restoration-identity.golden.json')

describe('PR1 public contract reset (TDD)', () => {
  test('OperationOptions normalizes timeoutMs to Deadline and validates strictly', () => {
    const { normalizeOperationOptions } = require('../lib/commonjs/public/operation-options')
    const now = () => 1000
    // valid
    const normalized = normalizeOperationOptions({ timeoutMs: 5000 }, now)
    expect(normalized.deadline).toBe(6000)
    expect(normalized.signal).toBeNull()

    // invalid values reject with argument.invalid
    expect(() => normalizeOperationOptions({ timeoutMs: 0 }, now)).toThrow()
    expect(() => normalizeOperationOptions({ timeoutMs: -1 }, now)).toThrow()
    expect(() => normalizeOperationOptions({ timeoutMs: NaN }, now)).toThrow()
    expect(() => normalizeOperationOptions({ timeoutMs: Infinity }, now)).toThrow()
    expect(() => normalizeOperationOptions({ timeoutMs: 1.5 }, now)).toThrow()
    expect(() => normalizeOperationOptions({ signal: 'not-a-signal' }, now)).toThrow()

    // nested helpers never extend deadline
    const early = normalizeOperationOptions({ timeoutMs: 1000 }, now)
    const later = normalizeOperationOptions({ timeoutMs: 5000 }, () => 1000, early.deadline)
    expect(later.deadline).toBe(early.deadline)
  })

  test('StreamPreset maps to exact bounded capacities', () => {
    const { resolveStreamPreset } = require('../lib/commonjs/public/stream-presets')
    const latest = resolveStreamPreset({ preset: 'latest' })
    const balanced = resolveStreamPreset({ preset: 'balanced' })
    const lossless = resolveStreamPreset({ preset: 'lossless-bounded' })
    expect(latest.itemCapacity).toBe(1)
    expect(balanced.itemCapacity).toBe(32)
    expect(lossless.overflowPolicy).toBe('error')
    expect(() => resolveStreamPreset({ preset: 'custom' })).toThrow()
    expect(() => resolveStreamPreset({ preset: 'custom', custom: { itemCapacity: 1, byteCapacity: 1 } })).toThrow()
    const custom = resolveStreamPreset({
      preset: 'custom',
      custom: { itemCapacity: 10, byteCapacity: 1024 }
    })
    expect(custom.itemCapacity).toBe(10)
    const customPolicy = require('../lib/commonjs/public/stream-presets').resolveStreamPolicy({
      preset: 'custom',
      budget: { itemCapacity: 3, byteCapacity: 256, reservedControlCapacity: 1, overflowPolicy: 'error' }
    })
    expect(customPolicy).toMatchObject({
      itemCapacity: 3,
      byteCapacity: 256,
      reservedControlCapacity: 1,
      overflowPolicy: 'error'
    })
    expect(() =>
      require('../lib/commonjs/public/stream-presets').resolveStreamPolicy({
        preset: 'custom',
        budget: { itemCapacity: 1, byteCapacity: 1 }
      })
    ).toThrow()
  })

  test('Restoration identity vectors are native bootstrap outputs with explicit generation invalidation', () => {
    const { createEphemeralHostIdentity } = require('../lib/commonjs/public/host-identity')
    for (const v of vectors) {
      expect(v).toMatchObject({
        applicationId: expect.any(String),
        restorationId: expect.any(String),
        generation: expect.any(String),
        rootSha256: expect.stringMatching(/^[0-9a-f]{64}$/),
        restoreIdentifier: expect.any(String),
        namespaceValue: expect.stringMatching(/^ubm-ns:[A-Za-z0-9_-]+$/),
        clientId: expect.stringMatching(/^ubm-client:[A-Za-z0-9_-]+$/),
        hostSessionScope: expect.stringMatching(/^ubm-host:[A-Za-z0-9_-]+$/)
      })
    }
    expect(vectors.find(v => v.generation === '1').clientId).not.toBe(vectors.find(v => v.generation === '2').clientId)

    // ephemeral IDs are unique
    const e1 = createEphemeralHostIdentity({ randomBytes: l => new Uint8Array(l).fill(1) })
    const e2 = createEphemeralHostIdentity({ randomBytes: l => new Uint8Array(l).fill(2) })
    expect(e1.attachmentNonce).not.toBe(e2.attachmentNonce)

    // deterministic ephemeral injection stable
    const d1 = createEphemeralHostIdentity({
      randomBytes: () => new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
    })
    const d2 = createEphemeralHostIdentity({
      randomBytes: () => new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16])
    })
    expect(d1.attachmentNonce).toBe(d2.attachmentNonce)
  })

  test('BleManagerCreateOptions validates and instanceId does not affect restoration', () => {
    const { normalizeBleManagerCreateOptions } = require('../lib/commonjs/public/host-identity')
    const base = normalizeBleManagerCreateOptions({ instanceId: 'my-instance' })
    expect(base.instanceId).toBe('my-instance')
    expect(() => normalizeBleManagerCreateOptions({ instanceId: 'bad:id' })).toThrow()
    expect(normalizeBleManagerCreateOptions({ restoration: { restorationId: 'ble' } })).toMatchObject({
      restoration: { restorationId: 'ble', generation: '1' }
    })
    expect(() => normalizeBleManagerCreateOptions({ restoration: { applicationId: 'com.example.app', restorationId: 'x' } })).toThrow()
    expect(() => normalizeBleManagerCreateOptions({ restoration: { restorationId: 'bad:id' } })).toThrow()
    expect(() => normalizeBleManagerCreateOptions({ unsupported: true })).toThrow()
  })

  test('Advanced entrypoint re-exports low-level contracts', () => {
    const advanced = require('../lib/commonjs/advanced')
    expect(typeof advanced.deadline).toBe('function')
    expect(typeof advanced.capacity).toBe('function')
    expect(typeof advanced.createBleManager).toBe('function')
    expect(typeof advanced.normalizeOperationOptions).toBe('function')
  })

  test('Expo entrypoint exists and is thin composition', () => {
    const expo = require('../lib/commonjs/expo')
    expect(typeof expo.createExpoBleManager).toBe('function')
    expect(typeof expo.createExpoBleManagerWithEnvironment).toBe('function')
    expect(typeof expo.mapExpoReadiness).toBe('function')
  })

  test('Expo restoration accepts one application-facing token without a caller application id', () => {
    const { normalizeBleManagerCreateOptions } = require('../lib/commonjs/public/host-identity')
    expect(normalizeBleManagerCreateOptions({ restoration: { restorationId: 'ble' } })).toMatchObject({
      restoration: { restorationId: 'ble', generation: '1' }
    })
  })
})
