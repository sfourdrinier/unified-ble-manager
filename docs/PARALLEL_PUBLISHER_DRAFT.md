# Parallel publisher: implementation and testing

## Preservation and safety

`.github/workflows/publish.yml` is now the generated parallel production
publisher. Its original serial source is preserved verbatim at
`.github/publish-serial-reference.yml`, outside executable workflows.
The draft is manual-only, grants only `contents: read`, has no `npm` environment
or OIDC write permission, and never publishes, pushes a tag or creates a release.
No dependency/lockfile, package identity or native runtime changes are required.

Both workflows are generated from the preserved serial steps rather than maintaining a
second copy of their commands. Regenerate with
`node scripts/ci/generate-parallel-publisher-draft.js`; verify freshness with
the same command followed by `--check`. Add `--production` to generate or
check the active publisher. Tests fail if a production gate is
omitted or changed. The explicit publication-only exclusion list covers OIDC
setup, version/channel/current-main admission, npm availability/publication,
registry digest/provenance binding and GitHub release creation. Those remain
mandatory in the protected final production job, not proven by this dry-run.
rc.20 is the first user-authorized end-to-end production test of this graph.
No hosted performance improvement is claimed before its receipts are available.

## Parallel structure

```text
Desktop matrix ─┐
Apple framework ├─ assemble + seal one canonical tarball ─┬─ source regression gates
                │                                       ├─ linked Tauri + digest binding
Classic Android source build ───────────────────────────┤  (independent lane)
Expo Android source build ──────────────────────────────┤  (independent lane)
                                                        ├─ install/export smoke
                                                        ├─ packed Expo/Tauri contracts
                                                        ├─ G6A consumers
                                                        ├─ packed Expo TV ARM32 build
                                                        └─ desktop negative acceptance
All jobs ── always-running aggregate; any failure/cancellation/skip fails
```

Classic and Expo source builds start immediately alongside native producers.
Every packed consumer downloads the same run-scoped artifact and compares its
source commit with `GITHUB_SHA` and its SHA-256 with the assembler job output.
Only generated `lib`, Apple staging and desktop prebuilts are extracted into
each checkout; checked-out source is not overwritten. Source regression tests
may rebuild their own generated outputs; they are source tests, not a claim
that unit tests executed entirely from the installed package.

Tauri, install/export, Expo/Tauri and G6A helpers accept the opt-in
`UBM_PACKED_TARBALL` absolute path plus `UBM_PACKED_TARBALL_SHA256`. Missing or
mismatched digests fail closed. Without either variable they preserve the old
pack path, including lifecycle checks.
The third-party backend fixture still packs its own distinct fixture package.

Optional caches store package/dependency downloads, not shipping native
binaries, build `target` trees or credentials. Keys include runner OS/CPU,
lockfiles, pinned Rust and Gradle configuration. The default `cold_cache=true`
skips caching entirely in the manual draft. Production enables download caches;
warm cache is not evidence reuse.
Gradle compilation-result caching and trusted main-CI artifact reuse are future
work; neither is claimed implemented here.

## Local checks

```sh
node scripts/ci/generate-parallel-publisher-draft.js --check
pnpm exec jest --config jest.package.config.js --runInBand \
  __tests__/ParallelPublisherDraft.test.js \
  __tests__/SuppliedPackedTarball.test.js \
  __tests__/PackedHostConsumerCheck.test.js \
  __tests__/G6APackedConsumerProof.test.js
pnpm lint
actionlint -shellcheck= .github/workflows/publish-parallel-draft.yml
git diff --exit-code -- .github/workflows/publish.yml
```

The focused guards test missing/corrupt supplied artifacts, permissions,
manual-only triggers, production-step parity, generated freshness, independent
Android lanes and the always-running all-jobs aggregate. They are not proof of
hosted runner behavior or a measured speedup. Run the mandatory clean detached
Linux preflight against a committed exact draft head before pushing it. No
physical-device reruns are necessary for these CI/helper changes.

## Hosted cold/warm test

GitHub requires a workflow using `workflow_dispatch` to exist on the default
branch before manual dispatch. First review and register this **nonpublishing**
draft through a normal gated PR; this does not replace `publish.yml`. Then
freeze the reviewed draft branch at one full commit SHA and dispatch that
branch. Verify the resulting run's `headSha` equals that recorded SHA before
accepting any comparison; do not advance the branch between benchmark runs:

```sh
gh workflow run publish-parallel-draft.yml --ref codex/parallel-publisher-draft -f cold_cache=true
gh run list --workflow publish-parallel-draft.yml --limit 5
gh run view <run-id> --json headSha,status,conclusion,jobs
```

Require all lanes to pass, check the aggregate's exact source/digest, and retain
the candidate artifact and logs. Run `cold_cache=false` once to populate caches,
then again on that same commit to measure the warmed result. Do not treat the
first caching-enabled run as a warm-cache benchmark. Do not retry a failed lane
and relabel the original run successful; fix a concrete cause on a new head.

Test a failure locally by supplying a wrong SHA-256 to a consumer helper; it
must fail before consumer compilation. The aggregate guard additionally tests
that non-success results cannot be accepted. Hosted cancellation/failure behavior
must also be checked against retained hosted run results. The first production
run is explicitly authorized for rc.20 rather than preceded by benchmark runs.

## Comparison and promotion

rc.19 publisher baseline: run `37361344224`, source
`f2e98f41e0d416e6abc6a33594b9d445e8c722b6`, about 75m41s end-to-end;
the serial canonical job took 62m57s, including 7m34s registry verification.
The dry-run omits registry processing. Compare **prepublication** wall time,
per-job execution, queue time, downloaded/uploaded bytes, runner minutes and
failure rates separately. Rebuilding native artifacts can change tarball bytes
even for identical source; require consistent hashes within each run, not an
assumption of bit-identical output across independent toolchain runs.

Prefer at least two cold successes and a repeated warm success. Confirm that
all original source gates, native matrix/Node/Electron checks, exact packed
consumer proofs and negative tests are represented. Parallelism may increase
runner minutes, downloads and queue contention even when wall time falls.
No measured speedup is claimed until hosted runs finish.

The active graph retains the canonical filename/trusted-publisher identity,
npm environment, exact-current-main/tag/channel guards, OIDC publish,
immutable recovery, registry/provenance verification and GitHub assets.
Only its final protected job has publication authority, after every parallel
gate succeeds. It publishes the sealed bytes, not a second package build.
Never publish a comparison version or retag an existing release just to test
this draft. Physical qualification and support labels remain independent.
