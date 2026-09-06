# Documentation map

Every document in this repository, what it is for, and whether it is live.
This index is the entry point for humans and AI agents alike: read the one-line
description, check the status, and open only what you need. The index is
machine-verified — `pnpm docs:check` fails when a document is missing from this
map, when a listed document does not exist, or when a row lacks a status.

Consumers of the published package should start with
[`../README.md`](../README.md) and [`../llms.txt`](../llms.txt). Contributors
and coding agents should read [`../AGENTS.md`](../AGENTS.md) first.

## Status taxonomy

- **Current** — live guidance. If it disagrees with the code, one of the two is
  a defect.
- **Historical** — a finished-lifecycle record (audit, fix tracker, delivered
  plan, superseded lineage doc). Never act on it as guidance; it is kept
  because the project treats its own history as evidence.
- **Generated** — owned by a generator. Never hand-edit; change the source of
  truth and rerun the generator (see [`AGENTS.md`](AGENTS.md) in this
  directory).

## Start here (consumers)

| Document | What it is | Status |
| --- | --- | --- |
| [`../README.md`](../README.md) | Product overview, install, entrypoint table, one complete React Native loop, method index | Current |
| [`../llms.txt`](../llms.txt) | Machine-readable package overview for AI coding agents: contract facts, entrypoints, curated doc links | Current |
| [`GETTING_STARTED.md`](GETTING_STARTED.md) | Host chooser and the first scan/connect/read/notify/teardown path | Current |
| [`TUTORIALS.md`](TUTORIALS.md) | Public API recipes: scan, connect, discover, read/write, subscribe, tear down | Current |
| [`HELPERS.md`](HELPERS.md) | Application helper recipes — `find`, scoped connections, GATT objects, notifications | Current |
| [`PROFILES_AND_COMMANDS.md`](PROFILES_AND_COMMANDS.md) | SIG profile codecs (heart rate, battery, DIS, thermometer, blood pressure) and GATT command helpers | Current |
| [`PEERS.md`](PEERS.md) | `PeerReference` peer directories, persistence, resolve-then-reconnect semantics | Current |
| [`CONNECTION_MANAGER.md`](CONNECTION_MANAGER.md) | Connection ownership, leases, generations, application-owned reconnect policy | Current |
| [`BONDING.md`](BONDING.md) | Pairing, bonding, encryption, and authentication semantics via `manager.security` | Current |
| [`../MIGRATION_4.0.md`](../MIGRATION_4.0.md) | Side-by-side migration map from `react-native-ble-plx` 3.x to 4.0 | Current |

## Host guides

| Document | What it is | Status |
| --- | --- | --- |
| [`WEB.md`](WEB.md) | Web Bluetooth host: chooser, HTTPS, user activation, lifecycle | Current |
| [`NODE.md`](NODE.md) | Node hosts: CoreBluetooth, WinRT, and BlueZ entrypoints, prebuilds, `dbus-next` peer | Current |
| [`ELECTRON.md`](ELECTRON.md) | Electron main/renderer split, IPC router, composition sequence | Current |
| [`ELECTRON_SECURITY_MODEL.md`](ELECTRON_SECURITY_MODEL.md) | Electron ownership and threat boundary; renderer permission snapshot rules | Current |
| [`TAURI.md`](TAURI.md) | Tauri v2 host: `createTauriBleManager()` and the Rust plugin install recipe | Current |
| [`EXPO_PLUGIN.md`](EXPO_PLUGIN.md) | Expo config-plugin option reference and development-build install | Current |
| [`CLI.md`](CLI.md) | `ubm` CLI command surface: `doctor`, `inspect`, `init`, `support-bundle`, `trace` | Current |

## Contract, architecture, and backend authoring

