const {
  createReactNativeScanFailureRecovery,
  readReactNativeRawRecoveryToken,
  readReactNativeScanFailureRecoveryPage
} = require('../src/backends/reactnative/react-native-scan-failure-recovery')

describe('React Native scan failure recovery page boundary', () => {
  afterEach(() => delete globalThis.__unifiedBleRustCoreScanFailureRecoveryV1)

  test('returns one owned atomic native page', () => {
    globalThis.__unifiedBleRustCoreScanFailureRecoveryV1 = {
      read: jest.fn(() => ({ offset: '0', nextOffset: '3', done: true, bytes: new Uint8Array([1, 2, 3]) }))
    }
    const page = readReactNativeScanFailureRecoveryPage('head-opaque', '0')
    expect(page).toEqual({ offset: '0', nextOffset: '3', done: true, bytes: new Uint8Array([1, 2, 3]) })
    expect(globalThis.__unifiedBleRustCoreScanFailureRecoveryV1.read).toHaveBeenCalledWith('head-opaque', '0')
  })

  test('rejects a stale runtime page that is malformed', () => {
    globalThis.__unifiedBleRustCoreScanFailureRecoveryV1 = {
      read: () => ({ offset: '0', nextOffset: '1', done: false, bytes: [1] })
    }
    expect(() => readReactNativeScanFailureRecoveryPage('forged', '0')).toThrow(/protocol\.malformed/)
  })

  test.each([
    [{ kind: 'null' }, { kind: 'null' }],
    [{ kind: 'bool', value: true }, { kind: 'bool', value: true }],
    [{ kind: 'i64', value: '-42' }, { kind: 'i64', value: '-42' }],
    [{ kind: 'u64', value: '18446744073709551615' }, { kind: 'u64', value: '18446744073709551615' }],
    [{ kind: 'f64Bits', value: '9223372036854775808' }, { kind: 'f64Bits', value: '9223372036854775808' }],
    [{ kind: 'map', count: '2' }, { kind: 'map', count: '2' }],
    [{ kind: 'array', count: '0' }, { kind: 'array', count: '0' }],
    [{ kind: 'text', total: '3', bytes: new Uint8Array([97, 98, 99]) }, { kind: 'text', total: '3', bytes: new Uint8Array([97, 98, 99]) }],
    [{ kind: 'bytes', total: '2', bytes: new Uint8Array([4, 5]) }, { kind: 'bytes', total: '2', bytes: new Uint8Array([4, 5]) }]
  ])('projects raw token tag %# without lossy JSON', (nativeToken, expected) => {
    globalThis.__unifiedBleRustCoreScanFailureRecoveryV1 = { read: jest.fn(), rawToken: jest.fn(() => nativeToken) }
    expect(readReactNativeRawRecoveryToken('opaque-owner', 0, '0', '65536')).toEqual(expected)
    expect(globalThis.__unifiedBleRustCoreScanFailureRecoveryV1.rawToken).toHaveBeenCalledWith('opaque-owner', 0, '0', '65536')
  })

  test('rejects malformed raw token and invalid bounded windows', () => {
    globalThis.__unifiedBleRustCoreScanFailureRecoveryV1 = { read: jest.fn(), rawToken: () => ({ kind: 'bytes', total: '01', bytes: new Uint8Array([1]) }) }
    expect(() => readReactNativeRawRecoveryToken('opaque-owner', 0, '0', '1')).toThrow(/protocol\.malformed/)
    expect(() => readReactNativeRawRecoveryToken('opaque-owner', 0, '0', '0')).toThrow(/argument\.invalid/)
    expect(() => readReactNativeRawRecoveryToken('opaque-owner', 0, '0', '65537')).toThrow(/argument\.invalid/)
  })

  test('validates raw metadata and terminal control responses at the public facade', async () => {
    const native = {
      scanFailureRecoveryOpenSelected: jest.fn(), scanFailureRecoveryOwners: jest.fn(), scanFailureRecoveryMember: jest.fn(), scanFailureRecoveryHead: jest.fn(), scanFailureRecoveryControl: jest.fn(), scanFailureRecoveryPendingCleanup: jest.fn(), scanFailureRecoveryRetireCleanup: jest.fn(),
      scanFailureRecoveryRawInfo: jest.fn(async () => JSON.stringify({ identity: { host: 'aa', scan: 'bb', start: 'cc', sourceGeneration: 'dd', occurrence: 'ee' }, memberCount: 1, tokenCount: 2 })),
      scanFailureRecoveryRawMember: jest.fn(async () => JSON.stringify({ member: { recipient: '11', delivery: '22' } })),
      scanFailureRecoveryRawControl: jest.fn(async (_, operation) => JSON.stringify({ cleanupReceiptId: operation === 'finalize' ? 'opaque-receipt' : null }))
    }
    const recovery = createReactNativeScanFailureRecovery(native)
    await expect(recovery.rawInfo('opaque-owner')).resolves.toEqual({ identity: { host: 'aa', scan: 'bb', start: 'cc', sourceGeneration: 'dd', occurrence: 'ee' }, memberCount: 1, tokenCount: 2 })
    await expect(recovery.rawMember('opaque-owner', 0)).resolves.toEqual({ member: { recipient: '11', delivery: '22' } })
    await expect(recovery.rawRetire('opaque-owner')).resolves.toEqual({ completed: true })
    await expect(recovery.rawRelease('opaque-owner')).resolves.toEqual({ completed: true })
    await expect(recovery.rawFinalize('opaque-owner')).resolves.toEqual({ cleanupReceiptId: 'opaque-receipt' })
  })
})
