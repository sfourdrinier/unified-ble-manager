<!-- docs/audits/2026-08-24-rc3-audit-current-inventory.md -->

# RC3 audit vs current `4.0.0-rc.4` (`763397fe`)

**Source review:** `~/Downloads/unified-ble-manager-a423a73-code-review.md`  
**Reviewed SHA:** `a423a73` (RC3)  
**Verified SHA:** `763397fe` (RC4 / current `main`)  
**Date:** 2026-08-24  
**Disposition:** PR11 / `4.0.0-rc.5` paused. Remaining live findings land on `release/4.0.0-rc.4.1`.

Verification used six independent read-only agents, then a host pass over overlapping files.

| ID | Severity | Verdict at `763397fe` | Action on `rc.4.1` |
|---|---|---|---|
| RB-01 | Blocker | **SUPERSEDED** | Native restoration authority; no JS SHA-256 identity path |
| RB-02 | Blocker | STILL_LIVE | Electron `connection.maximum-write-length` route + tests |
| RB-03 | Blocker | STILL_LIVE | Signature-aware API report gate |
| P1-01 | High | STILL_LIVE | Bind public occurrence to path occurrence |
| P1-02 | High | STILL_LIVE | Abort/deadline on Electron `adapter.state` |
| P1-03 | High | STILL_LIVE | One current IPC database per connection |
| P1-04 | High | PARTIAL | Fail-closed connection-control echo validation |
| P1-05 | High | STILL_LIVE / PARTIAL | Runtime discriminant guards |
| P1-06 | High | STILL_LIVE | Honest IPC `deliverySelection` |
| P1-07 | High | PARTIAL | `stop()` must not return `released` while late work owns the session |
| P1-08 | High | STILL_LIVE | Peer-scoped monotonic security watch |
| P1-09 | High | STILL_LIVE | One end-to-end deadline for helpers and IPC |
| P1-10 | High | STILL_LIVE | Local stream terminal tears down remote producer |
| P1-11 | High | STILL_LIVE | Prune overflowed lifecycle subscribers |
| P1-12 | High | PARTIAL | Bound/clear coalesced fingerprint maps on the helper/IPC path |
| P1-13 | High | PARTIAL | Complete GATT snapshot validation |
| P1-14 | High | STILL_LIVE | Rehydrate host-factory errors to `BleError` |
| P2-01 | Medium | STILL_LIVE | No raw TypeError on malformed public options |
| P2-02 | Medium | STILL_LIVE | Validate notification metadata |
| P2-03 | Medium | STILL_LIVE | Diagnostics use public errors and integer counters |
| P2-04 | Medium | PARTIAL | Validate injected `randomBytes` |
| P2-05 | Medium | PARTIAL | Stream presets throw `argument.invalid` |
| P2-06 | Medium | STILL_LIVE | Locale-independent peer order; finite RSSI |
| P2-07 | Medium | PARTIAL | Preserve exact cleanup records |
| P2-08 | Medium | STILL_LIVE | Protect `main` (repo settings) |
| P2-09 | Medium | STILL_LIVE | Pin `dorny/paths-filter@v4.0.3` |
| P2-10 | Medium | STILL_LIVE | Correct RC3 changelog provenance |
| P2-11 | Medium | STILL_LIVE | Remove fully stale `agent/*` remotes (0 ahead) |
| P2-12 | Medium | STILL_LIVE | Tauri must apply or reject `instanceId`/`diagnostics` |

Governance items P2-08 and P2-11 are repository operations, not portable BLE semantics. They still belong to this hardening train.

## Implementation status on `release/4.0.0-rc.4.1`

Addressed in this branch (with tests where they are unit-reproducible):

- RB-02, P1-02, P1-03, P1-05 (mode/delivery/write-response), P1-06 Electron controllable delivery, P1-08, P1-09 helper/IPC deadline, P1-10 local overflow teardown, P1-11 subscriber prune, P1-12 helper-map bound, P1-14 factory rehydrate
- P1-01 sibling path occurrences, P1-04 RSSI/identity coercion, P2-01 create/scan/connect records, P2-02 notification metadata, P2-03 diagnostics, P2-04 randomBytes, P2-05 stream presets, P2-06 peer order/RSSI, P2-09 paths-filter pin, P2-10 RC3 changelog SHA, P2-12 Tauri option rejection

Still open on this branch:

- RB-03 signature-aware API report gate
- P1-07 `ConnectionSupervisor.stop()` released-while-late-configure (existing tests currently lock the old receipt)
- P1-13 remaining included-service/property schema validation
- P1-04 remaining MTU/PHY/write-length echo validation
- P2-07 unified cleanup-error type
- P2-08 GitHub `main` protection (repo settings)
- P2-11 stale `agent/*` remotes (operator deletion)
