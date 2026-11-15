// __tests__/backends/desktop/ordered-marker-handle.test.js
const { createOrderedMarkerReservations } = require('../../../src/core/ordered-marker-handle')
const controls = { signal: null, deadline: null }
test('lost reservation response retries same key and never duplicates the write', async () => {
  const keys = []
  let attempts = 0
  const transport = {
    reserve: jest.fn(async key => {
      keys.push(key)
      if (++attempts === 1) throw new Error('lost reserve response')
      return '0'
    }),
    start: jest.fn(async () => ({ state: 'pending' })),
    status: jest.fn(async () => ({ state: 'admitted', ordinal: '4', write: { state: 'settled', error: null } })),
    cancel: jest.fn(async () => ({ state: 'not-admitted', error: null })),
    forget: jest.fn(async () => {})
  }
  const handle = createOrderedMarkerReservations(
    transport,
    target => target,
    () => 0,
    () => 'unique-key'
  )()
  const target = {}
  await expect(handle.start(target, new Uint8Array([1]), 'with-response', controls)).rejects.toThrow(
    'lost reserve response'
  )
  expect(await handle.start(target, new Uint8Array([1]), 'with-response', controls)).toEqual({ state: 'pending' })
  expect(keys).toEqual(['unique-key', 'unique-key'])
  expect(transport.start).toHaveBeenCalledTimes(1)
  expect((await handle.status()).state).toBe('admitted')
  await handle.forget()
  await expect(handle.start(target, new Uint8Array([1]), 'with-response', controls)).rejects.toThrow(
    'lifecycle.invalid-state'
  )
})
test('idle cancellation consumes no native ID and is retained through retry', async () => {
  const transport = { reserve: jest.fn(), start: jest.fn(), status: jest.fn(), cancel: jest.fn(), forget: jest.fn() }
  const handle = createOrderedMarkerReservations(
    transport,
    target => target,
    () => 0,
    () => 'key'
  )()
  expect((await handle.cancel()).state).toBe('not-admitted')
  expect((await handle.start({}, new Uint8Array([1]), 'with-response', controls)).state).toBe('not-admitted')
  await handle.forget()
  expect(transport.reserve).not.toHaveBeenCalled()
  expect(transport.start).not.toHaveBeenCalled()
})
test('abort bounds a pending reservation and cancels its late id without starting', async () => {
  let finishReserve
  const reserved = new Promise(resolve => {
    finishReserve = resolve
  })
  const actual = { state: 'not-admitted', error: null }
  const transport = {
    reserve: jest.fn(() => reserved),
    start: jest.fn(),
    status: jest.fn(async () => actual),
    cancel: jest.fn(async () => actual),
    forget: jest.fn(async () => {})
  }
  const handle = createOrderedMarkerReservations(
    transport,
    target => target,
    () => 0,
    () => 'reservation-key'
  )()
  const abort = new AbortController()
  const pending = handle.start({}, Uint8Array.of(3), 'with-response', { signal: abort.signal, deadline: null })
  const rejected = expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  abort.abort()
  await rejected
  expect(transport.cancel).not.toHaveBeenCalled()
  expect(transport.reserve.mock.calls).toEqual([['reservation-key']])
  finishReserve('0')
  expect(await handle.status()).toEqual(actual)
  expect(transport.cancel).toHaveBeenCalledTimes(1)
  expect(transport.cancel).toHaveBeenCalledWith('0')
  expect(transport.start).not.toHaveBeenCalled()
  await handle.forget()
})

