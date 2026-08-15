# Contributing to Unified BLE Manager

Thank you for improving `unified-ble-manager`. This repository is the canonical home of the 4.x package and accepts changes against `main`.

## Development setup

Requirements follow `package.json` and the host/toolchain you are changing. For the JavaScript/package surface:

```sh
corepack enable
pnpm install --frozen-lockfile
```

Run the focused tests for your change while iterating, then run the canonical package checks before opening a PR:

```sh
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js
```

CI owns additional macOS, Windows, Android, Expo, Apple, Electron, and native ABI/build lanes when relevant paths change.

## Branch and pull-request flow

1. Branch from current `main`.
2. Keep the change narrowly scoped.
3. Add or update deterministic tests for behavioral changes.
4. Update public documentation when a public contract changes.
5. Regenerate derived docs/artifacts instead of hand-editing generated output.
6. Open a PR against `main` and let canonical CI complete.

Do not target the legacy `react-native-ble-plx` repository for 4.x work.

## Architectural rules

The 4.0 contract intentionally has strong boundaries. Changes should preserve them unless a deliberate versioned contract change is being proposed:

- public BLE payloads are bytes (`Uint8Array`), not implicit Base64 strings;
- cancellation uses `AbortSignal` rather than caller-generated transaction IDs;
- the neutral root does not select a radio backend;
- host integrations are explicit public entrypoints;
- managers and backend resources have explicit ownership and asynchronous cleanup;
- Electron renderers do not own or load the radio implementation;
- native/private protocol changes remain versioned and fail closed;
- no production path silently falls back to Web Bluetooth, Noble, or a simulated backend;
- support/capability claims must match what the selected backend and retained evidence actually prove.

Read [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md) before making cross-cutting contract changes.

## Tests and evidence

A passing deterministic test is not automatically physical-radio evidence. Likewise, successful compilation does not automatically justify a Supported label.

When a change affects a support claim:

- add the deterministic/contract tests required for regression safety;
- capture the relevant platform evidence at the level actually achieved;
- update generated support data through the repository tooling;
- do not promote a label beyond the retained evidence.

See [`docs/PLATFORMS.md`](docs/PLATFORMS.md) and [`docs/GAPS.4.0.md`](docs/GAPS.4.0.md).

## Public API changes

4.0.0 establishes the stable 4.x package/API contract. After that release, public breaking changes require an appropriate SemVer boundary; they should not be slipped into a patch/minor release under an internal-refactor label.

When proposing a public API addition, include:

- the user problem and host(s) affected;
- public TypeScript contract;
- ownership/cancellation/error semantics;
- backend capability implications;
- deterministic tests and relevant native/host tests;
- migration or compatibility impact;
- documentation.

## Native changes

Keep native lifecycle ownership explicit and preserve cancellation, late-completion quarantine, adapter-loss, teardown, and generation semantics. Native changes should exercise the applicable compiled harness/build lane in addition to JavaScript tests.

Do not rename inherited native fixture/scheme/module identifiers merely for branding unless the rename has an explicit compatibility/build rationale and complete platform coverage.

## Documentation

Prefer current product terminology: **Unified BLE Manager** for the product and `unified-ble-manager` for the npm package. References to `react-native-ble-plx` should be historical or migration-specific.

Generated documentation should be regenerated through its script rather than edited manually.

## Security issues

Do not open a public issue containing vulnerability details. Follow [`SECURITY.md`](SECURITY.md).

## Licensing

The project is licensed under the **Apache License 2.0**. Unless explicitly agreed otherwise, contributions intentionally submitted for inclusion in this repository are provided under the same Apache-2.0 terms, consistent with the repository [`LICENSE`](LICENSE).

By contributing, you confirm that you have the right to submit the contribution and that required third-party attribution/license notices are preserved.
