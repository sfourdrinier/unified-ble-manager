// __tests__/backend-contract/CancellationContractReachable.test.js
//
// 4.0.7 wrote a contract into AGENTS.md that a third-party backend could not
// follow or pass. Two halves: the function defining the vocabulary was not
// reachable from the SDK, and the conformance suite still demanded the old
// word. Both are guarded here because both are invisible from inside the repo -
// first-party backends import by deep relative path and the fixture forces the
// cancel to win, so nothing failed while the contract was unusable.
const fs = require('fs')
const path = require('path')
const backendSdk = require('../../src/backend-sdk')
const { cancelOutcomeForPairResult } = require('../../src/backend-contract/security')

describe('a third-party backend can follow the cancellation contract', () => {
  test('the mapper that defines the vocabulary is reachable from the SDK', () => {
    // Deep relative imports are a first-party privilege. An external backend
    // has only the published entrypoint.
    expect(typeof backendSdk.cancelOutcomeForPairResult).toBe('function')
    expect(backendSdk.cancelOutcomeForPairResult({ outcome: 'paired', state: {} })).toEqual({ outcome: 'paired' })
  })

  /**
   * The compile-time switch is the primary defence and stays exhaustive. This
   * guard is for a backend outside the type system: without it the caller got
   * `undefined` and a raw TypeError several frames away instead of being told
   * which contract was broken.
   */
  test('an out-of-contract outcome is named as a protocol violation, not a TypeError', () => {
    expect(() => cancelOutcomeForPairResult({ outcome: 'invented-by-a-third-party' })).toThrow(
      expect.objectContaining({
        normalized: expect.objectContaining({
          code: 'protocol.violation',
          domain: 'core',
          operation: 'security.cancel-pairing.outcome'
        })
      })
    )
    expect(() => backendSdk.cancelOutcomeForPairResult({ outcome: 'invented-by-a-third-party' })).toThrow(
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'protocol.violation' }) })
    )
  })

  test('the TCK consistency rule accepts a bond that won the race and rejects a contradiction', () => {
    const cancellationIsConsistent = (cancelled, cancelledPair) =>
      cancelOutcomeForPairResult(cancelledPair).outcome === cancelled.outcome
    const bonded = { outcome: 'paired', state: {} }
    expect(cancellationIsConsistent({ outcome: 'paired' }, bonded)).toBe(true)
    expect(cancellationIsConsistent({ outcome: 'cancelled' }, { outcome: 'cancelled' })).toBe(true)
    expect(cancellationIsConsistent({ outcome: 'cancelled' }, bonded)).toBe(false)
    expect(cancellationIsConsistent({ outcome: 'paired' }, { outcome: 'cancelled' })).toBe(false)
  })

  /**
   * The TCK used to hard-require `'cancelled'` from both `pair()` and
   * `cancelPairing()`. That penalised a backend for conforming: when the bond
   * wins the race, both calls must report `'paired'`. The suite must judge
   * consistency through the shared mapper, not a fixed word.
   */
  test('the TCK judges cancellation by mapper consistency, not a fixed cancelled word', () => {
    const source = fs.readFileSync(path.join(__dirname, '../../src/tck/runner-public-scenarios.ts'), 'utf8')
    expect(source).toContain('cancellationIsConsistent')
    expect(source).toContain('cancelOutcomeForPairResult')
    expect(source).not.toMatch(/cancelled(?:\?\.|\.)outcome === 'cancelled'/)
    expect(source).not.toMatch(/cancelledPair(?:\?\.|\.)outcome === 'cancelled'/)
  })
})
