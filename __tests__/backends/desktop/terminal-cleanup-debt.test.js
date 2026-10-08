const { DesktopRustCoreBackend } = require('../../../src/backends/desktop/desktop-rust-core-provider')
const { contractError } = require('../../../src/backend-contract/errors')

test('lost public connection retains native cleanup refusal and retries the same lease', async () => {
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  const record = {
    nativePeerId: 'peer',
    lease: 'original-lease',
    coreGeneration: 'original-generation',
    state: 'disconnected',
    acquiredClosers: new Set()
  }
  const disconnect = jest
    .fn()
    .mockRejectedValueOnce(contractError('operation.timed-out', 'cleanup', 'native.discovery-retirement'))
    .mockResolvedValueOnce({
      schema: 'ubm-desktop-release/1',
      state: 'released',
      peerId: 'peer',
      lease: 'original-lease',
      connectionGeneration: 'original-generation'
    })
  Object.assign(backend, { op: operation => operation, central: { disconnect }, connectionsById: new Map() })
  const failed = await backend.disconnectConnectionCurrent(record)
  expect(failed).toMatchObject({
    state: 'release-failed',
    failures: [
      { resourceKind: 'connection', error: { code: 'operation.timed-out', operation: 'native.discovery-retirement' } }
    ]
  })
  expect(record.state).toBe('disconnected')
  await expect(backend.disconnectConnectionCurrent(record)).resolves.toMatchObject({ state: 'released', failures: [] })
  expect(disconnect.mock.calls).toEqual([
    [{ peerId: 'peer', lease: 'original-lease' }],
    [{ peerId: 'peer', lease: 'original-lease' }]
  ])
})


test('terminal cleanup preserves the exact native lease while another logical owner remains live', async () => {
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  const record = { nativePeerId: 'peer', lease: 'shared-lease', coreGeneration: 'shared-generation',
    state: 'disconnected', acquiredClosers: new Set() }
  const survivor = { ...record, state: 'connected' }
  const disconnect = jest.fn()
  Object.assign(backend, { op: operation => operation, central: { disconnect },
    connectionsById: new Map([['retired', record], ['surviving', survivor]]) })
  await expect(backend.disconnectConnectionCurrent(record)).resolves.toEqual({ state: 'released', failures: [] })
  expect(disconnect).not.toHaveBeenCalled()
})

test('a newer live native generation does not erase old terminal cleanup debt', async () => {
  const backend = Object.create(DesktopRustCoreBackend.prototype)
  const record = { nativePeerId: 'peer', lease: 'old-lease', coreGeneration: 'old-generation',
    state: 'disconnected', acquiredClosers: new Set() }
  const newer = { ...record, lease: 'new-lease', coreGeneration: 'new-generation', state: 'connected' }
  const disconnect = jest.fn().mockRejectedValue(contractError('operation.timed-out', 'cleanup', 'native.discovery-retirement'))
  Object.assign(backend, { op: operation => operation, central: { disconnect },
    connectionsById: new Map([['old', record], ['new', newer]]) })
  await expect(backend.disconnectConnectionCurrent(record)).resolves.toMatchObject({ state: 'release-failed' })
  expect(disconnect).toHaveBeenCalledWith({ peerId: 'peer', lease: 'old-lease' })
})
