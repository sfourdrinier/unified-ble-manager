# Handoff: `4.0.0-rc.1` → stable `4.0.0`

**Audience:** an implementation agent with **no prior session context**.  
**Date of this handoff:** 2026-08-20  
**Status at handoff:** RC1 is published. The 12-PR train has **not** started.

This file is the operating procedure. The product specification is:

[`docs/superpowers/plans/2026-08-20-next-12-prs.md`](./2026-08-20-next-12-prs.md)  
**(Revision 4 — authoritative)**

If those two files disagree on **product/API**, the Revision-4 plan wins.  
If they disagree on **how we integrate** (branching, reviews, GitHub PRs, merge), **this handoff wins**.

---

## 0. What you are taking over

You are finishing **Unified BLE Manager 4.x**: take the published prototype `unified-ble-manager@4.0.0-rc.1` to stable `4.0.0` by executing **twelve sequential numbered PRs**.

RC1 implementation, merge, tag, and npm publish are **done**. Do not resume RC1. Do not rewrite first-party product apps. Do not `npm publish` from a laptop.

When `4.0.0` is on npm `latest`, stop unless the user asks for the one-pass client migration in plan §10.

---

## 1. Repository facts (verify on session start)

| Item | Value |
|---|---|
| Workspace | `/Users/sfourdrinier/src-trackourhealth/tmpBle/unified-ble-manager` |
| Canonical remote | `origin` → `https://github.com/sfourdrinier/unified-ble-manager.git` |
| Do **not** push 4.x to | `legacy` → `https://github.com/sfourdrinier/react-native-ble-plx.git` |
| Handoff `main` | `a33cfede72b3d642f94064d793b3c6465e252f4c` |
| Handoff tag | `v4.0.0-rc.1` (equals that commit) |
| Handoff package version | `4.0.0-rc.1` |
| npm `latest` at handoff | `4.0.0-rc.1` |
| RC1 GitHub PR | https://github.com/sfourdrinier/unified-ble-manager/pull/26 |
| RC1 publish run | https://github.com/sfourdrinier/unified-ble-manager/actions/runs/32338265781 |
| GitHub prerelease | https://github.com/sfourdrinier/unified-ble-manager/releases/tag/v4.0.0-rc.1 |
| Package manager | **pnpm** via Corepack (`packageManager`: `pnpm@10.14.0`) |

If `origin/main` has moved past `a33cfed`, **follow current `main`**. Never rewind. Re-orient with §10.

Mandatory reading, in order:

1. This handoff
2. `AGENTS.md`
3. `Claude.md`
4. `docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`
5. `README.md`
6. `RELEASE.md`
7. The full Revision-4 plan, then the **current numbered PR section only** as the implementation spec
8. Plan §10 (client migration — do not execute unless asked)
9. Plan §14–§16 (discipline, definition of done, agent brief)

---

## 2. Operating loop (non-negotiable)

We work **one numbered PR at a time**. We do not start N+1 until N is merged to `main` and we are back on an up-to-date `main`.

### Loop for numbered PR N

```text
1. Orient          → §10 tracker + git/tags/PRs. Confirm N is next.
2. Branch          → from current origin/main. Isolate a worktree.
3. Implement       → TDD. Stay inside PR N's spec. Track progress in §11.
4. Local gates     → focused tests, then required package gates.
5. Commit + push   → commit ALL work for this PR to the feature branch
                     and push the branch. Do not open a GitHub PR yet.
6. Adversarial     → multiple independent review rounds against
                     (a) the diff vs origin/main and
                     (b) the PR N goals / acceptance criteria / non-goals.
                     Fix findings. Commit + push each fix round.
                     Repeat until reviews are actually clean — not until
                     you are tired.
7. Open GitHub PR  → only after adversarial review is accepted.
8. CodeRabbit      → wait for CodeRabbit on the GitHub PR.
                     Do one or two review+fix rounds. Push fixes.
                     Do not merge with unanswered Critical/Important
                     CodeRabbit findings unless the user explicitly
                     overrules a false positive in writing.
9. Merge           → user-approved merge to main (prefer merge commit:
                     `gh pr merge <n> --merge`).
10. Return to main → checkout main, pull --ff-only, delete local feature
                     branch. If this PR is a freeze checkpoint, publish
                     the RC per §8. Update the tracker in this file.
11. Next           → start numbered PR N+1. Repeat until PR 12 and
                     4.0.0 are on npm latest.
```

