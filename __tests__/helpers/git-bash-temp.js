'use strict'

const { execFileSync } = require('node:child_process')
const os = require('node:os')

// Windows GitHub runners can put TEMP on D:, where Git Bash resolves a PATH
// entry but a copied python.exe exits 127. Git Bash's own /tmp is on an
// executable Windows volume. Keep only test command shims there; callers keep
// their real artifacts in os.tmpdir().
function gitBashExecutableTemp() {
  if (process.platform !== 'win32') return os.tmpdir()
  // CI launches Jest from PowerShell, whose PATH exposes Git's `bash.exe`
  // but not necessarily its `usr/bin/cygpath.exe`. Resolve inside Git Bash,
  // matching the shell that later executes the shim.
  return execFileSync('bash', ['-lc', 'cygpath -w /tmp'], { encoding: 'utf8' }).trim()
}

module.exports = { gitBashExecutableTemp }
