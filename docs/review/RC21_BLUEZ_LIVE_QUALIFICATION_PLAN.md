# rc.21 live BlueZ qualification on lx5090

The corrected versioned bundle has been built and verified without installing it or changing the running Bluetooth service.

- Current service: `/opt/unified-ble-manager/bluez/5.87-ubm.9-f740c3151c20/libexec/bluetoothd --experimental`.
- Prepared bundle: `/tmp/ubm-rc21-bluez-bundle-30c195c3` on lx5090.
- Proposed prefix: `/opt/unified-ble-manager/bluez/5.87-ubm.10-30c195c3c53f`.
- Corrected source patch SHA-256: `30c195c3c53f732bb4afa35c444d6f31dd05f57c228db6a4c6cef25a99a8e6b8`.
- Deployment binary SHA-256: `a4546cd9c8faedacec36a1309075f4b9f1c23ad51d5568f7621dd0ba9c23cb94`.
- Authority protocol: `[1,3,1]`.
- The owning bundle gate built the isolated production functions/tests and a separate deployment binary, retained corresponding source and build settings, and verified every bundle file hash. This is not a physical-radio receipt.

The proposed operation requires a host-supplied privileged invocation of the existing deployment owner. Its `activate.mjs` explicitly requires file-install/override-removal confirmation and never escalates itself.

1. Verify the existing root-owned installation receipt and exact service override hash; preserve the old override and service state for rollback.
2. Remove only the verified old UBM override using its owning rollback command. Retain the old versioned installation and Bluetooth state/storage.
3. Capture the effective base unit and exact original ExecStart argv; install the verified new bundle through its owning installer with explicit experimental/file-install confirmation. Preserve all unit sandbox/capability/storage directives.
4. Reload the service definition and restart Bluetooth. **This disconnects active Bluetooth clients on lx5090.** Verify the new daemon path/hash/version/protocol and selected adapter before tests.
5. Run bounded live ownership/pairing/foreign-operation/protected cleanup and eligible acquired-FD public Node/Bun scenarios, retaining exact source, bundle, runtime and controller evidence. Do not invent pairing/FD eligibility or physical outcomes from fixtures.
6. Restore the previously verified override and restart Bluetooth if deployment or qualification fails; retain failure logs and the unused new prefix. A successful qualification can keep the corrected daemon, with the original versioned prefix and rollback record retained.

No bond deletion, pairing reset, OS package replacement or default public package publication is part of this operation. Windows physical qualification remains independently pending identification of an available Windows host and peripheral.
