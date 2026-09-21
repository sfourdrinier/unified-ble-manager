// __tests__/MetroPortGuard.test.js
//
// Finding 241: a dev server port held by ANOTHER project is the failure that
// cost 75 GB of memory overnight — the example loaded a foreign bundle,
// registered no callable JavaScript modules, and React Native then threw on
// every native call for hours. The guard's whole job is to turn that into one
// sentence before the app starts, and to say plainly when it cannot tell.

const fs = require('fs')
const path = require('path')

const guardPath = path.join(__dirname, '..', 'examples-shared', 'dev', 'metro-port-guard.js')
const { inspectPort, describeOutcome, EXIT_CODES } = require(guardPath)

const projectRoot = '/repo/example'

function probes({ listener = null, cwd = null, command = null, available = true }) {
  return {
    listenerAvailable: () => available,
    findListener: () => listener,
    processCwd: () => cwd,
    processCommand: () => command
  }
}

describe('metro port guard', () => {
  test('the guard module is executable tooling that lives with the examples', () => {
    fs.accessSync(guardPath, fs.constants.R_OK)
    expect(fs.readFileSync(guardPath, 'utf8')).toContain("'use strict'")
  })

  test('a free port is available to this project', () => {
    const outcome = inspectPort({ port: 8081, projectRoot, ...probes({ listener: null }) })
    expect(outcome.state).toBe('free')
    expect(outcome.ok).toBe(true)
  })

  test('a port held from inside this project is this project', () => {
    const outcome = inspectPort({
      port: 8081,
      projectRoot,
      ...probes({ listener: 4321, cwd: path.join(projectRoot, 'ios'), command: 'node metro' })
    })
    expect(outcome.state).toBe('held-by-project')
    expect(outcome.ok).toBe(true)
    expect(outcome.pid).toBe(4321)
  })

  test('a port held from another project is refused, naming the holder', () => {
    const outcome = inspectPort({
      port: 8081,
      projectRoot,
      ...probes({
        listener: 43907,
        cwd: '/elsewhere/other-repo/apps/mobile',
        command: 'node expo run:android'
      })
    })
    expect(outcome.ok).toBe(false)
    expect(outcome.state).toBe('held-by-other')
    const message = describeOutcome(outcome)
    expect(message).toContain('8081')
    expect(message).toContain('/elsewhere/other-repo/apps/mobile')
    expect(message).toContain('43907')
    expect(message).toContain('expo run:android')
  })

  test('a holder whose working directory cannot be read is reported as unknown, never assumed ours', () => {
    const outcome = inspectPort({
      port: 8081,
      projectRoot,
      ...probes({ listener: 51, cwd: null, command: 'some-server' })
    })
    expect(outcome.state).toBe('held-by-unknown')
    expect(outcome.ok).toBe(false)
    expect(describeOutcome(outcome)).toContain('could not read')
  })

  test('a platform without the listener probe says so out loud and does not block', () => {
    const outcome = inspectPort({ port: 8081, projectRoot, ...probes({ available: false }) })
    expect(outcome.state).toBe('undetermined')
    expect(outcome.ok).toBe(true)
    expect(outcome.warning).toBe(true)
    expect(describeOutcome(outcome)).toMatch(/cannot determine/i)
  })

  test('outcomes that let the run continue exit 0; the two refusals are distinguishable', () => {
    const states = Object.keys(EXIT_CODES)
    expect(states).toEqual(
      expect.arrayContaining(['free', 'held-by-project', 'held-by-other', 'held-by-unknown', 'undetermined'])
    )
    expect(EXIT_CODES.free).toBe(0)
    expect(EXIT_CODES['held-by-project']).toBe(0)
    expect(EXIT_CODES.undetermined).toBe(0)
    expect(EXIT_CODES['held-by-other']).not.toBe(0)
    expect(EXIT_CODES['held-by-unknown']).not.toBe(0)
    expect(EXIT_CODES['held-by-other']).not.toBe(EXIT_CODES['held-by-unknown'])
  })
})

describe('the examples run the guard before they start a dev server', () => {
  const bare = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'example', 'package.json'), 'utf8'))

  test('the bare example guards its port on start, ios and android', () => {
    for (const script of ['start', 'ios', 'android']) {
      expect(bare.scripts[script]).toContain('metro-port-guard.js')
    }
  })
})