test('deadline expires pending reservation, retains ownership, and cancels the late id without starting', async () => {
  jest.useFakeTimers()
  try {
    let finishReserve
    let finishCancel
    const reserved = new Promise(resolve => {
      finishReserve = resolve
    })
    const cancel = new Promise(resolve => {
      finishCancel = resolve
    })
    const admitted = { state: 'admitted', ordinal: '4', write: { state: 'settled', error: null } }
    const transport = {
      reserve: jest.fn(() => reserved),
      start: jest.fn(),
      status: jest.fn(async () => admitted),
      cancel: jest.fn(() => cancel),
      forget: jest.fn(async () => {})
    }
    const make = createOrderedMarkerReservations(
      transport,
      target => target,
      () => 0,
      () => 'key'
    )
    const handle = make()
    const target = {}
    const pending = handle.start(target, Uint8Array.from([3]), 'with-response', { signal: null, deadline: 10 })
    const rejected = expect(pending).rejects.toThrow('operation.timed-out')
    jest.advanceTimersByTime(10)
    await rejected
    await expect(make().start({}, Uint8Array.from([1]), 'with-response', controls)).rejects.toThrow(
      'lifecycle.invalid-state'
    )
    finishReserve('7')
    await Promise.resolve()
    await Promise.resolve()
    await Promise.resolve()
    expect(transport.cancel).toHaveBeenCalledWith('7')
    expect(transport.start).not.toHaveBeenCalled()
    finishCancel(admitted)
    expect(await handle.status()).toEqual(admitted)
    await handle.forget()
    expect(transport.forget).toHaveBeenCalledWith('7')
  } finally {
    jest.useRealTimers()
  }
})

test('deadline expires stalled start without waiting for cancellation and retains the request', async () => {
  jest.useFakeTimers()
  try {
    let finishStart
    let finishCancel
    const firstStart = new Promise(resolve => {
      finishStart = resolve
    })
    const cancellation = new Promise(resolve => {
      finishCancel = resolve
    })
    const admitted = { state: 'admitted', ordinal: '9', write: { state: 'settled', error: null } }
    const transport = {
      reserve: jest.fn(async () => '5'),
      start: jest.fn().mockReturnValueOnce(firstStart).mockResolvedValue(admitted),
      status: jest.fn(async () => admitted),
      cancel: jest.fn(() => cancellation),
      forget: jest.fn(async () => {})
    }
    const handle = createOrderedMarkerReservations(
      transport,
      target => target,
      () => 0,
      () => 'key'
    )()
    const target = {}
    const pending = handle.start(target, Uint8Array.from([2]), 'with-response', { signal: null, deadline: 10 })
    await Promise.resolve()
    await Promise.resolve()
    await Promise.resolve()
    const rejected = expect(pending).rejects.toThrow('operation.timed-out')
    jest.advanceTimersByTime(10)
    await rejected
    expect(transport.cancel).toHaveBeenCalledWith('5')
    finishStart(admitted)
    finishCancel(admitted)
    expect(await handle.status()).toEqual(admitted)
    expect(await handle.start(target, Uint8Array.from([2]), 'with-response', controls)).toEqual(admitted)
    expect(transport.reserve).toHaveBeenCalledTimes(1)
    expect(transport.start).toHaveBeenCalledTimes(1)
    await expect(handle.start(target, Uint8Array.from([3]), 'with-response', controls)).rejects.toThrow(
      'argument.invalid'
    )
    await handle.forget()
  } finally {
    jest.useRealTimers()
  }
})

test('completed or failed starts clear the deadline without requesting cancellation', async () => {
  jest.useFakeTimers()
  try {
    for (const failure of [false, true]) {
      const transport = {
        reserve: jest.fn(async () => '1'),
        start: jest.fn(async () => {
          if (failure) throw new Error('transport failure')
          return { state: 'pending' }
        }),
        status: jest.fn(),
        cancel: jest.fn(),
        forget: jest.fn(async () => {})
      }
      const handle = createOrderedMarkerReservations(
        transport,
        target => target,
        () => 0,
        () => 'key'
      )()
      const pending = handle.start({}, Uint8Array.from([1]), 'with-response', { signal: null, deadline: 10 })
      if (failure) await expect(pending).rejects.toThrow('transport failure')
      else await expect(pending).resolves.toEqual({ state: 'pending' })
      jest.advanceTimersByTime(10)
      expect(transport.cancel).not.toHaveBeenCalled()
      await handle.forget()
    }
  } finally {
    jest.useRealTimers()
  }
})

