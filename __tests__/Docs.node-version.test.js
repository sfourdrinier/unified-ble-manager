'use strict'

// The Node guide identifies the version shipped by this source tree.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')

describe('desktop docs version', () => {
  test('docs/NODE.md targets the current release candidate', () => {
    const node = fs.readFileSync(path.join(root, 'docs', 'NODE.md'), 'utf8')
    expect(node).toMatch('targets `5.0.0-rc.11`')
  })
})
