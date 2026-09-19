'use strict'

// Every N-API staging hook (`stage*`, `staged*`, radio-op blocking and fault
// scripting) is synthetic-only: its body reaches the radio through
// `DispatchRadio::synthetic`, which refuses a production central with
// `capability.unsupported` before anything touches hardware. The production
// radio cannot be opened in a test, so the refusal is proven on the source:
// a hook that bypasses the guard fails here.

const fs = require('node:fs')
const path = require('node:path')

const DISPATCH = path.join(__dirname, '..', '..', '..', 'bindings', 'napi', 'src', 'dispatch.rs')
const HOOK = /pub async fn ((?:stage|staged|block_radio_op|unblock_radio_op|fail_next_radio_op)\w*)\s*\(/gu

function stagingHooks(source) {
  const hooks = []
  for (const match of source.matchAll(HOOK)) {
    const bodyStart = source.indexOf('{', match.index + match[0].length)
    let depth = 0
    let end = bodyStart
    for (; end < source.length; end += 1) {
      if (source[end] === '{') depth += 1
      if (source[end] === '}') {
        depth -= 1
        if (depth === 0) break
      }
    }
    hooks.push({ name: match[1], body: source.slice(bodyStart, end + 1) })
  }
  return hooks
}

describe('N-API staging hooks are synthetic-only', () => {
  const hooks = stagingHooks(fs.readFileSync(DISPATCH, 'utf8'))

  test('the hook scan finds the per-instance value and GATT access hooks', () => {
    expect(hooks.map(hook => hook.name)).toEqual(
      expect.arrayContaining(['stage_characteristic_value', 'staged_gatt_accesses', 'stage_services'])
    )
  })

  test.each(hooks.map(hook => [hook.name, hook]))(
    '%s reaches the radio only through the synthetic guard',
    (_name, hook) => {
      expect(hook.body).toMatch(/\.synthetic\("dispatch\.[a-z-]+"\)/u)
    }
  )
})
