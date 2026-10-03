const {
  createReactNativeAccessoryChooser
} = require('../../../src/backends/reactnative/react-native-accessory-chooser')

const selected = JSON.stringify({
  revision: 'ubm-accessory-chooser/1',
  peripheralIdentifier: '12345678-1234-1234-1234-123456789abc',
  name: 'Sensor'
})
const options = { filters: [{ serviceUuids: ['180d'], localNamePrefix: 'Sensor' }] }
function fixture() {
  let time = 0
  const binding = {
    randomBytes: jest.fn(async () => new Uint8Array(16).fill(7)),
    chooseAccessory: jest.fn(async () => selected),
    cancelAccessoryChoice: jest.fn(async () => {})
  }
  const choose = createReactNativeAccessoryChooser(
    binding,
    id => `scoped:${id}`,
    () => time
  )
  return {
    binding,
    choose,
    advance: value => {
      time = value
    }
  }
}

test('system setup returns an attachment-bound authorized peer, never fabricated scan/restoration evidence', async () => {
  const { choose, binding } = fixture()
  const peer = await choose(options)
  expect(peer).toMatchObject({
    id: 'scoped:12345678-1234-1234-1234-123456789abc',
    name: 'Sensor',
    reference: null,
    sources: ['origin-authorized'],
    lastAdvertisement: null
  })
  expect(JSON.parse(binding.chooseAccessory.mock.calls[0][1])).toEqual({
    revision: 'ubm-accessory-chooser/1',
    filters: [{ serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', namePrefix: 'Sensor' }]
  })
})

test.each([
  { acceptAllDevices: true },
  { filters: [{ serviceUuids: ['180d', '180f'], localNamePrefix: 'Sensor' }] },
  { filters: [{ serviceUuids: ['180d'] }] },
  { filters: [] }
])('unsupported filters cannot silently widen setup: %j', async input => {
  const { choose, binding } = fixture()
  await expect(choose(input)).rejects.toMatchObject({ normalized: { code: 'capability.unsupported' } })
  expect(binding.chooseAccessory).not.toHaveBeenCalled()
})

test('pre-abort and elapsed entropy acquisition budget prevent native allocation', async () => {
  const first = fixture()
  const abort = new AbortController()
  abort.abort()
  await expect(first.choose({ ...options, signal: abort.signal })).rejects.toMatchObject({
    normalized: { code: 'operation.aborted' }
  })
  expect(first.binding.randomBytes).not.toHaveBeenCalled()
  const second = fixture()
  second.binding.randomBytes.mockImplementation(async () => {
    second.advance(101)
    return new Uint8Array(16)
  })
  await expect(second.choose({ ...options, timeoutMs: 100 })).rejects.toMatchObject({
    normalized: { code: 'operation.timed-out' }
  })
  expect(second.binding.chooseAccessory).not.toHaveBeenCalled()
})

test('cancellation reaches native owner once, late setup cannot publish a peer or erase OS authorization', async () => {
  const { choose, binding } = fixture()
  let finish
  binding.chooseAccessory.mockImplementation(
    () =>
      new Promise(resolve => {
        finish = resolve
      })
  )
  const abort = new AbortController()
  const pending = choose({ ...options, signal: abort.signal })
  while (!finish) await Promise.resolve()
  abort.abort()
  finish(selected)
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  expect(binding.cancelAccessoryChoice).toHaveBeenCalledTimes(1)
})

test.each([
  '{}',
  '{"revision":"old"}',
  JSON.stringify({ revision: 'ubm-accessory-chooser/1', peripheralIdentifier: 'bad', name: 'Sensor' })
])('malformed native setup result is rejected: %s', async result => {
  const { choose, binding } = fixture()
  binding.chooseAccessory.mockResolvedValue(result)
  await expect(choose(options)).rejects.toMatchObject({ normalized: { code: 'protocol.malformed' } })
})
