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
- Current 5.0 prerelease npm dist-tag: `next`. The 4.0 stable line remains on `latest` until a final 5.0 release.

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
8. the license metadata (`package.json` license field, Cargo `license-file` pointers, SBOM expression) and the license documents (`LICENSE` — the UBM text —, `LICENSE-UBM-SOURCE-AVAILABLE-1.0.md`, `LICENSES/Apache-2.0.txt` for retained Apache material, `NOTICE`) agree.
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
The clean Linux preflight checks pinned workspace Rust formatting before
package pretests; its separate Tauri formatting check does not replace that
workspace gate.

On Linux, the existing Rust CI lane and clean preflight share the same BlueZ
lifecycle regression gate:

```sh
bash scripts/ci/test-bluez-private-bus.sh
```

Each suite runs on a separate private D-Bus, covering owned match cleanup,
split connection signals, LE bearer ownership, strict GATT snapshot consumers,
and the source-only daemon-extension build against the pinned official archive.
The Rust-only lane needs no pnpm installation for that source gate. A failing
suite stops the gate. No daemon is installed or launched. This tests protocol
handling against controlled services; it does not
qualify physical-radio behavior or replace native binary matrix checks.
Deployment and rollback require a separate approved host window; see
[`docs/BLUEZ_LE_GATT.md`](docs/BLUEZ_LE_GATT.md).

## Historical 4.0.0-rc.\* release train

The former `4.0.0-rc.*` release-train candidates published to npm `latest` so a bare `pnpm add unified-ble-manager` installed the then-current 4.0 line. The GitHub Release was marked prerelease. Each candidate was cut from the exact current `main` merge commit; the workflow verifies tag/package version equality.

On release day, set `release_candidate` to the exact candidate required by the
release plan. RC2, RC3, RC4, `4.0.0-rc.4.1`, and RC5 are already immutable
once tagged. Stable `4.0.0` through `4.0.20` are immutable. The unpublished
`v4.0.21` tag is also immutable after its cancelled workflow. `4.0.22`,
`4.0.23`, `4.0.24`, `4.0.25`, `4.0.26`, and `4.0.27` are immutable tagged
history. `4.0.28` is immutable tagged history. The unpublished
`v5.0.0-rc.5` tag is immutable after its publish-only Tauri consumer failure.
The current candidate is `5.0.0-rc.15`; rc.14 is immutable published history.

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

The first stable tag `v4.0.0` is immutable published history. Do not recreate or move it.

```sh
git tag -a v4.0.0 -m "v4.0.0"
```

## Releasing 5.0.0-rc.15

Release only from the exact current `main` commit after the focused rc.14
correction PR and canonical CI succeed. Verify `package.json` and the changelog
identify `5.0.0-rc.15`, the worktree is clean, and refreshed native artifacts match
the corrected source. Push a new annotated `v5.0.0-rc.15` tag; the existing
workflow publishes to npm `next` with provenance and creates a GitHub prerelease.
Never publish manually or move an earlier tag.

Candidate-specific regressions must synchronously force independent storage
stop followed by late outbox ingress before producer disposal, then prove
continuing admission and bounded inactive handles across 1,000 completed cycles.
Real uncommitted I/O failures must remain visible and pinned; live ownership,
prepared-prefix replay, restart, cross-process refusal and independent export
controls remain mandatory. Direct and IPC public helpers must expose `BleError`
before cleanup aggregation while preserving application exceptions, the original
deadline and exactly-once release. Run the focused native acceptance against the
newly built and actual published binary; identify synthetic-radio scope explicitly.

Use the existing clean-worktree preflight and cross-platform/release gates, not
new workflows. No version-only phone campaign or support-label promotion is
required. This is a corrective candidate, not final stable 5.0.

## Releasing 5.0.0-rc.14 (historical)

`v5.0.0-rc.14` is immutable published history. The following records its release
procedure; do not execute its tag-creation instructions again.

