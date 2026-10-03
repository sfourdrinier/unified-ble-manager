const path = require('node:path')
const packageConfig = require('../jest.package.config')
const parityConfig = require('../jest.config')

test.each([packageConfig, parityConfig])('bundler consumer inputs are fixtures, not Jest suites', config => {
  const ignored = filename => config.testPathIgnorePatterns.some(pattern => new RegExp(pattern).test(filename))
  expect(ignored(path.join(__dirname, 'fixtures/tauri-bundler/main.js'))).toBe(true)
  expect(ignored(path.join(__dirname, 'PackedHostConsumerCheck.test.js'))).toBe(false)
  expect(ignored(path.join(__dirname, 'backends/desktop/desktop-dispatch-radio-forwarding.test.js'))).toBe(false)
})
