<!-- ROADMAP.4.0.md -->

# Roadmap 4.0 — `unified-ble-manager`

> **Status: Historical record.** This roadmap's scope was delivered with the
> stable 4.0 releases. It is kept as a decision record and is not current
> guidance. See the [documentation map](docs/README.md).

**Status:** implemented 4.0 product scope; stable package/API release target

**Architecture authority:** [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)

**Platform proof inventory:** [`docs/GAPS.4.0.md`](docs/GAPS.4.0.md)

## Product decision

`unified-ble-manager@4.0.0` is a new open-source package line with no released 4.0 consumer baseline before this work. It is a clean-baseline product, not a compatibility release of `react-native-ble-plx` 3.x.

4.0 establishes one versioned backend contract, one shared policy core, bytes-only public/backend BLE contracts, `AbortSignal` cancellation, typed capabilities reported by instantiated backends, explicit manager/resource ownership, and bounded normalized events.

It intentionally does not preserve a permanent 3.x API, Base64/bytes dual API, static host capability table, legacy manager/port architecture, Noble wrapper, or scoped-package shim.

## 4.0 package scope

The stable 4.0 package includes:

- host-neutral public manager/core and public backend SDK;
- deterministic backend, conformance/TCK surface, scenarios, diagnostics, testing utilities, CLI, profiles, and generated contract documentation;
- React Native Android and Apple backends;
- browser Web Bluetooth integration;
- owned BlueZ, CoreBluetooth, and WinRT desktop backends;
- Electron main/renderer ownership and IPC boundary;
- explicit package exports and independent packed-consumer validation;
- supply-chain artifacts, governance, security, support, release automation, and evidence infrastructure.

Meta Quest, peripheral mode, Bluetooth Classic, LE Audio, L2CAP CoC, and the controllable nRF52840 physical fault-injection controller remain deferred to 4.1. Deterministic fault injection remains part of the 4.0 engineering proof, but is never presented as physical-radio evidence.

Tauri v2 desktop is part of 4.0: the package includes an isolated webview
surface and Rust plugin source for macOS, Windows, and Linux. Its public support
label remains evidence-driven; compile and deterministic proof alone do not
claim a physical-radio run.

## Product ownership

The package owns portable BLE-central mechanics: adapter state, scanning/chooser behavior, connection and GATT lifecycles, cancellation, operation ordering, bounded streams, capability composition, normalized errors, and diagnostics.

Applications and vendor libraries own device choice, vendor protocols, product reconnect policy, persistence, telemetry, UI, and product state. First-party consuming applications may be proving fixtures; they are never public API authority.

## Stable package and support claims

Stable `4.0.0` defines the public 4.x package/API contract. It replaces the alpha train as the normal installation target and publishes through the canonical `sfourdrinier/unified-ble-manager` repository.

`v4.0.0-alpha.40` is retained as the final alpha and repository-migration checkpoint. The earlier alpha train remains historical evidence of implementation and hardening, not the current consumer installation target.

Platform support labels remain evidence-based and independent from package SemVer:

| Label                 | Minimum evidence meaning                                                              |
| --------------------- | ------------------------------------------------------------------------------------- |
| Experimental          | Contract/implementation exists but support qualification is incomplete or may change. |
| Preview               | Intended surface plus deterministic/package proof with explicit live limitations.     |
| Live Preview          | Preview requirements plus the declared essential physical-radio vertical slice.       |
| Supported             | Declared live-radio scenarios and packaging requirements pass.                        |
| Reliability-qualified | Required background, reconnect, soak, and reliability evidence also passes.           |

No static platform matrix, package version, compile result, or mock can substitute for an instantiated backend's typed capability report and retained evidence.

A backend may remain Experimental while the package/API is stable. That is an explicit design choice: SemVer answers “what public contract can consumers depend on?” while the evidence label answers “what has this backend been proven to do on real host/radio conditions?”

## 4.0 completion boundary

The stable package release requires:

- final public package/API contract and exports;
- deterministic/TCK regression proof;
- relevant native compile/ABI gates;
- packed-consumer and clean-install proof;
- generated docs/artifacts in sync;
- SBOM/license inventory and source licensing consistency;
- trusted-publishing release path from `main`;
- honest generated support labels.

It does not require every implemented backend to reach the same support label on the same day. Higher platform labels continue to require their own retained evidence after 4.0.0 where evidence is incomplete.

## Release progression

| Milestone            | Meaning                                                                                                      |
| -------------------- | ------------------------------------------------------------------------------------------------------------ |
| Alpha train          | Public 4.0 contract implementation and hardening; published on npm `next`.                                   |
| Repository migration | `v4.0.0-alpha.40`; canonical source moves to `sfourdrinier/unified-ble-manager`.                             |
| Stable 4.0.0         | Public package/API contract moves to normal SemVer and npm `latest`; support labels remain evidence-derived. |
| 4.0.x                | Backward-compatible fixes/hardening within the stable 4.0 contract.                                          |
| 4.1                  | Deferred features/platform work and any compatible contract extensions planned for the next minor line.      |

The 3.x `react-native-ble-plx` line remains separate historical/maintenance context. Its API, Base64 bridge, `BlePort`, `PortBleManager`, static `supports()` matrix, examples, and source layout are migration/characterization inputs only and do not define 4.x behavior.

## Documentation rules

- [`MIGRATION_4.0.md`](MIGRATION_4.0.md) describes an explicit migration, not zero-change compatibility.
- [`RELEASE.md`](RELEASE.md) is the canonical tag/OIDC publication procedure from `main`.
- [`docs/PLATFORMS.md`](docs/PLATFORMS.md) distinguishes stable package SemVer from backend evidence labels.
- [`docs/GAPS.4.0.md`](docs/GAPS.4.0.md) records remaining platform/lab proof without blocking unrelated package/API stability.
- Generated support pages remain source-of-truth projections of evidence rather than hand-maintained marketing matrices.

## Historical baseline boundary

Historical 3.x source/docs remain audit and migration material. Historical live runs, compilation, or examples do not automatically establish current 4.0 backend support.

Before a backend receives a higher public support label, its relevant contract/TCK work, live scenario, evidence record, artifact binding where required, and revalidation rules must justify that label.

## Related records

- [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md) — implemented architecture and sequencing record
- [`docs/GAPS.4.0.md`](docs/GAPS.4.0.md) — platform, CI, lab, and proof inventory
- [`docs/PLATFORMS.md`](docs/PLATFORMS.md) — support/evidence interpretation
- [`MIGRATION_4.0.md`](MIGRATION_4.0.md) — migration boundary
- [`RELEASE.md`](RELEASE.md) — release procedure
- [`ROADMAP.md`](ROADMAP.md) — historical 3.x product record