Release only from the exact current `main` commit after the focused recording
correction PR and canonical CI succeed. Verify `package.json` and the changelog
identify `5.0.0-rc.14`, the worktree is clean, and all applicable native artifacts
match the corrected source. Push a new annotated `v5.0.0-rc.14` tag; the existing
workflow publishes the exact package to npm `next` with provenance and creates
a GitHub prerelease. Never publish manually or move an earlier tag.

The candidate-specific gates cover independent-store export during acquisition,
bounded inactive journal ownership, safe cross-process admission, retained
storage failure truth, append-only loss deltas, control-only journal attachment,
and two-peer mobile recording through the actual typed controller. Verify the
patched native binary with the original export/retention controls and a clean
packed consumer. Repeat physical scenarios only for changed runtime behavior
or a concrete reproduction; version and documentation changes need no phone
rerun. Preserve the exact scope and limitations of prior physical evidence.
This is not stable 5.0 and does not promote backend qualification labels.

## Releasing 5.0.0-rc.13 (historical)

`v5.0.0-rc.13` is immutable published history. The following records its
release procedure; do not execute its tag-creation instructions again.

`v5.0.0-rc.12` is immutable published history. Release `v5.0.0-rc.13` only
from the exact current `main` commit after the review-remediation PR and
canonical CI succeed. Verify `package.json` is `5.0.0-rc.13`, the worktree is
clean, and release-note extraction finds `## [5.0.0-rc.13]`. Push a new
annotated `v5.0.0-rc.13` tag; the existing tag workflow publishes the exact
packed artifact to npm `next` with provenance and creates a GitHub prerelease.
Never publish manually or move an earlier tag. This is not the final 5.0
release and does not promote backend support labels.

This candidate includes native-continuation ownership and durable recording,
per-generation setup replay, native finite scan lifetime, connected-peer
retrieval, and the lifecycle and consumer fixes recorded in its changelog.
Follow the [native artifact lifecycle](docs/NATIVE_ARTIFACTS.md) for changed
native sources and verify every packed artifact's source/schema identity.
Package version changes alone do not change native source identities; they
still require fresh JavaScript seals, generated release artifacts, and exact
packed-consumer validation. Keep deterministic, compile, and physical-radio
evidence distinct, including the exact artifact and limitations of each
retained physical run. Do not relabel earlier candidate evidence.

## Releasing 5.0.0-rc.12

The instructions below record the historical rc.12 IPC remediation, not the
subsequent native-continuation work. The published `v5.0.0-rc.12` tag is
immutable; do not execute its tag-creation instructions again.

`v5.0.0-rc.11` is immutable published history. Release `v5.0.0-rc.12` only
from the exact current `main` commit after the review-remediation PR and
canonical CI succeed. Verify `package.json` is `5.0.0-rc.12`, the worktree is
clean, and release-note extraction finds `## [5.0.0-rc.12]`. Push a new
annotated `v5.0.0-rc.12` tag; the existing tag workflow publishes the exact
packed artifact to npm `next` with provenance and creates a GitHub prerelease.
Never publish manually or move an earlier tag.

The candidate-specific gate covers immediate GATT invalidation and early
subscription ownership, held/rejected/failed-receipt child cleanup against
scoped parent-release outcomes, retry and late completion, discovery timeout
and cancellation while old cleanup is pending, and once-only pending-replay
terminal notification with retained upstream loss counters. Keep unrelated
connections usable and local iterator failures distinct from confirmed native
release. No native implementation changed in that historical IPC remediation;
its source-bound native artifacts required their existing identity checks. These
checks do not constitute physical-radio qualification.

Current native-continuation work changes native sources and must follow the
[native artifact lifecycle](docs/NATIVE_ARTIFACTS.md): regenerate the expected
identity, refresh each affected source-bound artifact through its canonical
builder, and rerun native identity, native host, and packaged-consumer gates.
The historical no-native-change statement is not an exemption for this work.
This clarification does not select a new release version or authorize publication.

## Releasing 5.0.0-rc.11

