'use strict'

const { execFileSync } = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

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

// where.exe returns native paths on Windows; unlike `bash command -v`, those
// remain meaningful to this Node process. Skip Store aliases with no payload.
function findExecutableOnPath(names) {
  if (process.platform === 'win32') {
    const systemRoot = process.env.SystemRoot ?? 'C:\\Windows'
    const where = path.join(systemRoot, 'System32', 'where.exe')
    for (const name of names) {
      try {
        const candidates = execFileSync(where, [name], { encoding: 'utf8' }).split(/\r?\n/)
        for (const candidate of candidates) {
          if (candidate === '') continue
          const stat = fs.statSync(candidate)
          if (stat.isFile() && stat.size > 0) return candidate
        }
      } catch {
        // This name is absent or only resolves to an unreadable candidate.
      }
    }
    return ''
  }

  for (const entry of process.env.PATH.split(path.delimiter)) {
    if (entry === '') continue
    for (const name of names) {
      const candidate = path.join(entry, name)
      try {
        const stat = fs.statSync(candidate)
        if (stat.isFile() && stat.size > 0) return candidate
      } catch {
        // Missing or unreadable: keep scanning.
      }
    }
  }
  return ''
}

function shimExecutableIntoDir(shim, source, name) {
  const target = path.join(shim, name ?? path.basename(source))
  if (process.platform === 'win32') {
    fs.copyFileSync(source, target)
    try {
      fs.chmodSync(target, 0o755)
    } catch {
      // Best-effort: chmod is a no-op on Windows, where the extension resolves.
    }
  } else {
    fs.symlinkSync(fs.realpathSync(source), target)
  }
  return target
}

function shimPythonIntoDir(shim, source) {
  if (process.platform !== 'win32') return shimExecutableIntoDir(shim, source, 'python')
  // CPython loads DLLs beside python.exe, so copying that executable alone
  // cannot form a runnable shim. Execute the installed interpreter in place.
  const shellSource = execFileSync('bash', ['-lc', 'cygpath -u -- "$1"', 'bash', source], {
    encoding: 'utf8'
  }).trim()
  const quotedSource = shellSource.replace(/'/g, "'\"'\"'")
  const target = path.join(shim, 'python')
  fs.writeFileSync(target, `#!/bin/sh\nexec '${quotedSource}' "$@"\n`, { mode: 0o755 })
  return target
}

module.exports = { findExecutableOnPath, gitBashExecutableTemp, shimExecutableIntoDir, shimPythonIntoDir }
