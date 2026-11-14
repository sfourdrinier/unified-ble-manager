// __tests__/electron/ordered-recording-registry.test.js

const { ElectronOrderedRecordingRegistry } = require('../../src/electron/ordered-recording-registry')

function active(id) {
  return { kind: 'started', receipt: { recordingId: id, state: 'active' } }
}

test('retains started receipt across revocation and isolates foreign windows', async () => {
  const registry = new ElectronOrderedRecordingRegistry(4)
  const id = registry.reserve('app-window', 'lease-old', 'token').startHandle
  expect(registry.reserve('app-window', 'lease-old', 'token').startHandle).toBe(id)
  const unused = registry.reserve('app-window', 'lease-old', 'unused').startHandle
  let settle
  const result = new Promise(resolve => {
    settle = resolve
  })
  const handle = {
    start: jest.fn(() => result),
    status: jest.fn(async () => ({ state: 'pending' })),
    retire: jest.fn(async () => {})
  }
  const start = registry.start(
    'app-window',
    id,
    'recording',
    'peer',
    { request: 'same' },
    () => handle,
    owner => owner.start()
  )
  const retry = registry.start(
    'app-window',
    id,
    'recording',
    'peer',
    { request: 'same' },
    () => handle,
    owner => owner.start()
  )
  expect(await registry.status('app-window', id)).toEqual({ state: 'pending' })
  expect(() => registry.reserve('app-window', 'lease-new', '')).toThrow()
  await expect(registry.status('other-window', id)).rejects.toMatchObject({ normalized: { code: 'ownership.denied' } })
  registry.retireUnused('app-window', 'lease-old')
  expect(await registry.status('app-window', unused)).toEqual({ state: 'unknown' })
  settle(active('recording'))
  expect(await start).toMatchObject({ kind: 'started' })
  expect(await retry).toMatchObject({ kind: 'started' })
  expect(handle.start).toHaveBeenCalledTimes(1)
  expect(await registry.status('app-window', id)).toMatchObject({ state: 'result' })
  expect(registry.list('app-window', null, 1)).toMatchObject({
    starts: [{ startHandle: id, recordingId: 'recording', state: 'result' }],
    truncated: false
  })
  await registry.retire('app-window', id)
  expect(await registry.status('app-window', id)).toEqual({ state: 'retired' })
  expect(registry.list('app-window', null, 1)).toMatchObject({ starts: [{ startHandle: id, state: 'retired' }] })
  registry.confirmFinished('recording')
  expect(await registry.status('app-window', id)).toEqual({ state: 'unknown' })
})

test('cursor pagination probes one extra started owner', async () => {
  const registry = new ElectronOrderedRecordingRegistry(4)
  for (let index = 0; index < 3; index += 1) {
    const id = registry.reserve('owner', 'lease', `token-${index}`).startHandle
    const handle = {
      start: async () => active(`r${index}`),
      status: async () => ({ state: 'unknown' }),
      retire: async () => {}
    }
    await registry.start(
      'owner',
      id,
      `r${index}`,
      'peer',
      { index },
      () => handle,
      owner => owner.start()
    )
  }
  const first = registry.list('owner', null, 2)
  expect(first).toMatchObject({
    truncated: true,
    nextCursor: '2',
    starts: [{ startHandle: '1' }, { startHandle: '2' }]
  })
  expect(registry.list('owner', first.nextCursor, 2)).toMatchObject({
    truncated: false,
    nextCursor: null,
    starts: [{ startHandle: '3' }]
  })
})

test('authoritative stop before receipt retirement is remembered until retirement', async () => {
  const registry = new ElectronOrderedRecordingRegistry(2)
  const id = registry.reserve('owner', 'lease', 'token').startHandle
  const handle = { start: async () => active('r'), status: async () => ({ state: 'unknown' }), retire: async () => {} }
  await registry.start(
    'owner',
    id,
    'r',
    'peer',
    { id: 'r' },
    () => handle,
    owner => owner.start()
  )
  registry.confirmFinished('r')
  expect(await registry.status('owner', id)).toMatchObject({ state: 'result' })
  await registry.retire('owner', id)
  expect(await registry.status('owner', id)).toEqual({ state: 'unknown' })
})

test('finished retired recording stays recoverable while marker acknowledgement is pending', async () => {
  const registry = new ElectronOrderedRecordingRegistry(2)
  const id = registry.reserve('owner', 'lease', 'token').startHandle
  const handle = { start: async () => active('r'), status: async () => ({ state: 'unknown' }), retire: async () => {} }
  await registry.start('owner', id, 'r', 'peer', { id: 'r' }, () => handle, owner => owner.start())
  await registry.retire('owner', id, false)
  registry.confirmFinished('r', () => false)
  expect(registry.has('owner', id)).toBe(true)
  await registry.retire('owner', id, true)
  expect(registry.has('owner', id)).toBe(false)
})
