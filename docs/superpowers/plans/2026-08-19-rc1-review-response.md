# 4.0.0-rc.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Execute **one slice plan at a time**, in the order below. Do not start the next slice until the previous slice’s validation command is green.

**Goal:** Land every verified item from the 2026-08-19 PR #26 review on this branch and cut `unified-ble-manager@4.0.0-rc.1`.

**Architecture:** This branch **is** the rc.1 branch. All four slices ship here as stacked commits, not as a later RC. Public API corrections land before the documentation rewrite so README/host guides are written once against the final surface. Another engineer owns Tauri IPC/`example-tauri`; this plan consumes their landed surface and does not edit it.

**Tech Stack:** pnpm, Jest, host-neutral `BleManager`, profiles, Expo config plugin, DeterministicTestBackend, existing Node/Electron/Web/RN entrypoints.

---

## Authority

| Document | Role |
| --- | --- |
| `docs/review/2026-08-19-pr-26-external-review.md` | Source review (verbatim) |
| `docs/review/2026-08-19-pr-26-external-review-verification.md` | What is true on this tree |
| This file | Sequencing, locked decisions, ownership, stop conditions |
| `docs/superpowers/plans/2026-08-19-rc1-slice-b-api.md` | Slice B |
| `docs/superpowers/plans/2026-08-19-rc1-slice-d-hosts.md` | Slice D (code first, then host docs with A) |
| `docs/superpowers/plans/2026-08-19-rc1-slice-a-docs.md` | Slice A |
| `docs/superpowers/plans/2026-08-19-rc1-slice-c-tests.md` | Slice C |

Do not invent a fifth product policy. If a slice plan and this file disagree, this file wins.

---

## Locked decisions

These are no longer open. Agents must not re-litigate them.

1. **`isBackgroundEnabled` is removed.** The option is `requiresBluetoothLeHardware`. No deprecated alias. No production users.
2. **iOS `peripheral` background mode is rejected** by the plugin. This package is a BLE central. Do not write `bluetooth-peripheral` into Info.plist.
3. **Three construction façades remain:** host-neutral `BleManager`, desktop-webview `IpcBleManager`, `ElectronRendererBleClient`. Do not create a fourth façade. Converge *workflow names* (`snapshot`, `read`, `write`, `subscribe`, `release`, `destroy`) only where the Tauri owner already did that on `IpcGattDatabase`.
4. **`BleManager.adapterStates` ships in rc.1.** Backends already implement `adapter.watchState()`. Public manager wraps it as a bounded stream. Migration must not tell people to poll.
5. **Convenience factories ship in rc.1** for RN, Web, and Node. Injectable/environment overloads remain for tests.
6. **`createReactNativeBleManager({ clientId, managerId, hostSessionScope })` becomes the app factory.** The current six-field form is renamed `createReactNativeBleManagerWithEnvironment`.
7. **Electron rc.1 example is a runnable composition app** (`example-electron/` main + preload + renderer) that can still use the deterministic backend in CI. It is not live-radio evidence. Keep the existing L1 `smoke.js`. Split security prose into `docs/ELECTRON_SECURITY_MODEL.md`.
8. **Do not publish a crates.io crate in rc.1.** Consumer install remains `path = ".../node_modules/unified-ble-manager/native/tauri"`. Document brittleness honestly.
9. **`scanUntil` / `find` are the “first match” helpers.** Do not add `scanForFirstMatch`. Add `defaultScanDelivery()` and `scanForServices(...)`.
10. **C5 (empty License) is skipped.** License section is already Apache-2.0.
11. **`readTemperatureMeasurement` is removed** with the other invalid SIG reads. HTS Temperature Measurement is Indicate-only.
12. **Commits require the user’s approval.** Stage, do not `git push`, do not create tags.

---

## Tauri ownership (hard freeze)

Another engineer/agent owns the Tauri proving surface. **Do not modify these paths** unless this file is updated:

- `src/ipc/**`
- `src/tauri.ts`
- `src/tauri/**`
- `native/tauri/**`
- `example-tauri/**`
- `__tests__/Tauri*.js`
- `__tests__/TauriContractFoundation.test.js`

Uncommitted work already in this checkout (`src/ipc/manager.ts`, `__tests__/TauriContractFoundation.test.js`) is theirs. Leave it alone.

`docs/TAURI.md` may be updated **only in the last D-docs task**, after reading the *then-current* `IpcBleManager` public methods. If their snapshot/path API is present, document that. If not, document the handle-based API that actually exists. Never document stub fields (`connectionGeneration: '1'`) as contract.

