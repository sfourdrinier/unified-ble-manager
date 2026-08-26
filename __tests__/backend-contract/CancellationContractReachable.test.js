// __tests__/backend-contract/CancellationContractReachable.test.js
//
// 4.0.7 wrote a contract into AGENTS.md that a third-party backend could not
// follow or pass. Two halves: the function defining the vocabulary was not
// reachable from the SDK, and the conformance suite still demanded the old
// word. Both are guarded here because both are invisible from inside the repo -
// first-party backends import by deep relative path and the fixture forces the
// cancel to win, so nothing failed while the contract was unusable.
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
      expect.objectContaining({ normalized: expect.objectContaining({ code: 'protocol.violation' }) })
    )
  })
})
