// __tests__/tck/rust-parity/staged-faults.test.js
//
// U7 staged race/fault vectors: duplicate completions, stale generations,
// cancel-across-boundaries, overflow terminals, invalidation mid-IO,
// live-peer disconnect failures (per code, through the release-failed
// terminal), and out-of-order settle ordinals (first valid contender wins;
// reordered duplicates suppress) — all driven through the REAL napi staged
// core from synthetic host events (no BLE hardware exists). Every vector
// pins exact wires and receipts: faults settle truthfully, never silently,
// and bounded-batch overflow preserves dropped-not-staged accounting.

const path = require('node:path')
const {
  normalizeStagedLines,
  RUST_PARITY_REVISION,
  RustBackendDriver,
  STAGED_CAPABILITY_ROWS
} = require('../../../src/tck/rust-driver')

const ADDON_PATH =
  process.env.UBM_NAPI_ADDON ||
  path.join(__dirname, '..', '..', '..', 'bindings', 'napi', 'ubm_echo.linux-x64.node')

function loadRustAddon() {
  let addon
  try {
    addon = require(ADDON_PATH)
  } catch (error) {
    throw new Error(
      `U7 staged faults need the real napi build at ${ADDON_PATH} ` +
        '(run pnpm test:parity first): ' +
        (error && error.message)
    )
  }
  return addon
}

const SVC = '12345678-1234-5678-1234-56789abcdef0'
const CHR = '12345678-1234-5678-1234-56789abcdef1'
const CCCD = '00002902-0000-1000-8000-00805f9b34fb'

function linkDriver(addon) {
  const driver = new RustBackendDriver(addon, RUST_PARITY_REVISION)
  const run = line => {
    const outcome = driver.stagedStep(line)
    if (!outcome.ok) {
      throw new Error(`binding-lifetime rejection for ${line}: ${outcome.error.code}`)
    }
    return outcome.value
  }
  run('{"step":"peer.advertise","peer":"p","domain":"platform-guid","value":"peer-1"}')
  run('{"step":"link.connect","peer":"p","lease":"lease-a","op":"conn0"}')
  run('{"step":"link.established","peer":"p","op":"conn0"}')
  run(
    `{"step":"gatt.discover","peer":"p","owner":"lease-a","services":[{` +
      `"uuid":"${SVC}","occurrence":0,"characteristics":[{` +
      `"uuid":"${CHR}","occurrence":0,"properties":"read+write+notify",` +
      `"descriptors":[{"uuid":"${CCCD}","occurrence":0}]}]}]}`
  )
  return { driver, run }
}

function normalized(lines) {
  return normalizeStagedLines(lines)
}

