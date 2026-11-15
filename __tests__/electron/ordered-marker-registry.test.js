// __tests__/electron/ordered-marker-registry.test.js

const { ElectronOrderedMarkerRegistry } = require('../../src/electron/ordered-marker-registry')

test('marker transaction retains credit until source marker acknowledgement and forget', async () => {
  const registry = new ElectronOrderedMarkerRegistry()
  const status = { state: 'admitted', ordinal: '18446744073709551615', write: { state: 'settled', error: null } }
  const handle = {
    start: jest.fn(async () => status),
    status: jest.fn(async () => status),
    cancel: jest.fn(async () => status),
    forget: jest.fn(async () => {})
  }
  const target = { characteristicUuid: '2a37' }
  const id = registry.reserve('group', 'token').markerHandle
  expect(registry.reserve('group', 'token').markerHandle).toBe(id)
  const start = () =>
    registry.start('group', id, { target: 'same' }, () => handle, target, Uint8Array.of(1), true, {
      signal: undefined,
      deadline: null
    })
  await expect(start()).resolves.toMatchObject({ statusFormat: 'public', status })
  expect(() => registry.reserve('group', 'other')).toThrow()
  await expect(registry.forget('group', id, () => false)).rejects.toMatchObject({
    normalized: { code: 'lifecycle.invalid-state' }
  })
  expect(handle.forget).not.toHaveBeenCalled()
  await expect(registry.forget('group', id, ordinal => ordinal === status.ordinal)).resolves.toEqual({
    forgotten: true
  })
  await expect(registry.forget('group', id)).resolves.toEqual({ forgotten: true })
  expect(handle.forget).toHaveBeenCalledTimes(1)
})

test('canceling a reservation before start never creates a native marker owner', async () => {
  const registry = new ElectronOrderedMarkerRegistry()
  const id = registry.reserve('group', 'cancel-token').markerHandle
  const create = jest.fn()
  await expect(registry.status('group', id, true)).resolves.toMatchObject({
    status: { state: 'not-admitted', error: null }
  })
  await expect(
    registry.start('group', id, {}, create, {}, Uint8Array.of(1), true, { signal: undefined, deadline: null })
  ).resolves.toMatchObject({ status: { state: 'not-admitted' } })
  expect(create).not.toHaveBeenCalled()
  await expect(registry.forget('group', id)).resolves.toEqual({ forgotten: true })
})

test('renderer revoke releases only unstarted reservations from its lease', async () => {
  const registry = new ElectronOrderedMarkerRegistry()
  const unused = registry.reserve('unused', 'one', 'old').markerHandle
  const retained = registry.reserve('retained', 'two', 'new').markerHandle
  registry.retireUnusedForLease('old')
  await expect(registry.status('unused', unused, false)).rejects.toMatchObject({
    normalized: { code: 'ownership.denied' }
  })
  await expect(registry.status('retained', retained, false)).resolves.toMatchObject({ status: { state: 'idle' } })
})

test('exact startup retry reaches the retained native handle after a lost reservation reply', async () => {
  const { createOrderedMarkerReservations } = require('../../src/core/ordered-marker-handle')
  const registry = new ElectronOrderedMarkerRegistry()
  const reservationFailure = new Error('lost reservation response')
  const status = { state: 'admitted', ordinal: '7', write: { state: 'settled', error: null } }
  const transport = {
    reserve: jest.fn().mockRejectedValueOnce(reservationFailure).mockResolvedValue('0'),
    start: jest.fn(async () => status),
    status: jest.fn(async () => status),
    cancel: jest.fn(async () => ({ state: 'not-admitted', error: null })),
    forget: jest.fn(async () => {})
  }
  const handle = createOrderedMarkerReservations(
    transport,
    target => target,
    () => 0,
    () => 'same-key'
  )()
  const create = jest.fn(() => handle)
  const target = { characteristicUuid: '2a37' }
  const options = { signal: null, deadline: null }
  const id = registry.reserve('group', 'retry-token').markerHandle
  const start = fingerprint => registry.start('group', id, fingerprint, create, target, Uint8Array.of(1), true, options)
  await expect(start({ value: 'original' })).rejects.toBe(reservationFailure)
  await expect(start({ value: 'changed' })).rejects.toMatchObject({ normalized: { code: 'argument.invalid' } })
  await expect(start({ value: 'original' })).resolves.toMatchObject({ status })
  await expect(start({ value: 'original' })).resolves.toMatchObject({ status })
  expect(create).toHaveBeenCalledTimes(1)
  expect(transport.reserve.mock.calls).toEqual([['same-key'], ['same-key']])
  expect(transport.start).toHaveBeenCalledTimes(1)
  await registry.forget('group', id)
})
