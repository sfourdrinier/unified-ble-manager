const { acquireWebRecordingLock } = require('../../src/web/web-recording-lock')

function locks() {
  let owner = false
  const names = []
  return {
    names,
    request(name, options, callback) {
      names.push(name)
      expect(options).toEqual({ mode: 'exclusive', ifAvailable: true })
      if (owner) return Promise.resolve(callback(null))
      owner = true
      return Promise.resolve(callback({})).finally(() => {
        owner = false
      })
    }
  }
}

test('holds exclusive authority until explicit release without revealing the recording ID', async () => {
  const authority = locks()
  const hash = async () => 'ab'.repeat(32)
  const first = await acquireWebRecordingLock(authority, hash, 'private-id')
  expect(first.state).toBe('acquired')
  expect(authority.names[0]).toBe(`ubm-ordered-recording:${'ab'.repeat(32)}`)
  expect(authority.names[0]).not.toContain('private-id')
  expect(await acquireWebRecordingLock(authority, hash, 'private-id')).toEqual({ state: 'busy' })
  await first.release()
  await first.release()
  const next = await acquireWebRecordingLock(authority, hash, 'private-id')
  expect(next.state).toBe('acquired')
  await next.release()
})

test('fails closed if lock acquisition or hashing fails', async () => {
  const authority = { request: jest.fn(() => Promise.reject(new Error('lock denied'))) }
  await expect(acquireWebRecordingLock(authority, async () => 'ab'.repeat(32), 'id')).rejects.toMatchObject({
    normalized: { code: 'capability.unsupported' }
  })
  await expect(
    acquireWebRecordingLock(
      authority,
      async () => {
        throw new Error('no crypto')
      },
      'id'
    )
  ).rejects.toThrow('no crypto')
  await expect(acquireWebRecordingLock(authority, async () => 'short', 'id')).rejects.toThrow()
  expect(authority.request).toHaveBeenCalledTimes(1)
})

test('exposes unexpected authority loss to its owner', async () => {
  let fail
  const manager = {
    request: (_name, _options, callback) => {
      callback({})
      return new Promise((_resolve, reject) => {
        fail = reject
      })
    }
  }
  const lease = await acquireWebRecordingLock(manager, async () => 'ab'.repeat(32), 'id')
  const failure = new Error('authority lost')
  fail(failure)
  expect(await lease.ended).toEqual({ state: 'lost', error: failure })
  await expect(lease.release()).rejects.toThrow('authority lost')
})

test('aborting a delayed request settles immediately and compensates a late grant', async () => {
  let grant,
    finished = false
  const manager = {
    request: (_name, _opts, callback) =>
      new Promise(resolve => {
        grant = () =>
          Promise.resolve(callback({})).then(() => {
            finished = true
            resolve()
          })
      })
  }
  const controller = new AbortController()
  const pending = acquireWebRecordingLock(manager, async () => 'ab'.repeat(32), 'id', undefined, controller.signal)
  while (!grant) await Promise.resolve()
  controller.abort()
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  grant()
  await Promise.resolve()
  await Promise.resolve()
  expect(finished).toBe(true)
})

test('lock request failure is a normalized capability error', async () => {
  const manager = { request: () => Promise.reject(new Error('denied')) }
  await expect(acquireWebRecordingLock(manager, async () => 'ab'.repeat(32), 'id')).rejects.toMatchObject({
    normalized: { code: 'capability.unsupported', platform: { domain: 'Web Locks' } }
  })
})