`v5.0.0-rc.10` is immutable published history. Release `v5.0.0-rc.11` only
from the exact current `main` commit after the review-remediation PR and
canonical CI succeed. Verify `package.json` is `5.0.0-rc.11`, the worktree is
clean, and release-note extraction finds `## [5.0.0-rc.11]`. Push a new
annotated `v5.0.0-rc.11` tag; the tag workflow publishes the exact packed
artifact to npm `next` with provenance and creates a GitHub prerelease. Never
publish manually or move an earlier tag.

The candidate-specific gate covers automatic scan-stop rejection followed by
successful native retry or parent release, while retaining actual local and
native cleanup failures; mobile scan request accounting across spawned
admission, cancellation, and orphan cleanup; and the original acquisition
deadline across IPC connect and discovery, including cancellation between
stages and exactly-once release. Refresh the sealed Android prebuilts and
expected native identity after Rust changes. The tag workflow allows a bounded
20-minute npm tarball-visibility window after accepted publication; a green
publish command alone is not proof that the registry serves the artifact.
These checks do not constitute physical-radio qualification.

## Releasing 5.0.0-rc.10

`v5.0.0-rc.9` is immutable published history. Release `v5.0.0-rc.10` only
from the exact current `main` commit after the review-remediation PR and
canonical CI succeed. Verify `package.json` is `5.0.0-rc.10`, the worktree is
clean, and release-note extraction finds `## [5.0.0-rc.10]`. Push a new
annotated `v5.0.0-rc.10` tag; the tag workflow publishes the exact packed
artifact to npm `next` with provenance and creates a GitHub prerelease. Never
publish manually or move an earlier tag.

The candidate-specific gate covers authoritative parent release settling
public scan stop and provisional compensation without masking local iterator
failure; single-flight cleanup retry across repeated bounded destroy;
control-only overflow displacement with source-policy preservation; and
cancellation during cleanup-only shared-scan reconciliation, including the
orphan-created-after-admission interleaving. Refresh the sealed Android
prebuilts and expected native identity after Rust changes. The mobile
golden-wire test must attach its listener before injecting live restoration;
pre-session restoration is validated through the durable claim path. These
checks do not constitute physical-radio qualification.

## Releasing 5.0.0-rc.9

`v5.0.0-rc.8` is immutable published history. Release `v5.0.0-rc.9` only
from the exact current `main` commit after the review-remediation PR and
canonical CI succeed. Verify `package.json` is `5.0.0-rc.9`, the worktree is
clean, and release-note extraction finds `## [5.0.0-rc.9]`. Push a new
annotated `v5.0.0-rc.9` tag; the tag workflow publishes the exact packed
artifact to npm `next` with provenance and creates a GitHub prerelease. Never
publish manually or move an earlier tag.

The candidate-specific gate covers cleanup-only mobile scan replacement with
a fresh native start and delivered observation; bounded parent release with
hung child cleanup and retained late diagnostics; original GATT caller budgets
and cross-device isolation; IPC terminal, pending, and evicted loss arithmetic;
and truthful missing-plan compensation. Refresh the sealed Android prebuilts
and expected native identity after the Rust changes. These checks do not
constitute physical-radio qualification.

## Releasing 5.0.0-rc.8

`v5.0.0-rc.7` is immutable published history. Release `v5.0.0-rc.8` only
from the exact current `main` commit after the review-remediation PR and
canonical CI succeed. Verify `package.json` is `5.0.0-rc.8`, the worktree is
clean, and release-note extraction finds `## [5.0.0-rc.8]`. Push a new
annotated `v5.0.0-rc.8` tag; the tag workflow publishes the exact packed
artifact to npm `next` with provenance and creates a GitHub prerelease. Never
publish manually or move an earlier tag.

The candidate-specific gate covers same-host scan recovery before shutdown,
the Rust/TypeScript scan lifecycle parity, shared IPC inner-queue loss and
terminal diagnostics through Electron and Tauri public paths, manager-scoped
GATT acquisition compensation, and executable Tauri cleanup-receipt examples.
Refresh the sealed Android prebuilts and expected native identity after the
Rust changes. These checks do not constitute physical-radio qualification.

