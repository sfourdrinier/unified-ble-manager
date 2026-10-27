// __tests__/AdvanceNextDistTag.test.js

'use strict'

// npm `next` is the release-candidate channel. A stable release used to leave
// it on an older candidate (5.1.3 shipped with `next` still on 5.0.0-rc.21),
// so `npm install unified-ble-manager@next` downgraded. A stable release now
// advances `next` when it holds an older version, and never past a newer RC.

const { advanceNextDistTag, nextDistTagDecision } = require('../scripts/release/advance-next-dist-tag')

describe('nextDistTagDecision', () => {
  test.each([
    ['5.1.3', { latest: '5.1.2', next: '5.0.0-rc.21' }, true],
    ['5.1.3', { latest: '5.1.2' }, true],
    ['5.2.0', { latest: '5.1.3', next: '5.2.0-rc.4' }, true],
    ['5.1.4', { latest: '5.1.3', next: '5.2.0-rc.1' }, false],
    ['5.1.3', { latest: '5.1.3', next: '5.1.3' }, false]
  ])('stable %s with %j advances next: %s', (version, distTags, advance) => {
    expect(nextDistTagDecision(version, distTags).advance).toBe(advance)
  })

  test('a release candidate publishes to next itself and is never re-tagged here', () => {
    expect(nextDistTagDecision('5.2.0-rc.1', { latest: '5.1.3', next: '5.0.0-rc.21' })).toEqual({
      advance: false,
      reason: 'release candidate 5.2.0-rc.1 publishes to next itself'
    })
  })

  test('refuses a next value it cannot order rather than guessing', () => {
    expect(() => nextDistTagDecision('5.1.3', { next: 'canary' })).toThrow('npm next holds canary')
  })

  test('refuses an unauthorized release version', () => {
    expect(() => nextDistTagDecision('6.0.0', {})).toThrow('Unauthorized release version 6.0.0')
  })
})

function registry(initial, { applyAdd = true, visibleAfterReads = 1 } = {}) {
  let tags = { ...initial }
  let pending = null
  let readsSinceAdd = 0
  const calls = []
  return {
    calls,
    viewDistTags: () => {
      calls.push('view')
      if (pending && ++readsSinceAdd >= visibleAfterReads) {
        tags = { ...tags, next: pending }
        pending = null
      }
      return { ...tags }
    },
    addDistTag: version => {
      calls.push(`add ${version}`)
      if (applyAdd) pending = version
    }
  }
}

describe('advanceNextDistTag', () => {
  const quiet = { sleep: () => {}, log: () => {} }

  test('adds next for a newer stable and confirms it from the registry', () => {
    const npm = registry({ latest: '5.1.3', next: '5.0.0-rc.21' })
    expect(advanceNextDistTag({ version: '5.1.3', ...npm, ...quiet })).toEqual({
      advanced: true,
      next: '5.1.3',
      reason: 'next 5.0.0-rc.21 is older than stable 5.1.3'
    })
    expect(npm.calls).toEqual(['view', 'add 5.1.3', 'view'])
  })

  test('waits a bounded time for the registry to show the new tag', () => {
    const npm = registry({ latest: '5.1.3', next: '5.0.0-rc.21' }, { visibleAfterReads: 3 })
    const sleeps = []
    advanceNextDistTag({ version: '5.1.3', ...npm, sleep: ms => sleeps.push(ms), log: () => {}, delayMs: 7 })
    expect(npm.calls).toEqual(['view', 'add 5.1.3', 'view', 'view', 'view'])
    expect(sleeps).toEqual([7, 7])
  })

  test('reports a tag the registry never shows instead of assuming the add worked', () => {
    const npm = registry({ latest: '5.1.3', next: '5.0.0-rc.21' }, { applyAdd: false })
    expect(() => advanceNextDistTag({ version: '5.1.3', ...npm, ...quiet, attempts: 3 })).toThrow(
      'npm next still resolves to 5.0.0-rc.21 after adding 5.1.3'
    )
  })

  test('leaves a newer release candidate on next without writing', () => {
    const npm = registry({ latest: '5.1.3', next: '5.2.0-rc.1' })
    expect(advanceNextDistTag({ version: '5.1.4', ...npm, ...quiet })).toEqual({
      advanced: false,
      next: '5.2.0-rc.1',
      reason: 'next 5.2.0-rc.1 is not older than stable 5.1.4'
    })
    expect(npm.calls).toEqual(['view'])
  })

  test('surfaces a refused dist-tag write with the npm setting it needs', () => {
    const npm = registry({ latest: '5.1.3', next: '5.0.0-rc.21' })
    npm.addDistTag = () => {
      throw new Error('npm dist-tag add exited 1')
    }
    expect(() => advanceNextDistTag({ version: '5.1.3', ...npm, ...quiet })).toThrow(
      /5\.1\.3 is published; npm next was not advanced \(npm dist-tag add exited 1\)\. Enable "Allow npm dist-tag"/
    )
  })
})
