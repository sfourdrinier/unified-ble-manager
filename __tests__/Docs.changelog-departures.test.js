'use strict'

// D4 (finding 159): the CHANGELOG records the deliberate truthful RN
// diagnostics/limitation wording departure from legacy.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const changelog = fs.readFileSync(path.join(root, 'CHANGELOG.md'), 'utf8')
const version = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8')).version
const heading = `## [${version}]`
const releaseStart = changelog.indexOf(heading)
const nextRelease = changelog.indexOf('\n## [', releaseStart + heading.length)
const currentRelease = changelog.slice(releaseStart, nextRelease === -1 ? undefined : nextRelease)

describe('CHANGELOG finding-159 departure entry', () => {
  // 'finding 159' is the review finding id (distinct from the `(#[number])`
  // issue references elsewhere in the file): only the new entry carries it.
  test.each(['ubm-mobile-wire/1', 'limitation', 'diagnostic', 'finding 159', 'truthful'])(
    '[current package release] names %s',
    keyword => {
      expect(releaseStart).toBeGreaterThanOrEqual(0)
      expect(currentRelease).toMatch(keyword)
    }
  )
})
