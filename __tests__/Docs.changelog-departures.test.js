'use strict'

// D4 (finding 159): the CHANGELOG records the deliberate truthful RN
// diagnostics/limitation wording departure from legacy.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const changelog = fs.readFileSync(path.join(root, 'CHANGELOG.md'), 'utf8')
// The departure was introduced in rc.5. Guard that immutable release note
// instead of requiring every subsequent candidate to repeat historical prose.
const heading = '## [5.0.0-rc.5]'
const releaseStart = changelog.indexOf(heading)
const nextRelease = changelog.indexOf('\n## [', releaseStart + heading.length)
const departureRelease = changelog.slice(releaseStart, nextRelease === -1 ? undefined : nextRelease)

describe('CHANGELOG finding-159 departure entry', () => {
  // 'finding 159' is the review finding id (distinct from the `(#[number])`
  // issue references elsewhere in the file).
  test.each(['ubm-mobile-wire/1', 'limitation', 'diagnostic', 'finding 159', 'truthful'])(
    '[originating release] names %s',
    keyword => {
      expect(releaseStart).toBeGreaterThanOrEqual(0)
      expect(departureRelease).toMatch(keyword)
    }
  )
})
