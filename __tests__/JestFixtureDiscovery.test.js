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
    expect(ignored(fixturePath('fixtures/discovery-compat.ts'))).toBe(true)
    expect(ignored(fixturePath('web/web-active-upgrade-browser-fixture.js'))).toBe(true)
    expect(ignored(fixturePath('web/web-ordered-recording-journal-helper.js'))).toBe(true)
    expect(ignored(fixturePath('web/web-cold-recovery-browser-fixture.js'))).toBe(true)
    expect(ignored(fixturePath('web/web-real-indexeddb-upgrade-fixture.ts'))).toBe(true)
    expect(ignored(fixturePath('web/web-marker-ledger-upgrade-522-browser-fixture.js'))).toBe(true)
    expect(ignored(fixturePath('web/web-marker-ledger-transactions-522-browser-fixture.js'))).toBe(true)
    expect(ignored(fixturePath('web/web-ordered-recording-journal.test.js'))).toBe(false)
    expect(ignored(fixturePath('PackedHostConsumerCheck.test.js'))).toBe(false)
    expect(ignored(fixturePath('backends/desktop/desktop-dispatch-radio-forwarding.test.js'))).toBe(false)
  }
)

test.each([packageConfig, parityConfig])("uses YAML's CommonJS entrypoint under the React Native Jest resolver", config => {
  expect(config.moduleNameMapper['^yaml$']).toBe('<rootDir>/node_modules/yaml/dist/index.js')
})

test.each([packageConfig, parityConfig].flatMap(config => ['/', '\\'].map(separator => [config, separator])))(
  'transforms React Native modules but ignores ordinary dependencies on %s %s',
  (config, separator) => {
    let normalized
    jest.isolateModules(() => {
      jest.doMock('path', () => ({ ...path, sep: separator }))
      normalized = config.transformIgnorePatterns.map(require(regexUtility).replacePathSepForRegex)
    })
    jest.dontMock('path')
    const ignored = filename => normalized.some(pattern => new RegExp(pattern).test(filename.replace(/\//g, separator)))
    expect(ignored('/work/node_modules/typescript/lib/typescript.js')).toBe(true)
    expect(ignored('/work/node_modules/lodash/index.js')).toBe(true)
    expect(ignored('/work/node_modules/react-native/index.js')).toBe(false)
    expect(ignored('/work/node_modules/.pnpm/react-native@0.86/node_modules/react-native/index.js')).toBe(false)
  }
)