Tauri *proving* (live radio, packed consumer, extra app) is out of this plan’s implementation scope. We only make the package API/docs they can prove against.

---

## Execution order

| Step | Slice | Why this order |
| --- | --- | --- |
| 1 | **B** | Correct the public API and canonical example *before* rewriting docs. |
| 2 | **D-code** | Convenience factories, `adapterStates`, scan presets, cleanup helpers. |
| 3 | **A + D-docs** | One documentation pass against the final API. |
| 4 | **C** | Semantic, recipe, and link tests that lock the final docs. |
| 5 | Version/`CHANGELOG` for `4.0.0-rc.1` | Last, after C is green. Do not tag. |

Do not rewrite README in B or D-code except to keep existing tests from going red (minimal string updates only). The real docs pass is step 3.

---

## Claim coverage

| Claims | Slice |
| --- | --- |
| C17–C31, C20–C28, C32–C33 (plugin identity), C43 | B |
| C36 additions, C38, C39, C40, C45, C46, C47 (positioning) | D |
| C1–C16, C4, C6–C10, C34–C35, C37, C41, C44, SECURITY/SUPPORT | A + D-docs |
| C5 | skip |
| C42 and review semantic rules | C |
| Tauri IPC implementation | frozen / other owner |

---

## Validation gates

After **B**:

```sh
pnpm exec jest __tests__/PackageSurface4.test.js __tests__/Package.identity.test.js __tests__/ExampleBleService.parity.test.js --runInBand
pnpm test:plugin
```

After **D-code**:

```sh
pnpm exec jest __tests__/PackageSurface4.test.js __tests__/manager --runInBand
pnpm exec jest __tests__/web __tests__/backends --runInBand
```

(If those globs miss the new tests, run the files named in the slice plan.)

After **A + D-docs**:

```sh
pnpm exec jest __tests__/Docs.consumer.test.js --runInBand
```

After **C**:

```sh
pnpm exec jest __tests__/Docs.consumer.test.js __tests__/Docs.semantic.test.js __tests__/Docs.recipes.test.js --runInBand
pnpm test:package
pnpm test:plugin
pnpm lint
```

Before calling rc.1 ready (human, not agent):

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

Never describe deterministic/compile evidence as physical-radio proof. rc.1 does not promote backend support labels.

---

## Stop conditions

Stop and ask the user if:

- A Tauri freeze path must be edited to make tests pass.
- `adapter.watchState()` cannot be exposed without a backend-contract change.
- The other engineer’s IPC snapshot API and helper-based docs cannot both be true.
- A required test file from a slice plan does not exist and the slice did not say to create it.

---

## Version bump (after C, not before)

- `package.json` `"version": "4.0.0-rc.1"`
- `CHANGELOG.md` entry: docs correctness, SIG command corrections, plugin rename, convenience factories, `adapterStates`, example lifecycle. Not live-radio.
- Follow `RELEASE.md`. Do not tag or publish from this plan.

---

## Spec coverage (self-review)

Every verified review item has a task, except the three deliberate cuts below.

| Review ask | Task |
| --- | --- |
| Finite helper-first README; optional HRS paths | A2, A4 |
| Migration write/cancel/coexist/`merged`/deadline/capabilities | A5 |
| Platform guard; Expo vs bare paths | A3 |
| Shared semantics; sponsor after H1; Bun; live-radio wording; SECURITY/SUPPORT | A2, D11 |
| License section | skip (already Apache-2.0) |
| Invalid SIG reads including HTS; property gate | B1, B2 |
| Plugin rename, `peripheral` reject, debug env, restoration identity, `neverForLocation` warning | B3 |
| Example lifecycle, overflow, identity, optional errors, AbortSignal | B4 |
| RN/Web/Node convenience factories | D2, D7, D8, A2 |
| `adapterStates` | D3, A5 |
| Scan presets; `withDiscoveredConnection`; `throwIfCleanupFailed` | D4, A4 |
| Electron composition + security split | D5, D9 |
| Tauri consumer guide | D10 (read-only vs IPC owner) |
| Semantic rules, executed finite recipe, links, version banners | C1–C3 |
| Packed fence typecheck of every markdown sample | **cut** — C4 is export/presence on packed package only |
| Generated option tables / version injection into markdown | **cut** — C3 asserts `package.json` version instead of a docs generator |
| crates.io Tauri crate; live-radio Electron/Tauri proving | **cut** — other engineer / later evidence work |

If an agent finds a review claim with no task and it is not in that cut list, add a task; do not drop it.
