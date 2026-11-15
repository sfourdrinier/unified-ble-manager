const { ElectronRendererBleClient } = require('../../src/electron-renderer')
const { requiredTerminalError } = require('../../src/ipc/manager')
const { toPublicCleanupRecord } = require('../../src/public/cleanup')

const entries = [
  { key: 'originalMetadataStart', value: { kind: 'int', value: '-9223372036854775808' } },
  { key: 'originalMetadataStart', value: { kind: 'text', value: 'é' } },
  { key: '__proto__', value: { kind: 'bool', value: true } },
  { key: 'originalMetadataStart', value: { kind: 'int', value: '9223372036854775807' } }
]
const failure = (operation, metadataEntries) => ({
  code: 'platform.failure', domain: 'gatt', operation,
  platform: { domain: 'native', code: 'failure', safeMessage: '', metadata: { originalMetadataStart: 'legacy' }, metadataEntries },
  retryability: 'never'
})

test('Electron renderer rehydrates a failure response with ordered typed entries', async () => {
  const transport = {
    invoke: async () => ({ kind: 'failure', error: failure('bootstrap', entries) }),
    acknowledge: async () => {},
    subscribe: () => () => {}
  }
  const client = new ElectronRendererBleClient(transport)
  const error = await client.initialize().catch(error => error)
  expect(error.normalized.platform.metadataEntries).toEqual(entries)
})

test('IPC terminal and separate public cleanup causes retain independent snapshots', () => {
  const first = requiredTerminalError(failure('first', entries), 'test')
  const second = requiredTerminalError(failure('second', []), 'test')
  const result = toPublicCleanupRecord({ state: 'release-failed', failures: [
    { resourceKind: 'first', error: first }, { resourceKind: 'second', error: second }
  ] })
  expect(result.failures.map(item => item.error.operation)).toEqual(['first', 'second'])
  expect(result.failures[0].error.platform.metadataEntries).toEqual(entries)
  expect(result.failures[1].error.platform.metadataEntries).toEqual([])
  expect(Object.isFrozen(result.failures[0].error.platform.metadataEntries)).toBe(true)
})

test('IPC terminal rejects malformed integer entries and preserves absence', () => {
  expect(() => requiredTerminalError(failure('bad', [{ key: 'x', value: { kind: 'int', value: '-0' } }]), 'test')).toThrow()
  const old = failure('old', [])
  delete old.platform.metadataEntries
  expect(requiredTerminalError(old, 'test').platform).not.toHaveProperty('metadataEntries')
})
