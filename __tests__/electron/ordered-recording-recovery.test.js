// __tests__/electron/ordered-recording-recovery.test.js
const { ElectronOrderedRecordingRecovery } = require('../../src/electron/ordered-recording-recovery')

test('capability and committed mapping remain owner, peer, database and lease scoped', async () => {
  const registry = new ElectronOrderedRecordingRecovery(4)
  const authority = Object.freeze({ kind: 'ordered-recording-recovery' })
  const token = registry.issue('owner', '1', 'peer', () => authority).recoveryToken
  const handle = {
    start: jest.fn(async () => ({ state: 'ready', error: null, mappingCommitted: false })),
    status: jest.fn(async () => ({ state: 'committed', error: null, mappingCommitted: true })),
    commit: jest.fn(async () => ({ state: 'committed', error: null, mappingCommitted: true })),
    cancel: jest.fn(async () => ({ state: 'released', error: null, mappingCommitted: false })),
    release: jest.fn(async () => ({ state: 'released', error: null, mappingCommitted: true })),
    retire: jest.fn(async () => undefined)
  }
  expect(() => registry.reserve('foreign', token, 'key', 'db', 'lease', 'peer', () => handle)).toThrow()
  expect(() => registry.reserve('owner', token, 'key', 'db', 'lease', 'foreign', () => handle)).toThrow()
  const id = registry.reserve('owner', token, 'key', 'db', 'lease', 'peer', received => { expect(received).toBe(authority); return handle }).reclaimHandle
  expect(registry.reserve('owner', token, 'key', 'db', 'lease', 'peer', () => { throw Error('duplicate allocation') }).reclaimHandle).toBe(id)
  expect(() => registry.markerOwner('owner', id, 'db', 'lease')).toThrow()
  await registry.action('owner', id, 'lease', 'start')
  await registry.action('owner', id, 'lease', 'commit')
  expect(registry.markerOwner('owner', id, 'db', 'lease')).toBe('1')
  expect(() => registry.markerOwner('owner', id, 'db', 'other')).toThrow()
  await expect(registry.action('foreign', id, 'lease', 'status')).rejects.toThrow()
  await registry.action('owner', id, 'lease', 'release')
  await registry.action('owner', id, 'lease', 'retire')
  expect(() => registry.forget('owner', id, 'lease')).toThrow()
  registry.acknowledge('owner', id, 'lease')
  registry.forget('owner', id, 'lease')
  registry.confirmForgotten('owner', id, 'lease')
  expect(registry.markerOwner('owner', id, 'db', 'lease')).toBe('1')
  registry.releaseLease('lease')
  expect(() => registry.markerOwner('owner', id, 'db', 'lease')).toThrow()
  registry.forgetRecording('owner', '1')
})

test('revoked in-flight reclaim remains recoverable for cleanup but cannot start or commit', async () => {
  const registry = new ElectronOrderedRecordingRecovery(2)
  const token = registry.issue('owner', '1', 'peer', () => ({ kind: 'ordered-recording-recovery' })).recoveryToken
  const receipt = { state: 'released', error: null, mappingCommitted: false }
  const handle = { start: async () => receipt, status: async () => receipt, commit: async () => receipt, cancel: async () => receipt, release: async () => receipt, retire: async () => undefined }
  const id = registry.reserve('owner', token, 'key', 'db', 'old', 'peer', () => handle).reclaimHandle
  registry.releaseLease('old')
  expect(registry.pending('owner', null, 1)).toMatchObject({ transactions: [{ reclaimHandle: id, state: 'revoked' }], truncated: false })
  expect(registry.pending('foreign', null, 1).transactions).toEqual([])
  await expect(registry.action('owner', id, 'new', 'start')).rejects.toThrow()
  await expect(registry.action('owner', id, 'new', 'commit')).rejects.toThrow()
  await expect(registry.action('owner', id, 'new', 'release')).resolves.toMatchObject({ status: receipt })
  await registry.action('owner', id, 'new', 'retire')
  registry.acknowledge('owner', id, 'new')
  registry.forget('owner', id, 'new')
  registry.confirmForgotten('owner', id, 'new')
})

test('pre-reservation refusal is preserved and never authorizes a marker mapping', async () => {
  const registry = new ElectronOrderedRecordingRecovery(2)
  const token = registry.issue('owner', '1', 'peer', () => ({ kind: 'ordered-recording-recovery' })).recoveryToken
  const failure = { code: 'gatt.stale-handle', domain: 'gatt', operation: 'test.reclaim', platform: null, retryability: 'never', commit: null }
  const refusal = { state: 'not-reserved', error: failure }
  const handle = { start: async () => refusal, status: async () => refusal, commit: async () => { throw Error('not reserved') }, cancel: async () => refusal, release: async () => { throw Error('not reserved') }, retire: async () => undefined }
  const id = registry.reserve('owner', token, 'key', 'db', 'lease', 'peer', () => handle).reclaimHandle
  expect(await registry.action('owner', id, 'lease', 'start')).toEqual({ status: refusal })
  expect(() => registry.markerOwner('owner', id, 'db', 'lease')).toThrow()
  await registry.action('owner', id, 'lease', 'retire')
  registry.acknowledge('owner', id, 'lease')
  registry.forget('owner', id, 'lease')
  registry.confirmForgotten('owner', id, 'lease')
})
