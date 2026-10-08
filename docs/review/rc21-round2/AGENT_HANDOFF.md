# Implementation handoff — PR #251, review round 2

Base this work on the reviewed head `b80542f39144949738b04dc7d19b4a7ee52cc8c6`. If the branch has moved, retain a clear mapping from each review finding to the new implementation. The detailed evidence and pinned source locations are in [REVIEW.md](REVIEW.md) and [findings.json](findings.json).

**The completion claim is not yet supported.** The last changes fixed several original defects, but current source still has ownership races, inconsistent public semantics, unreachable supported operations and incomplete runtime qualification. Address the 33 action items as actual behavior and lifecycle obligations. Do not merely change unsupported/limited prose or add tests that lock in the current incorrect result.

## 1. Repair the immediate gates and Linux ownership model

First address R2-01 and R2-02. The seven API reports are now fixed and the package pipeline executes. Repair the remaining same-head JS/native/format failures, especially the actual NAPI/RN missing methods in R2-34/35, and investigate the Windows recording setup timeout. Correct the Apple file-size gate and the two Electron tests’ synchronization without weakening assertions. The actual Electron retirement promises already settle correctly; do not implement a speculative cleanup fix for that intermediate state.

Treat R2-02–06 as one ownership model review across producer and consumer, with separate regression cases. An accepted Pair or one-shot GATT operation must protect its physical generation even before a UBM peer/token exists. Logical lease release must preserve an owner for deferred physical cleanup and retire the token without requiring the sender process to die. Every accepted acquisition that fails delivery must retire its exact admission. Physical end must clean pre-lease arrival/admission state as well as lease-peer state.

Retain the passing controls for earlier external Connect, immediate rejected admissions, dead-owner read completion and successful-Pair ATT security retry. Fixing one timing scenario must not revive the original bugs. Run the actual daemon/client integration after source controls pass; the supplied controls do not replace real D-Bus/SMP/controller qualification.

## 2. Apply one public operation and stream contract across hosts

Address R2-07–09/11 and R2-16–18/24–25/34–35 at the owning layers. Add the missing connection_parameters delegation to both NAPI DispatchRadio variants and the missing parameters/parameterEvents methods to RN NativeConnection and its production bridge shape. Keep ordinary public unsupported behavior guarded on incapable mobile platforms. A write helper must own bytes before awaiting, hold its connection FIFO position, retain the original deadline, settle on invalidation/link end, and keep cleanup retryable. RN’s custom wait loop violates three of those invariants. Electron/Tauri’s characteristic helper remains absent even though readiness is exposed. Reuse the common coordinator or implement an equivalent shared Rust-owned admission path; avoid a separate helper algorithm per host.

Snapshot and stream observations must share numeric validation, initial/live ordering and error vocabulary. A stream source failure must be delivered promptly with its original cause as a public `BleError`, independent of native cleanup success. The final commit fixes the IPC projection (R2-10); retain it while correcting RN’s remaining source-failure path. Queue loss at every transport hop must be observable or reconciled; downstream NAPI lag reconciliation is not triggered by earlier source loss.

Tauri’s live-watch admission must be reusable after successful close. Separate the concurrency bound from bounded idempotent release history, and verify more than 256 successful cycles per watch type on one unchanged renderer lease. Replacing the whole caller is currently the only capacity reset; callers should not need that for normal stream use.

## 3. Fix the complete scan-evidence pipeline

Address R2-12–15 and R2-33 together. Native ingress must retain all packets needed to prove a query. The shared evidence owner must combine partial fields with correct list semantics, retain the original freshness of each fact, use full peer identity including address type where applicable, and bound retention without requiring an expired peer to transmit again. Keep raw packet facts separate from accumulated query evidence.

Tests must start at the native ingress/public scan boundary, not only the cache helper. Include both advertisement/response orders, two nonempty service lists, long-running unrelated traffic, identical address bits across distinct identities, inactive-peer eviction and unknown connectability. Planner explanations must match actual available observations without pretending an unknown packet field is false.

## 4. Finish actual supported features and truthful graph projection

Address open R2-19/20/22/23 and R2-26–31. R2-21 is closed. Preserve hard requirements and soft preferences separately, honoring supported WinRT preferences before the first CCCD write while keeping genuine OS fallback. Keep primary/native cleanup errors structured. Tauri now serializes service restriction metadata correctly; retain that R2-21 closure.

Propagate primary/secondary service status and included-service relationships through all shared/native/wire/IPC layers. `primary:true` and `includedServices:[]` are not acceptable substitutes for omitted observations. Update strict wire schemas and generated surfaces from the source of truth, with occurrence-aware graph tests.

Complete the specific supported routes listed in the findings: Windows preferred connection presets; Linux/Windows known and system-connected directories; Android/iOS system-connected directories; optional Linux acquired-FD write/notify; Android 36.1 subrate, Android 36/36.1 encryption observations and Android scan batching/PHY; Windows scan mode/extended-advertisement options. Use actual OS/adapter/characteristic/permission admission. Keep genuine restrictions precise: Apple’s service-scoped retrieval, Windows build floors, Android minor SDK versions, BlueZ bearer identity and controller support.

Do not manufacture measurements from requested values, equate OS directory membership with an app-owned connection, infer LE connectivity from an ambiguous BR/EDR-capable field, or promise an unrestricted history of every device. Preserve unavailable/unknown for facts the OS cannot establish. No legacy shim is required; implement the correct shared contract.

## 5. Qualify the shipped consumer route

Address R2-32 after the package gates execute. Keep the useful raw-native identity/waker and generic facade tests, then join them through the **actual packed desktop factory/provider** with a controlled native backend. Run that route under Node and Bun on Linux, macOS and Windows, including actual CJS and ESM operations, events, cancellation, original deadlines, late completions, invalidation and zero-resource cleanup.

Record source-mode smoke, packed integration and change-scoped physical-radio evidence separately. Linux’s current source-mode smoke and packed qualifier successes are valid evidence of those specific paths; it does not establish macOS/Windows Bun or public-provider integration. The final-head Linux packed acceptance job now passes; its source still does not exercise the actual desktop public-provider/native route, and macOS/Windows lanes are absent.

## Required closure response

Return a compact table for tracked IDs `R2-01` through `R2-35`. R2-10 and R2-21 are already closed at this head; retain their regression evidence and address the remaining 33 open items:

| Field | Required content |
|---|---|
| Finding | Stable review ID and previous finding if linked |
| Resolution | Exact behavior now guaranteed, including genuine OS conditions |
| Implementation | Commit and source locations covering the complete owner/transport/public path |
| Regression | Test name and public/native route exercised; old behavior must fail the intended invariant |
| Validation | Same-head canonical CI job/result; native/radio evidence when required |
| Remaining limits | Specific OS/hardware/permission fact; never “not implemented” presented as an OS limit |

The supplied reproduction scripts establish the defects at the pinned head. Several intentionally assert the broken outcome. Replace those expectations with the desired invariant in lasting tests and retain the positive controls. A changed assertion, suppressed error, larger capacity or import-only smoke is not closure.

Finish with the same-head package/type/docs gates and the host/runtime qualification matrix. List any item that remains unresolved explicitly; do not describe the PR as complete while a supported route is still a placeholder or a mandatory gate has not executed.
