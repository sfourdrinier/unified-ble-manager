# Post-PR5 audit disposition and PR6 closure plan

Source audit: `~/Downloads/unified-ble-manager-post-pr5-audit.md`
Audit snapshot: `bc4a347c496e994e97cec9da06e2c0a6223a72f8`
Current release source: `main` / `ab331517083c5a580894adb3d79d075f299c9db5` / `v4.0.0-rc.2`

## Release decision

RC2 is an immutable, quarantined prerelease. It reached the tag workflow because no confirmed P0 required cancelling the already-running release operation, and the tag/package/CI/provenance gates are independent of the unresolved application defects. Publication recovery completed on workflow attempt 2 after npm registry attestation propagation; the GitHub Release, npm `latest` dist-tag, exact tag commit, tarball, and provenance source binding were independently read back successfully. RC2 remains quarantined: it must not be treated as the finished RC2 closure, stable-ready, or evidence that the normal user journey is production-safe.

The existing PR6 implementation WIP is preserved in `stash@{0}` and is applied to the PR6 closure branch without popping the stash. No audit correction is added to the immutable RC2 tag.

## Disposition legend

| Disposition         | Meaning                                                                                           |
| ------------------- | ------------------------------------------------------------------------------------------------- |
| Confirmed / closure | Verified against current source; must be fixed and tested in the named PR6 closure slice.         |
| Confirmed / defer   | Real, but intentionally scheduled for a later slice with an explicit acceptance test.             |
| Fixed               | Current source or RC2 release correction addresses the audited claim; retain regression coverage. |
| Stale               | The audited code claim is no longer true; do not “fix” it again.                                  |

## Implementation findings

| Findings                                                                                  | Current disposition | Target                                                                                                                     |
| ----------------------------------------------------------------------------------------- | ------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| F-001, F-002, F-008, F-010, F-011, F-012, F-015, F-017, F-018, F-020, F-021               | Confirmed / closure | PR6A — public contract, options, lifecycle, identity                                                                       |
| F-003, F-004, F-005, F-006, F-007                                                  | Confirmed / closure | PR6B — host parity, examples, and protocol truth                                                                            |
| F-019                                                                                     | Confirmed / defer   | PR10 — native-authoritative React Native/Expo restoration; PR6B must preserve the fail-closed boundary and not derive a fallback |
| F-022, F-023, F-024, F-026, F-027, F-028, F-029, F-030, F-031, F-032, F-033, F-034, F-035 | Confirmed / closure | PR6C — IPC, Tauri, GATT, and fail-closed host lifecycle                                                                    |
| F-037, F-039, F-040, F-042                                                                | Confirmed / closure | PR6C — peer observation producers, provider capability truth, Web grant preservation                                       |
| F-025                                                                                     | Confirmed / closure | PR6C native lease-safe cancellation plus PR6E supervisor arbitration; four late-cancel/shared-peer race tests are required |
| F-009                                                                                     | Confirmed / closure | PR6A custom stream input validation and end-to-end stream-budget test                                                      |
| F-013                                                                                     | Confirmed / closure | PR6D public adapter/chooser/GATT/peer error-rehydration matrix                                                             |
| F-014                                                                                     | Confirmed / defer   | Later diagnostics slice; acceptance test must include backend-owned chooser resources                                      |
| F-016                                                                                     | Confirmed / defer   | PR9 — provider discovery-kind descriptor matrix; Electron/Tauri bootstrap descriptors are covered in PR6C                 |
| F-036                                                                                     | Confirmed / closure | PR6C permutation-invariant normalized query digest tests                                                                   |
| F-038                                                                                     | Confirmed / closure | PR6C normalized public observation delivery tests                                                                          |
| F-041                                                                                     | Confirmed / closure | PR6C stalled Web `getDevices()` abort/deadline race tests                                                                  |

F-015 is partially fixed in the current PR6 WIP by widening public and Tauri helpers to accept `PeerReference`; closure requires the cross-facade regression tests. F-042 is fixed for the implemented Web origin-authorized capability; retain its Web capability regression test, while cross-host limitations remain under F-039. F-026 is distinct from F-031: descriptor metadata fabrication and rejection of unknown descriptor writes require separate tests.

## Documentation and developer-experience findings

| Findings                                                                           | Current disposition                  | Target                                                                                                                                                  |
| ---------------------------------------------------------------------------------- | ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| D-001, D-015                                                                       | Fixed in RC2 source/release metadata | Retain regression tests                                                                                                                                 |
| D-002, D-003, D-005, D-006, D-007, D-008, D-009, D-010, D-011, D-012 | Confirmed / closure                  | PR6D — current application-first docs and examples                                                                                                      |
| D-004, D-014                                                                       | Confirmed / defer                    | PR10 — Expo v2 schema and current native configuration; PR6D must label RC1 configuration as retired/deferred             |
| D-013                                                                              | Confirmed / defer                    | PR11 distribution/install closure; acceptance test is an independent Tauri consumer installing paired npm/crate artifacts without a `node_modules` path |
| D-016, D-017, D-018                                                                | Confirmed / closure                  | PR6D — generated API reports and executable packed recipes                                                                                              |
| F-043                                                                              | Confirmed / closure                  | PR6D — safe empty-directory documentation recipe                                                                                                        |

