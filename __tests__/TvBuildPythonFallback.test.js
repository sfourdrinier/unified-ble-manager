'use strict'

// G4: example-expo/scripts/build-tv.sh resolves PYTHON3 once at load
// (prefer python3, fall back to python) for every subcommand, but only the
// bundle-url sites were exercised with python3 absent. These tests pin the
// fallback for the stage and xcode_scheme sites without touching the script:
// every python invocation must go through ${PYTHON3}, and both sites must
// succeed with python3 hidden from PATH. (cmd_prebuild invokes no python, so
// there is no fallback site to exercise there.)
//
// TV_STAGE_DIR redirects the stage to a temporary directory so these tests
// never touch the real example-expo/ios-tv tree.

const { execFileSync } = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { gitBashExecutableTemp } = require('./helpers/git-bash-temp')

const SCRIPT = path.join(__dirname, '..', 'example-expo', 'scripts', 'build-tv.sh')
const ROOT = path.join(__dirname, '..')

function run(args, env) {
  try {
    const stdout = execFileSync('bash', [SCRIPT, ...args], {
      env: { ...process.env, ...env },
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe']
    })
    return { exit: 0, stdout }
  } catch (error) {
    return { exit: error.status ?? 1, stdout: error.stdout ?? '', stderr: error.stderr ?? '' }
  }
}

function stageDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-tv-stage-'))
}

// PATH with every python3-bearing directory removed and `python` shimmed to
// a real interpreter, re-shimming any tool the exercised path needs from a
// removed directory. `command -v python3` fails while `command -v python`
// succeeds, so only the ${PYTHON3} fallback can satisfy the script.
//
// Discovery runs in Node, not through `bash -lc 'command -v ...'`: on
// Windows that prints MSYS paths (/c/...) Node cannot resolve, and the
// first `python3` hit may be the 0-byte Microsoft Store stub. PATH entries
// are scanned directly, zero-byte stubs are skipped, and entries are copied
// (never symlinked — file symlinks need privilege on Windows).
function findOnPath(names) {
  const extensions = process.platform === 'win32' ? ['', '.exe', '.cmd', '.bat'] : ['']
  for (const entry of process.env.PATH.split(path.delimiter)) {
    if (entry === '') continue
    for (const name of names) {
      for (const extension of extensions) {
        const candidate = path.join(entry, name + extension)
        try {
          const stat = fs.statSync(candidate)
          if (stat.isFile() && stat.size > 0) return candidate
        } catch {
          // Missing or unreadable: keep scanning.
        }
      }
    }
  }
  return ''
}

function shimIntoDir(shim, source, name) {
  const target = path.join(shim, name ?? path.basename(source))
  if (process.platform === 'win32') {
    // File symlinks need privilege on Windows; copies run fine there.
    fs.copyFileSync(source, target)
    try {
      fs.chmodSync(target, 0o755)
    } catch {
      // Best-effort: chmod is a no-op on Windows, where the extension resolves.
    }
  } else {
    // Copies of platform-signed binaries are killed on exec; symlinking
    // keeps the original (and needs no privilege here).
    fs.symlinkSync(fs.realpathSync(source), target)
  }
  return target
}

function dirProvidesPython3(entry) {
  const names = process.platform === 'win32' ? ['python3', 'python3.exe'] : ['python3']
  return names.some(name => {
    try {
      fs.accessSync(path.join(entry, name), fs.constants.X_OK)
      return true
    } catch {
      return false
    }
  })
}

function pythonFallbackPath(tools) {
  const shim = fs.mkdtempSync(path.join(gitBashExecutableTemp(), 'ubm-py-shim-'))
  // A real interpreter to expose as `python` (on Windows there is no
  // python3 beyond the Store stub, so the real `python` is the source).
  const discovered = findOnPath(process.platform === 'win32' ? ['python'] : ['python3'])
  expect(discovered).not.toBe('')
  const resolved = p => {
    try {
      return fs.realpathSync(p)
    } catch {
      return p
    }
  }
  shimIntoDir(shim, discovered, process.platform === 'win32' ? 'python.exe' : 'python')
  const excluded = new Set()
  const kept = process.env.PATH.split(path.delimiter).filter(entry => {
    if (entry === '') return false
    if (dirProvidesPython3(entry)) {
      excluded.add(resolved(entry))
      return false
    }
    return true
  })
  expect(excluded.size).toBeGreaterThan(0)
  for (const tool of tools) {
    const found = findOnPath([tool])
    if (found !== '' && excluded.has(resolved(path.dirname(found)))) {
      shimIntoDir(shim, found)
    }
  }
  return { shim, pathValue: `${shim}${path.delimiter}${kept.join(path.delimiter)}` }
}

