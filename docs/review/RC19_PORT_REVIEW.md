# rc.19 — UBM port-review corrections

Status: Current release-branch tracker. Baseline: published rc.18 at
`bcb490a2cc96e5b1696a12db607caeaef1a95517`. This is one input review, not
the final release scope; later reviews join `release/5.0.0-rc.19`.

| Review item | UBM correction | Regression boundary |
| --- | --- | --- |
| 1 — migration | Current UBM 4.0.28-to-5.x guide shipped in the package; old ble-plx record explicitly historical/non-copyable; installation/restoration/capability/foreground-service recipes corrected | Consumer front-door links, version pin, package files and canonical packed-content gate |
| 2 — expert/export authority | `/advanced` listed in README and llms; AGENTS delegates to complete list; TV/background links present; Tauri has only generated signatures | Docs index/export coverage and generated API report checks |
| 3 — record-only iOS launch | Configured restoration bootstraps process host without native standing order; retain ASK authorization gate and tvOS refusal | Executable real Sessions bootstrap admission, startup identity policy, canonical Swift launch-observer compilation |
| 4 — Expo claim errors | Preserve normalized owner code/domain/platform/metadata/retryability/commit; shared Android no-source refusal also fixes host's earlier admission path | Real Expo Android no-source construction, Apple unavailable, structured failures and unknown failures |
| 5 — notifications | Document POST_NOTIFICATIONS as app-owned visibility permission, not foreground-service startup gate | Guidance checked against actual driver's required permissions |
| 6 — host recipes | Expo/bare TV construction and permissions distinguished; TVOS/GAPS historical; Tauri CCCD planning documented accurately | Executed Expo granted/denied recipe; doc checks against current native delivery planner |

One scoped CodeRabbit pass raised one minor ASK permission-recipe omission;
corrected in Getting Started and README/generated HTML with a focused regression.
No legacy IPC/error compatibility layer, new TV manager, Intel artifact,
speculative profile, dependency upgrade or backend-label promotion was added.
Bun-mono catalog and app-port changes are a separate downstream workstream.

## Evidence limits

The injected refusing installer test proves real record-only startup admission
and visible failure, not actual CBCentralManager allocation. Swift compilation
and ASK startup policy checks are not physical radio evidence. A configured
record-only physical cold-relaunch receipt remains to be obtained for this
runtime change. Existing timed picker-return/company-only, user-force-quit and
physical Apple TV gaps are unchanged. No phone was accessed for this patch.

## Validation

Focused regressions, canonical Apple native protocol, lint/typecheck, evidence
validation, prepack, 67 plugin tests, native status, generated docs and dependency
artifact checks passed locally. Clean detached Linux preflight and exact-head CI
are the remaining integration gates before the patch is merged into the release
branch. The eventual integrated release branch must pass its own gates before
one PR to main; this tracker does not authorize publication or stable promotion.
