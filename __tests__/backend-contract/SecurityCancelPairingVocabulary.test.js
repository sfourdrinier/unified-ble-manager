// __tests__/backend-contract/SecurityCancelPairingVocabulary.test.js

/**
 * `cancelPairing()` reports what the cancellation ACHIEVED, not what it asked
 * for, and every backend says it with the same words.
 *
 * The defect this pins: a cancellation can lose the race. The bond completes
 * while the cancel is in flight, and reporting that as `'cancelled'` tells the
 * caller no bond exists while one does — a caller who believes that never looks
 * again. Every backend did exactly this, unconditionally, because none of them
 * asked whether the cancellation had worked.
 *
 * WHAT THIS FILE ASSERTS, and what it deliberately does not. It asserts a
 * shared VOCABULARY: each backend answers from the same set of words, and each
 * word means the same thing everywhere. It does NOT assert that every backend
 * produces the same outcome for the same stimulus — radios differ, and a test
 * of hardware uniformity could only be made to pass by faking a capability.
 *
 * ORDERING IS FORCED, NEVER TIMED. Every wait here is on a promise that the
 * test itself resolves, so the interleaving is proven rather than raced. A
 * sibling test in this repo failed for months on loaded CI runners because it
 * bet on a 1 ms timer winning a race it was not guaranteed to win; the bug was
 * in what the test assumed, not in how long it waited.
 */

const { cancelOutcomeForPairResult } = require('../../src/backend-contract/security')

describe('the cancellation vocabulary is total', () => {
  /**
   * Exhaustive over SecurityPairResult. A fallback branch is what let the old
   * code report a peer refusal as a cancellation, so the mapping must name
   * every outcome rather than sweep the remainder into one.
   */
  test('every pairing outcome maps to exactly one cancellation word', () => {
    const bonded = { bond: 'bonded' }

    expect(cancelOutcomeForPairResult({ outcome: 'paired', state: bonded })).toEqual({ outcome: 'paired' })
    expect(cancelOutcomeForPairResult({ outcome: 'already-paired', state: bonded })).toEqual({ outcome: 'paired' })
    expect(cancelOutcomeForPairResult({ outcome: 'repaired', state: bonded })).toEqual({ outcome: 'paired' })
    expect(cancelOutcomeForPairResult({ outcome: 'cancelled' })).toEqual({ outcome: 'cancelled' })
  })

  /**
   * A peer that refused was not cancelled by anyone. Claiming `'cancelled'`
   * here would take credit for stopping something that stopped itself — the
   * same substitution as reporting a completed bond as cancelled, with the
   * arrow pointing the other way.
   */
  test('a peer refusal stays a refusal and carries its reason', () => {
    expect(cancelOutcomeForPairResult({ outcome: 'rejected', reason: 'peer said no' })).toEqual({
      outcome: 'rejected',
      reason: 'peer said no'
    })
    expect(cancelOutcomeForPairResult({ outcome: 'rejected', reason: null })).toEqual({
      outcome: 'rejected',
      reason: null
    })
  })

  /**
   * The words `pair()` and `cancelPairing()` share must mean the same thing in
   * both, which is why the "bond completed during your cancellation" outcome is
   * `'paired'` and not `'already-paired'` — the latter already means "bonded
   * BEFORE this call" on the pairing result.
   */
  test('no word does two jobs across the two result types', () => {
    const bondedBeforeTheCall = cancelOutcomeForPairResult({ outcome: 'already-paired', state: { bond: 'bonded' } })
    const bondedByThisOperation = cancelOutcomeForPairResult({ outcome: 'paired', state: { bond: 'bonded' } })

    // Both are "a bond exists because of this pairing operation" as far as a
    // cancelling caller is concerned; the re-pairing distinction stays on pair().
    expect(bondedByThisOperation).toEqual({ outcome: 'paired' })
    expect(bondedBeforeTheCall).toEqual({ outcome: 'paired' })
  })
})

describe('no backend forms a second opinion about the race', () => {
  /**
   * WinRT short-circuited UPSTREAM of the shared mapper: when the dispatcher
   * answered 'already-terminal' - meaning the pairing had settled before the
   * cancellation reached it, which is precisely the lost race - it returned
   * 'not-pairing'. That contradicted the pair() it targeted and made
   * 'not-pairing' mean two things: "there was nothing to stop" and "it was
   * already over". Routing through the mapper is necessary but not sufficient
   * if a backend answers before reaching it.
   */
  test('an already-terminal cancellation reports the bond, not "not-pairing"', () => {
    const source = require('fs').readFileSync(
      require('path').join(__dirname, '../../src/backends/winrt/winrt-security.ts'),
      'utf8'
    )
    const branch = source.slice(source.indexOf("acknowledgement.state !== 'cancellation-requested'"))
    const body = branch.slice(0, branch.indexOf('\n    }'))

    expect(body).toContain('cancelOutcomeForPairResult')
    expect(body).not.toContain("outcome: 'not-pairing'")
  })
})