describe('build-tv.sh PYTHON3 fallback beyond bundle-url', () => {
  test('every python invocation goes through ${PYTHON3}', () => {
    const lines = fs.readFileSync(SCRIPT, 'utf8').split('\n')
    const uses = lines.filter(line => !line.trimStart().startsWith('#') && /python/i.test(line))
    for (const line of uses) {
      // Four legitimate shapes: an invocation through ${PYTHON3}, a line of
      // the resolver itself (its `command -v` fallback or its error text), or
      // a command declaring that it needs one. Anything else is a bare
      // interpreter call, which is the bug this pins.
      const throughResolver =
        line.includes('PYTHON3') ||
        line.includes('require_python') ||
        line.includes('command -v python') ||
        line.includes('needs python3')
      expect(throughResolver).toBe(true)
    }
    // The resolver's eight lines (default, definition, two candidate
    // resolutions, two execute-checks, fail-closed guard, error), four
    // `require_python` declarations, and five call sites: two in stage, two
    // in bundle-url, one in xcode_scheme. A new bare-python site breaks the
    // assertion above; a removed fallback site or an unguarded command
    // breaks this count.
    expect(uses).toHaveLength(17)
  })

  test('stage rewrites the staged tree through the python fallback', () => {
    const stage = stageDir()
    // find/tar are the stage sync itself (no rsync on Windows); node
    // runs the staged package.json rewrite.
    const { shim, pathValue } = pythonFallbackPath(['bash', 'dirname', 'node', 'rm', 'mkdir', 'find', 'tar'])
    try {
      const result = run(['stage'], { TV_STAGE_DIR: stage, PATH: pathValue })
      expect(result.exit).toBe(0)
      const metro = fs.readFileSync(path.join(stage, 'metro.config.js'), 'utf8')
      // The staged path arrives via a native Windows python, whose argv
      // MSYS reports with forward slashes (C:/...), while path.join uses
      // backslashes — compare separator-insensitively (forward slashes are
      // also the safe spelling inside the staged JS string literal).
      const forward = s => s.replace(/\\/g, '/')
      expect(forward(metro)).toContain(forward(path.join(ROOT, 'examples-shared')))
      expect(metro).not.toContain("path.resolve(projectRoot, '../examples-shared')")
      const shared = fs.readFileSync(path.join(stage, 'src', 'driver', 'shared.ts'), 'utf8')
      expect(shared).toContain("from '../../../../examples-shared/driver/index.ts'")
    } finally {
      fs.rmSync(stage, { recursive: true, force: true })
      fs.rmSync(shim, { recursive: true, force: true })
    }
  }, 120000)

  test('Windows fallback executes when TEMP is outside Git Bash /tmp', () => {
    if (process.platform !== 'win32') return

    const stage = stageDir()
    // GitHub's Windows runners may point TEMP at D:\\a\\_temp. Git Bash can
    // resolve that directory on PATH, but a copied python.exe cannot execute
    // there. Exercise the same split explicitly instead of relying on the
    // runner's TEMP layout.
    const forcedTemp = fs.mkdtempSync(path.join(path.parse(stage).root, 'ubm-tv-non-msys-temp-'))
    const previousTemp = process.env.TEMP
    const previousTmp = process.env.TMP
    process.env.TEMP = forcedTemp
    process.env.TMP = forcedTemp
    let shim = ''
    try {
      const fallback = pythonFallbackPath(['bash', 'dirname', 'node', 'rm', 'mkdir', 'find', 'tar'])
      shim = fallback.shim
      if (previousTemp === undefined) delete process.env.TEMP
      else process.env.TEMP = previousTemp
      if (previousTmp === undefined) delete process.env.TMP
      else process.env.TMP = previousTmp
      const result = run(['stage'], {
        TV_STAGE_DIR: stage,
        PATH: fallback.pathValue
      })
      expect(result.exit).toBe(0)
    } finally {
      if (previousTemp === undefined) delete process.env.TEMP
      else process.env.TEMP = previousTemp
      if (previousTmp === undefined) delete process.env.TMP
      else process.env.TMP = previousTmp
      fs.rmSync(stage, { recursive: true, force: true })
      if (shim !== '') fs.rmSync(shim, { recursive: true, force: true })
      fs.rmSync(forcedTemp, { recursive: true, force: true })
    }
  }, 120000)

  test('xcode_scheme parses schemes through the python fallback', () => {
    const stage = stageDir()
    const { shim, pathValue } = pythonFallbackPath(['bash', 'sed', 'grep', 'head'])
    try {
      fs.mkdirSync(path.join(stage, 'ios', 'dummy.xcodeproj'), { recursive: true })
      fs.writeFileSync(
        path.join(shim, 'xcodebuild'),
        '#!/bin/sh\necho \'{"project":{"schemes":["BlePlxExample","BlePlxExample-tvOS"]}}\'\n',
        { mode: 0o755 }
      )
      const program = [
        'set -euo pipefail',
        'eval "$(sed -n \'/^PYTHON3="/p\' "$TV_SCRIPT")"',
        // The interpreter is resolved lazily now, so the extracted function
        // brings its own resolver with it: that is the behaviour under test.
        'eval "$(sed -n \'/^require_python()/,/^}/p\' "$TV_SCRIPT")"',
        'eval "$(sed -n \'/^xcode_scheme()/,/^}/p\' "$TV_SCRIPT")"',
        'STAGE="$TV_STAGE" xcode_scheme'
      ].join('\n')
      const stdout = execFileSync('bash', ['-c', program], {
        env: { ...process.env, TV_SCRIPT: SCRIPT, TV_STAGE: stage, PATH: pathValue },
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'pipe']
      })
      expect(stdout.trim()).toBe('BlePlxExample-tvOS')
    } finally {
      fs.rmSync(stage, { recursive: true, force: true })
      fs.rmSync(shim, { recursive: true, force: true })
    }
  }, 120000)
})