### What this supersedes in the plan

Plan §14.2 says “open the PR as draft at the first failing contract tests.” **Do not.** Keep the GitHub PR closed until implementation **and** adversarial review are done. Internal phases (A/B/C/D on milestone PRs) still happen on the **branch**, as local commit groups, not as premature GitHub PRs.

Lettered splits (`PR2A` / `PR2B`) are still allowed for milestone size. Each lettered unit follows this **same** loop (implement → commit/push branch → adversarial → GitHub PR → CodeRabbit → merge). Partial public surfaces stay **unexported** until the numbered milestone is complete. **No RC between lettered splits.**

### One PR at a time — concrete bans

- Do not open PR N+1’s branch until PR N is merged.
- Do not keep twelve stacked long-lived branches.
- Do not “get a head start” on PR9 while PR5 is in CodeRabbit, except **read-only design notes** that never become commits on another branch.
- Do not mix two numbered PR scopes in one branch.
- Do not publish an RC in the middle of a lettered split.

### Commit / push policy

- **During implementation:** keep the tree committable; you may make local commits as you complete phases (especially milestones 1, 2, 4, 10). Prefer atomic commits matching the plan’s suggested sequence.
- **End of implementation (required):** everything for this numbered PR is committed on the feature branch and **pushed to origin**. Working tree clean. Then adversarial review starts on **that pushed SHA**.
- **After each adversarial or CodeRabbit round that produces fixes:** commit the fixes and **push**.
- **Never commit or push without the user’s approval** if the user is in the session and has not delegated it. If the user says “run the train” / “do PR N,” that is approval to commit and push the **feature branch** for that PR. It is **not** approval to merge or to tag/publish.
- **Never push 4.x to `legacy`.** Never force-push `main`. Never move a published version tag.

### Agent management (meticulous, active)

The human owner and the implementing agent **keep a written log**. Update §11 in this file as part of the PR’s commits (or a follow-up commit on the same branch before opening the GitHub PR).

Use agents as a **managed pipeline**, not a pile of overlapping chats:

| Role | When | What |
|---|---|---|
| Implementer (this session) | Always | Owns the branch, TDD, gates, tracker, commit/push |
| Independent adversarial reviewer | After push, **before** GitHub PR | Hostile review vs spec; no access to implementer rationalizations |
| Second adversarial / dual-lens | After first-round fixes | Confirm or refute residual issues |
| CodeRabbit | After GitHub PR exists | Automated PR review; 1–2 rounds |
| Publish watcher | Freeze checkpoints only | `publish.yml` + npm dist-tags |

Rules for using subagents:

- Give each reviewer **only** the PR spec section, the acceptance criteria, the non-goals, the base SHA, the head SHA, and the instruction to be skeptical. Do **not** paste your justifications.
- Fix **Critical** and **Important** findings before the next round. Record **Minor** items as deferred only if they are truly out of scope for this numbered PR.
- If a reviewer is wrong, rebut with code/tests, not vibes. Put the rebuttal in the tracker.
- Do not mark a PR “ready for GitHub” because tests pass. Tests passing is necessary; adversarial review against **goals** is the completion gate.
- After merge, start a **fresh session** for the next numbered PR (especially after milestones 1, 2, 4, 10). Context rot is how façades pick up policy they must not own.

---

## 3. Adversarial review protocol (required, multiple rounds)

Do this **after** the feature branch is committed and pushed, **before** `gh pr create`.

### Round 0 — self-check (implementer)

Against the numbered PR section in the Revision-4 plan:

- [ ] Named scope only; no unrelated product feature
- [ ] Acceptance criteria all met, with pointers to tests/docs
- [ ] Non-goals not implemented
- [ ] Plan §15 definition of done
- [ ] Plan §14.4 DRY checklist searched
- [ ] Plan §14.7 evidence line chosen
- [ ] API report / generated artifacts / packed consumers as required
- [ ] No compatibility aliases
- [ ] No fabricated public method
- [ ] No portable-semantic break after PR4 (except documented release-blocking defect)

### Rounds 1..K — independent hostile review

Minimum **two** independent rounds. A round that only produces “looks good” without reading the spec is invalid. Keep going until a round produces **no Critical and no Important** findings (or only user-overruled false positives).

**Focus (from plan §16, mandatory):**

