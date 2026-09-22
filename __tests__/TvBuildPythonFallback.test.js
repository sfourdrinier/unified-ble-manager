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

const { execFileSync, spawnSync } = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

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
// the real python3, re-shimming any tool the exercised path needs from a
// removed directory. `command -v python3` fails while `command -v python`
// succeeds, so only the ${PYTHON3} fallback can satisfy the script.
function pythonFallbackPath(tools) {
  const shim = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-py-shim-'))
  const discoveredPy3 = spawnSync('bash', ['-lc', 'command -v python3'], { encoding: 'utf8' })
    .stdout.trim()
    .split('\n')[0]
  expect(discoveredPy3).not.toBe('')
  const resolved = p => {
    try {
      return fs.realpathSync(p)
    } catch {
      return p
    }
  }
  const isExec = p => {
    try {
      fs.accessSync(p, fs.constants.X_OK)
      return true
    } catch {
      return false
    }
  }
  fs.symlinkSync(fs.realpathSync(discoveredPy3), path.join(shim, 'python'))
  const excluded = new Set()
  const kept = process.env.PATH.split(path.delimiter).filter(entry => {
    if (entry === '') return false
    if (isExec(path.join(entry, 'python3'))) {
      excluded.add(resolved(entry))
      return false
    }
    return true
  })
  expect(excluded.size).toBeGreaterThan(0)
  for (const tool of tools) {
    const found = spawnSync('bash', ['-lc', `command -v ${tool}`], { encoding: 'utf8' })
      .stdout.trim()
      .split('\n')[0]
    if (found !== '' && excluded.has(resolved(path.dirname(found)))) {
      fs.symlinkSync(fs.realpathSync(found), path.join(shim, tool))
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
    // The resolver's five lines, four `require_python` declarations, and five
    // call sites: two in stage, two in bundle-url, one in xcode_scheme. A new
    // bare-python site breaks the assertion above; a removed fallback site or
    // an unguarded command breaks this count.
    expect(uses).toHaveLength(15)
  })

  test('stage rewrites the staged tree through the python fallback', () => {
    const stage = stageDir()
    const { shim, pathValue } = pythonFallbackPath(['bash', 'dirname', 'rsync', 'node', 'rm', 'mkdir'])
    try {
      const result = run(['stage'], { TV_STAGE_DIR: stage, PATH: pathValue })
      expect(result.exit).toBe(0)
      const metro = fs.readFileSync(path.join(stage, 'metro.config.js'), 'utf8')
      expect(metro).toContain(path.join(ROOT, 'examples-shared'))
      expect(metro).not.toContain("path.resolve(projectRoot, '../examples-shared')")
      const shared = fs.readFileSync(path.join(stage, 'src', 'driver', 'shared.ts'), 'utf8')
      expect(shared).toContain("from '../../../../examples-shared/driver/index.ts'")
    } finally {
      fs.rmSync(stage, { recursive: true, force: true })
      fs.rmSync(shim, { recursive: true, force: true })
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
