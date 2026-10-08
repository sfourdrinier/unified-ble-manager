# rc.21 live BlueZ qualification on lx5090

The user explicitly approved activation and live tests on 2026-10-07. The corrected versioned bundle is installed and active; the live qualification batch is still in progress.

- Current service: `/opt/unified-ble-manager/bluez/5.87-ubm.10-30c195c3c53f/libexec/bluetoothd --experimental`.
- Prepared bundle: `/tmp/ubm-rc21-bluez-bundle-30c195c3` on lx5090.
- Retained previous prefix: `/opt/unified-ble-manager/bluez/5.87-ubm.9-f740c3151c20`.
- Exact previous override retained at that prefix as `rollback-345833ce.conf`.
- Corrected source patch SHA-256: `30c195c3c53f732bb4afa35c444d6f31dd05f57c228db6a4c6cef25a99a8e6b8`.
- Deployment binary SHA-256: `a4546cd9c8faedacec36a1309075f4b9f1c23ad51d5568f7621dd0ba9c23cb94`.
- Authority protocol: `[1,3,1]`.
- The owning bundle gate built the isolated production functions/tests and a separate deployment binary, retained corresponding source and build settings, and verified every bundle file hash. This is not a physical-radio receipt.

Activation used a host-supplied privileged invocation of the existing deployment owner. Its `activate.mjs` explicitly requires file-install/override-removal confirmation and never escalates itself. The first post-start check named the wrong D-Bus interface and triggered exact rollback. The corrected activation verified `org.unifiedblemanager.LinuxAuthority1.GetContract` as `[1,3,1]`; both attempts and rollback are retained. The following was the approved deployment plan:

1. Verify the existing root-owned installation receipt and exact service override hash; preserve the old override and service state for rollback.
2. Remove only the verified old UBM override using its owning rollback command. Retain the old versioned installation and Bluetooth state/storage.
3. Capture the effective base unit and exact original ExecStart argv; install the verified new bundle through its owning installer with explicit experimental/file-install confirmation. Preserve all unit sandbox/capability/storage directives.
4. Reload the service definition and restart Bluetooth. **This disconnects active Bluetooth clients on lx5090.** Verify the new daemon path/hash/version/protocol and selected adapter before tests.
5. Run bounded live ownership/pairing/foreign-operation/protected cleanup and eligible acquired-FD public Node/Bun scenarios, retaining exact source, bundle, runtime and controller evidence. Do not invent pairing/FD eligibility or physical outcomes from fixtures.
6. Restore the previously verified override and restart Bluetooth if deployment or qualification fails; retain failure logs and the unused new prefix. A successful qualification can keep the corrected daemon, with the original versioned prefix and rollback record retained.

No bond deletion, pairing reset, OS package replacement or default public package publication is part of this operation.

## Executed live checks and remaining scope

Checkpoint `345833ce3f4b220c06e8055bd8115dd3d3e071d5` has live daemon/controller receipts for 1,200 protected reserve/connect/release/ACK cycles with continuously live lease senders, foreign notification sender lifetime, and pending Pair refusal, cancellation, sender death and Pair opening the link before the first lease. Public Node and Bun acquired write/notify routes pass with negotiated MTU 517, actual notification bytes, conflicting acquisition refusal, already-aborted write refusal, released teardown and zero public/backend counters. FD socket acceptance reports unknown commit; the simulated server rejects the unmodeled vendor payload. These results do not claim remote write acceptance.

Success pairing, held foreign one-shot operations across lease boundaries, controller generation cleanup and the remaining acquired-FD failure/HUP scenarios are still pending. The attempted reverse incoming-link fixture did not reach a GATT read and is retained as failed setup, not acceptance evidence.

The user also authorized the existing `windows-ubm` VM on lx5090. Windows 11 build 26200 now owns the TP-Link USB controller formerly exposed as Linux hci0; Linux hci1 hosts the simulated H10 GATT application. The exact non-publishing candidate from checkpoint 345833ce is installed in an isolated Windows consumer, with SHA-256 `9568a0338ebfb827c1f7a5fef5078d56aa94ba6e9f97f64a769d9e9c27f46def`. Actual public WinRT parameter read/watch, three preferred preset requests, battery read, HR notification, observed/unknown service metadata and connected-directory retrieval have executed. The joined Node/Bun CJS/ESM live batch remains in progress. The native adapter reports `IsExtendedAdvertisingSupported=false`; the library explicitly refuses extended reception. Raw WinRT also refuses None-mode watcher start with HRESULT `0x80070032`, which the public route preserves. Neither refusal is relabeled a successful scan.

These tests use actual USB controllers and an application-simulated peripheral over RF. They do not constitute fixed physical H10 interoperability qualification or promote retained platform support labels. Logs and executable qualification scripts currently remain under `/tmp/ubm-rc21-verification/`; the completed handoff must retain their source and binary identities, results and cleanup receipts.
