const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

const root = path.resolve(__dirname, '..')
const normalizer = path.join(root, 'bindings/uniffi/normalize-generated.js')

test('normalizes only UniFFI source files, preserving content and newlines', () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-uniffi-whitespace-'))
  try {
    const sources = [
      'kotlin/uniffi/ubm_echo/ubm_echo.kt',
      'swift/ubm_echo.swift',
      'swift/ubm_echoFFI.h',
      'python/ubm_echo.py'
    ]
    for (const source of sources) {
      const file = path.join(directory, source)
      fs.mkdirSync(path.dirname(file), { recursive: true })
      fs.writeFileSync(file, 'value  \n  \nnext\t\r\nlast  ')
    }
    const modulemap = path.join(directory, 'swift/ubm_echoFFI.modulemap')
    fs.writeFileSync(modulemap, 'module test { }  \n')

    for (let run = 0; run < 2; run += 1) {
      const result = spawnSync(process.execPath, [normalizer, directory], { encoding: 'utf8' })
      expect(result.status).toBe(0)
      for (const source of sources) {
        expect(fs.readFileSync(path.join(directory, source), 'utf8')).toBe('value\n\nnext\r\nlast')
      }
      expect(fs.readFileSync(modulemap, 'utf8')).toBe('module test { }  \n')
    }
  } finally {
    fs.rmSync(directory, { recursive: true, force: true })
  }
})

test('round-trip recipe normalizes generated output before comparison', () => {
  const runner = fs.readFileSync(path.join(root, 'bindings/uniffi/run_uniffi_roundtrip.sh'), 'utf8')
  expect(runner.indexOf('node normalize-generated.js "$REGEN"')).toBeGreaterThan(
    runner.indexOf('--language python --out-dir "$REGEN/python"')
  )
  expect(runner.indexOf('node normalize-generated.js "$REGEN"')).toBeLessThan(
    runner.indexOf('diff -r generated/kotlin')
  )
})

test('UniFFI config preserves the checked-in Kotlin and Python loader names', () => {
  const config = fs.readFileSync(path.join(root, 'bindings/uniffi/uniffi.toml'), 'utf8')
  expect(config).toMatch(/\[bindings\.kotlin\]\s+cdylib_name = "uniffi_ubm_echo"/)
  expect(config).toMatch(/\[bindings\.python\]\s+cdylib_name = "uniffi"/)
})

test('round-trip Python fixture uses its configured platform library name', () => {
  const runner = fs.readFileSync(path.join(root, 'bindings/uniffi/run_uniffi_roundtrip.sh'), 'utf8')
  const python = fs.readFileSync(path.join(root, 'bindings/uniffi/tests/python_roundtrip.py'), 'utf8')
  expect(runner).toContain('cp "$LIB" "$PYRUN/libuniffi.$LIB_SUFFIX"')
  expect(python).toContain('"libuniffi." + LIB_SUFFIX')
  expect(python).not.toContain('"libubm5_uniffi_echo.so"')
})
