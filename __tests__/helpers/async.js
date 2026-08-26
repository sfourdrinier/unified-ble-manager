// __tests__/helpers/async.js
//
// The one place in the test suite where a timer may live, and it may only live
// on a failure path.
//
// A test's verdict must not depend on how fast the machine is; only its runtime
// may. Polling breaks that rule: a loop that asks "has it happened yet?" is
// racing real time, so its answer changes with load and core count. Two defects
// reached CI that way and were caught only by the slowest runner in the matrix -
// first a poll that drained microtasks and so could never observe a timer, then
// a `setImmediate` spin that starved the very timer it awaited.
//
// The alternative is to await the event itself. Where a test owns the mock the
// signal passes through, it can resolve a promise there and await that promise:
// no polling, no timer, no speed dependence. If the event has already happened
// the await returns immediately, which is why a promise beats a predicate.

'use strict'

/**
 * Await a promise that some collaborator resolves, failing with a description
 * if it never settles.
 *
 * The timer here fires only when the test is already failing. On the success
 * path nothing is scheduled and nothing is polled, so the test behaves the same
 * on a loaded CI runner as on an idle workstation - a slower machine can only
 * change how quickly an already-broken test reports.
 *
 * The budget sits below Jest's 5s default deliberately. A larger one can never
 * be reached: Jest kills the test first and reports "Exceeded timeout of 5000
 * ms", which names neither what was awaited nor for how long. A test that
 * raises its own timeout should raise this with it.
 */
async function awaitSignal(promise, description, budgetMs = 4_000) {
  let timer
  const failure = new Promise((_resolve, reject) => {
    timer = setTimeout(
      () => reject(new Error(`no ${description} within ${budgetMs}ms`)),
      budgetMs
    )
  })
  try {
    return await Promise.race([promise, failure])
  } finally {
    clearTimeout(timer)
  }
}

module.exports = { awaitSignal }
