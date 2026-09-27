const fs = require('fs')
const path = require('path')

test('loading recording controls does not install the Apple radio as a module side effect', () => {
  const module = fs.readFileSync(path.join(__dirname, '../../ios/UnifiedBleRustCore.mm'), 'utf8')
  expect(module).not.toContain('[[UnifiedBleRustCoreSessions shared] ensureHost]')
  const bootstrap = fs.readFileSync(path.join(__dirname, '../../ios/UnifiedBleContinuationBootstrap.mm'), 'utf8')
  expect(bootstrap).toContain('bootstrapNativeContinuation')
})
