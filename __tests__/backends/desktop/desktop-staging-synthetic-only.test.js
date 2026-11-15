'use strict'

// Every N-API hook that stages synthetic radio state (or blocks/fault-scripts
// radio operations) is synthetic-only: its body reaches the radio through
// `DispatchRadio::synthetic`, which refuses a production central with
// `capability.unsupported` before anything touches hardware. The production
// radio cannot be opened in a test, so the refusal is proven on the source:
// a hook that bypasses the guard fails here.
//
// `stage_terminal_evidence` is deliberately different: it persists an
// already-reserved terminal record in the trusted host ledger and must work
// with a production radio. Keep that ledger-only path explicitly tested, so a
// future broad `stage*` matcher cannot accidentally turn it into a synthetic
// radio hook.

const fs = require('node:fs')
const path = require('node:path')

const DISPATCH = path.join(__dirname, '..', '..', '..', 'bindings', 'napi', 'src', 'dispatch.rs')
const SYNTHETIC_HOOK =
  /pub async fn ((?:stage_(?!terminal_evidence\b)|staged|block_radio_op|unblock_radio_op|fail_next_radio_op)\w*)\s*\(/gu
const TERMINAL_EVIDENCE_STAGE = /pub async fn (stage_terminal_evidence)\s*\(/gu

function hooksMatching(source, matcher) {
  const hooks = []
  for (const match of source.matchAll(matcher)) {
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

const syntheticStagingHooks = source => hooksMatching(source, SYNTHETIC_HOOK)
const terminalEvidenceStage = source => hooksMatching(source, TERMINAL_EVIDENCE_STAGE)[0]

describe('N-API staging hooks are synthetic-only', () => {
  const source = fs.readFileSync(DISPATCH, 'utf8')
  const hooks = syntheticStagingHooks(source)

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

  test('terminal evidence staging is a production ledger-only operation, not synthetic radio staging', () => {
    const hook = terminalEvidenceStage(source)
    expect(hook).toBeDefined()
    expect(hook.body).toContain('self.terminal_evidence()')
    expect(hook.body).toContain('.stage(&reservation_id, &recipient, canonical_bytes.as_ref())')
    expect(hook.body).not.toContain('self.central')
    expect(hook.body).not.toContain('.synthetic(')
    expect(hooks.map(candidate => candidate.name)).not.toContain('stage_terminal_evidence')
  })
})
