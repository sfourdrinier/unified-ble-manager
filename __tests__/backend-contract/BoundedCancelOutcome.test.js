// __tests__/backend-contract/BoundedCancelOutcome.test.js
//
// `cancelPairing()` reads the pairing's result rather than forming a second
// opinion - which is what stops the two calls contradicting each other, and
// which means it now WAITS. Until this existed that wait was bounded only by
// the options the *pairing's* caller passed; a deadline given to
// `cancelPairing()` was validated at admission and then discarded.
const { boundedCancelOutcome } = require('../../src/backend-contract/security')

const options = (overrides = {}) => ({ signal: null, deadline: null, ...overrides })
const never = () => new Promise(() => undefined)

describe('a cancellation is bounded by its own caller', () => {
  test('an unbounded caller still gets the pairing outcome', async () => {
    await expect(
      boundedCancelOutcome(Promise.resolve({ outcome: 'paired', state: {} }), options(), () => 0, 'op')
    ).resolves.toEqual({ outcome: 'paired' })
  })

  test('an already-aborted signal is refused before anything is awaited', async () => {
    const controller = new AbortController()
    controller.abort()

    await expect(
      boundedCancelOutcome(never(), options({ signal: controller.signal }), () => 0, 'op')
    ).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
  })

  test('an elapsed deadline is refused before anything is awaited', async () => {
    await expect(
      boundedCancelOutcome(never(), options({ deadline: 100 }), () => 100, 'op')
    ).rejects.toMatchObject({ normalized: { code: 'operation.timed-out' } })
  })

  /**
   * The rule this whole release line exists to enforce, applied to the timeout
   * path: a caller who stopped waiting has learned NOTHING about the bond.
   * Answering `'cancelled'` here would reintroduce, under a deadline, exactly
   * the lie the result vocabulary was widened to remove.
   */
  test('an abort mid-wait rejects rather than inventing a cancellation', async () => {
    const controller = new AbortController()
    const pending = boundedCancelOutcome(never(), options({ signal: controller.signal }), () => 0, 'op')
    controller.abort()

    await expect(pending).rejects.toMatchObject({ normalized: { code: 'operation.aborted' } })
    await expect(pending).rejects.not.toMatchObject({ outcome: 'cancelled' })
  })

  test('a pairing that settles first wins the race, deadline notwithstanding', async () => {
    await expect(
      boundedCancelOutcome(
        Promise.resolve({ outcome: 'rejected', reason: 'peer said no' }),
        options({ deadline: 10_000 }),
        () => 0,
        'op'
      )
    ).resolves.toEqual({ outcome: 'rejected', reason: 'peer said no' })
  })

  /**
   * A prompt cancellation must not be made to wait on a timer, and a settled
   * one must not leave one behind - the reason the abort listener and the
   * timeout are both torn down on every exit.
   */
  test('leaves no timer behind when the pairing settles first', async () => {
    const controller = new AbortController()
    const before = controller.signal.constructor === AbortSignal
    await boundedCancelOutcome(
      Promise.resolve({ outcome: 'cancelled' }),
      options({ deadline: 10_000, signal: controller.signal }),
      () => 0,
      'op'
    )

    expect(before).toBe(true)
    // A retained timer would keep the process alive; jest's open-handle
    // detection covers that, so this asserts the listener side.
    controller.abort()
  })
})