describe('U7 staged race/fault vectors (synthetic radio, real core)', () => {
  test('duplicate completions suppress without a second settlement', () => {
    const addon = loadRustAddon()
    const { driver, run } = linkDriver(addon)
    run('{"step":"gatt.read","op":"r0","path":1,"value":"aa","settle":"dispatched"}')
    const [first, dup] = normalized([
      run('{"step":"op.settle","op":"r0","kind":"success"}'),
      run('{"step":"op.settle","op":"r0","kind":"success"}')
    ])
    expect(JSON.parse(first)).toMatchObject({ settle: 'settled', terminal: 'succeeded' })
    const duplicate = JSON.parse(dup)
    expect(duplicate.settle).toBe('duplicate-suppressed')
    expect(duplicate.suppressed).toBe(1)
    driver.close()
  })

  test('stale generations settle truthfully and stay stale after rediscovery', () => {
    const addon = loadRustAddon()
    const { driver, run } = linkDriver(addon)
    run('{"step":"gatt.read","op":"r0","path":1,"value":"aa","settle":"admitted"}')
    run('{"step":"gatt.services-changed","peer":"p"}')
    run('{"step":"op.dispatch","op":"r0"}')
    const [late] = normalized([run('{"step":"op.settle","op":"r0","kind":"success"}')])
    expect(JSON.parse(late)).toMatchObject({ settle: 'settled', terminal: 'failed' })
    const [reread] = normalized([
      run('{"step":"gatt.read","op":"r1","path":1,"value":"aa","settle":"success"}')
    ])
    expect(JSON.parse(reread)).toMatchObject({
      ok: false,
      error: 'gatt.stale-handle|gatt|staged-gatt-read|path.generation'
    })
    driver.close()
  })

  test('cancel across the dispatch boundary reports the commit truthfully', () => {
    const addon = loadRustAddon()
    const { driver, run } = linkDriver(addon)
    run('{"step":"gatt.read","op":"r0","path":1,"value":"aa","settle":"dispatched"}')
    run('{"step":"gatt.read","op":"r1","path":1,"value":"aa","settle":"admitted"}')
    const [afterDispatch, beforeDispatch] = normalized([
      run('{"step":"op.cancel","op":"r0"}'),
      run('{"step":"op.cancel","op":"r1"}')
    ])
    expect(JSON.parse(afterDispatch)).toMatchObject({
      settle: 'settled',
      terminal: 'aborted',
      cause: 'operation.aborted',
      commit: 'released'
    })
    expect(JSON.parse(beforeDispatch)).toMatchObject({
      settle: 'settled',
      terminal: 'aborted',
      cause: 'operation.aborted',
      commit: 'not-dispatched'
    })
    driver.close()
  })

  test('bounded-batch overflow fails loudly and preserves dropped-not-staged accounting', () => {
    const addon = loadRustAddon()
    const driver = new RustBackendDriver(addon, RUST_PARITY_REVISION)
    const run = line => {
      const outcome = driver.stagedStep(line)
      if (!outcome.ok) {
        throw new Error(`binding-lifetime rejection for ${line}`)
      }
      return outcome.value
    }
    run('{"step":"batch.set-cap","cap":1}')
    run('{"step":"peer.advertise","peer":"q","domain":"platform-guid","value":"peer-9"}')
    run('{"step":"link.connect","peer":"q","lease":"lq","op":"cq"}')
    const [overflow] = normalized([run('{"step":"link.established","peer":"q","op":"cq"}')])
    expect(JSON.parse(overflow)).toMatchObject({
      ok: false,
      error: 'stream.quota|stream|staged-link-established|effect-batch.full'
    })
    const counters = JSON.parse(run('{"step":"staged.counters"}'))
    expect(counters.cap).toBe(1)
    expect(counters.dropped_not_staged).toBe(1)
    const [rejectCap, rejectZero] = normalized([
      run('{"step":"batch.set-cap","cap":65}'),
      run('{"step":"batch.set-cap","cap":0}')
    ])
    for (const line of [rejectCap, rejectZero]) {
      expect(JSON.parse(line)).toMatchObject({
        ok: false,
        error: 'argument.invalid|core|staged-batch-set-cap|staged-batch-cap-range'
      })
    }
    driver.close()
  })

  test('invalidation mid-IO rejects new work until rediscovery re-arms', () => {
    const addon = loadRustAddon()
    const { driver, run } = linkDriver(addon)
    const [changed, reread, rediscover, again, read] = normalized([
      run('{"step":"gatt.services-changed","peer":"p"}'),
      run('{"step":"gatt.read","op":"r0","path":1,"value":"aa","settle":"success"}'),
      run('{"step":"gatt.require-rediscovery","peer":"p"}'),
      run(
        `{"step":"gatt.discover","peer":"p","owner":"lease-a","services":[{` +
          `"uuid":"${SVC}","occurrence":0,"characteristics":[{` +
          `"uuid":"${CHR}","occurrence":0,"properties":"read+write+notify"}]}]}`
      ),
      run('{"step":"gatt.read","op":"r1","path":4,"value":"bb","settle":"success"}')
    ])
    expect(JSON.parse(changed)).toMatchObject({ database: 'changed' })
    expect(JSON.parse(reread)).toMatchObject({
      ok: false,
      error: 'gatt.stale-handle|gatt|staged-gatt-read|path.generation'
    })
    expect(JSON.parse(rediscover)).toMatchObject({ database: 'undiscovered' })
    expect(JSON.parse(again)).toMatchObject({ paths: 5, first_path: 3 })
    expect(JSON.parse(read)).toMatchObject({ bytes: 'bb', terminal: 'succeeded' })
    driver.close()
  })

  test('live-peer disconnect failures retain cleanup ownership per code', () => {
    const addon = loadRustAddon()
    for (const code of [
      'connection-failed',
      'connection-lost',
      'operation-timed-out',
      'adapter-unavailable'
    ]) {
      const driver = new RustBackendDriver(addon, RUST_PARITY_REVISION)
      const run = line => {
        const outcome = driver.stagedStep(line)
        if (!outcome.ok) {
          throw new Error(`binding-lifetime rejection for ${line}`)
        }
        return outcome.value
      }
      run('{"step":"peer.advertise","peer":"p","domain":"platform-guid","value":"peer-1"}')
      run('{"step":"link.connect","peer":"p","lease":"lease-a","op":"conn0"}')
      run('{"step":"link.established","peer":"p","op":"conn0"}')
      const [failed] = normalized([
        run(`{"step":"link.disconnect-failed","peer":"p","code":"${code}"}`)
      ])
      expect(JSON.parse(failed)).toMatchObject({ ok: true, connection: 'connected' })
      const [destroy] = normalized([run('{"step":"staged.destroy"}')])
      expect(JSON.parse(destroy)).toMatchObject({ state: 'release-failed' })
      driver.close()
    }
  })

  test('descending settle ordinals suppress after terminal instead of reordering', () => {
    const addon = loadRustAddon()
    const { driver, run } = linkDriver(addon)
    run('{"step":"gatt.read","op":"r0","path":1,"value":"aa","settle":"dispatched"}')
    const [ignored] = normalized([
      run('{"step":"op.settle","op":"r0","kind":"timeout","valid":false,"ordinal":9}')
    ])
    expect(JSON.parse(ignored)).toMatchObject({ settle: 'contender-ignored' })
    const [first] = normalized([
      run('{"step":"op.settle","op":"r0","kind":"success","ordinal":2}')
    ])
    expect(JSON.parse(first)).toMatchObject({ settle: 'settled', terminal: 'succeeded' })
    const [dup] = normalized([
      run('{"step":"op.settle","op":"r0","kind":"success","ordinal":1}')
    ])
    expect(JSON.parse(dup)).toMatchObject({ settle: 'duplicate-suppressed', suppressed: 1 })
    driver.close()
  })

  test('unknown inputs fail closed with exact identities', () => {
    const addon = loadRustAddon()
    const driver = new RustBackendDriver(addon, RUST_PARITY_REVISION)
    const run = line => {
      const outcome = driver.stagedStep(line)
      if (!outcome.ok) {
        throw new Error(`binding-lifetime rejection for ${line}`)
      }
      return outcome.value
    }
    const [unknownStep, badHex, regressed, ghost, badOp] = normalized([
      run('{"step":"nope.unknown"}'),
      run('{"step":"gatt.read","op":"r","path":1,"value":"zz","settle":"success"}'),
      run('{"step":"clock.set","now":10}'),
      run('{"step":"link.disconnect-failed","peer":"ghost","code":"connection-failed"}'),
      run('{"step":"op.settle","op":"ghost","kind":"success"}')
    ])
    expect(JSON.parse(unknownStep)).toMatchObject({
      ok: false,
      error: 'argument.invalid|core|staged-step|staged-unknown-step'
    })
    expect(JSON.parse(badHex)).toMatchObject({
      ok: false,
      error: 'argument.invalid|core|staged-gatt-read|staged-bad-hex'
    })
    // Clock advanced to 10 above is fine; going back fails.
    const [back] = normalized([run('{"step":"clock.set","now":9}')])
    expect(JSON.parse(back)).toMatchObject({
      ok: false,
      error: 'argument.invalid|core|staged-clock-set|clock-regressed'
    })
    expect(JSON.parse(ghost)).toMatchObject({
      ok: false,
      error: 'peer.not-found|peer|staged-link-disconnect-failed|staged-peer-unknown'
    })
    expect(JSON.parse(badOp)).toMatchObject({
      ok: false,
      error: 'argument.invalid|core|staged-op-settle|staged-op-unknown'
    })
    // Post-close staged calls reject with lifecycle.destroyed like every call.
    driver.close()
    const after = driver.postCloseStagedStep('{"step":"cap.project"}')
    expect(after.ok).toBe(false)
    if (!after.ok) {
      expect(
        `${after.error.code}|${after.error.domain}|${after.error.operation}|${after.error.detail}`
      ).toBe('lifecycle.destroyed|core|staged-step|session-closed')
    }
  })

  test('capability projection reports six limited rows and gates unknown ids', () => {
    const addon = loadRustAddon()
    const driver = new RustBackendDriver(addon, RUST_PARITY_REVISION)
    const run = line => {
      const outcome = driver.stagedStep(line)
      if (!outcome.ok) {
        throw new Error(`binding-lifetime rejection for ${line}`)
      }
      return outcome.value
    }
    const [projected, rows, check, unknown] = normalized([
      run('{"step":"cap.project"}'),
      run('{"step":"cap.rows"}'),
      run('{"step":"cap.check","id":"central.scan"}'),
      run('{"step":"cap.check","id":"adapter.enumeration"}')
    ])
    expect(JSON.parse(projected)).toMatchObject({ rows: 6 })
    expect(JSON.parse(rows)).toMatchObject({ rows: STAGED_CAPABILITY_ROWS, count: 6 })
    expect(JSON.parse(check)).toMatchObject({ admission: 'proceed-with-limitation' })
    expect(JSON.parse(unknown)).toMatchObject({
      ok: false,
      error: 'capability.unavailable|capability|staged-cap-check|capability.unknown'
    })
    driver.close()
  })
})
