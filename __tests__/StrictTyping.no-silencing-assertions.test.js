'use strict'

// D6/D7: no checker-silencing assertions in the TCK helpers or the shipped
// stream presets. Guards replace `as unknown` / `as any`; presets use typed
// constants instead of `as T`.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const files = [
  'src/tck/rust-driver/staged.ts',
  'src/tck/test-only-fault-hooks.ts',
  'src/public/stream-presets.ts'
]

describe('no silencing assertions', () => {
  test.each(files)('%s has no `as` assertion', file => {
    const source = fs.readFileSync(path.join(root, file), 'utf8')
    expect(source).not.toMatch(/\bas\s+(unknown|any)\b/)
    expect(source).not.toMatch(/\bas\s+StreamPreset\b/)
  })

  test('stream preset defaults still type-check as presets', () => {
    const { STREAM_PRESET_DEFAULTS, resolveStreamPreset } = require('../src/public/stream-presets')
    for (const preset of Object.values(STREAM_PRESET_DEFAULTS)) {
      expect(['latest', 'balanced', 'lossless-bounded']).toContain(preset)
      expect(resolveStreamPreset({ preset }).overflowPolicy).toBeDefined()
    }
  })
})
