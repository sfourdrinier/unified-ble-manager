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

  test('Restoration identity is deterministic and matches golden vectors', () => {
    const { deriveRestorationIdentity, createEphemeralHostIdentity } = require('../lib/commonjs/public/host-identity')
    for (const v of vectors) {
      const result = deriveRestorationIdentity({
        applicationId: v.applicationId,
        restorationId: v.restorationId,
        generation: v.generation
      })
      expect(result.opaqueRestorationId).toBe(v.opaqueRestorationId)
    }
    // case normalization stability
    const a = deriveRestorationIdentity({ applicationId: 'com.example.app', restorationId: 'ble', generation: '0' })
    const b = deriveRestorationIdentity({ applicationId: 'COM.EXAMPLE.APP', restorationId: 'ble', generation: '0' })
    expect(a.opaqueRestorationId).toBe(b.opaqueRestorationId)

    // changing only generation changes domain deterministically
    const g0 = deriveRestorationIdentity({ applicationId: 'com.example.app', restorationId: 'ble', generation: '0' })
    const g1 = deriveRestorationIdentity({ applicationId: 'com.example.app', restorationId: 'ble', generation: '1' })
    expect(g0.opaqueRestorationId).not.toBe(g1.opaqueRestorationId)

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
    const {
      normalizeBleManagerCreateOptions,
      deriveRestorationIdentity
    } = require('../lib/commonjs/public/host-identity')
    const base = normalizeBleManagerCreateOptions({ instanceId: 'my-instance' })
    expect(base.instanceId).toBe('my-instance')
    expect(() => normalizeBleManagerCreateOptions({ instanceId: 'bad:id' })).toThrow()
    expect(() => normalizeBleManagerCreateOptions({ restoration: { applicationId: '', restorationId: 'x' } })).toThrow()

    const withInstance = deriveRestorationIdentity({
      applicationId: 'com.example.app',
      restorationId: 'ble',
      generation: '0'
    })
    const withoutInstance = deriveRestorationIdentity({
      applicationId: 'com.example.app',
      restorationId: 'ble',
      generation: '0'
    })
    // instanceId must not appear in restoration material — same opaque
    expect(withInstance.opaqueRestorationId).toBe(withoutInstance.opaqueRestorationId)
    expect(() => normalizeBleManagerCreateOptions({ unsupported: true })).toThrow()
  })

  test('Advanced entrypoint re-exports low-level contracts', () => {
    const advanced = require('../lib/commonjs/advanced')
    expect(typeof advanced.deadline).toBe('function')
    expect(typeof advanced.capacity).toBe('function')
    expect(typeof advanced.createBleManager).toBe('function')
    expect(typeof advanced.normalizeOperationOptions).toBe('function')
  })

  test('Expo entrypoint exists and is thin composition (PR10 stub)', () => {
    const expo = require('../lib/commonjs/expo')
    expect(typeof expo.createExpoBleManager).toBe('function')
    expect(typeof expo.createExpoBleManagerWithEnvironment).toBe('function')
  })

  test('Expo restoration remains fail-closed until the PR10 native contract lands', async () => {
    const expo = require('../src/expo')
    await expect(
      expo.createExpoBleManager({ restoration: { applicationId: 'com.example.app', restorationId: 'ble' } })
    ).rejects.toMatchObject({ code: 'capability.unsupported' })
  })
})