## Releasing 5.0.0-rc.7

`v5.0.0-rc.6` is immutable published history. Release `v5.0.0-rc.7` only
from the exact current `main` commit after the review-remediation PR and
canonical CI succeed. Verify `package.json` is `5.0.0-rc.7`, the worktree is
clean, and release-note extraction finds `## [5.0.0-rc.7]`. Push a new
annotated `v5.0.0-rc.7` tag; the tag workflow publishes the exact packed
artifact to npm `next` with provenance and creates a GitHub prerelease. Never
publish manually, move an earlier tag, or tag the review branch before it has
merged to `main`.

The candidate-specific gate includes the linked external Tauri app built from
the packed npm bytes, an exact tarball digest binding in the publish workflow,
the 18-case shared-scan matrix, disposal and cleanup-retention regressions,
observed GATT delivery across host bridges, Android no-shell packaging, and
the executable Tauri documentation recipe. These are deterministic and compile
checks, not physical-radio qualification.

## Releasing 5.0.0-rc.6

`v5.0.0-rc.4` is immutable published history. The immutable
`v5.0.0-rc.5` tag stopped before npm publication because the packed external
Tauri consumer ran in the canonical publish job without GTK/WebKit system
libraries. Release `v5.0.0-rc.6` only from the exact current `main` commit
after the release PR and canonical CI succeed. Verify `package.json` is
`5.0.0-rc.6`, the worktree is clean, and release-note extraction finds
`## [5.0.0-rc.6]`. Push a new annotated `v5.0.0-rc.6` tag;
the tag workflow publishes to npm `next` with provenance and creates a GitHub
prerelease. Never publish this candidate manually, move an earlier tag, or tag
the release branch before it has merged to `main`.

The candidate-specific gate includes the packed external Tauri Cargo consumer,
merged Android manifest fixtures, queued Android invalidation and cleanup-retry
tests, React late-admission and terminal-error tests, and the shared-scan
widening compensation tests. These deterministic and compile checks do not
claim physical-radio qualification.

## Releasing 5.0.0-rc.4

The immutable `v5.0.0-rc.0` tag stopped before npm publication because its Linux
native-prebuild Electron smoke had no display server; `v5.0.0-rc.1` stopped
before npm publication because Android setup requested Google's removed legacy
`tools` SDK package; `v5.0.0-rc.2` stopped before npm publication because the
publish-only Android source build did not install the pinned Rust Android
targets. The corrected workflow uses Xvfb, explicitly installs only
`platform-tools`, and installs `aarch64-linux-android` plus
`x86_64-linux-android` for the pinned toolchain. The immutable `v5.0.0-rc.3`
tag also stopped before npm publication because its release-only clean-tarball
acceptance created a temporary consumer without a `packageManager` pin;
Corepack selected pnpm 12.5.1 rather than the repository's pnpm 10.14.0, and
the strict offline install could not resolve `@babel/runtime@^7.29.7` from the
empty pnpm-v11 cache. The corrected acceptance pins the temporary consumer and
primes that metadata. Do not move any failed tag or publish any failed version
from a workstation.

Integrate the `5.0.0` release branch into `main`, then release
`v5.0.0-rc.4` only from that exact current `main` commit after canonical CI
succeeds. This matches the publish workflow's immutable main-source gate.
Verify `package.json` is `5.0.0-rc.4`, the worktree is clean, and release-note
extraction finds `## [5.0.0-rc.4]`. Push a new
annotated `v5.0.0-rc.4` tag with the GitHub Release marked prerelease and the
npm dist-tag `next` (never `latest` for a 5.0 RC). The candidate must pack
the Rust workspace sources plus the committed Android native prebuilds, and
the F01 runtime proof must pass against the packed artifact. Follow the
required local validation, publish workflow, and registry verification below.

## Releasing 4.0.28

