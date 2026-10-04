const { assertPublicChooseOptions } = require('../../../src/public/ble-manager')
const { nativeChooserFilters } = require('../../../src/backends/reactnative/react-native-native-chooser-filters')
const {
  createReactNativeAccessoryChooser
} = require('../../../src/backends/reactnative/react-native-accessory-chooser')
const {
  createReactNativeCompanionChooser
} = require('../../../src/backends/reactnative/react-native-companion-chooser')

const service = { serviceUuids: ['180d'] }
const name = { localNamePrefix: 'Sensor' }
const manufacturer = { manufacturerData: [{ companyIdentifier: 1, dataPrefix: new Uint8Array([7]) }] }
const constraints = [
  [service],
  [name],
  [manufacturer],
  [{ ...service, ...name, ...manufacturer }],
  [service, name, manufacturer]
]

test.each(constraints.map(filters => [filters]))(
  'shared selection policy rejects accept-all plus constraints: %j',
  filters => {
    expect(() => assertPublicChooseOptions({ acceptAllDevices: true, filters })).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'scan.filter-invalid' }) })
    )
  }
)

test.each(['apple', 'android'])('%s refuses conflicting filters before native resource allocation', async platform => {
  const binding = {
    randomBytes: jest.fn(async () => new Uint8Array(16)),
    chooseAccessory: jest.fn(async () =>
      JSON.stringify({
        revision: 'ubm-accessory-chooser/1',
        peripheralIdentifier: '12345678-1234-1234-1234-123456789abc',
        name: null
      })
    ),
    cancelAccessoryChoice: jest.fn(async () => {})
  }
  const services = { associateCompanion: jest.fn(async () => ({ peerId: 'AA:BB:CC:DD:EE:FF' })) }
  const scoped = jest.fn(id => `scoped:${id}`)
  const choose =
    platform === 'apple'
      ? createReactNativeAccessoryChooser(binding, scoped, () => 0)
      : createReactNativeCompanionChooser(services, scoped, () => 0)
  for (const filters of constraints) {
    await expect(choose({ acceptAllDevices: true, filters })).rejects.toMatchObject({
      normalized: { code: 'scan.filter-invalid' }
    })
  }
  expect(binding.randomBytes).not.toHaveBeenCalled()
  expect(binding.chooseAccessory).not.toHaveBeenCalled()
  expect(services.associateCompanion).not.toHaveBeenCalled()
  expect(scoped).not.toHaveBeenCalled()
})

test.each([{}, { filters: [] }])('accept-all retains empty/absent filter compatibility: %j', options => {
  expect(() => assertPublicChooseOptions({ ...options, acceptAllDevices: true })).not.toThrow()
  expect(nativeChooserFilters({ ...options, acceptAllDevices: true }, 'android')).toEqual([{}])
  expect(() => nativeChooserFilters({ ...options, acceptAllDevices: true }, 'apple')).toThrow(
    expect.objectContaining({ normalized: expect.objectContaining({ code: 'capability.unsupported' }) })
  )
})

test.each([{}, { filters: [] }])('default selection admits Android all-device capability: %j', options => {
  expect(nativeChooserFilters(options, 'android')).toEqual([{}])
  expect(() => nativeChooserFilters(options, 'apple')).toThrow(
    expect.objectContaining({ normalized: expect.objectContaining({ code: 'capability.unsupported' }) })
  )
  expect(() => assertPublicChooseOptions({ ...options, acceptAllDevices: false })).toThrow(
    expect.objectContaining({ normalized: expect.objectContaining({ code: 'scan.filter-invalid' }) })
  )
})

test.each([{}, { filters: [] }, { acceptAllDevices: true }])(
  'Android default/explicit all-device selection reaches the existing association owner: %j',
  async options => {
    const services = { associateCompanion: jest.fn(async () => ({ peerId: 'AA:BB:CC:DD:EE:FF' })) }
    const choose = createReactNativeCompanionChooser(
      services,
      id => `scoped:${id}`,
      () => 0
    )
    expect((await choose(options)).id).toBe('scoped:AA:BB:CC:DD:EE:FF')
    expect(JSON.parse(services.associateCompanion.mock.calls[0][0].filtersJson)).toEqual([{}])
  }
)

test.each(['apple', 'android'])('%s preserves conjunction and OR filtering', platform => {
  const filters = [
    { ...service, ...name, ...manufacturer },
    { ...service, localNamePrefix: 'Other' }
  ]
  expect(nativeChooserFilters({ filters, acceptAllDevices: false }, platform)).toEqual([
    {
      serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
      namePrefix: 'Sensor',
      companyIdentifier: 1,
      manufacturerPrefix: [7]
    },
    { serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb', namePrefix: 'Other' }
  ])
})
