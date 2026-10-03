// Executable tests of the ownership primitives used by the production WinRT adapter.
// Rust models compile on every host; this is not a Windows radio qualification.
'use strict'

const { spawnSync } = require('node:child_process')
const path = require('node:path')
const result = spawnSync('cargo', ['test', '--locked', '-p', 'ubm-desktop', '--lib', 'os::winrt_cleanup::tests'], {
  cwd: path.resolve(__dirname, '../..'),
  stdio: 'inherit',
  shell: false
})
if (result.error) throw result.error
if (result.status !== 0) throw new Error(`Production WinRT ownership tests failed: ${String(result.status)}`)
