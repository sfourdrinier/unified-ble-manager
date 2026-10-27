// __tests__/AdvanceNextDistTag.test.js

'use strict'

// npm `next` is the release-candidate channel. A stable release used to leave
// it on an older candidate (5.1.3 shipped with `next` still on 5.0.0-rc.21),
// so `npm install unified-ble-manager@next` downgraded. A stable release now
// converges `next` on the highest published version. `npm dist-tag add` is an
// unconditional write and release tags publish concurrently, so a candidate
// published during the step must survive it.

const { advanceNextDistTag, nextDistTagDecision } = require('../scripts/release/advance-next-dist-tag')

const PUBLISHED = ['5.0.0-rc.21', '5.0.0', '5.1.2', '5.1.3']

describe('nextDistTagDecision', () => {
  test.each([
    ['5.1.3', { next: '5.0.0-rc.21' }, PUBLISHED, '5.1.3'],
    ['5.1.3', {}, PUBLISHED, '5.1.3'],
    ['5.2.0', { next: '5.2.0-rc.4' }, [...PUBLISHED, '5.2.0-rc.4', '5.2.0'], '5.2.0'],
    ['5.1.4', { next: '5.0.0-rc.21' }, [...PUBLISHED, '5.2.0-rc.1', '5.1.4'], '5.2.0-rc.1'],
    ['5.1.4', { next: '5.1.3' }, PUBLISHED, '5.1.4']
  ])('stable %s with next %j over %j advances next to %s', (version, distTags, versions, target) => {
    expect(nextDistTagDecision(version, { distTags, versions })).toMatchObject({ advance: true, target })
  })

  test.each([
    ['5.1.4', { next: '5.2.0-rc.1' }, [...PUBLISHED, '5.2.0-rc.1', '5.1.4']],
    ['5.1.3', { next: '5.1.3' }, PUBLISHED]
  ])('stable %s leaves next %j when it is not older than the highest published', (version, distTags, versions) => {
    expect(nextDistTagDecision(version, { distTags, versions }).advance).toBe(false)
  })

  test('a release candidate publishes to next itself and is never re-tagged here', () => {
    expect(nextDistTagDecision('5.2.0-rc.1', { distTags: { next: '5.0.0-rc.21' }, versions: PUBLISHED })).toEqual({
      advance: false,
      target: undefined,
      reason: 'release candidate 5.2.0-rc.1 publishes to next itself'
    })
  })

  test('refuses a next value it cannot order rather than guessing', () => {
    expect(() => nextDistTagDecision('5.1.3', { distTags: { next: 'canary' }, versions: PUBLISHED })).toThrow(
      'npm next holds canary'
    )
  })

  test('refuses an unauthorized release version', () => {
    expect(() => nextDistTagDecision('6.0.0', { distTags: {}, versions: [] })).toThrow(
      'Unauthorized release version 6.0.0'
    )
  })
})

// A registry whose dist-tag writes become visible after `visibleAfterReads`
// reads, and whose `onRead` hook lets a concurrent release publish after that.
function registry(initial, { applyAdd = true, visibleAfterReads = 1, onRead = () => {} } = {}) {
  const state = { distTags: { ...initial.distTags }, versions: [...initial.versions] }
  let pending = null
  let readsSinceAdd = 0
  let reads = 0
  const calls = []
  return {
    calls,
    state,
    viewRegistry: () => {
      calls.push('view')
      if (pending && ++readsSinceAdd >= visibleAfterReads) {
        state.distTags.next = pending
        pending = null
      }
      onRead(++reads, state)
      return { distTags: { ...state.distTags }, versions: [...state.versions] }
    },
    addDistTag: version => {
      calls.push(`add ${version}`)
      if (applyAdd) {
        pending = version
        readsSinceAdd = 0
      }
    }
  }
}

function publishCandidate(state, version) {
  state.versions.push(version)
  state.distTags.next = version
}

describe('advanceNextDistTag', () => {
  const quiet = { sleep: () => {}, log: () => {} }
  const afterStable = { distTags: { latest: '5.1.3', next: '5.0.0-rc.21' }, versions: PUBLISHED }

  test('adds next for a newer stable and confirms it from the registry', () => {
    const npm = registry(afterStable)
    expect(advanceNextDistTag({ version: '5.1.3', ...npm, ...quiet })).toEqual({
      advanced: true,
      next: '5.1.3',
      reason: 'next already resolves to 5.1.3'
    })
    expect(npm.calls).toEqual(['view', 'add 5.1.3', 'view'])
  })

  test('waits a bounded time for the registry to show the new tag without rewriting it', () => {
    const npm = registry(afterStable, { visibleAfterReads: 3 })
    const sleeps = []
    advanceNextDistTag({ version: '5.1.3', ...npm, sleep: ms => sleeps.push(ms), log: () => {}, delayMs: 7 })
    expect(npm.calls).toEqual(['view', 'add 5.1.3', 'view', 'view', 'view'])
    expect(sleeps).toEqual([7, 7, 7])
  })

  test('restores a candidate published between the read and the write', () => {
    const npm = registry(afterStable, {
      onRead: (read, state) => {
        if (read === 2) state.versions.push('5.2.0-rc.1')
      }
    })
    const result = advanceNextDistTag({ version: '5.1.3', ...npm, ...quiet })
    expect(npm.calls).toEqual(['view', 'add 5.1.3', 'view', 'add 5.2.0-rc.1', 'view'])
    expect(result).toMatchObject({ advanced: true, next: '5.2.0-rc.1' })
    expect(npm.state.distTags.next).toBe('5.2.0-rc.1')
  })

  test('accepts a candidate that published over its write instead of waiting for its own value', () => {
    const npm = registry(afterStable, {
      onRead: (read, state) => {
        if (read === 2) publishCandidate(state, '5.2.0-rc.1')
      }
    })
    const result = advanceNextDistTag({ version: '5.1.3', ...npm, ...quiet })
    expect(result).toMatchObject({ advanced: true, next: '5.2.0-rc.1' })
    expect(npm.calls.filter(call => call.startsWith('add'))).toEqual(['add 5.1.3'])
  })

  test('reports a tag the registry never shows instead of assuming the add worked', () => {
    const npm = registry(afterStable, { applyAdd: false })
    expect(() => advanceNextDistTag({ version: '5.1.3', ...npm, ...quiet, attempts: 3 })).toThrow(
      'npm next still resolves to 5.0.0-rc.21 after adding 5.1.3'
    )
    expect(npm.calls.filter(call => call.startsWith('add'))).toEqual(['add 5.1.3'])
  })

  test('leaves a newer release candidate on next without writing', () => {
    const npm = registry({
      distTags: { latest: '5.1.4', next: '5.2.0-rc.1' },
      versions: [...PUBLISHED, '5.2.0-rc.1', '5.1.4']
    })
    expect(advanceNextDistTag({ version: '5.1.4', ...npm, ...quiet })).toEqual({
      advanced: false,
      next: '5.2.0-rc.1',
      reason: 'next already resolves to 5.2.0-rc.1'
    })
    expect(npm.calls).toEqual(['view'])
  })

  test('surfaces a refused dist-tag write with the npm setting it needs', () => {
    const npm = registry(afterStable)
    npm.addDistTag = () => {
      throw new Error('npm dist-tag add exited 1')
    }
    expect(() => advanceNextDistTag({ version: '5.1.3', ...npm, ...quiet })).toThrow(
      /5\.1\.3 is published; npm next was not advanced \(npm dist-tag add exited 1\)\. Enable "Allow npm dist-tag"/
    )
  })
})
