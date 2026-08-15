<!-- docs/FORK.md -->

# Project lineage and 4.x boundary

`unified-ble-manager` evolved from the `react-native-ble-plx` lineage, including the maintained 4.0 work that previously lived at `sfourdrinier/react-native-ble-plx`. The complete Git ancestry is intentionally preserved for authorship, debugging history, and design provenance.

4.x is a new package and architecture. Historical `react-native-ble-plx` source remains useful attribution and migration material; it is not the 4.x API authority or a compatibility promise.

## Canonical 4.x project

- npm package: `unified-ble-manager`
- source: <https://github.com/sfourdrinier/unified-ble-manager>
- issues: <https://github.com/sfourdrinier/unified-ble-manager/issues>
- canonical branch: `main`
- license and attribution: [`../LICENSE`](../LICENSE) and [`../THIRD_PARTY_LICENSES.json`](../THIRD_PARTY_LICENSES.json)
- migration boundary: [`../MIGRATION_4.0.md`](../MIGRATION_4.0.md)

`v4.0.0-alpha.40` is the repository-migration checkpoint and final published alpha. Stable `4.0.0` establishes the normal 4.x package/API contract. The legacy repository remains the historical and 3.x home.

## Clean-baseline architecture

4.x provides:

- one versioned backend contract and one shared policy core;
- bytes-only public and backend GATT operations;
- `AbortSignal` cancellation and monotonic deadlines;
- generation-bound connection, database, attribute, and subscription handles;
- bounded streams with explicit overflow;
- runtime feature registrations that bind capabilities to implementations;
- first-party React Native Apple/Android, Web Bluetooth, BlueZ, CoreBluetooth, and WinRT backends;
- versioned Electron main/renderer IPC;
- a deterministic backend, public TCK, backend SDK, CLI, profiles, codecs, diagnostics, and evidence system.

It does not ship a 3.x manager shim, port abstraction, static host capability table, public transaction identifiers, normal-path Base64 GATT API, Noble fallback, or hidden global radio manager.

## Modernization floor

| Surface | Minimum |
| --- | --- |
| React Native | 0.86 |
| Expo | SDK 57 |
| Node.js | package `engines` declaration |
| Android | API 24 minimum; current project compile/target configuration |
| iOS and tvOS | 16.4 deployment target |
| React Native architecture | Generated TurboModule/JSI protocol boundary |

Host-specific support remains evidence-based. A stable package version, successful build, simulator, ABI load, or deterministic test does not create a physical-radio support claim; see [`PLATFORMS.md`](PLATFORMS.md).

## Repository fixtures

- `example/` validates a bare React Native integration.
- `example-expo/` validates Expo CNG and is regenerated during its gate.
- `example-web/` characterizes/builds the Web surface.
- `example-electron/` is the deterministic public Electron IPC fixture.

Repository fixtures may use `file:..` to test source changes. Independent and release gates install the canonical packed or published artifact where package-bound proof is required.

## Documentation authority

The root README and active Markdown pages under `docs/` describe the current public surface. Historical plans/audits remain useful records but do not override the current 4.x contract.

The architecture authority is [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md). Release scope is [`../ROADMAP.4.0.md`](../ROADMAP.4.0.md), support interpretation is [`PLATFORMS.md`](PLATFORMS.md), and remaining evidence work is [`GAPS.4.0.md`](GAPS.4.0.md).