| Document | What it is | Status |
| --- | --- | --- |
| [`UNIFIED_SEMANTICS.md`](UNIFIED_SEMANTICS.md) | Normative behavior contract (MUST/MUST NOT) for any conforming unified BLE implementation | Current |
| [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md) | Clean-baseline architecture authority for the 4.0 package | Current |
| [`BACKEND_AUTHORING.md`](BACKEND_AUTHORING.md) | Authoring a third-party backend against `unified-ble-manager/backend-sdk` | Current |
| [`TCK.md`](TCK.md) | Backend TCK: required scenarios and running `runBackendAuthorTck` externally | Current |
| [`DISCOVERY_AND_PROFILES.md`](DISCOVERY_AND_PROFILES.md) | Discovery helpers and the profile subpath import map (inherited helpers marked transitional) | Current |
| [`FORK.md`](FORK.md) | Project lineage from `react-native-ble-plx` and the 4.x authority boundary | Current |
| [`ADR/2026-07-4.0-backend-contract.md`](ADR/2026-07-4.0-backend-contract.md) | ADR: one versioned host-neutral `BleCentralBackend` contract | Current |
| [`ADR/2026-07-4.0-boundary.md`](ADR/2026-07-4.0-boundary.md) | ADR: native/IPC wire projections and boundary protocol rules | Current |
| [`ADR/2026-07-4.0-capability-registry.md`](ADR/2026-07-4.0-capability-registry.md) | ADR: runtime capability registry, four states, evidence binding | Current |
| [`ADR/2026-07-4.0-open-source-governance.md`](ADR/2026-07-4.0-open-source-governance.md) | ADR: evidence-governed support claims, certification, deprecation | Current |
| [`ADR/2026-07-4.0-packaging.md`](ADR/2026-07-4.0-packaging.md) | ADR: ESM-first package, inert root, strict subpath exports | Current |
| [`ADR/2026-07-4.0-public-api.md`](ADR/2026-07-4.0-public-api.md) | ADR: clean-baseline public API, no 3.x emulation | Current |
| [`ADR/2026-07-4.0-rn-restoration-bootstrap.md`](ADR/2026-07-4.0-rn-restoration-bootstrap.md) | ADR: native-owned Apple restoration bootstrap before JS manager construction | Current |
| [`ADR/2026-08-4.0-public-contract-reset.md`](ADR/2026-08-4.0-public-contract-reset.md) | ADR: stable application boundary; supersedes RC1 provisional names | Current |

## Platform support, evidence, and performance

| Document | What it is | Status |
| --- | --- | --- |
| [`PLATFORMS.md`](PLATFORMS.md) | Platform support as an evidence index — label definitions, not a static matrix | Current |
| [`generated/PLATFORM_SUPPORT.md`](generated/PLATFORM_SUPPORT.md) | Platform support evidence projection for the current package version | Generated |
| [`generated/BACKEND_SDK_REFERENCE.md`](generated/BACKEND_SDK_REFERENCE.md) | Capability states, evidence levels, and required TCK scenario IDs | Generated |
| [`GAPS.4.0.md`](GAPS.4.0.md) | Platform/CI/evidence inventory — what proof exists per platform | Current |
| [`PERFORMANCE.md`](PERFORMANCE.md) | Performance and resource verification harness and its evidence limits | Current |
| [`evidence/react-native-apple-physical-device-readiness.md`](evidence/react-native-apple-physical-device-readiness.md) | What is proven before Apple hardware exists; what a live-radio receipt adds | Current |
| [`platforms/META_QUEST_4.1_SCOPE.md`](platforms/META_QUEST_4.1_SCOPE.md) | Maintainer decision deferring Meta Quest support to 4.1 | Current |

## Policy, security, and process

