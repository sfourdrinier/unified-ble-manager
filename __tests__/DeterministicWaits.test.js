// __tests__/DeterministicWaits.test.js
//
// A guard for the defect class that reached CI twice and was caught only by
// the slowest runner in the matrix: a test whose verdict depends on how busy
// the machine is. Both escapes looked reasonable in review, and both passed on
// every fast machine, so review is not where this gets caught.
//
// Two shapes are banned outright, because neither has a correct use here:
//
//   Promise.race([work, sleep])  decides between "the work finished" and "time
//   passed", so a loaded runner votes for the wrong branch. Await the work and
//   let `awaitSignal` bound the failure path instead.
//
//   setInterval  is polling by construction: it asks "has it happened yet?"
//   against the wall clock, which is the question a test can never ask
//   deterministically.
//
// A single `await new Promise(resolve => setImmediate(resolve))` is NOT banned.
// One turn of the macrotask queue always happens; that wait is brittle if the
// implementation later needs two, but it is not a race against real time.

const fs = require('node:fs')
const path = require('node:path')

const TESTS_ROOT = __dirname
// The one file allowed to schedule a timer, and only on a failure path.
const SANCTIONED = path.join(TESTS_ROOT, 'helpers', 'async.js')
const TIMER = /\b(?:setTimeout|setInterval|setImmediate)\s*\(/

function sourceFiles(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const full = path.join(directory, entry.name)
    if (entry.isDirectory()) return entry.name === 'node_modules' ? [] : sourceFiles(full)
    return /\.[jt]sx?$/.test(entry.name) && full !== SANCTIONED ? [full] : []
  })
}

/**
 * The text of each `Promise.race(...)` call's own arguments.
 *
 * Matched by balancing parentheses rather than by reading a fixed window of
 * following lines: a `await new Promise(resolve => setTimeout(resolve, 40))`
 * that merely sits above an unrelated race is not this defect, and a line
 * window cannot tell the two apart.
 */
function raceArguments(source) {
  const calls = []
  const opener = /Promise\s*\.\s*race\s*\(/g
  let match
  while ((match = opener.exec(source)) !== null) {
    let depth = 1
    let index = opener.lastIndex
    while (index < source.length && depth > 0) {
      if (source[index] === '(') depth += 1
      else if (source[index] === ')') depth -= 1
      index += 1
    }
    calls.push(source.slice(opener.lastIndex, index - 1))
  }
  return calls
}

const files = sourceFiles(TESTS_ROOT)

describe('test waits do not race the clock', () => {
  it('finds the suite to inspect', () => {
    expect(files.length).toBeGreaterThan(100)
    expect(fs.existsSync(SANCTIONED)).toBe(true)
  })

  it('never races a promise against a timer', () => {
    const offenders = files.filter(file =>
      raceArguments(fs.readFileSync(file, 'utf8')).some(args => TIMER.test(args))
    )
    expect(
      offenders.map(file => path.relative(TESTS_ROOT, file))
    ).toEqual([])
  })

  it('never polls with setInterval', () => {
    const offenders = files.filter(file => /\bsetInterval\s*\(/.test(fs.readFileSync(file, 'utf8')))
    expect(
      offenders.map(file => path.relative(TESTS_ROOT, file))
    ).toEqual([])
  })
})