test.each(['deadline', 'abort'])('late cancellation failure remains observable after %s', async kind => {
  jest.useFakeTimers()
  try {
    let finishReserve
    const reserved = new Promise(resolve => {
      finishReserve = resolve
    })
    const transport = {
      reserve: jest.fn(() => reserved),
      start: jest.fn(),
      status: jest.fn(),
      cancel: jest.fn(async () => {
        throw new Error('cancel failed')
      }),
      forget: jest.fn(async () => {})
    }
    const handle = createOrderedMarkerReservations(
      transport,
      target => target,
      () => 0,
      () => 'key'
    )()
    const controller = new AbortController()
    const pending = handle.start({}, Uint8Array.from([1]), 'with-response', {
      signal: controller.signal,
      deadline: kind === 'deadline' ? 10 : null
    })
    const rejected = expect(pending).rejects.toThrow(kind === 'deadline' ? 'operation.timed-out' : 'operation.aborted')
    if (kind === 'deadline') jest.advanceTimersByTime(10)
    else controller.abort()
    await rejected
    finishReserve('6')
    await jest.advanceTimersByTimeAsync(0)
    await expect(handle.status()).rejects.toThrow('cancel failed')
    await expect(handle.cancel()).rejects.toThrow('cancel failed')
    expect(transport.start).not.toHaveBeenCalled()
    await handle.forget()
  } finally {
    jest.useRealTimers()
  }
})

test('deadline cancellation prevents retries from starting a late reservation', async () => {
  jest.useFakeTimers()
  try {
    let finishReserve
    let finishCancel
    const reserved = new Promise(resolve => {
      finishReserve = resolve
    })
    const cancellation = new Promise(resolve => {
      finishCancel = resolve
    })
    const actual = { state: 'not-admitted', error: null }
    const transport = {
      reserve: jest.fn(() => reserved),
      start: jest.fn(async () => ({ state: 'pending' })),
      status: jest.fn(async () => actual),
      cancel: jest.fn(() => cancellation),
      forget: jest.fn(async () => {})
    }
    const handle = createOrderedMarkerReservations(
      transport,
      target => target,
      () => 0,
      () => 'key'
    )()
    const target = {}
    const first = handle.start(target, Uint8Array.from([1]), 'with-response', { signal: null, deadline: 10 })
    const rejected = expect(first).rejects.toThrow('operation.timed-out')
    jest.advanceTimersByTime(10)
    await rejected
    const retry = handle.start(target, Uint8Array.from([1]), 'with-response', controls)
    finishReserve('6')
    for (let i = 0; i < 6; i++) await Promise.resolve()
    expect(transport.start).not.toHaveBeenCalled()
    expect(transport.cancel).toHaveBeenCalledTimes(1)
    expect(transport.cancel).toHaveBeenCalledWith('6')
    finishCancel(actual)
    expect(await retry).toEqual(actual)
    expect(await handle.start(target, Uint8Array.from([1]), 'with-response', controls)).toEqual(actual)
    expect(transport.start).not.toHaveBeenCalled()
    expect(transport.reserve).toHaveBeenCalledTimes(1)
    await handle.forget()
  } finally {
    jest.useRealTimers()
  }
})

test.each(['abort', 'explicit'])(
  '%s settles a stalled start while retaining native cancellation status',
  async method => {
    let finishStart
    const stalled = new Promise(resolve => {
      finishStart = resolve
    })
    const actual = { state: 'not-admitted', error: null }
    const transport = {
      reserve: jest.fn(async () => '3'),
      start: jest.fn(() => stalled),
      status: jest.fn(async () => actual),
      cancel: jest.fn(async () => actual),
      forget: jest.fn(async () => {})
    }
    const handle = createOrderedMarkerReservations(
      transport,
      target => target,
      () => 0,
      () => 'key'
    )()
    const controller = new AbortController()
    const pending = handle.start({}, Uint8Array.from([1]), 'with-response', {
      signal: controller.signal,
      deadline: null
    })
    for (let i = 0; i < 6; i++) await Promise.resolve()
    if (method === 'abort') {
      const rejected = expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
      controller.abort()
      await rejected
    } else {
      expect(await handle.cancel()).toEqual(actual)
      expect(await pending).toEqual(actual)
    }
    finishStart({ state: 'pending' })
    expect(await handle.status()).toEqual(actual)
    await handle.forget()
  }
)