## Required PR6 sequence

PR6 is a milestone with explicit internal slices. The currently applied supervisor implementation is provisional WIP; it must not merge before A–D evidence is recorded. Use a stacked PR6A → PR6E checklist (or equivalent commits/PRs) so the supervisor implementation is not the first or only PR6 change.

1. **PR6A — public contract and primary journey:** fix `find` in every façade (including Tauri), chooser filter/grant semantics, custom stream input, normalized public observations, cleanup/error propagation, all accepted-option behavior, `PeerReference` target consistency, secure identity generation, and the exact README journey.
2. **PR6B — host truth:** migrate Node and Electron renderer façades, remove all `@ts-nocheck` from TypeScript under both `example/` and `example-expo/`, align protocol v2 axes, and preserve a fail-closed restoration boundary. Native-authoritative React Native/Expo restoration remains the explicit PR10 implementation gate; PR6B must not create a JavaScript fallback.
3. **PR6C — IPC/GATT/peer safety:** close invalidation, overflow, lease-safe late cancellation, Tauri duplicate/authorization/generation behavior, descriptor metadata and unknown-write semantics, GATT schema/topology, peer-reference producers, Web service-grant preservation, stalled `getDevices()` cancellation/deadline races, normalized observation delivery, provider discovery descriptors, and runtime capability truth.
4. **PR6D — documentation/evidence:** rewrite current guides, compile packed recipes, replace stale positive assertions with forbidden-token/current-recipe checks, generate/diff API reports, and run the public error-rehydration matrix.
5. **PR6E — connection intents and supervisor:** resume the preserved PR6 WIP only after A–D’s blocking tests are green. Add direct/when-available capability gating, deterministic retry/gate state, generation/session cleanup, and arbitration without hidden reconnect.

## RC3 entry gate

RC3 may be prepared only after all confirmed closure findings above have either:

- a current-code fix plus focused RED→GREEN regression coverage; or
- an explicit capability/host limitation that makes the public claim truthful, with a linked test and documentation.

No P1 from this ledger may remain silently deferred. P2 findings may remain deferred only with the target PR and a finding-specific acceptance test recorded here. Before RC3, additionally require all non-outdated P1 review findings to be resolved, the full package/native/TCK gates, packed-consumer checks, generated API-report diff, explicit deterministic-versus-physical evidence labels, and a fresh current-source re-audit of every deferred finding. The RC2 package remains immutable; RC3 is the first release candidate eligible to claim this closure work.

## Release-process requirement

Every deferred finding must become a linked blocking issue with one target PR, one acceptance test, and one owner. The current deferred register is:

| Finding | Issue | Owner | Target |
| --- | --- | --- | --- |
| F-019 | [#34](https://github.com/sfourdrinier/unified-ble-manager/issues/34) | `sfourdrinier` | PR10 |
| D-004, D-014 | [#35](https://github.com/sfourdrinier/unified-ble-manager/issues/35) | `sfourdrinier` | PR10 |
| F-014 | [#36](https://github.com/sfourdrinier/unified-ble-manager/issues/36) | `sfourdrinier` | PR7 |
| F-016 | [#37](https://github.com/sfourdrinier/unified-ble-manager/issues/37) | `sfourdrinier` | PR9 |
| D-013 | [#38](https://github.com/sfourdrinier/unified-ble-manager/issues/38) | `sfourdrinier` | PR11 |

Each RC checkpoint must re-audit this register against current source; stale or unsupported findings must be explicitly closed rather than silently disappearing.

## PR6 current checkpoint

This branch has now verified the following PR6 slices against current source:

- the shared IPC public façade is used by both Electron and Tauri, with no duplicated public IPC/GATT policy;
- Electron’s authenticated router projects the public manager through scan, adapter state, connect, RSSI, discovery, characteristic/descriptor read/write, notification, and cleanup flows;
- cleanup receipts, write receipts, notification metadata, GATT properties/access, host-projected leases, discovery descriptors, and native-rich scan observations have focused regression coverage;
- Electron renderer documentation, composition examples, package-surface fixtures, and the reviewed renderer API report describe the public factory;
- Tauri late-cancellation cleanup and lifecycle terminal delivery retain retry ownership rather than abandoning failed cleanup.

The remaining PR6 gates are packed public-manager smoke on a clean supported
Node toolchain, the full PR6D documentation/recipe sweep, and the preserved
PR6E supervisor implementation/review. Native-authoritative React Native/Expo
restoration is an explicit PR10 deferral tracked by [#34](https://github.com/sfourdrinier/unified-ble-manager/issues/34);
PR6 must not claim it is implemented. The local Node22/npm packed smoke is
environment-blocked at `npm pack` (`Exit handler never called`) before consumer
assertions; hosted Node24 evidence must be refreshed for this branch before
RC3.
