// __tests__/IosRustCoreSelection.test.js
//
// F01 iOS lane: the 5.x podspec selects the shared Rust core. Runs the
// static selection check (fail-closed on any drift).

'use strict'

const { execFileSync } = require('child_process')
const path = require('path')

test('5.x podspec selects the Rust core beside the Owned radio', () => {
  const script = path.join(__dirname, '..', 'scripts', 'ci', 'check-podspec-rust-selection.js')
  const output = execFileSync(process.execPath, [script], { encoding: 'utf8' })
  expect(output).toContain('podspec-rust-selection PASS')
})
