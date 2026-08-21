# AGENTS.md — Unified BLE Manager 4.x

## Canonical project

This repository is the canonical home of `unified-ble-manager` 4.x. `main` is the canonical development/release branch. `sfourdrinier/react-native-ble-plx` is historical and the home of the 3.x line; do not reintroduce its public contract into 4.x.

## Contract invariants

Read `docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md` before cross-cutting changes. Preserve these 4.x boundaries unless the user explicitly requests a versioned contract change:

- public BLE payloads are bytes (`Uint8Array` / `Readonly<Uint8Array>`), not implicit Base64 strings;
- cancellation uses `AbortSignal`, not caller-owned transaction IDs;
- the root package is host-neutral and selects no radio backend;
- host entrypoints are explicit for React Native, Web, Electron, and Node backends;
- managers, connections, GATT databases, subscriptions, and backend resources have explicit ownership and cleanup;
- runtime capabilities come from the instantiated backend, not a static platform matrix;
- Electron renderers do not own/load the native radio;
- production code must not silently fall back to Noble, Web Bluetooth, or a deterministic/simulated backend;
- backend/native protocols are versioned and fail closed;
- package SemVer and backend support/evidence labels are separate dimensions.

## Modernization floor

React Native host support targets React Native 0.86+. Expo integration targets Expo SDK 57+. Keep package metadata, examples, native defaults, and docs aligned unless the project intentionally raises a floor.

Do not add deprecated APIs, libraries, configuration, or build patterns when a current supported alternative exists. If deprecated usage cannot be removed safely, document why and add focused regression coverage.

## Validation

Use test-first changes for behavior, metadata, build configuration, and contract guards. Prefer focused tests while iterating, then run the relevant canonical checks:

```sh
corepack enable
pnpm install --frozen-lockfile
pnpm validate:evidence
pnpm test:package
pnpm test:plugin
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js
```

CI owns additional host/native build and ABI lanes. Never describe deterministic/mock/compile evidence as physical-radio proof.

## Releases

Follow `RELEASE.md`. Stable releases are tag-driven from the exact current `main` commit and published by `.github/workflows/publish.yml` through npm trusted publishing/OIDC. Do not publish manually or recreate/move a published version tag.

## Historical names

Old names such as `BlePlxExample` may remain in inherited native fixtures where renaming them adds build/ABI risk without changing the public package. Do not perform cosmetic native-symbol renames unless the compatibility/build impact is understood and fully validated.

## GOD rule — typing

No `as unknown`, `as any`, or `as T` to silence the checker. Infer by default; annotate only exported boundaries; use mappers/guards and fix the types.
