// __tests__/expo-readiness-pending-adapter.test.js
// A boundary that has not yet received an adapter-state event reports a
// pending snapshot. Readiness must wait for the authoritative one rather than
// mapping "unknown" to "unavailable" — a working radio reported as unavailable
// leaves consumers with nothing to act on.
const { getExpoBleReadiness, mapExpoReadiness } = require('../src/expo')

const PENDING = Object.freeze({
  availability: 'unknown',
  authorization: 'unavailable',
  power: 'unknown',
  safeReason: 'The Android radio has not emitted its authoritative adapter state yet.'
})

const READY = Object.freeze({
  availability: 'available',
  authorization: 'granted',
  power: 'on',
  safeReason: null
})

const OFF = Object.freeze({
  availability: 'available',
  authorization: 'granted',
  power: 'off',
  safeReason: null
})

const UNAVAILABLE = Object.freeze({
  availability: 'unavailable',
  authorization: 'granted',
  power: 'unsupported',
  safeReason: null
})

function managerReturning(...states) {
  let call = 0
  return {
    adapter: {
      state: async () => states[Math.min(call++, states.length - 1)]
    },
    calls: () => call
  }
}

// The retry interval is a module-private setTimeout; drive it with fake timers
// rather than widening the public signature for testability.
async function readinessWithTimers(manager, configuration) {
  jest.useFakeTimers()
  try {
    const pending = getExpoBleReadiness(manager, configuration)
    // Enough virtual time for every bounded attempt (20 x 100ms).
    await jest.advanceTimersByTimeAsync(20 * 100 + 50)
    return await pending
  } finally {
    jest.useRealTimers()
  }
}

describe('expo readiness with a pending adapter snapshot', () => {
  test('waits for the authoritative snapshot instead of reporting unavailable', async () => {
    const manager = managerReturning(PENDING, PENDING, READY)
    const readiness = await readinessWithTimers(manager)
    expect(readiness.state).toBe('ready')
    expect(manager.calls()).toBe(3)
  })

  test('does not poll when the first snapshot is already authoritative', async () => {
    const manager = managerReturning(READY)
    const readiness = await readinessWithTimers(manager)
    expect(readiness.state).toBe('ready')
    expect(manager.calls()).toBe(1)
  })

  test('a genuinely unavailable radio is still unavailable', async () => {
    const manager = managerReturning(UNAVAILABLE)
    const readiness = await readinessWithTimers(manager)
    expect(readiness.state).toBe('unavailable')
    expect(manager.calls()).toBe(1)
  })

  test('a radio that is merely off still asks the user to enable it', async () => {
    const manager = managerReturning(OFF)
    const readiness = await readinessWithTimers(manager)
    expect(readiness.state).toBe('action-required')
    expect(readiness.actions.map(action => action.kind)).toContain('enable-bluetooth')
  })

  test('gives up after a bounded number of attempts rather than hanging', async () => {
    const manager = managerReturning(PENDING)
    const readiness = await readinessWithTimers(manager)
    expect(readiness.state).toBe('unavailable')
    expect(manager.calls()).toBeLessThanOrEqual(21)
    expect(manager.calls()).toBeGreaterThan(1)
  })

  test('the pure mapper is unchanged and still maps a pending snapshot verbatim', () => {
    expect(mapExpoReadiness(PENDING).state).toBe('unavailable')
  })
})
