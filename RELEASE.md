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
- Numbered 5.x release-candidate npm dist-tag: `next`. Stable 5.x publishes to
  `latest`; the registry establishes which version is currently published.

The current publisher and packaging guards share
`scripts/release/release-version-policy.js`: stable `5.x.y` selects `latest`;
numbered `5.x.y-rc.N` candidates select `next`. Numeric components cannot have
leading zeroes or exceed safe integers; unapproved prerelease channels, build
metadata, other majors and mismatched package/tag identities are refused.
Historical immutable tags retain their original publisher source and behavior.

Releases are tag-driven and published by GitHub Actions through npm trusted publishing/OIDC. Do not use a long-lived `NPM_TOKEN` or publish a normal release from a developer laptop.

### Parallel publisher and preserved serial reference

The production `publish.yml` uses parallel gates and one sealed candidate
tarball. The previous serial workflow is preserved at
`.github/publish-serial-reference.yml`, outside executable workflows.
The separate manual-only
`publish-parallel-draft.yml` exercises its prepublication gates concurrently,
without the npm environment, publishing permissions, tag writes or GitHub
release creation. It is not a replacement publisher and its green result is
not publication authorization. See [the draft testing procedure](docs/PARALLEL_PUBLISHER_DRAFT.md)
for cold/warm comparisons and exact-tarball binding. rc.20 is the first
user-authorized production test; no measured speedup is claimed yet.
Same-tag runs queue without cancelling an in-flight publish. Production
artifacts request 90-day retention; an expired/deleted candidate fails closed.
Recovery requires an entire immutable-tag workflow rerun and all gates again,
not a protected-job repack or manual publish. See the linked procedure for
approval holds and existing-version recovery boundaries.

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

A stable `5.0.0` release means the documented public package/API contract is the supported 5.0 contract and is governed by normal SemVer expectations. It does **not** automatically promote any React Native, Web, Electron, CoreBluetooth, WinRT, or BlueZ backend to Preview, Supported, or Reliability-qualified.

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
11. the complete five-target desktop Node-API prebuild matrix (macOS `arm64`; Windows/Linux `arm64`/`x64`) is produced from the release tag and verified under Node and Electron. Intel macOS desktop is outside UBM's support policy; iOS/tvOS simulators are `arm64` only and physical iPhone support is unchanged.

## Remediation and release qualification

Use the release's existing review closure record and retained evidence; for
rc.21, start with [`docs/review/RC21_REMEDIATION.md`](docs/review/RC21_REMEDIATION.md).
Keep the original finding IDs and review denominator. The following readiness
requirements apply before claiming remediation complete or qualifying a release:

1. **Identify the violated invariant.** Trace each finding to its underlying
   ownership, cancellation, ordering, cleanup or recovery rule. Investigate
   other paths susceptible to the same cause, and write regressions before
   changing the implementation.
2. **Assess and complete every affected route.** Record each fix's cross-platform
   impact assessment for shared core, native implementation and applicable
   NAPI, React Native, Electron, Tauri and Web routes, including their public
   entrypoints, behavior and capability declarations.
   Preserve equivalent supported semantics. Record genuine OS limitations and
   their runtime capability/error; a library omission is not an OS limitation.
3. **Verify equivalent adverse behavior continuously.** Use focused checks
   throughout implementation through real public entrypoints and production
   boundaries. Cover success, failure, cancellation, concurrency, stale
   callbacks, reconnect and cleanup/retry on affected hosts. Reuse existing
   package, native-protocol, private-bus, packed-consumer and CI checks. One
   mock or one host pass cannot establish cross-platform correctness. Repeat
   successful unrelated checks only for a changed dependency or concrete failure.
4. **Prove the root cause is resolved.** Re-read each complete route, including
   admission, forwarding, ordering, deadlines, terminal errors and teardown.
   Verify the invariant and susceptible paths, not only the original
   reproduction. Complete tests, types, guides, changelog and generated
   artifacts through their existing generators. Supported capabilities must
   have no unresolved TODOs, placeholders or deferred implementation. Keep
   unverified qualification requirements explicit.
