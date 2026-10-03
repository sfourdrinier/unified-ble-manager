const { withCleanup, runNpmPack } = require('../scripts/ci/pack-fixture-process')
const path = require('node:path')

function resolveFixtureNpm(platform, environment, files, links = {}) {
  const platformProperty = Object.getOwnPropertyDescriptor(process, 'platform')
  Object.defineProperty(process, 'platform', { ...platformProperty, value: platform })
  let result
  try {
    jest.isolateModules(() => {
      jest.doMock('node:path', () => (platform === 'win32' ? path.win32 : path.posix))
      jest.doMock('node:fs', () => ({
        existsSync: filename => files.includes(filename),
        realpathSync: filename => {
          if (!Object.hasOwn(links, filename)) throw new Error(`Unmodeled realpath: ${filename}`)
          return links[filename]
        }
      }))
      result = require('../scripts/ci/pack-fixture-process').npmCli(environment)
    })
    return result
  } finally {
    Object.defineProperty(process, 'platform', platformProperty)
    jest.dontMock('node:path')
    jest.dontMock('node:fs')
  }
}

test('Windows PATH casing and missing first shim CLI select a later usable CLI', () => {
  const expected = 'C:\\node\\node_modules\\npm\\bin\\npm-cli.js'
  expect(
    resolveFixtureNpm('win32', { Path: 'C:\\bad;C:\\node' }, ['C:\\bad\\npm.cmd', 'C:\\node\\npm.cmd', expected])
  ).toBe(expected)
})

test.each([{ PATH: 'C:\\bad' }, { pAtH: 'C:\\bad' }, {}])(
  'missing Windows npm CLI fails closed for %s',
  environment => {
    expect(() => resolveFixtureNpm('win32', environment, ['C:\\bad\\npm.cmd'])).toThrow('Cannot locate npm CLI')
  }
)

test('POSIX executable symlink resolves to its actual npm CLI', () => {
  const expected = '/opt/node/lib/node_modules/npm/bin/npm-cli.js'
  expect(
    resolveFixtureNpm('linux', { PATH: '/usr/bin:/opt/node/bin' }, ['/opt/node/bin/npm', expected], {
      '/opt/node/bin/npm': expected
    })
  ).toBe(expected)
})

test('pack runs the npm CLI directly without an orphanable command shell', () => {
  const spawn = jest.fn(() => ({ status: 0, stdout: '[]', stderr: '' }))
  runNpmPack('/fixture', {}, '/npm/npm-cli.js', spawn)
  expect(spawn).toHaveBeenCalledWith(
    process.execPath,
    ['/npm/npm-cli.js', 'pack', '--ignore-scripts', '--json', '--loglevel=warn'],
    expect.objectContaining({ shell: false, timeout: 30000, cwd: '/fixture' })
  )
})

test('cleanup failure preserves primary failure and reports both in order', () => {
  const primary = Object.assign(new Error('npm timed out'), { code: 'ETIMEDOUT' })
  const cleanup = Object.assign(new Error('fixture locked'), { code: 'EBUSY' })
  try {
    withCleanup(
      () => {
        throw primary
      },
      () => {
        throw cleanup
      }
    )
    throw new Error('expected failure')
  } catch (error) {
    expect(error).toBeInstanceOf(AggregateError)
    expect(error.errors).toEqual([primary, cleanup])
    expect(error.cause).toBe(primary)
  }
})

test('cleanup alone fails rather than being silently swallowed', () => {
  const failure = new Error('cleanup failed')
  expect(() =>
    withCleanup(
      () => 1,
      () => {
        throw failure
      }
    )
  ).toThrow(failure)
})

test('npm timeout remains visible when fixture cleanup also fails', () => {
  const timeout = Object.assign(new Error('spawn timeout'), { code: 'ETIMEDOUT' })
  const spawn = jest.fn(() => ({ error: timeout, status: null, stdout: '', stderr: '' }))
  const cleanup = new Error('locked working directory')
  expect(() =>
    withCleanup(
      () => {
        const result = runNpmPack('/fixture', {}, '/npm/npm-cli.js', spawn)
        if (result.error) throw result.error
      },
      () => {
        throw cleanup
      }
    )
  ).toThrow(expect.objectContaining({ cause: timeout, errors: [timeout, cleanup] }))
})
