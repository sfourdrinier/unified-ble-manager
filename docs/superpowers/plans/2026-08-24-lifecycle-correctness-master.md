# 4.0.x lifecycle correctness — master plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Execute **serially**: Plan A, merge, Plan B, merge, Plan C. Do not start B while A is open. Do not start C while B is open. Inside Plan A, complete its four phases in order and run each phase gate before the next phase.

**Goal:** Close the remaining source-confirmed 4.0.x lifecycle, stream-ownership, and React-hook defects as three sequential PRs off exact `origin/main` (`v4.0.2` / `109ce0ca`).

**Architecture:** Three PRs, one host layer each. Plan A is large, so it is **one PR with four separately gated internal phases** (clone → public stream/cleanup → IPC ownership → Web/Tauri). Plan B is backend registry unregister and overflow native release. Plan C is React hook/store terminal and bound state. Do not mix native-backend overflow work into A or React hook work into B.

**Tech Stack:** TypeScript public/IPC/core/backends, Tauri Rust dispatcher, Jest JS fixtures, Cargo for `native/tauri`, `pnpm` canonical gates.

**Spec:** This master plan. Child plans:

- [Plan A — IPC lifecycle, stream close, clone](./2026-08-24-pr-a-ipc-lifecycle-and-clone.md)
- [Plan B — backend unregister and overflow release](./2026-08-24-pr-b-backend-unregister.md)
- [Plan C — React hooks and adapter store](./2026-08-24-pr-c-react-hooks.md)

## Global constraints

- Branch each PR from current `origin/main` after the previous PR has merged. Do not stack on `release/4.0.2-prepare` or the leftover local `feat: expose typed public manager facade` commit.
- Test-first. No `as unknown` / `as any` / `as T` to silence the checker.
- Public BLE payloads stay `Uint8Array`. Cancellation stays `AbortSignal`. Root stays host-neutral. No production backend fallback.
- Do not retag `v4.0.0`, `v4.0.1`, or `v4.0.2`. Version bump is a later prepare PR after C.
- Do not describe deterministic tests as live-radio evidence.
- Hosted merge requirement: **all jobs that the PR actually runs must be green**, not only Ubuntu JS 22/24. The branch-protection pair (Ubuntu JS 22/24) is necessary but not sufficient.

## Serial order

| Order | Plan | Invariant                                                                                          | First commit |
| ----- | ---- | -------------------------------------------------------------------------------------------------- | ------------ |
| 1     | A    | Clone boundary, stream close vs finish, public cleanup collector, IPC ownership, Web/Tauri cleanup | **#58**      |
| 2     | B    | Backend watch/security/scan unregister and overflow native release                                 | #61          |
| 3     | C    | React adapter store, discovered-peers bound, hook terminal loading                                 | #62          |

### Plan A internal phases (same PR, sequential gates)

| Phase | Issues                       | Stop-the-line gate                           |
| ----- | ---------------------------- | -------------------------------------------- |
| A1    | #58                          | dedicated serializable + Tauri wire tests    |
| A2    | #59, #75, #60                | bounded-stream + public-scan + destroy tests |
| A3    | #72, #63, #73, #76, #74, #79 | IPC Jest files; keep #56 green               |
| A4    | #67, #77, #78                | Web lifecycle + Tauri Jest + **Cargo**       |

## Issue tracker

Check an issue off here only after its child-plan task is green **and** the GitHub issue is closed against the merged PR.

### Already shipped (do not reopen)

- [x] #50 Tauri `heard` — 4.0.1 / PR #52
- [x] #53 scan fingerprint accounting — 4.0.2 / PR #57
- [x] #54 IPC pending-stream bounds — 4.0.2 / PR #57
- [x] #56 IPC unsubscribe must not skip disconnect — 4.0.2 / PR #57

### Plan A

- [ ] #58 `__proto__` clone / decode **(A1, first task)**
- [ ] #59 CoreBoundedStream close after finish still drains
- [ ] #75 Public stop/destroy drop `release-failed` **(before #60)**
- [ ] #60 Public scan stop after resolve **and** local emit overflow ignores terminal
- [ ] #72 pending-stream tombstone leaves a live sink
- [ ] #63 global event pump leaves children hanging
- [ ] #73 malformed connect leaks native connection
- [ ] #76 events-admission unsubscribe swallowed
- [ ] #74 expired IPC deadlines still dispatch
- [ ] #79 hung lifecycle admission deadlocks disconnect
- [ ] #67 Web disconnect skipped after subscription cleanup fail
- [ ] #77 Tauri completed-correlation replay
- [ ] #78 Tauri unbounded cancelled-success retries

### Plan B

- [ ] #61 adapter-state watch dropped before close succeeds
- [ ] #68 security watch close does not unregister (RN Android, WinRT, deterministic; BlueZ is regression control)
- [ ] #69 CoreBluetooth, BlueZ, deterministic adapter/event streams remain registered after close
- [ ] #70 CoreBluetooth overflow does not release native owners
- [ ] #71 WinRT scan overflow does not release scan ownership

### Plan C

- [ ] #62 React adapter-state store wedges or replaces a watch before failed cleanup is released
- [ ] #65 `useDiscoveredPeers` unbounded map; ignores lost-peer events
- [ ] #66 connection/characteristic hooks remain loading after terminal completion

### Out of these three PRs

- [ ] #19 First-class Tauri v2 (crates.io / live-radio still blocked)
- [ ] #38 Independent Tauri npm/crate installation

## Canonical validation (every PR, after its focused gates)

```sh
corepack enable
pnpm install --frozen-lockfile
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js
```

Plan A phase A4 and Plan A merge also require:

```sh
cargo fmt --manifest-path native/tauri/Cargo.toml -- --check
cargo test --manifest-path native/tauri/Cargo.toml
cargo clippy --manifest-path native/tauri/Cargo.toml -- -D warnings
cargo check --manifest-path example-tauri/src-tauri/Cargo.toml
```

Plan B merge also requires the focused native-protocol commands named in that plan (`pnpm test:native-protocol`, `pnpm test:native-protocol:apple`, `pnpm test:native-protocol:winrt`, `pnpm test:native-protocol:android` when Android security tests are touched).

Hosted CI: wait for every non-skipped job on the PR. Required protection is Ubuntu JS 22 and 24; still treat failed Windows/macOS JS, Tauri Rust, Classic RN Android, or Expo CNG as blocking for these PRs.

Do not bump `package.json` version in A/B/C.

## Regression locks

- Plan A must keep `__tests__/ipc/connection-cleanup.test.js` green (#56).
- Plan A #79 must not re-serialize physical disconnect behind hung admission.
- Plan A public scan cleanup has one shared view/native stop state machine across overflow, explicit stop, and destroy; a successful phase is never repeated to retry another phase.
- Plan A Tauri replay protection never evicts an unexpired completed correlation to admit new work; a full bounded window fails closed.
- Plan B must keep BlueZ overflow-unregister tests green as the control implementation.
- Plan B stream unregister is terminal-aware (`close`, `finish`, and overflow), not a close-only wrapper.
- Plan C must not start a second adapter watch while a previous watch cleanup is unresolved.
- Plan C treats `ScanSession.events` as optional and treats every unexpected natural hook-stream end as fail-visible even after prior values.