5. **Qualify one immutable candidate.** Once implementation, tests and docs are
   complete, record the exact commit, clean tree, release version and applicable
   native identities; bind each candidate artifact to its digest. Refresh
   affected native artifacts through their canonical consumers/builders in
   [`docs/NATIVE_ARTIFACTS.md`](docs/NATIVE_ARTIFACTS.md). Run clean builds,
   required tests, artifact/packed-consumer checks, clean cross-platform CI and
   independent review against that candidate using existing infrastructure.
   Final publication remains subject to the current `main`/tag requirements.
   Linux preflight, including `--fast`, does not replace missing host lanes or
   required physical qualification.
6. **Requalify after changes.** A code change invalidates the freeze. Record the
   new candidate identity, repeat affected regression, host/native/consumer
   checks and independent review, and complete every required final release
   gate against the new candidate.
   Retain unaffected earlier evidence with its original identity and explicit
   applicability; never relabel it as a fresh run. Metadata, documentation and
   version-only changes need applicable identity, generated-artifact and
   package checks. Rerun affected actual-radio scenarios when runtime behavior
   changes or a concrete hardware failure requires reproduction.
7. **Close each item with evidence.** In the existing closure record, retain
   finding ID, root cause, affected platforms/paths, fixing commit, regression
   tests, commands/results and receipt links, evidence level, genuine OS
   limitations and remaining qualification gaps. The final report identifies
   the qualified commit/artifact digests, required gate results, independent
   review result and every remaining limitation. No item or required release
   qualification is complete without its acceptance evidence. Optional platform
   qualification remains separate from package SemVer; declared release
   requirements remain mandatory.

### Evidence levels for closure

These distinctions describe receipts; they do not change generated backend
support labels or confer production readiness on an untested route.

| Evidence                             | Proves within its recorded scope                                                                                           |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| Source inspection                    | Traced implementation/control flow; no execution claim.                                                                    |
| Compilation/typecheck                | Compatibility with the recorded target/toolchain; no runtime or radio claim.                                               |
| Mock/deterministic test              | Behavior under controlled replacements; no production native or radio claim.                                               |
| Native synthetic runtime integration | Actual public/provider/native routing with an explicit synthetic radio; no RF/controller claim.                            |
| Actual-radio qualification           | Executed scenarios on the recorded OS, adapter, physical peripherals and native artifact; no untested host/scenario claim. |
| Production deployment                | Observed deployed behavior in the real application environment; no automatic broader reliability or support promotion.     |

For execution receipts, retain source/artifact identity, host/tool versions,
scenario, result, cleanup outcome and limitations. State when real production
functions execute against controlled native boundaries. A real D-Bus or native
binary alone does not make a controller-free test actual-radio evidence.
Production readiness requires the declared acceptance evidence at its required
levels; compilation, mocks and synthetic radios cannot substitute for required
actual-radio or deployment evidence. Never manufacture, backdate or relabel
evidence to make a release pass.

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
The `v5.0.0-rc.21` tag records the final candidate; its registry receipt
establishes publication. Stable `5.0.0` follows only after rc.21 publication
and verification. `5.0.0-rc.20` is immutable published history. rc.19, rc.18, rc.17, rc.16 and rc.14 are immutable published history.
The immutable `v5.0.0-rc.15` tag remains unpublished: its publisher was cancelled
before npm publication when the Apple architecture policy changed.

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

## Releasing 5.0.2

Unpublished patch candidate.

The prepared `5.0.2` candidate is unpublished. Its exact source, packed
artifact, and validation receipts must be reviewed before any tag or trusted
publisher action. This patch includes the awaited WinRT null-device correction,
Android callback and metadata ownership fixes, and Tauri characteristic
attachment cleanup. Source-inventory assertions and the recording contention
fixture are corrected without reducing observation retention or production
durability. Package SemVer does not promote any backend to a support label.
Adapter-reset causality is also preserved during pending desktop release.

Required remaining release steps are: refresh the applicable gates from the
exact source commit, confirm the packed-consumer contract and both digests for
the same sealed tarball, complete the authorized final physical receipt if
requested, then use the normal tag-driven trusted publisher under release-owner
authorization. Retain the publisher workflow's SHA-256 digest and calculate
the requested independent SHA-512 receipt from that exact file, for example:

