const {
  IPC_PROTOCOL_VERSION,
  IPC_PROTOCOL_MINIMUM_VERSION,
  IPC_CLIENT_COMPATIBILITY_OFFER
} = require('../../src/ipc/protocol')
const { setupReactNativeOrdered } = require('../../src/backends/reactnative/react-native-ordered-setup')

test('desktop IPC negotiates ordinary BLE from v6 through the current revision', () => {
  expect(IPC_PROTOCOL_VERSION).toBe(11)
  expect(IPC_PROTOCOL_MINIMUM_VERSION).toBe(6)
  expect(IPC_CLIENT_COMPATIBILITY_OFFER.ipcProtocol.minimum.value).toBe(6)
  expect(IPC_CLIENT_COMPATIBILITY_OFFER.ipcProtocol.maximum.value).toBe(11)
})

test('mobile rejects the previous ordered wire before reserving a group', async () => {
  const invoke = jest.fn(async () => ({ wireVersion: 1 }))
  await expect(setupReactNativeOrdered({ invoke, members: [] })).rejects.toMatchObject({ normalized: { code: 'protocol.incompatible' } })
  expect(invoke.mock.calls.map(call => call[0])).toEqual(['gatt.ordered.version'])
})
