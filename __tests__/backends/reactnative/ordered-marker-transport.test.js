// __tests__/backends/reactnative/ordered-marker-transport.test.js
const {
  createReactNativeOrderedMarkerReservations
} = require('../../../src/backends/reactnative/react-native-ordered-marker')

test('retains reservation identity and exact request across lost start reply', async () => {
  const calls = []
  let first = true
  const invoke = jest.fn(async (op, args) => {
    calls.push([op, args])
    if (op === 'gatt.ordered.marker.reserve') return { handle: '18446744073709551615' }
    if (op === 'gatt.ordered.marker.start') {
      if (first) {
        first = false
        throw new Error('lost reply')
      }
      return { state: 'pending' }
    }
    if (op === 'gatt.ordered.marker.status')
      return { state: 'admitted', ordinal: '7', write: { state: 'settled', error: null } }
    return null
  })
  const target = { id: 'target' }
  const reserve = createReactNativeOrderedMarkerReservations(
    invoke,
    'peer',
    '3',
    () => ({ serviceUuid: 's', characteristicUuid: 'c' }),
    () => 0,
    () => 'request-key'
  )
  const marker = reserve()
  const options = { signal: null, deadline: null }
  await expect(marker.start(target, Uint8Array.from([1, 2]), 'with-response', options)).rejects.toThrow('lost reply')
  expect(await marker.status()).toMatchObject({ state: 'admitted', ordinal: '7' })
  expect(await marker.start(target, Uint8Array.from([1, 2]), 'with-response', options)).toEqual({ state: 'pending' })
  await expect(marker.start(target, Uint8Array.from([2]), 'with-response', options)).rejects.toThrow()
  expect(calls.filter(([op]) => op === 'gatt.ordered.marker.reserve')).toHaveLength(1)
  expect(calls.filter(([op]) => op === 'gatt.ordered.marker.start').map(([, args]) => args)).toEqual([
    {
      peerId: 'peer',
      collectionId: '3',
      handle: '18446744073709551615',
      selector: { serviceUuid: 's', characteristicUuid: 'c' },
      valueB64: 'AQI=',
      mode: 'with-response'
    },
    {
      peerId: 'peer',
      collectionId: '3',
      handle: '18446744073709551615',
      selector: { serviceUuid: 's', characteristicUuid: 'c' },
      valueB64: 'AQI=',
      mode: 'with-response'
    }
  ])
  await marker.forget()
})

test('deadline bounds a pending native start and requests cancellation of the retained handle', async () => {
  jest.useFakeTimers()
  try {
    const invoke = jest.fn(op => {
      if (op === 'gatt.ordered.marker.reserve') return Promise.resolve({ handle: '8' })
      if (op === 'gatt.ordered.marker.start' || op === 'gatt.ordered.marker.cancel') return new Promise(() => {})
      return Promise.resolve(null)
    })
    const marker = createReactNativeOrderedMarkerReservations(
      invoke,
      'peer',
      '3',
      () => ({ serviceUuid: 's', characteristicUuid: 'c' }),
      () => 0,
      () => 'key'
    )()
    const pending = marker.start({}, Uint8Array.from([1]), 'with-response', { signal: null, deadline: 10 })
    for (let i = 0; i < 6; i++) await Promise.resolve()
    const rejected = expect(pending).rejects.toThrow('operation.timed-out')
    jest.advanceTimersByTime(10)
    await rejected
    expect(invoke).toHaveBeenCalledWith('gatt.ordered.marker.cancel', expect.objectContaining({ handle: '8' }))
  } finally {
    jest.useRealTimers()
  }
})

test('abort settles a stalled native start with its cancellation result', async () => {
  const actual = { state: 'not-admitted', error: null }
  const invoke = jest.fn(op => {
    if (op === 'gatt.ordered.marker.reserve') return Promise.resolve({ handle: '8' })
    if (op === 'gatt.ordered.marker.start') return new Promise(() => {})
    if (op === 'gatt.ordered.marker.cancel' || op === 'gatt.ordered.marker.status') return Promise.resolve(actual)
    return Promise.resolve(null)
  })
  const marker = createReactNativeOrderedMarkerReservations(
    invoke,
    'peer',
    '3',
    () => ({ serviceUuid: 's', characteristicUuid: 'c' }),
    () => 0,
    () => 'key'
  )()
  const controller = new AbortController()
  const pending = marker.start({}, Uint8Array.from([1]), 'with-response', { signal: controller.signal, deadline: null })
  for (let i = 0; i < 6; i++) await Promise.resolve()
  controller.abort()
  expect(await pending).toEqual(actual)
  expect(await marker.status()).toEqual(actual)
  await marker.forget()
})