1. public API drift or accidental complexity
2. duplicated policy / schema / matcher / state-machine logic
3. lifecycle, ownership, generation, cancellation, deadline, stream-bound, cleanup regressions
4. renderer/webview trust-boundary violations
5. fabricated or overclaimed platform semantics
6. missing unsupported/limited capability registrations
7. reconnect / pairing / background behavior that became implicit product policy
8. scan-query semantic drift (especially PR4 vs PR9)
9. RC1 compatibility aliases sneaking back in
10. incomplete acceptance criteria; work parked as “follow-up” that belongs in this PR

**How to run:**

- Use the `adversarial-review` skill (hostile Grok review) with `--target` = the feature branch vs `origin/main`, and a task file that pastes the **full numbered PR section** plus “fail the review if acceptance criteria are not evidenced.”
- Then a second pass: `dual-lens` or `requesting-code-review` / Grok `review` on the **same SHA**.
- Optionally a third pass after fixes, scoped only to residual risks.

**After each round:**

1. Classify every finding: Critical / Important / Minor / False positive.
2. Fix Critical and Important. Commit. Push.
3. Write the round into §11 (reviewer, SHA, finding counts, dispositions).
4. Do not open the GitHub PR until the last round is clean.

---

## 4. GitHub PR + CodeRabbit + merge

### Create the PR only when

- Feature branch is pushed
- Working tree clean
- Adversarial rounds recorded and clean
- Tracker §11 updated for this PR

```sh
gh pr create --repo sfourdrinier/unified-ble-manager \
  --base main \
  --head "$BRANCH" \
  --title "$TITLE" \
  --body "$(cat <<'EOF'
## Summary
- Numbered 4.0 train PR N: <one sentence>
- Spec: docs/superpowers/plans/2026-08-20-next-12-prs.md (Revision 4), section “PR N”
- Adversarial review: <K> rounds, last clean SHA <sha>

## Contract
- Breaking vs RC1: yes/no (PRs 1–4 expected yes; 5–12 portable no)
- Support-label impact: <one of the plan §14.7 phrases>

## Test plan
- [ ] pnpm validate:evidence
- [ ] pnpm test:package / test:plugin / lint / prepack as relevant
- [ ] packed-consumer / native-protocol gates as relevant
- [ ] CodeRabbit round addressed

## Tracker
See docs/superpowers/plans/2026-08-20-handoff-rc1-to-stable-4.0.md §11
EOF
)"
```

Use the plan’s **Suggested PR title**.

### CodeRabbit

1. Wait until CodeRabbit has actually posted on the PR (do not merge on a silent bot).
2. Address findings. Push.
3. Wait for the follow-up review if CodeRabbit re-reviews.
4. **One or two rounds total**, then merge — unless new Critical issues appear, in which case continue.
5. Do not start a third wandering round of style nits. If CodeRabbit is stuck on a false positive, document the rebuttal on the PR and in §11, then merge with user approval.

If CodeRabbit is not configured on the repo, say so plainly, enable/wait as the user directs, and do not pretend a review happened.

### Merge

```sh
gh pr merge <n> --merge
```

