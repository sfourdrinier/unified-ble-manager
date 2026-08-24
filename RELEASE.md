<!-- RELEASE.md -->

# Release process

This document is the canonical release procedure for `unified-ble-manager`.

## Canonical release identity

- GitHub repository: `sfourdrinier/unified-ble-manager`
- Release branch: `main`
- npm package: `unified-ble-manager`
- GitHub Actions workflow: `.github/workflows/publish.yml`
- GitHub Environment used by the publish job: `npm`
- Stable npm dist-tag: `latest`
- General prerelease npm dist-tag: `next`; active `4.0.0-rc.*` release-train candidates publish to `latest` until stable 4.0.0.

Releases are tag-driven and published by GitHub Actions through npm trusted publishing/OIDC. Do not use a long-lived `NPM_TOKEN` or publish a normal release from a developer laptop.

## Trusted publisher configuration

The npm package's trusted publisher must identify this repository, not the legacy `react-native-ble-plx` repository:

- provider: GitHub Actions
- owner/user: `sfourdrinier`
- repository: `unified-ble-manager`
- workflow filename: `publish.yml`
- environment: `npm`
- package: `unified-ble-manager`

The workflow requests `id-token: write` and publishes with provenance.

If the trusted publisher still points at the legacy repository, update it before pushing a stable tag. A valid source tree and green CI cannot compensate for an OIDC publisher identity mismatch.

## Stable package versus platform support

Stable SemVer and platform support qualification are independent.

A stable `4.0.0` release means the documented public package/API contract is the supported 4.0 contract and is governed by normal SemVer expectations. It does **not** automatically promote any React Native, Web, Electron, CoreBluetooth, WinRT, or BlueZ backend to Preview, Supported, or Reliability-qualified.

Backend labels are derived from retained evidence and remain fail-closed. See [`docs/PLATFORMS.md`](docs/PLATFORMS.md) and [`docs/generated/PLATFORM_SUPPORT.md`](docs/generated/PLATFORM_SUPPORT.md).

## Release invariants

Before a stable release tag is pushed:

1. `main` is the exact source to be released.
2. `package.json` contains the final version with no prerelease suffix.
3. `CHANGELOG.md` contains the release entry and intended release date.
4. generated platform documentation is current.
5. `SBOM.cdx.json` and `THIRD_PARTY_LICENSES.json` are generated from the same package metadata/lockfile.
6. canonical CI is green for the release commit.
7. package/repository/homepage/bug URLs point at `sfourdrinier/unified-ble-manager`.
8. the license metadata and root `LICENSE` agree.
9. the npm trusted publisher points at this repository/workflow/environment.
10. GitHub private vulnerability reporting is enabled for the canonical repository.
11. the complete macOS/Windows `arm64`/`x64` Node-API prebuild matrix is produced from the release tag and verified under Node and Electron.

## Required local validation

From a clean checkout of the release commit:

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
node scripts/ci/g6a-packed-consumer-proof.js
npm pack --dry-run
```

CI additionally owns the platform-specific native compilation and ABI lanes.

## Releasing 4.0.0-rc.*

Active `4.0.0-rc.*` release-train candidates publish to npm `latest` so a bare `pnpm add unified-ble-manager` installs the current 4.0 line. The GitHub Release is marked prerelease. Each candidate is cut from the exact current `main` merge commit; the workflow verifies tag/package version equality.

On release day, set `release_candidate` to the exact candidate required by the
release plan. RC2, RC3, RC4, `4.0.0-rc.4.1`, and RC5 are already immutable
once tagged. Stable `4.0.0` is the current train head after PR12.

```sh
release_candidate=4.0.0-rc.N

git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "$release_candidate"
git diff --exit-code
git diff --cached --exit-code

git tag -a "v$release_candidate" -m "v$release_candidate"
git push origin "v$release_candidate"
```

Do not push another commit to `main` between the final verification and the tag push.

## Releasing 4.0.0

The source version is prepared on `main` before the tag. The release workflow verifies that every initial release tag points at the exact current `main` commit before publication; do not create that tag from a side branch or an older commit.

On release day:

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.0"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.0 -m "v4.0.0"
git push origin v4.0.0
```

Do not push another commit to `main` between the final verification and the tag push.

## What the publish workflow does

For a valid version tag, `.github/workflows/publish.yml`:

1. checks out the tagged commit and builds Node-API v8 prebuilds for macOS and Windows on `arm64` and `x64` native runners;
2. loads each prebuild under Node and the same file under Electron;
3. assembles and hashes the complete prebuild matrix into `native/PREBUILDS.json`;
4. verifies tag name and `package.json` version agree;
5. classifies the npm dist-tag (`4.0.0-rc.*` and later stables to `latest`; other prereleases to `next`);
6. before any initial publication, verifies the tag commit equals the current `main` commit;
7. validates evidence-record syntax/integrity without manufacturing support claims;
8. runs package, plugin, lint/typecheck, generated-artifact, packed-consumer, and deterministic Electron checks;
9. runs the required Android/Expo/native-host gates;
10. verifies package contents and generated dependency artifacts;
11. publishes the exact prebuild-bearing tarball through npm trusted publishing with provenance;
12. waits for the registry artifact and verifies the published tarball/digest path;
13. on a post-publish recovery rerun, replaces any newly built local tarball with the immutable npm registry tarball;
14. creates the GitHub Release only after npm publication and provenance verification succeed.

Stable versions publish to `latest`. Active `4.0.0-rc.*` candidates also publish to `latest`; other hyphenated SemVer prereleases publish to `next` and create GitHub prereleases.

## Post-release verification

After the workflow succeeds:

```sh
release_candidate=4.0.0-rc.N

npm view "unified-ble-manager@$release_candidate" version
npm view unified-ble-manager dist-tags --json
npm view "unified-ble-manager@$release_candidate" repository --json
npm view "unified-ble-manager@$release_candidate" license
```

Then verify:

- npm `latest` resolves to the selected release candidate;
- the npm package page shows provenance for the published artifact;
- the GitHub Release exists at the selected candidate tag and is marked prerelease;
- its attached tarball/SBOM/license artifacts correspond to the release workflow output;
- a clean consumer can install `unified-ble-manager` (no version pin) and import the documented host entrypoints.

After the later stable tag, repeat the same checks for `4.0.0`. `latest` then moves to `4.0.0`.

## Failed release or partial publish

Never move or recreate a published version tag to hide a failed release.

- If the workflow fails **before npm publication**, fix the source on `main`, increment/version as appropriate, and create the correct new tag.
- If npm publication succeeds but a later GitHub-release step fails, preserve the immutable npm version and rerun the workflow. The recovery path skips the current-`main` admission check and attaches the exact npm registry tarball rather than newly linked native binaries.
- If a defect is discovered after `4.0.0` is published, fix it and release `4.0.1`; do not replace `4.0.0`.

## Prereleases after 4.0.0

Future prereleases use normal SemVer suffixes such as `4.1.0-alpha.1`. They publish to `next` and must never replace `latest` until a final version is released.

## Release artifacts and evidence

`SBOM.cdx.json`, `THIRD_PARTY_LICENSES.json`, generated platform support, and retained evidence records must be reproducible from the tagged source. Evidence records can justify platform support claims, but absence of an optional physical-radio qualification record does not change the SemVer of an otherwise validated stable package.

The release process must never synthesize, backdate, or relabel hardware evidence merely to make a release gate pass.

## Architecture authority

The normative 4.0 architecture and public-contract decisions are recorded in [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md). This release procedure controls publication mechanics; it does not override those architecture decisions.