| Document | What it is | Status |
| --- | --- | --- |
| [`../AGENTS.md`](../AGENTS.md) | Single source of agent/contributor principles for this repository | Current |
| [`../CLAUDE.md`](../CLAUDE.md) | One-line import shim for `AGENTS.md` so the two cannot drift | Current |
| [`AGENTS.md`](AGENTS.md) | Documentation-tree conventions: status taxonomy, index maintenance, generated artifacts | Current |
| [`../scripts/AGENTS.md`](../scripts/AGENTS.md) | Script-tree conventions: generators own their outputs; verification commands | Current |
| [`../native/AGENTS.md`](../native/AGENTS.md) | Native-tree conventions: ABI exactness, versioned protocols, prebuild authority | Current |
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | Dev setup, canonical pre-PR checks, branch and PR flow | Current |
| [`../GOVERNANCE.md`](../GOVERNANCE.md) | Maintainer roles, decision process, ADR requirement | Current |
| [`../RELEASE.md`](../RELEASE.md) | Canonical tag-driven release procedure and invariants | Current |
| [`../SECURITY.md`](../SECURITY.md) | Vulnerability reporting policy | Current |
| [`../SUPPORT.md`](../SUPPORT.md) | Support policy: package SemVer vs evidence-backed backend labels | Current |
| [`security/UNIFIED_BLE_4.0_THREAT_MODEL.md`](security/UNIFIED_BLE_4.0_THREAT_MODEL.md) | Repo-wide threat model: trust boundaries, attacker classes, objectives | Current |
| [`DEPENDENCY_AND_ARTIFACT_POLICY.md`](DEPENDENCY_AND_ARTIFACT_POLICY.md) | SBOM and third-party license generation and policy | Current |
| [`../CHANGELOG.md`](../CHANGELOG.md) | Active 4.x changelog | Current |

## Historical records

Kept as evidence of how the project got here. Nothing below is guidance.

### Lineage and superseded scope

| Document | What it is | Status |
| --- | --- | --- |
| [`../CHANGELOG_HISTORY.md`](../CHANGELOG_HISTORY.md) | Archived 4.0.0-alpha prerelease changelog entries | Historical |
| [`../CHANGELOG-pre-3.0.0.md`](../CHANGELOG-pre-3.0.0.md) | Changelog of the `react-native-ble-plx` 1.x–2.x lineage | Historical |
| [`../ROADMAP.md`](../ROADMAP.md) | 3.x product roadmap for the forked `react-native-ble-plx`; does not govern 4.0 | Historical |
| [`../ROADMAP.4.0.md`](../ROADMAP.4.0.md) | 4.0 product scope decision record; scope delivered | Historical |
| [`../TVOS_SUPPORT_SPEC.md`](../TVOS_SUPPORT_SPEC.md) | Spec for adding tvOS support to the 3.x fork | Historical |
| [`README_V1.md`](README_V1.md) | Original `react-native-ble-plx` v1 README | Historical |
| [`MIGRATION_V1.md`](MIGRATION_V1.md) | 1.1.0 → 2.0.0 Podfile migration note | Historical |
| [`BACKGROUND.md`](BACKGROUND.md) | Background/restoration behavior characterization of inherited source; transitional, not 4.0 semantics | Historical |
| [`TVOS.md`](TVOS.md) | tvOS current-source characterization; not a 4.0 support claim | Historical |

### Fix trackers and review rounds

| Document | What it is | Status |
| --- | --- | --- |
| [`FIX_TRACKER.4.0.md`](FIX_TRACKER.4.0.md) | E2E review round-1 fix tracker | Historical |
| [`FIX_TRACKER.4.0-round2.md`](FIX_TRACKER.4.0-round2.md) | E2E review round-2 fix tracker | Historical |
| [`FIX_TRACKER.4.0-round3.md`](FIX_TRACKER.4.0-round3.md) | E2E review round-3 confirmed findings | Historical |
| [`review/README.md`](review/README.md) | What the review directory holds and why it is frozen | Current |
| [`review/2026-08-19-pr-26-external-review.md`](review/2026-08-19-pr-26-external-review.md) | Verbatim external review of PR #26 at rc.0 | Historical |
| [`review/2026-08-19-pr-26-external-review-verification.md`](review/2026-08-19-pr-26-external-review-verification.md) | Multi-agent verification of that review with per-item verdicts | Historical |
| [`review/2026-09-05-4.0.23-reliability-review.md`](review/2026-09-05-4.0.23-reliability-review.md) | External 4.0.23 reliability review (BLE-01..BLE-18) | Historical |
| [`review/2026-09-05-4.0.23-reliability-review-verification.md`](review/2026-09-05-4.0.23-reliability-review-verification.md) | Independent verification of those 18 findings | Historical |