`v4.0.28` is immutable tagged history. It was released only from the exact
current `main` commit after its canonical CI succeeded. Never move the
immutable `v4.0.28` tag.

## Releasing 4.0.27

Release `v4.0.27` only from the exact current `main` commit after its canonical
CI succeeds. Verify `package.json` is `4.0.27`, the worktree is clean, and
release-note extraction finds `## [4.0.27]`. Push a new annotated `v4.0.27`
tag; never move the immutable `v4.0.26` tag. Follow the required local validation,
publish workflow, and registry verification below.

## Releasing 4.0.26

Release `v4.0.26` only from the exact current `main` commit after its canonical
CI succeeds. Verify `package.json` is `4.0.26`, the worktree is clean, and
release-note extraction finds `## [4.0.26]`. Push a new annotated `v4.0.26`
tag; never move the immutable `v4.0.25` tag. Follow the required local validation,
publish workflow, and registry verification below.

## Releasing 4.0.25

Release `v4.0.25` only from the exact current `main` commit after its canonical
CI succeeds. Verify `package.json` is `4.0.25`, the worktree is clean, and
release-note extraction finds `## [4.0.25]`. Push a new annotated `v4.0.25`
tag; never move the immutable `v4.0.24` tag. Follow the required local validation,
publish workflow, and registry verification below.

## Releasing 4.0.24

Release `v4.0.24` only from the exact current `main` commit after its canonical
CI succeeds. Verify `package.json` is `4.0.24`, the worktree is clean, and
release-note extraction finds `## [4.0.24]`. Push a new annotated `v4.0.24`
tag; never move an earlier immutable tag. Follow the required local validation,
publish workflow, and registry verification below.

## Releasing 4.0.23

Release `v4.0.23` only from the exact current `main` commit after its canonical
CI succeeds. Verify `package.json` is `4.0.23`, the worktree is clean, and
release-note extraction finds `## [4.0.23]`. Push a new annotated `v4.0.23`
tag; never move the immutable `v4.0.22` tag. Follow the required local validation,
publish workflow, and registry verification below.

## Releasing 4.0.22

The `v4.0.22` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag the release branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.22"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.22 -m "v4.0.22"
git push origin v4.0.22
```

Before tagging, confirm release-note extraction finds `## [4.0.22]`.

## Unpublished 4.0.21 tag

The immutable `v4.0.21` tag triggered workflow `33862393779`, which was
cancelled during native prebuilds before the npm version check or publication
after physical Android hardware exposed a remaining CCCD callback-order race.
It must not be recreated, moved, or published manually.

## Releasing 4.0.20

The `v4.0.20` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag the release branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.20"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.20 -m "v4.0.20"
git push origin v4.0.20
```

Before tagging, confirm release-note extraction finds `## [4.0.20]`.

## Releasing 4.0.19

The `v4.0.19` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag the release branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.19"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.19 -m "v4.0.19"
git push origin v4.0.19
```

Before tagging, confirm release-note extraction finds `## [4.0.19]`.

## Releasing 4.0.1

The `v4.0.1` tag is immutable published history. Do not recreate or move it.

```sh
git tag -a v4.0.1 -m "v4.0.1"
```

## Releasing 4.0.2

The `v4.0.2` tag is immutable published history. Do not recreate or move it.

```sh
git tag -a v4.0.2 -m "v4.0.2"
```

## Releasing 4.0.3

The `v4.0.3` tag is immutable published history. Do not recreate or move it.

```sh
git tag -a v4.0.3 -m "v4.0.3"
```

## Releasing 4.0.18

The `v4.0.18` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag the release branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.18"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.18 -m "v4.0.18"
git push origin v4.0.18
```

Before tagging, confirm release-note extraction finds `## [4.0.18]`.

## Releasing 4.0.17

The `v4.0.17` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag the release branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.17"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.17 -m "v4.0.17"
git push origin v4.0.17
```

Before tagging, confirm release-note extraction finds `## [4.0.17]`.

## Releasing 4.0.16