test('explicit cancellation failure settles a stalled start', async () => {
  const transport = {
    reserve: jest.fn(async () => '3'),
    start: jest.fn(() => new Promise(() => {})),
    status: jest.fn(),
    cancel: jest.fn(async () => {
      throw new Error('cancel failed')
    }),
    forget: jest.fn(async () => {})
  }
  const handle = createOrderedMarkerReservations(
    transport,
    target => target,
    () => 0,
    () => 'key'
  )()
  const controller = new AbortController()
  const pending = handle.start({}, Uint8Array.from([1]), 'with-response', { signal: controller.signal, deadline: null })
  for (let i = 0; i < 6; i++) await Promise.resolve()
  const rejected = expect(pending).rejects.toThrow('cancel failed')
  await expect(handle.cancel()).rejects.toThrow('cancel failed')
  await rejected
  await expect(handle.cancel()).rejects.toThrow('cancel failed')
  await handle.forget()
})

test('a retry uses its current abort signal after a lost reservation response', async () => {
  let reportStarted
  const started = new Promise(resolve => {
    reportStarted = resolve
  })
  const actual = { state: 'not-admitted', error: null }
  const transport = {
    reserve: jest.fn().mockRejectedValueOnce(new Error('lost reply')).mockResolvedValue('3'),
    start: jest.fn(() => {
      reportStarted()
      return new Promise(() => {})
    }),
    cancel: jest.fn(async () => actual),
    status: jest.fn(async () => actual),
    forget: jest.fn(async () => {})
  }
  const handle = createOrderedMarkerReservations(
    transport,
    target => target,
    () => 0,
    () => 'key'
  )()
  const target = {}
  await expect(handle.start(target, Uint8Array.of(1), 'with-response', controls)).rejects.toThrow('lost reply')
  const controller = new AbortController()
  const retry = handle.start(target, Uint8Array.of(1), 'with-response', { signal: controller.signal, deadline: null })
  await started
  const rejected = expect(retry).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  controller.abort()
  await rejected
  expect(await handle.status()).toEqual(actual)
  expect(transport.reserve.mock.calls).toEqual([['key'], ['key']])
  expect(transport.cancel).toHaveBeenCalledTimes(1)
  await handle.forget()
})

test('a late start reply does not consume the cancellation failure after abort', async () => {
  jest.useFakeTimers()
  try {
    let finishStart, reportStart
    const started = new Promise(resolve => {
      reportStart = resolve
    })
    const response = new Promise(resolve => {
      finishStart = resolve
    })
    const actual = { state: 'admitted', ordinal: '7', write: { state: 'settled', error: null } }
    const cancellationFailure = new Error('cancel failed')
    const transport = {
      reserve: jest.fn(async () => '3'),
      start: jest.fn(() => {
        reportStart()
        return response
      }),
      cancel: jest.fn(async () => {
        throw cancellationFailure
      }),
      status: jest.fn(async () => actual),
      forget: jest.fn(async () => {})
    }
    const handle = createOrderedMarkerReservations(
      transport,
      target => target,
      () => 0,
      () => 'key'
    )()
    const controller = new AbortController()
    const pending = handle.start({}, Uint8Array.of(1), 'with-response', { signal: controller.signal, deadline: null })
    await started
    const aborted = expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    controller.abort()
    await aborted
    finishStart(actual)
    await jest.advanceTimersByTimeAsync(0)
    await expect(handle.status()).rejects.toBe(cancellationFailure)
    await expect(handle.status()).resolves.toEqual(actual)
    expect(transport.start).toHaveBeenCalledTimes(1)
    await handle.forget()
  } finally {
    jest.useRealTimers()
  }
})
