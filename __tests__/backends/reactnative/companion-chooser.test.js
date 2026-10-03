const {
  createReactNativeCompanionChooser
} = require('../../../src/backends/reactnative/react-native-companion-chooser')
function fixture() {
  const services = {
    associateCompanion: jest.fn(async () => ({
      source: 'associated',
      associationId: 1,
      peerId: 'AA:BB:CC:DD:EE:FF',
      displayName: 'Association label'
    }))
  }
  return {
    services,
    choose: createReactNativeCompanionChooser(
      services,
      id => `scoped:${id}`,
      () => 100
    )
  }
}
test('Android generic chooser faithfully forwards OR/prefix/service/manufacturer to existing association owner', async () => {
  const { choose, services } = fixture()
  const peer = await choose({
    filters: [
      { serviceUuids: ['180d'], localNamePrefix: 'Polar.+' },
      { manufacturerData: [{ companyIdentifier: 107, dataPrefix: new Uint8Array([0, 255]) }] }
    ],
    timeoutMs: 200
  })
  const request = services.associateCompanion.mock.calls[0][0]
  expect(request.deadline).toBe(300)
  expect(JSON.parse(request.filtersJson)).toEqual([
    { serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', namePrefix: 'Polar.+' },
    { companyIdentifier: 107, manufacturerPrefix: [0, 255] }
  ])
  expect(peer).toMatchObject({
    id: 'scoped:AA:BB:CC:DD:EE:FF',
    name: null,
    rssi: null,
    reference: null,
    lastAdvertisement: null
  })
})
test('accept-all requests use an explicit empty LE filter, never arbitrary single-device selection', async () => {
  const { choose, services } = fixture()
  await choose({ acceptAllDevices: true })
  expect(JSON.parse(services.associateCompanion.mock.calls[0][0].filtersJson)).toEqual([{}])
})
test('association without a Bluetooth identifier is not fabricated into a peer', async () => {
  const { choose, services } = fixture()
  services.associateCompanion.mockResolvedValue({ peerId: null })
  await expect(choose({ acceptAllDevices: true })).rejects.toMatchObject({
    normalized: { code: 'chooser.permitted-device-unavailable' }
  })
})

test('a held association is abort-aware and remains observed after late rejection', async () => {
  const { choose, services } = fixture()
  let fail
  services.associateCompanion.mockImplementation(() => new Promise((_, reject) => { fail = reject }))
  const abort = new AbortController()
  const pending = choose({ acceptAllDevices: true, signal: abort.signal })
  abort.abort()
  await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  fail(new Error('late native refusal'))
  await Promise.resolve()
})
