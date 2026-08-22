jest.mock('../src/react-native', () => ({
  createReactNativeBleManager: jest.fn(),
  createReactNativeBleManagerWithEnvironment: jest.fn()
}))

const { contractError } = require('../src/backend-contract/errors')
const { BleError } = require('../src/public/errors')
const { createReactNativeBleManager } = require('../src/react-native')
const { createExpoBleManager } = require('../src/expo')

describe('Expo factory', () => {
  test('rehydrates asynchronous React Native factory failures as public errors', async () => {
    createReactNativeBleManager.mockRejectedValue(
      contractError('adapter.unavailable', 'adapter', 'react-native-manager.adapter')
    )

    await expect(createExpoBleManager()).rejects.toMatchObject({
      constructor: BleError,
      code: 'adapter.unavailable',
      domain: 'adapter',
      operation: 'react-native-manager.adapter'
    })
  })
})
