const path = require('node:path')
const { createRequire } = require('node:module')
const jestRequire = createRequire(require.resolve('jest'))
const configRequire = createRequire(jestRequire.resolve('jest-config'))
const regexUtility = configRequire.resolve('jest-regex-util')
const packageConfig = require('../jest.package.config')
const parityConfig = require('../jest.config')

test.each([packageConfig, parityConfig].flatMap(config => ['/', '\\'].map(separator => [config, separator])))(
  'bundler consumer inputs are fixtures, not Jest suites with %s %s',
  (config, separator) => {
    // Jest normalizes these regex patterns for the host separator before discovery.
    // Exercise its real utility under both host separators, not a guessed copy.
    let normalized
    jest.isolateModules(() => {
      jest.doMock('path', () => ({ ...path, sep: separator }))
      normalized = config.testPathIgnorePatterns.map(require(regexUtility).replacePathSepForRegex)
    })
    jest.dontMock('path')
    const ignored = filename => normalized.some(pattern => new RegExp(pattern).test(filename))
    const fixturePath = relative => path.join(__dirname, relative).replace(/[\\/]/g, separator)
    expect(ignored(fixturePath('fixtures/tauri-bundler/main.js'))).toBe(true)
    expect(ignored(fixturePath('PackedHostConsumerCheck.test.js'))).toBe(false)
    expect(ignored(fixturePath('backends/desktop/desktop-dispatch-radio-forwarding.test.js'))).toBe(false)
  }
)