The `review/` directory also holds machine-readable findings data
(`findings-4.0.json`, `findings-4.0-round2.json`,
`findings-4.0-round3-confirmed.json`), frozen alongside the rounds above.

### Audits

| Document | What it is | Status |
| --- | --- | --- |
| [`audits/README.md`](audits/README.md) | What the audits directory holds and why it is frozen | Current |
| [`audits/2026-08-21-post-pr5-audit-disposition.md`](audits/2026-08-21-post-pr5-audit-disposition.md) | Post-PR5 audit disposition and continuation ledger | Historical |
| [`audits/2026-08-22-pr41-review-inventory.md`](audits/2026-08-22-pr41-review-inventory.md) | PR41 reviewer-finding table with dispositions | Historical |
| [`audits/2026-08-23-pr10-review-inventory.md`](audits/2026-08-23-pr10-review-inventory.md) | PR10 (first-class Expo host) review disposition ledger | Historical |
| [`audits/2026-08-24-rc3-audit-current-inventory.md`](audits/2026-08-24-rc3-audit-current-inventory.md) | RC3 external review verified against RC4, per-finding verdicts | Historical |
| [`audits/ECOSYSTEM_BACKEND_AUTHOR_AUDIT.md`](audits/ECOSYSTEM_BACKEND_AUTHOR_AUDIT.md) | Phase 0 clean-room ecosystem/backend-author audit | Historical |
| [`audits/FIRST_CONSUMER_AUDIT.md`](audits/FIRST_CONSUMER_AUDIT.md) | Audit of the first consumer and its live BLE host families | Historical |
| [`audits/HOST_BACKEND_PACKAGE_AUDIT.md`](audits/HOST_BACKEND_PACKAGE_AUDIT.md) | Non-RN host, backend, and package-isolation audit | Historical |
| [`audits/REACT_NATIVE_FULL_SURFACE_AUDIT.md`](audits/REACT_NATIVE_FULL_SURFACE_AUDIT.md) | Full React Native surface inventory as Phase 0 ADR input | Historical |

### Early evidence reports

| Document | What it is | Status |
| --- | --- | --- |
| [`evidence/g0/core-model-correction-report.md`](evidence/g0/core-model-correction-report.md) | G0 spike: core model corrections after the draft-root adapter pass | Historical |
| [`evidence/g0/draft-contract-correction-report.md`](evidence/g0/draft-contract-correction-report.md) | Draft-types correction report on the declaration fixture | Historical |
| [`evidence/g0/draft-contract-coverage.md`](evidence/g0/draft-contract-coverage.md) | Draft-types coverage table: contract concerns to compile proof | Historical |

### Delivered plans and specs

