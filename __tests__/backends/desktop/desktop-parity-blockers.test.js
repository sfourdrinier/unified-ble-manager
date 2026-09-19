'use strict'

// OPEN PARITY BLOCKERS (release-blocking by design; owner directive
// 2026-09-17: no capability the legacy desktop backends provided may be
// lost). Every `blocked` row of DESKTOP_RUST_CORE_PARITY needs a probe in
// PROBES that exercises the real Rust path and FAILS until the capability
// works. None is skipped or marked todo. The test title names the row id.
//
// When a row turns green, flip its `rust` entry to `implemented` in
// src/backends/desktop/desktop-rust-core-parity.ts and move the proof into
// desktop-rust-core-provider.test.js / desktop-adapter-loss.test.js.
// Every row is implemented today; PROBES is empty.

const { DESKTOP_RUST_CORE_PARITY } = require('../../../src/backends/desktop/desktop-rust-core-parity')

jest.setTimeout(30000)

const PROBES = Object.freeze({})

describe('desktop Rust path parity blockers (PARITY-INVENTORY §1–3, LEGACY-AUDIT-1)', () => {
  test('every blocked row has a probe, and every probe names a blocked row', () => {
    const blocked = DESKTOP_RUST_CORE_PARITY.filter(row => row.rust.state === 'blocked').map(row => row.id)
    expect([...blocked].sort()).toEqual(Object.keys(PROBES).sort())
  })

  test('every parity row names its platforms and either how it works or what blocks it', () => {
    for (const row of DESKTOP_RUST_CORE_PARITY) {
      expect(row.platforms.length).toBeGreaterThan(0)
      expect(row.rust.state === 'implemented' ? row.rust.how : row.rust.missing).toEqual(expect.any(String))
    }
  })

  for (const [id, probe] of Object.entries(PROBES)) {
    test(`blocked row ${id} works on the Rust path`, probe)
  }
})
