// __tests__/SelectXcode.test.js

const { selectXcode, parseXcodeVersion } = require('../scripts/ci/select-xcode')

describe('SDK-specific Xcode selection', () => {
  const installations = [
    { path: '/Applications/Xcode_26.4.app', version: 'Xcode 26.4\nBuild version a' },
    { path: '/Applications/Xcode_26.6.app', version: 'Xcode 26.6\nBuild version b' },
    { path: '/Applications/Xcode_27.0.app', version: 'Xcode 27.0\nBuild version c' }
  ]

  test('keeps SDK 57 on the newest installed Xcode 26', () => {
    expect(selectXcode(installations, '26').path).toBe('/Applications/Xcode_26.6.app')
  })

  test('SDK 58 requires Xcode 27 rather than silently selecting 26', () => {
    expect(selectXcode(installations, '27').path).toBe('/Applications/Xcode_27.0.app')
    expect(() => selectXcode(installations.slice(0, 2), '27')).toThrow('Xcode 27')
  })

  test('refuses SDK 57 toolchains older than its existing 26.4 floor', () => {
    expect(() => selectXcode([{ path: 'old', version: 'Xcode 26.3' }], '26')).toThrow('26.4')
  })

  test('compares minor and patch components numerically', () => {
    expect(selectXcode([
      { path: 'nine', version: 'Xcode 27.9' },
      { path: 'ten', version: 'Xcode 27.10.1' }
    ], '27').path).toBe('ten')
  })

  test('invalid series and malformed tool output fail closed', () => {
    expect(() => selectXcode(installations, '28')).toThrow('26 or 27')
    expect(() => parseXcodeVersion('not Xcode')).toThrow('Xcode version')
  })
})