| Document | What it is | Status |
| --- | --- | --- |
| [`superpowers/README.md`](superpowers/README.md) | What the plans/specs directory holds and why it is frozen | Current |
| [`superpowers/plans/2026-07-08-expo57-turbomodule-cng.md`](superpowers/plans/2026-07-08-expo57-turbomodule-cng.md) | Plan: Expo SDK 57 / RN 0.86 TurboModule modernization of the 3.x fork | Historical |
| [`superpowers/plans/2026-07-23-3.9.0-gated-cm-restored-state.md`](superpowers/plans/2026-07-23-3.9.0-gated-cm-restored-state.md) | Plan: 3.9.0 gated ConnectionManager and iOS restored-state handoff | Historical |
| [`superpowers/plans/2026-08-17-markdown-first-docs.md`](superpowers/plans/2026-08-17-markdown-first-docs.md) | Plan: markdown-first 4.x consumer docs rewrite | Historical |
| [`superpowers/plans/2026-08-19-rc1-review-response.md`](superpowers/plans/2026-08-19-rc1-review-response.md) | Master plan for landing the PR #26 review and cutting rc.1 | Historical |
| [`superpowers/plans/2026-08-19-rc1-slice-a-docs.md`](superpowers/plans/2026-08-19-rc1-slice-a-docs.md) | RC1 Slice A: documentation correctness pass | Historical |
| [`superpowers/plans/2026-08-19-rc1-slice-b-api.md`](superpowers/plans/2026-08-19-rc1-slice-b-api.md) | RC1 Slice B: pre-stable API and example corrections | Historical |
| [`superpowers/plans/2026-08-19-rc1-slice-c-tests.md`](superpowers/plans/2026-08-19-rc1-slice-c-tests.md) | RC1 Slice C: documentation verification tests | Historical |
| [`superpowers/plans/2026-08-19-rc1-slice-d-hosts.md`](superpowers/plans/2026-08-19-rc1-slice-d-hosts.md) | RC1 Slice D: host factories, adapter-state stream, host guides | Historical |
| [`superpowers/plans/2026-08-20-handoff-rc1-to-stable-4.0.md`](superpowers/plans/2026-08-20-handoff-rc1-to-stable-4.0.md) | Agent handoff operating procedure from rc.1 to stable 4.0.0 | Historical |
| [`superpowers/plans/2026-08-20-next-12-prs.md`](superpowers/plans/2026-08-20-next-12-prs.md) | "Next 12 Pull Requests" product plan from rc.1 to stable 4.0.0 | Historical |
| [`superpowers/plans/2026-08-22-android-phy-runtime-capability.md`](superpowers/plans/2026-08-22-android-phy-runtime-capability.md) | Plan: Android `connection:phy` capability truth via protocol-v2 handshake | Historical |
| [`superpowers/plans/2026-08-22-pr8-write-when-ready-remediation.md`](superpowers/plans/2026-08-22-pr8-write-when-ready-remediation.md) | Plan: bound and correct the frozen `writeWhenReady` path | Historical |
| [`superpowers/plans/2026-08-24-lifecycle-correctness-master.md`](superpowers/plans/2026-08-24-lifecycle-correctness-master.md) | Master plan sequencing lifecycle-correctness Plans A/B/C | Historical |
| [`superpowers/plans/2026-08-24-pr-a-ipc-lifecycle-and-clone.md`](superpowers/plans/2026-08-24-pr-a-ipc-lifecycle-and-clone.md) | Plan A: IPC lifecycle, stream close, clone/prototype safety | Historical |
| [`superpowers/plans/2026-08-24-pr-b-backend-unregister.md`](superpowers/plans/2026-08-24-pr-b-backend-unregister.md) | Plan B: backend stream unregister and overflow native release | Historical |
| [`superpowers/plans/2026-08-24-pr-c-react-hooks.md`](superpowers/plans/2026-08-24-pr-c-react-hooks.md) | Plan C: React hooks and adapter store ownership fixes | Historical |
| [`superpowers/plans/2026-08-30-web-bluetooth-example.md`](superpowers/plans/2026-08-30-web-bluetooth-example.md) | Plan: TypeScript/Vite Web Bluetooth example and `docs/WEB.md` | Historical |
| [`superpowers/plans/2026-09-05-release-4.0.25-reliability.md`](superpowers/plans/2026-09-05-release-4.0.25-reliability.md) | Plan: 4.0.25 reliability fixes for BLE-01..BLE-18 | Historical |
| [`superpowers/specs/2026-07-23-ios-tvos-ci-design.md`](superpowers/specs/2026-07-23-ios-tvos-ci-design.md) | Design spec: CI iOS and tvOS compile checks | Historical |
| [`superpowers/specs/2026-08-30-web-bluetooth-example-design.md`](superpowers/specs/2026-08-30-web-bluetooth-example-design.md) | Design spec for the Web Bluetooth TypeScript example | Historical |