```sh
sha256sum .release-package/canonical.tgz
sha512sum .release-package/canonical.tgz
```

No npm publication, tag, push, or registry claim is made by this checklist.

## Published 5.0.1

`v5.0.1` was published on 2026-10-09 at 18:18:46 UTC through the normal
[GitHub release](https://github.com/sfourdrinier/unified-ble-manager/releases/tag/v5.0.1).
The registry independently returns `unified-ble-manager@5.0.1` with integrity
`sha512-QMlW+cdUNxtmLPoKCNXo1IVwLWDRAhpLcqjdc/FWrgDm4jAakBNL5rrAtD7BFToMDXUplZmZ+LAFmTaF3x2ikg==`.
The tag and package are immutable. Later corrections belong to `5.0.2`; this
publication does not promote Fire TV or any other backend to a support label.

## Releasing 5.0.0

Publish and independently verify `5.0.0-rc.21` before integrating this stable
release. Keep its immutable tag, registry artifact and receipts. Stable 5.0.0
carries the rc.21 runtime unchanged: prepare final package identities, generated
artifacts, installation/migration guides and this changelog entry, then qualify
one frozen commit through the existing clean preflight, cross-platform CI,
independent review and sealed-package publisher gates. Version-only preparation
does not require rebuilding source-identical native artifacts or repeating
unaffected physical-device scenarios; their identities and all final release
gates remain required.

Retain the existing [rc.21 closure record](docs/review/RC21_REMEDIATION.md) and
the owner's deferral of physical Classic/LE dual-mode bearer qualification
recorded in [the rc.21 publication receipt](#releasing-500-rc21). That unexecuted scenario is not a pass. Preserve every other
qualification limitation, runtime capability boundary and generated backend
evidence label. Stable package SemVer does not promote host/profile radio
qualification or establish real-application production readiness.

After the prepared release integrates into `main`, verify the exact current
`main` commit, clean tree, `5.0.0` package/generated identities, matching
`## [5.0.0]` changelog entry, canonical CI and all required release gates.
Confirm both the npm version and annotated `v5.0.0` tag are absent. Create one
new annotated `v5.0.0` tag on that qualified exact `main` commit, then let the
trusted `publish.yml` workflow publish with provenance to npm `latest` and
create a non-prerelease GitHub Release. Do not advance `main` during initial
publication, retag any candidate or publish manually. Verify registry bytes,
provenance, release assets and explicit plus bare fresh-consumer installs using
[Post-release verification](#post-release-verification). Keep `next` on the
separately published release candidate.

## Releasing 5.0.0-rc.21

`5.0.0-rc.21` was published on 2026-10-08 from
`030aeba43f21e1016d97d385526a26a680cbe483` by trusted publisher run
[37814804439](https://github.com/sfourdrinier/unified-ble-manager/actions/runs/37814804439).
Registry integrity, source/tag provenance, fresh external host imports and npm
signatures/attestations were verified. The registry tarball SHA-256 is
`ca0fd519e51f5f82c1975a76a15748bdd2fc2761f915d95e8ba0ea5d5e889e0b`,
matching the GitHub prerelease asset. npm `next` advanced to rc.21; `latest`
remained 4.0.28. The owner explicitly deferred only the remaining Classic-only
physical bearer qualification to permit real-application testing. That scenario
remains unverified; backend evidence labels and other release gates are unchanged.
The preparation requirements below are retained history, not instructions to
recreate this immutable release tag.

This candidate includes the PR #251 remediation batch. Before publication,
complete the item-by-item acceptance ledger in
[`docs/review/RC21_REMEDIATION.md`](docs/review/RC21_REMEDIATION.md) on the exact
integrated head. The maintained source daemon is `5.87-ubm.10`, with authority
contract `(1, 3, 1)`; older contracts fail closed. The packed qualifier executes
the installed public desktop factory, production provider and sealed addon
under Node and Bun 1.4.2, in CommonJS and ESM, on Linux, macOS and Windows.
Its explicit synthetic radio proves integration and resource ownership.
Changed BlueZ ownership and acquired-FD radio scenarios require their separate
native-daemon and physical qualification receipts. No pending remediation
entry or required qualification gap may be presented as closed.

Retained prior qualification history follows; it does not qualify this new
batch. `bun scripts/ci/bun-desktop-host-smoke.js` loads the sealed prebuild and
runs the synthetic central. Source daemon
`5.87-ubm.6` ends a finished GATT read or write hold when the call completes.
Installing that daemon changed the glibc Linux H10 disconnect from
`lease-released-protected` to `lease-released-indeterminate` and left the
link up, because profile-probe auto-connect bookkeeping was recorded as an
unknown holder. `5.87-ubm.7` does not treat that bookkeeping as a hold on an
exclusive link this process created, and that exclusive release stops kernel
auto-connect for an untrusted device. Installing it changed the glibc Linux
H10 disconnect to `lease-released-protected` and left the link up, because
the controller had already initiated the bonded link and the lease adopted
it as borrowed. `5.87-ubm.8` releases a locally initiated link when no other
application hold remains. That prior authority contract was `(1, 2, 1)`.
Installing `5.87-ubm.8`, the glibc Linux H10 session reported disconnect
`released` and close `released`, and the link was down. `5.87-ubm.9` fails an
unbonded LE attribute operation that returns Insufficient Encryption or
Insufficient Authentication instead of raising link security. Installing it,
the unbonded glibc Linux H10 session on source digest `0b31ce8e` completed the
same exchange, reported disconnect `released` and close `released`, and left
the link down with no pairing request. The same digest completed that session
on macOS Apple Silicon and, twice back to back, on Windows x64. It does not
promote backend qualification labels and it does not make a physical radio
receipt.
Verify exact current `main`, all `5.0.0-rc.21` identities, required CI and
release gates, and absence of the registry version and annotated tag before
creating `v5.0.0-rc.21`. Use the trusted tag publisher only; `next` advances to
rc.21 and `latest` remains 4.0.28.

## Releasing 5.0.0-rc.20

### One-time owner-authorized unpublished-tag replacement

The repository owner explicitly authorized replacing the failed, unpublished
`v5.0.0-rc.20` tag once. Its original annotated object was
`2473c5677a0b2edddf6b29b7f7cee357d40992c0`, pointing to
`306179508ba553af3e85934156cdd9e740e2742e`; publisher run `37415179598`
failed desktop offline-consumer installation and never entered the protected
OIDC publish job. Retain that failed run and original tag identity. Before
replacement, recheck npm rc.20 is absent, qualify the complete publisher-fix
batch and exact current `main`, and ensure the remote tag still has that
original object. Recreate one annotated tag on qualified current `main` and
let the normal trusted `publish.yml` OIDC workflow publish with provenance.
This exception permits neither manual npm publication nor weakened gates,
and does not apply to any published version or other tag.

This corrective release aligns installation guidance, historical release status,
Tauri MTU errors and Windows write-limit descriptions with the implementation.
It also corrects public stream initialization, retryable cleanup and terminal
failure reporting, and adds Linux initial deferred acquisition through the
optional LE observer in maintained daemon `5.87-ubm.5`. It does not promote
backend qualification labels. Existing daemon installations are not replaced
implicitly; private-bus and producer tests are not physical qualification.
Verify exact current `main`, all `5.0.0-rc.20` identities, required CI and release
gates, and absence of the registry version and annotated tag before creating
`v5.0.0-rc.20`. Use the trusted tag publisher only; `next` advances to rc.20 and
`latest` remains 4.0.28. No unrelated physical-device rerun is required.

## Releasing 5.0.0-rc.19 (historical)

`v5.0.0-rc.19` was published on 2026-10-05 from
`f2e98f41e0d416e6abc6a33594b9d445e8c722b6`. Its npm provenance, registry
tarball, native identities, GitHub prerelease assets and outside-repository
installed consumer were verified. The procedure below is historical: do not
repeat its absent-tag check or recreate the immutable tag.

Integrate all release PRs through `release/5.0.0-rc.19`, then merge that
qualified combination into `main`. Release only from exact current `main`
after its canonical CI and required release gates pass. Verify package,
implementation, Tauri compatibility, changelog and generated artifacts identify
`5.0.0-rc.19`, and confirm the registry version and annotated tag are absent
before creating `v5.0.0-rc.19`. The trusted tag workflow publishes to `next`;
`latest` remains 4.0.28. Never retag an earlier candidate or publish manually.

This candidate closes the reviewed desktop capability and acquisition paths,
adds the complete Android ARM32 distribution/consumer gates, corrects native
restoration and documentation, and advances the maintained BlueZ producer to
`5.87-ubm.4`. Preserve every retained receipt's original source, artifact and
scenario identity. Deterministic TCK, private-bus and native-graph compilation
proofs are not physical-radio qualification. Remaining timed Apple picker,
actual user-force-quit and physical Apple TV qualification stays open.

Version-only preparation requires generated/packed consistency and existing
release gates, not another native rebuild or unrelated phone-duration test.

## Releasing 5.0.0-rc.18 (historical)

`v5.0.0-rc.18` is immutable published history. The following records its release
procedure; do not repeat its tag instructions.

Release the corrective PR #246 only from exact current `main` after canonical
CI and the existing release gates pass. Verify package, implementation, Tauri
compatibility, changelog and generated artifacts identify `5.0.0-rc.18`, and
that this version and tag are absent before creating annotated `v5.0.0-rc.18`.
The trusted tag workflow publishes to `next`; `latest` stays on 4.0.28.
Never retag rc.17, publish manually or replace an immutable artifact.

The corrective scope is public desktop IPC security/targeting parity, bounded
scan-cleanup retry, ordered watches, Tauri address metadata, valid Apple chooser
admission and the maintained BlueZ UUID-filter fix. Preserve the actual packed
desktop and Apple receipts with their original source and artifact identities.
Picker completion and remaining physical-platform qualification stay open;
this RC does not claim all-platform GA. Metadata-only release preparation
requires generated/packed consistency, not another phone-duration campaign.

## Releasing 5.0.0-rc.17 (historical)

`v5.0.0-rc.17` is immutable published history. Do not repeat its tag instructions.

Release only from the exact current `main` commit after the integrated
completion PR and canonical CI succeed. Verify package, implementation, Tauri
compatibility, changelog, generated metadata and packed artifact all identify
`5.0.0-rc.17`. Push a new annotated `v5.0.0-rc.17` tag only after those gates
pass; the existing workflow publishes to npm `next` with provenance and creates
a GitHub prerelease. Never publish manually or move an earlier tag.

This candidate adds native ASK/CDM chooser integration and owned cancellation,
authoritative Linux lease recovery and disconnect detail, complete TV reference
consumer packaging, shared Rust-only desktop distribution, restored HTML
examples and stable-version publication admission. Exercise the actual
refreshed artifacts with focused chooser ownership, lease recovery, event
projection and packed-consumer regressions, then the existing cross-platform
and release gates. Preserve any hardware evidence's exact source/artifact and
scenario scope; synthetic radios, private buses and builds are not new physical
qualification. Runtime changes require their relevant qualification, but this
release identity update alone does not require another phone campaign.

Keep current platform limitations visible: Apple desktop and simulators are
arm64-only; Linux connection/GATT needs the documented daemon integration.
The candidate does not promote backend support labels or claim stable 5.0.

## Releasing 5.0.0-rc.16 (historical)

`v5.0.0-rc.16` is immutable published history. The following preserves its
release procedure and acceptance scope; do not execute its tag instructions
again.

Release only from the exact current `main` commit after the focused corrections
and Apple Silicon-only distribution PRs and canonical CI succeed. Verify `package.json` and the changelog
identify `5.0.0-rc.16`, the worktree is clean, and refreshed native artifacts match
the corrected source. Push a new annotated `v5.0.0-rc.16` tag; the existing
workflow publishes to npm `next` with provenance and creates a GitHub prerelease.
Never publish manually or move an earlier tag.

The maintained matrix excludes both Intel macOS desktop producers and the
Intel-only iOS Simulator Rust target. Verify the emitted desktop matrix and
arm64-only Apple simulator slices from the packed bytes. Physical iPhone and
tvOS device support and Windows/Linux x64 targets are unchanged.

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

For a valid version tag, `.github/workflows/publish.yml` performs the following
gates and publication operations. This list describes responsibilities, not a
serial schedule: Android/example lanes run independently, while the
packed Tauri consumer depends on the sealed canonical package. The aggregate
requires that every required lane succeeds before publication.

1. checks out the tagged commit and builds Node-API v8 prebuilds for macOS `arm64` and Windows/Linux `arm64`/`x64` native runners;
2. loads each prebuild under Node and the same file under Electron through the shared `scripts/ci/run-electron-main-smoke.sh` launcher (Linux uses Xvfb and `--no-sandbox`; other hosts retain normal Electron launch). Missing addons and smoke failures remain fatal; this synthetic check makes no physical-radio claim;
3. assembles and hashes the complete prebuild matrix into `native/PREBUILDS.json`;
4. verifies tag name and `package.json` version agree;
5. applies the shared version policy: stable `5.x.y` selects `latest`, numbered
   `5.x.y-rc.N` selects `next`, and other release identities are refused;
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
`apt-get install` package lists in workflow YAML. Native producers feed the
canonical package assembly; the packed external Tauri Cargo consumer verifies
that sealed candidate in its own lane. It has no ordering guarantee relative
to the independent Android/example lanes; any required lane failure prevents
publication.

Current stable `5.x.y` versions publish to `latest`. Numbered `5.x.y-rc.N`
candidates publish to `next` and create GitHub prereleases. Historical 4.0 RC
channel rules are retained above as history, not current publisher admission.

## Post-release verification

After the workflow succeeds, verify the registry rather than the workflow log:
a green publish job and a package a consumer can actually install are not the
same claim.

```sh
version=5.0.2

npm view "unified-ble-manager@$version" version
npm view unified-ble-manager dist-tags --json
npm view "unified-ble-manager@$version" repository --json
npm view "unified-ble-manager@$version" license
npm view "unified-ble-manager@$version" dist.integrity
```

Then verify:

- for stable `5.0.2`, npm `latest` resolves to `5.0.2` and `next` retains the
  separately published rc.21; for a numbered RC, verify `next` resolves to that
  exact candidate without changing `latest`;
- the npm package page shows provenance for the published artifact;
- the GitHub Release exists at that tag, and is marked prerelease only if the
  version is one;
- its attached tarball/SBOM/license artifacts correspond to the release
  workflow output;
- a clean consumer, in a directory outside this repository, can install
  `unified-ble-manager@5.0.2` explicitly and import the documented host
  entrypoints. A separate bare install must select npm `latest` (`5.0.2` after
  stable publication). For RC verification, pin the actual numbered candidate
  instead and verify `next` separately. This
  catches a packaging gap the repository's
  own tests cannot see: `@babel/runtime` shipped undeclared in 4.0.4 and only a
  real external consumer surfaced it.

## Failed release or partial publish

Never move or recreate a published version tag to hide a failed release.

- If the workflow fails **before npm publication**, fix the source on `main`, increment/version as appropriate, and create the correct new tag.
- If npm publication succeeds but a later GitHub-release step fails, preserve the immutable npm version and rerun the workflow. The recovery path skips the current-`main` admission check and attaches the exact npm registry tarball rather than newly linked native binaries.
- If a defect is discovered after a stable tag is published, fix it and release a new patch; do not replace the published tag.

## Current 5.x release candidates

Numbered `5.x.y-rc.N` prereleases use normal SemVer suffixes. They publish to
`next` and never replace `latest`. Stable `5.x.y` releases publish to `latest`;
other prerelease channels are refused by the shared release policy.

## Release artifacts and evidence

`SBOM.cdx.json`, `THIRD_PARTY_LICENSES.json`, generated platform support, and retained evidence records must be reproducible from the tagged source. Evidence records can justify platform support claims, but absence of an optional physical-radio qualification record does not change the SemVer of an otherwise validated stable package.

The release process must never synthesize, backdate, or relabel hardware evidence merely to make a release gate pass.

## Architecture authority

Follow [Current 5.0 authority](docs/README.md#current-50-authority) for the
current behavior contracts, distribution guidance and evidence rules. This
release procedure controls publication mechanics; historical 4.0 migration
gates and draft distribution proposals do not override it or the current
native artifact lifecycle.
