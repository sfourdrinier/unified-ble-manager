'use strict'

// D8: docs/NODE.md describes the 5.0.0 release, not a stale prerelease.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')

describe('desktop docs version', () => {
  test('docs/NODE.md targets 5.0.0, not 5.0.0-rc.0', () => {
    const node = fs.readFileSync(path.join(root, 'docs', 'NODE.md'), 'utf8')
    expect(node).toMatch('targets `5.0.0`')
    expect(node).not.toMatch('5.0.0-rc.0')
  })
})
