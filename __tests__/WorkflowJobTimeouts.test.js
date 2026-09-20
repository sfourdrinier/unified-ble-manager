'use strict'

// A deadlocked test must fail a CI job in minutes, not occupy a runner until
// GitHub's six-hour ceiling. A same-peer connect deadlock stalled a local gate
// run for eight hours before this guard existed, so every job declares its own
// bound.
const fs = require('node:fs')
const path = require('node:path')
const YAML = require('yaml')

const WORKFLOWS = path.join(__dirname, '..', '.github', 'workflows')

function jobsOf(file) {
  const document = YAML.parse(fs.readFileSync(path.join(WORKFLOWS, file), 'utf8'))
  return Object.entries(document.jobs ?? {})
}

describe('every CI job is bounded in time', () => {
  const files = fs.readdirSync(WORKFLOWS).filter(name => name.endsWith('.yml') || name.endsWith('.yaml'))

  test('there are workflows to check', () => {
    expect(files.length).toBeGreaterThan(0)
  })

  test.each(files)('%s declares timeout-minutes on every job', file => {
    const missing = jobsOf(file)
      .filter(([, job]) => job.uses === undefined && typeof job['timeout-minutes'] !== 'number')
      .map(([name]) => name)
    expect(missing).toEqual([])
  })
})