The `v4.0.16` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag the release branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.16"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.16 -m "v4.0.16"
git push origin v4.0.16
```

Before tagging, confirm release-note extraction finds `## [4.0.16]`.

## Releasing 4.0.15

The `v4.0.15` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag this feature branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.15"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.15 -m "v4.0.15"
git push origin v4.0.15
```

Before tagging, confirm release-note extraction finds `## [4.0.15]`.

## Releasing 4.0.14

The `v4.0.14` tag is immutable history, but its publish workflow was cancelled
before the npm version check and publish steps. Do not recreate or move it.

## Releasing 4.0.13

The `v4.0.13` tag is immutable published history. Do not recreate or move it.

```sh
git tag -a v4.0.13 -m "v4.0.13"
```

## Releasing 4.0.12

The `v4.0.12` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag this feature branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.12"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.12 -m "v4.0.12"
git push origin v4.0.12
```

Before tagging, confirm release-note extraction finds `## [4.0.12]`.

## Releasing 4.0.11

The `v4.0.11` tag is immutable published history. Do not recreate or move it.

```sh
git tag -a v4.0.11 -m "v4.0.11"
```

## Releasing 4.0.10

The `v4.0.10` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag this release branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.10"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.10 -m "v4.0.10"
git push origin v4.0.10
```

Before tagging, confirm release-note extraction finds `## [4.0.10]`.

## Releasing 4.0.9

The `v4.0.9` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag this release branch directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.9"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.9 -m "v4.0.9"
git push origin v4.0.9
```

Before tagging, confirm release-note extraction finds `## [4.0.9]`.

## Releasing 4.0.8

The `v4.0.8` tag must identify the exact current `main` commit after canonical
CI passes. Do not tag this feature branch or `release/4.0.8` directly.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.8"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.8 -m "v4.0.8"
git push origin v4.0.8
```

Before tagging, confirm release-note extraction finds `## [4.0.8]`.

## Releasing 4.0.7

Same shape as 4.0.6. The release workflow verifies that the tag points at the
exact current `main` commit before publication; do not create it from a side
branch or an older commit. Do not retag any immutable version.

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.7"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.7 -m "v4.0.7"
git push origin v4.0.7
```

Do not push another commit to `main` between the final verification and the tag
push.

Before tagging, confirm the release-notes extraction finds the entry — the
workflow's awk matches `^## \[4.0.7\]`, and a heading left as `[Unreleased]`
publishes a stub instead of the changelog:

```sh
awk -v ver=4.0.7 '$0 ~ ("^## \\[" ver "\\]") {p=1;next} p && $0 ~ /^## \[/ {exit} p {print}' CHANGELOG.md
```

## Releasing 4.0.6

The source version is prepared on `main` before the tag. The release workflow verifies that every initial release tag points at the exact current `main` commit before publication; do not create that tag from a side branch or an older commit. Do not retag immutable `v4.0.0`, `v4.0.1`, `v4.0.2`, or `v4.0.3`.

On release day:

```sh
git fetch origin --tags
git checkout main
git pull --ff-only origin main

test "$(git branch --show-current)" = "main"
test "$(node -p "require('./package.json').version")" = "4.0.6"
git diff --exit-code
git diff --cached --exit-code

git tag -a v4.0.6 -m "v4.0.6"
git push origin v4.0.6
```

Do not push another commit to `main` between the final verification and the tag push.

## Native build identity gates (5.x)

Mobile native artifacts are bound to their sources by
`scripts/release/native-build-identity.js` (see
`docs/5.0.0-DISTRIBUTION_CONTRACT.md` §4). Before tagging a 5.x release:

- `node scripts/release/native-build-identity.js --check` passes (`prepack`
  runs it);
- the committed Android prebuilts were refreshed from the tagged sources with
  `sh android/refresh-prebuilt-jniLibs.sh` (pinned toolchain, NDK 27.x) and
  `node scripts/release/native-build-identity.js --check-android-prebuilts`
  passes — any Rust, lockfile, toolchain or JNI declaration change since the
  last refresh fails it.