Prefer a **merge commit** (that is how RC1 PR #26 landed). If GitHub MCP returns 403, use `gh` as above.

Then:

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main
git log -1 --oneline
# confirm the merge commit is on main
git branch -d "$BRANCH"   # after origin has the merge
```

If this numbered PR is a freeze checkpoint (after 4, 8, 10, 11, 12), do **not** start N+1 until the RC/stable tag is published and verified (§8).

---

## 5. Product intent (do not reopen)

`4.0.0-rc.1` is the **last public prototype**, not a compatibility baseline.

- PRs 1–4: pre-stable contract-reset window
- PRs 5–11: additive against the RC2 freeze (two staged host/tool exceptions)
- PR 12: qualify and publish stable `4.0.0`

This is **not** a rewrite of the reliable core (ownership, generations, bounded streams, cancellation, TCK, protocol negotiation, diagnostics, evidence). It is a simpler consumer surface **over** that core.

First-party product clients stay on `unified-ble-manager@4.0.0-rc.1` until `4.0.0` is npm `latest`. One adapter rewrite, once, using plan §10. **You do not start that rewrite.**

---

## 6. Locked decisions — never relitigate

### Package invariants (`AGENTS.md`)

- Public BLE values are `Uint8Array` / `Readonly<Uint8Array>`
- Cancellation is `AbortSignal`, not caller transaction IDs
- Root package is host-neutral; no silent radio selection
- Explicit host entrypoints
- Explicit ownership + async teardown
- Capabilities from the instantiated backend at runtime
- Electron renderers / Tauri webviews never own the radio
- No silent fallback to Noble / Web Bluetooth / mocks
- Versioned native/IPC protocols, fail closed
- SemVer ≠ support/evidence labels
- RN floor 0.86+; Expo SDK 57+; no Expo Go
- Do not reintroduce the 3.x `BleManager` / `Device` / `Service` / `Characteristic` façade

### Train-specific locks

- No compatibility aliases for RC1 names
- No `isBackgroundEnabled` alias (`requiresBluetoothLeHardware` already in RC1)
- No iOS peripheral / GATT-server in 4.0
- Three layers only: application (root + host entrypoints), `/advanced`, `/backend-sdk`
- Public `BleManager` is **non-generic**
- One factory options bag: `BleManagerCreateOptions`
- Factories derive identity; apps do not pass `clientId` / `managerId` / `hostSessionScope`
- Restoration identity is host-derived from app identifier + one restoration ID (+ optional generation). **Never** random, **never** from package version or wall clock
- Ephemeral manager/operation IDs **may** be random
- Expo factory is a **thin** composition over the RN factory/native provider — not a second manager
- `connect()` never hides reconnect; reconnect is an opt-in supervisor (PR6)
- GATT objects are generation-bound views, **not** immortal 3.x `Device` graphs
- Stale objects fail with `gatt.stale-handle` before native dispatch
- `ScanQuery` v1 + Boolean rules + normalization + canonical matcher freeze in **PR4**
- PR4 does **not** include `ScanClause.peers` (no dormant placeholder)
- PR5 adds `ScanClause.peers` additively; unknown clause keys fail closed in PR4
- Application current-view is `duplicates: 'coalesced'` only; no second `scan.peers({ retentionMs })` helper
- PR9 may add an optional derived lost-peer timeout on that **same** coalesced view
- PR9 must be observationally equivalent to PR4’s pure matcher; if native cannot prove safe pushdown, broader native scan + residual filter
- PR9 may **not** change which observations match
- Bytes stay bytes; Base64 is an explicit codec only
- No crates.io crate until PR11
- No `scanForFirstMatch` name from older drafts; PR4 uses `find` / `scan` / `choose` as specified

Deferred for 4.0 (do not pull in): peripheral/GATT-server, L2CAP CoC, Classic/RFCOMM, Mesh, LE Audio, vendor DFU, universal profile registry, RF fault-injection hardware, system Bluetooth settings replacement.

### Freeze checkpoints

| Surface | Last planned break | Freeze |
|---|---:|---|
| Portable runtime manager/GATT/error/capability/scan semantics | PR4 | `4.0.0-rc.2` |
| Expo config-plugin schema and restoration configuration | PR10 | `4.0.0-rc.4` |
| Tauri Cargo/npm coordination, install shape, CLI taxonomy | PR11 | `4.0.0-rc.5` |

After PR4, no portable runtime breaks except a documented release-blocking defect. Host/tool exceptions may **not** reopen portable semantics.

### Canaries / publishes

Every PR packs and tests artifacts. Registry publish only at:

- `rc.2` after PR4
- `rc.3` after PR8
- `rc.4` after PR10
- `rc.5` after PR11
- `4.0.0` after PR12

**or** when a PR changes package shape, native ABI/protocol, native prebuilds, Expo plugin schema, or npm/Cargo coordination.

Never laptop-publish. Tag the **exact current `main`** commit. `.github/workflows/publish.yml` publishes via npm trusted publishing/OIDC. `4.0.0-rc.*` goes to npm dist-tag `latest`. GitHub Release is prerelease for RCs. Tag name equals `package.json` version.

---

## 7. The twelve PRs (execute in order)

Use plan §16 brief + the **full** numbered PR section as the spec. Inspect current code; do not assume RC1 paths still match after later PRs.

TDD order (plan §5.4): type tests → TCK → deterministic backend → shared core → first-party backends or explicit unsupported/limited → generated native/IPC → host composition + packed consumers → examples/docs → physical evidence only if a live support claim changes.

Change authority (plan §14.3): ADR/spec → schema/API report → TCK → core → generated bindings → backends → host packaging → examples/docs → evidence.

Milestone PRs **1, 2, 4, 10** use internal phases on the branch (see plan §14.2). PR2 lettered split is **pre-authorized**: 2A wire/identity/receipts, then 2B capability/recovery; public surfaces unexported until numbered PR2 is complete.

### PR 1 — `feat/4.0-public-contract-reset`

**Title:** `feat!: reset the pre-stable public API and host factories`  
**Goal:** Small non-generic application API. Low-level contracts move to `/advanced` and `/backend-sdk`. Host factories derive identity. No shims.  
**Must:** root application-only; one public name per resource; `{ signal?, timeoutMs? }`; stream presets; zero-plumbing factories; same public `BleManager` from all hosts; Tauri apps do not pass `invoke`/`Channel`; Web does not return a chooser/manager tuple; restoration-identity golden fixtures.  
**Must not:** new BLE features, pairing/reconnect/known-peers, React hooks, support-label promotion, compatibility aliases.  
**Façade rule:** no policy state beyond option normalization and one internal handle. Lifecycle/ownership/generations stay in the existing core.  
**Publish:** no.

### PR 2 — `feat/4.0-cross-host-semantics`

**Title:** `feat!: unify cross-host semantics, capabilities, and recoverable errors`  
**Goal:** One v2 host protocol; delete v1; trusted host issues every identity; truthful lifecycle/notification/write receipts; expanded capability catalog; public `BleError` with recovery actions; IPC records validated before resource creation.  
**Note:** Tauri protocol v2 **is in scope** (the RC1 Tauri path freeze is lifted).  
**Must not:** known-peers, reconnect supervisor, pairing, scan-query redesign.  
**Publish:** no. No RC between 2A and 2B.

### PR 3 — `feat/4.0-gatt-object-model`

**Title:** `feat!: ship the stable GATT object and property model`  
**Goal:** Generation-bound `GattDatabase` / `Service` / `Characteristic` / `Descriptor`; complete properties; indications; Service Changed invalidation; write receipts; UUID ergonomics.  
**Must:** objects wrap complete internal paths; stale → `gatt.stale-handle`; never auto-connect/rediscover/resubscribe; not a 3.x `Device` graph.  
**Publish:** no.

### PR 4 — `feat/4.0-high-level-workflow-scan-freeze`

**Title:** `feat!: freeze the high-level central workflow and scan-query contract`  
**Goal:** Five-minute path + freeze `ScanQuery` v1 (types, Boolean rules, normalization, golden vectors, **one** canonical residual matcher). `scan` / `find` / `choose`. `withScan` / `withConnection` / `withSubscription`. Defaults. Diagnostics. Cross-host quickstarts. Committed API reports + semantic contract suite.  
**Must:** no `ScanClause.peers`; unknown clause keys fail closed; `duplicates: 'coalesced'` is the only application current-view; golden vectors committed for PR9.  
**Publish after merge:** `4.0.0-rc.2`.

### PR 5 — `feat/4.0-known-peers`

**Title:** `feat: add known, connected, bonded, restored, and authorized peers`  
**Goal:** `BlePeerDirectory` with distinct sources. Portable `PeerReference`. **Add** `ScanClause.peers` under frozen Boolean rules.  
**Must not:** collapse into `getDevices()`; must not reinterpret RC2 scan fields.  
**Publish:** no (unless canary policy triggers).

### PR 6 — `feat/4.0-connection-intents-reconnect`

**Title:** `feat: add connection intents and deterministic reconnect supervision`  
**Goal:** connect-now vs when-available; opt-in deterministic supervisor.  
**Must:** `connect()` never becomes hidden reconnect; supervisor is caller-owned.  
**Publish:** no.

### PR 7 — `feat/4.0-security-pairing`

**Title:** `feat: add security state, pairing, bonding, and ceremonies`  
**Goal:** System-mediated pairing; truthful per-platform security/bond capabilities. Precise terms: pairing vs bonding vs encryption vs authorization.  
**Must not:** fake cross-platform pairing API.  
**Publish:** no.

### PR 8 — `feat/4.0-link-controls`

**Title:** `feat: add advanced link controls and GATT recovery`  
**Goal:** RSSI/MTU/PDU/priority/PHY/parameters/subrate, operation readiness, service-change/cache recovery. Capability-checked.  
**Publish after merge:** `4.0.0-rc.3`.

### PR 9 — `feat/scan-native-residual-planner`

**Title:** `feat: optimize scanning with native/residual planning and platform controls`  
**Goal:** Compile frozen PR4 queries into native pushdown + residual match. Plan diagnostics. Additive platform scan controls. Optional derived lost-peer timeout on the **existing** coalesced view.  
**Must:** consume PR4 golden vectors; observational equivalence; no second matcher.  
**Merge after PR8** (design notes may exist earlier; no commits until then).  
**Publish:** no unless canary policy triggers.

### PR 10 — `feat/expo-first-class-host`

**Title:** `feat: make Expo a first-class Unified BLE host`  
**Goal:** Plugin v2, permissions, thin `createExpoBleManager`, StrictMode-safe hooks, restoration, optional FGS, real example. Not a second BLE manager.  
**Publish after merge:** `4.0.0-rc.4`. Expo schema freeze.

### PR 11 — `feat/distribution-tooling-testkit`

**Title:** `feat: complete distribution tooling examples and testkit`  
**Goal:** crates.io Tauri crate, doctor/init CLI, executable examples as public-API consumers, install/diagnostics. Tauri install + CLI taxonomy freeze.  
**Publish after merge:** `4.0.0-rc.5`.

### PR 12 — `release/4.0.0-stable`

**Title:** `release: qualify and publish unified-ble-manager 4.0.0`  
**Goal:** Qualification only. Live evidence, performance/soak/security, final docs, immutable artifacts. Code changes only for blockers, tests/evidence, docs, packaging/provenance.  
**Publish after merge:** `4.0.0` (no prerelease suffix) via `RELEASE.md`.

After `4.0.0` is on npm `latest`, **stop**. Hand the user plan §10. Do not migrate product clients unless asked.

---

## 8. How to publish an RC or stable

Never `npm publish` from the laptop.

1. Numbered PR merged to `main`. CI green on that commit.
2. Version is on `main` (`package.json` **and** `src/implementation-version.ts`, changelog, generated docs/SBOM if required). Version bump may live in the milestone PR or a tiny follow-up **before** the tag.
3. Then:

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.0-rc.N"  # or 4.0.0
git diff --exit-code
git diff --cached --exit-code

# RELEASE.md local validation:
corepack enable
pnpm install --frozen-lockfile
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js

git tag -a v4.0.0-rc.N -m "v4.0.0-rc.N"   # or v4.0.0
git push origin v4.0.0-rc.N
```

4. Watch `.github/workflows/publish.yml`. Confirm npm version, dist-tags, GitHub Release (RCs are prerelease).
5. Do not push more commits to `main` between final verify and tag push.
6. Never move or recreate a published version tag. Never weaken evidence labels to make a release pass.

`4.0.0-rc.*` publishes to npm `latest` (same policy as RC1).

---

## 9. Validation commands

Focused while iterating, then before commit/push of a finished PR:

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

As relevant:

```sh
pnpm test:native-protocol
pnpm test:native-protocol:android
pnpm test:native-protocol:apple
pnpm test:native-protocol:winrt
pnpm performance:check
```

CI owns host/native build and ABI lanes. Never describe deterministic/mock/compile evidence as live-radio proof. Do not hand-edit generated support docs, `SBOM.cdx.json`, or `THIRD_PARTY_LICENSES.json`. Historical fixture names such as `BlePlxExample` may remain.

---

## 10. Session start — re-orient every time

```sh
cd /Users/sfourdrinier/src-trackourhealth/tmpBle/unified-ble-manager
git fetch origin --tags
git checkout main
git pull --ff-only origin main
git status -sb
git log -1 --oneline
git tag -l 'v4.0.0*'
npm view unified-ble-manager version dist-tags --json
gh pr list --repo sfourdrinier/unified-ble-manager --state all --limit 30
git branch -a | rg 'feat/4.0|feat/scan|feat/expo|feat/distribution|release/4.0' || true
```

Then read **this file’s §11**. Decide:

| Observation | Action |
|---|---|
| No 4.0 feat PR merged, latest tag `v4.0.0-rc.1` | Start **PR 1** |
| Feature branch exists, GitHub PR **not** open | Resume that numbered PR at implement or adversarial stage |
| GitHub PR open, not merged | Resume CodeRabbit / merge for **that** PR only |
| PR N merged, freeze RC not yet tagged | Publish that RC; do not start N+1 |
| `v4.0.0-rc.2` exists | PRs 1–4 done; start PR 5 |
| `v4.0.0-rc.3` exists | PRs 1–8 done; start PR 9 |
| `v4.0.0-rc.4` exists | Through PR 10 done; start PR 11 |
| `v4.0.0-rc.5` exists | Start PR 12 |
| `v4.0.0` exists and npm `latest` is `4.0.0` | **Train complete** |
| Dirty worktree | Inspect and report; do not discard |

If `package.json` is no longer `4.0.0-rc.1` and you expected to start PR 1, **stop and re-orient**. Someone already moved the train.

---

## 11. Living tracker (update on every PR)

Update this section in the **same branch** as the work, before opening the GitHub PR, and again after merge/publish.

### Train status

| # | Branch | GitHub PR | Adv. rounds | CodeRabbit rounds | Merged SHA | Tag / npm | Status |
|---:|---|---|---:|---:|---|---|---|
| 1 | `feat/4.0-public-contract-reset` | — | 0 | 0 | — | — | **in-progress** |
| 2 | `feat/4.0-cross-host-semantics` | — | 0 | 0 | — | — | blocked on 1 |
| 3 | `feat/4.0-gatt-object-model` | — | 0 | 0 | — | — | blocked on 2 |
| 4 | `feat/4.0-high-level-workflow-scan-freeze` | — | 0 | 0 | — | `v4.0.0-rc.2` after merge | blocked on 3 |
| 5 | `feat/4.0-known-peers` | — | 0 | 0 | — | — | blocked on 4 + rc.2 |
| 6 | `feat/4.0-connection-intents-reconnect` | — | 0 | 0 | — | — | blocked on 5 |
| 7 | `feat/4.0-security-pairing` | — | 0 | 0 | — | — | blocked on 6 |
| 8 | `feat/4.0-link-controls` | — | 0 | 0 | — | `v4.0.0-rc.3` after merge | blocked on 7 |
| 9 | `feat/scan-native-residual-planner` | — | 0 | 0 | — | — | blocked on 8 |
| 10 | `feat/expo-first-class-host` | — | 0 | 0 | — | `v4.0.0-rc.4` after merge | blocked on 9 |
| 11 | `feat/distribution-tooling-testkit` | — | 0 | 0 | — | `v4.0.0-rc.5` after merge | blocked on 10 |
| 12 | `release/4.0.0-stable` | — | 0 | 0 | — | `v4.0.0` after merge | blocked on 11 |

**Status vocabulary:** `next` | `in-progress` | `pushed-awaiting-adversarial` | `adversarial-round-K` | `ready-for-github-pr` | `github-pr-open` | `coderabbit-round-K` | `ready-to-merge` | `merged` | `publishing-rc` | `done` | `blocked on N`.

### Baseline (handoff)

```
date:           2026-08-20
main:           a33cfede72b3d642f94064d793b3c6465e252f4c
tag:            v4.0.0-rc.1
npm latest:     4.0.0-rc.1
plan:           docs/superpowers/plans/2026-08-20-next-12-prs.md  (Revision 4)
handoff:        docs/superpowers/plans/2026-08-20-handoff-rc1-to-stable-4.0.md
```

### Per-PR log template (copy for each numbered PR)

```
## PR N log
- started: YYYY-MM-DD
- branch: 
- implementer session:
- head SHA at first push:
- local gates run:
- adversarial round 1: reviewer / SHA / Critical / Important / Minor / dispositions
- adversarial round 2: ...
- ready-for-github: SHA
- GitHub PR:
- CodeRabbit round 1: ...
- CodeRabbit round 2: ...
- merged: SHA / date
- publish: tag / workflow URL / npm version (or n/a)
- leftover follow-ups (must be empty unless truly out of this PR's spec):
```

### PR 1 log — `feat/4.0-public-contract-reset` (in-progress)

- started: 2026-08-20
- branch: `feat/4.0-public-contract-reset`
- implementer session: mineral-mira (66d2d0bc-e491-46cb-9bbe-bc855931eae5)
- head SHA at first push: (pending — phase 1 local only)
- local gates run: `pnpm validate:evidence` ✓, `pnpm test:package` 103/103 ✓, `pnpm lint` ✓, `pnpm prepack` ✓, `pnpm release:artifacts:check` ✓, `node scripts/ci/pack-install-smoke.js` ✓ (npm exit-handler warning ignored, smoke still verified)
- phase 1 (ADR + public façade utilities):
  - `docs/ADR/2026-08-4.0-public-contract-reset.md` — accepted design baseline, 8th canonical ADR
  - `src/public/operation-options.ts` — `OperationOptions` + `normalizeOperationOptions` + `composeAbortSignal` (timeoutMs→Deadline, preserves earliest deadline, validates signal)
  - `src/public/stream-presets.ts` — `StreamPreset` → exact `StreamBudget` (`latest`/`balanced`/`lossless-bounded`/`custom`)
  - `src/public/host-identity.ts` — `deriveRestorationIdentity` (SHA-256 domain-separated, case-normalized appId, fixtures), `createEphemeralHostIdentity`, `BleManagerCreateOptions` + `normalizeBleManagerCreateOptions`
  - `src/public/index.ts` — façade barrel
  - `src/advanced.ts` — expert entrypoint re-exports
  - `src/expo.ts` — thin Expo composition stub (PR10)
  - `__tests__/fixtures/restoration-identity.golden.json` — 6 vectors
  - `__tests__/public-contract-reset.test.js` — TDD coverage for above
  - `package.json` exports `+ ./advanced`, `+ ./expo`
  - `scripts/ci/verify-package-artifacts.js` — allow `advanced.ts`, `expo.ts`, `public/**`
  - updated 5 package-surface tests to reflect new exports and 8th ADR
- adversarial round 1: (pending)
- adversarial round 2: (pending)
- ready-for-github: (pending)
- GitHub PR: (pending)
- CodeRabbit round 1: (pending)
- CodeRabbit round 2: (pending)
- merged: (pending)
- publish: n/a (PR1 has no publish per §6)
- leftover follow-ups (must be empty unless truly out of this PR's spec):
  - Façade non-generic BleManager, full host-factory zero-plumbing (RN/Web/Tauri/Node), root application-only re-export, removal of RC1 compatibility aliases, API reports `etc/api/*.api.md`, README/migration table — all still to land in same PR before adversarial review (per §7 PR1 spec). Phase 1 keeps tests green while preserving RC1 exports; subsequent commits will replace root exports and update helpers/packed consumers.


---

## 12. Stop and ask the user

Stop rather than guess if:

- A lock in §6 would have to be broken
- Portable semantics would change after PR4
- A public method would ship with fabricated backend behavior
- A support label would be promoted without retained live evidence
- `publish.yml` fails
- You want to skip TDD, skip adversarial review, skip CodeRabbit, or laptop-publish
- First-party product apps need changes before `4.0.0`
- You are unsure whether a later PR’s field belongs in the current PR (example: do **not** put `ScanClause.peers` in PR4)
- The tracker and git disagree

---

## 13. Skills to follow

- `using-superpowers` at session start
- `using-git-worktrees` before implementation
- `executing-plans` while implementing the numbered PR section
- `test-driven-development` for every behavior change (failing test first)
- `systematic-debugging` on failures
- `adversarial-review` then `dual-lens` / `review` / `requesting-code-review` **before** GitHub PR
- `verification-before-completion` before claiming a stage is done
- `finishing-a-development-branch` only after adversarial review is clean — and the choice for this train is **push already done; now create the GitHub PR**, not merge locally, not discard

---

## 14. First message to paste into a new agent session

```text
Read and follow, in order:

1. docs/superpowers/plans/2026-08-20-handoff-rc1-to-stable-4.0.md
2. docs/superpowers/plans/2026-08-20-next-12-prs.md  (Revision 4)

Re-orient with handoff §10. Update/consult the living tracker in §11.

Do ONE numbered PR only — whichever the tracker says is next (or in-flight).
Do not start the following PR.

Process (handoff wins over plan §14.2 on GitHub timing):
- branch from current origin/main
- implement with TDD, staying inside that PR's spec
- run gates
- commit and push ALL work to the feature branch
- run multiple independent adversarial reviews against the diff AND the PR goals
- fix, commit, push, repeat until Critical/Important are gone
- only then create the GitHub PR
- wait for CodeRabbit; do one or two review rounds
- merge with a merge commit after user approval
- return to main; publish an RC only if this PR is a freeze checkpoint
- stop

Do not resume RC1. Do not migrate product clients. Do not npm publish from a laptop.
Do not add RC1 compatibility aliases. Do not open a GitHub PR before adversarial review is clean.
```

To start the train from the handoff baseline, append:

```text
Execute PR 1 only. Branch feat/4.0-public-contract-reset from current main.
```
