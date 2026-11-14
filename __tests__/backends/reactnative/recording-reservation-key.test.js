const { initializeReactNativeReservationKeys, nextReactNativeReservationKey } = require('../../../src/backends/reactnative/react-native-reservation-key')
const keys = ['incarnation', 'incarnation-flight', 'sequence'].map(suffix => Symbol.for(`unified-ble-manager.react-native.reservation-${suffix}.v2`))
const reset = () => keys.forEach(key => Reflect.deleteProperty(globalThis, key))
beforeEach(reset)
afterEach(reset)

test('independent runtime incarnations allocate different caller identities', async () => {
  await initializeReactNativeReservationKeys(async length => new Uint8Array(length).fill(1))
  const first = [nextReactNativeReservationKey(), nextReactNativeReservationKey()]
  reset() // A separate runtime has its own global object.
  await initializeReactNativeReservationKeys(async length => new Uint8Array(length).fill(2))
  const second = [nextReactNativeReservationKey(), nextReactNativeReservationKey()]
  expect(new Set([...first, ...second]).size).toBe(4)
})

test('concurrent initialization shares entropy and allocation fails closed without it', async () => {
  expect(() => nextReactNativeReservationKey()).toThrow()
  const random = jest.fn(async length => new Uint8Array(length).fill(3))
  await Promise.all([initializeReactNativeReservationKeys(random), initializeReactNativeReservationKeys(random)])
  expect(random).toHaveBeenCalledTimes(1)
  expect(nextReactNativeReservationKey()).not.toEqual(nextReactNativeReservationKey())
  reset()
  await expect(initializeReactNativeReservationKeys(async () => new Uint8Array(0))).rejects.toThrow()
  expect(() => nextReactNativeReservationKey()).toThrow()
})

test('constructing a start handle does not require entropy before capability negotiation', () => {
  const { createReactNativeOrderedRecordingStart } = require('../../../src/backends/reactnative/react-native-ordered-recording-start')
  expect(() => createReactNativeOrderedRecordingStart({})).not.toThrow()
})