The publish workflow's macOS `native-rustcore` job builds `ios/RustCore` with
`ios/build-rust-core.sh` and verifies it with `ios/verify-rust-core.sh` and
`--check-apple`; the publish job re-runs `--check-apple` on the downloaded
staging and `--check-android-prebuilts` before packing. None of these gates
may be bypassed to make a release pass.

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
9. runs the required Android/Expo/native-host gates, including the native build identity gates above;
10. verifies package contents and generated dependency artifacts;
11. publishes the exact prebuild-bearing tarball through npm trusted publishing with provenance;
12. waits for the registry artifact and verifies the published tarball/digest path;
13. on a post-publish recovery rerun, replaces any newly built local tarball with the immutable npm registry tarball;
14. creates the GitHub Release only after npm publication and provenance verification succeed.

Linux native system-package profiles have one source of truth:
`scripts/ci/install-linux-native-system-dependencies.sh`. CI and publish jobs
call its `bluez`, `tauri`, or `desktop-prebuild` profile and must not duplicate
`apt-get install` package lists in workflow YAML. The packed external Tauri
Cargo consumer runs immediately after `prepack`, before examples, Android
builds and later packaging gates, so an inconsistent runner or consumer fails
early.

Stable versions publish to `latest`. Active `4.0.0-rc.*` candidates also publish to `latest`; other hyphenated SemVer prereleases publish to `next` and create GitHub prereleases.

## Post-release verification

After the workflow succeeds, verify the registry rather than the workflow log:
a green publish job and a package a consumer can actually install are not the
same claim.

```sh
version=5.0.0-rc.15

npm view "unified-ble-manager@$version" version
npm view unified-ble-manager dist-tags --json
npm view "unified-ble-manager@$version" repository --json
npm view "unified-ble-manager@$version" license
npm view "unified-ble-manager@$version" dist.integrity
```

Then verify:

- npm `next` resolves to `5.0.0-rc.15`, while `latest` remains on the 4.0 stable
  line; a stable release moves `latest`;
- the npm package page shows provenance for the published artifact;
- the GitHub Release exists at that tag, and is marked prerelease only if the
  version is one;
- its attached tarball/SBOM/license artifacts correspond to the release
  workflow output;
- a clean consumer, in a directory outside this repository, can install
  `unified-ble-manager@5.0.0-rc.15` explicitly and import the documented host
  entrypoints. A bare install still selects `latest` (the 4.0 line). This
  catches a packaging gap the repository's
  own tests cannot see: `@babel/runtime` shipped undeclared in 4.0.4 and only a
  real external consumer surfaced it.

## Failed release or partial publish

Never move or recreate a published version tag to hide a failed release.

- If the workflow fails **before npm publication**, fix the source on `main`, increment/version as appropriate, and create the correct new tag.
- If npm publication succeeds but a later GitHub-release step fails, preserve the immutable npm version and rerun the workflow. The recovery path skips the current-`main` admission check and attaches the exact npm registry tarball rather than newly linked native binaries.
- If a defect is discovered after a stable tag is published, fix it and release a new patch; do not replace the published tag.

## Prereleases after 4.0.0

Prereleases such as `5.0.0-rc.6` use normal SemVer suffixes. They publish to
`next` and must never replace `latest` until a final version is released.

## Release artifacts and evidence

`SBOM.cdx.json`, `THIRD_PARTY_LICENSES.json`, generated platform support, and retained evidence records must be reproducible from the tagged source. Evidence records can justify platform support claims, but absence of an optional physical-radio qualification record does not change the SemVer of an otherwise validated stable package.

The release process must never synthesize, backdate, or relabel hardware evidence merely to make a release gate pass.

## Architecture authority

Follow [Current 5.0 authority](docs/README.md#current-50-authority) for the
current behavior contracts, distribution guidance and evidence rules. This
release procedure controls publication mechanics; historical 4.0 migration
gates and draft distribution proposals do not override it or the current
native artifact lifecycle.
