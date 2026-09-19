'use strict'

// D4 (finding 159): the CHANGELOG records the deliberate truthful RN
// diagnostics/limitation wording departure from legacy.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const changelog = fs.readFileSync(path.join(root, 'CHANGELOG.md'), 'utf8')
const unreleased = changelog.slice(changelog.indexOf('## [Unreleased]'))

describe('CHANGELOG finding-159 departure entry', () => {
  // 'finding 159' is the review finding id (distinct from the `(#[number])`
  // issue references elsewhere in the file): only the new entry carries it.
  test.each(['ubm-mobile-wire/1', 'limitation', 'diagnostic', 'finding 159', 'truthful'])(
    '[Unreleased] names %s',
    keyword => {
      expect(unreleased).toMatch(keyword)
    }
  )
})
