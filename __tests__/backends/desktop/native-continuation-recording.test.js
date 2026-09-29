const { openNativeContinuationRecordings } = require('../../../src/backends/desktop/native-continuation-recording')

test('store opening transport rejection is a public typed platform failure', async () => {
  await expect(openNativeContinuationRecordings({ openRecordingStore: async () => { throw new Error('worker rejected') } }, '/private/app'))
    .rejects.toMatchObject({ code: 'platform.failure', operation: 'continuation.recording.open', platform: { domain: 'ubm-desktop' } })
})

test('opens durable recording access without touching radio admission or enumeration', async () => {
  const calls = []
  const binding = {
    async openRecordingStore(directory) {
      calls.push(directory)
      return {
        async prepare(id) {
          return JSON.stringify({ ok: true, value: { token: null, records: [], bytes: 0, more: false } })
        }
      }
    },
    openProduction: jest.fn(() => {
      throw new Error('radio unavailable')
    }),
    listAdapters: jest.fn(() => {
      throw new Error('Bluetooth denied')
    })
  }
  const recordings = await openNativeContinuationRecordings(binding, '/private/app/recordings')
  expect(await recordings.prepare('recording-1', { maxItems: 10, maxBytes: 4096 })).toEqual({
    token: null,
    records: [],
    bytes: 0,
    more: false
  })
  expect(calls).toEqual(['/private/app/recordings'])
  expect(binding.openProduction).not.toHaveBeenCalled()
  expect(binding.listAdapters).not.toHaveBeenCalled()
})

test('missing recording support reports unsupported rather than an empty recording', async () => {
  await expect(openNativeContinuationRecordings({}, '/private/app/recordings')).rejects.toMatchObject({
    code: 'capability.unsupported'
  })
})
