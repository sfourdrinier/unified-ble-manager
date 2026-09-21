# Native artifacts lifecycle (F9)

Every Rust change (`crates/**`, `bindings/**`, `vendor/**`) stales up to
three precompiled artifacts. Each one carries the sealed `sourceDigest` and
`bindingSchema` it was built with, computed by the single implementation in
[`../scripts/release/native-build-identity.js`](../scripts/release/native-build-identity.js)
(never reimplemented anywhere else). Status compares the sealed digests
against the current sources; a mismatch is `stale`, never silent.

## The artifacts

| Artifact        | Tree                                                                              | Committed?                     | Canonical builder                                          | Refresh command                                                                                                                     |
| --------------- | --------------------------------------------------------------------------------- | ------------------------------ | ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Android jniLibs | `android/src/main/jniLibs/<abi>/libubm5_jni_echo.so` + `build-identity.json`      | Yes — packed consumers load it | `android/refresh-prebuilt-jniLibs.sh`                      | `sh android/refresh-prebuilt-jniLibs.sh`                                                                                            |
| Apple RustCore  | `ios/RustCore/RustCore.xcframework` + `build-identity.json`                       | No (gitignored)                | `ios/build-rust-core.sh` (`pnpm native:apple:prepare`)     | `pnpm native:apple:prepare`                                                                                                         |
| Desktop N-API   | `native/desktop-core/prebuilds/<platform>-<arch>/ubm_desktop_core.node` + sidecar | No (gitignored)                | `scripts/ci/build-napi-addon.js --profile release --out …` | `node scripts/ci/build-napi-addon.js --profile release --out native/desktop-core/prebuilds/<platform>-<arch>/ubm_desktop_core.node` |

The Tauri plugin (`native/tauri`) is not a precompiled artifact: it compiles
inside the Tauri app build, so it needs no status row and no refresh.

## The fourth thing a Rust change stales: the expected identity

`src/generated/native-build-identity.ts` is the identity every host compares a
loaded binary against. It is generated from the same digest, so a Rust change
stales it too — **including a formatting-only `cargo fmt`**, which changes the
sources the digest covers without changing a single artifact's behaviour.

It has its own gate, separate from `native:status`: `pnpm prepack` runs
`node scripts/release/native-build-identity.js --check`, and a stale file fails
the build before anything is packed. `pnpm native:refresh` rebuilds artifacts;
it does not write this file. Regenerate it with

```sh
node scripts/release/native-build-identity.js --write
```

and commit the result with the Rust change that caused it. `--check` is the
question "does the expected identity match the sources?"; `pnpm native:status`
is the question "were the artifacts built from these sources?". Both have to
answer yes, and one can be stale while the other is fresh.

## One-command status and refresh

- `pnpm native:status` prints one line per artifact — `fresh`, `stale` or
  `missing` with the staged and current digests and the exact refresh
  command — and exits non-zero when anything applicable is stale or missing.
  Rows that cannot be built on this host (Apple off macOS, other platforms'
  desktop prebuilds) are listed as `not-applicable`, never skipped.
  `--only android,apple,desktop` scopes the gate; `--json` emits one JSON
  line for tooling.
- `pnpm native:refresh [--only …]` rebuilds only what is stale, sequentially,
  with the canonical builders above, then re-checks and fails while anything
  is still stale. A rebuild prints one line naming what was rebuilt and why
  (old/new digest); fresh groups are a silent no-op.
- `UBM_NATIVE_REFRESH=off` switches consumers to check-only: stale or missing
  artifacts fail loudly instead of rebuilding.

The committed Android tree is refreshed the same way: the refresh leaves the
`jniLibs` diff in git, so the maintainer sees it and commits it.

## Where the checks run (fully automated — no hand rebuilds)

| Consumer                                               | What it refreshes      | Entry point                                                                 |
| ------------------------------------------------------ | ---------------------- | --------------------------------------------------------------------------- |
| `examples-shared/driver/hosts.sh up` (electron/node)   | `desktop`              | `scripts/native/ensure-native.js` before spawn; Tauri is exempt (own build) |
| `example-expo/scripts/build-tv.sh build`               | `apple`                | `ensure-native.js` before the TV link                                       |
| Phone Expo iOS (`pnpm --dir example-expo ios`)         | `apple`                | `example-expo/package.json` `ios` script                                    |
| Phone Expo Android (`pnpm --dir example-expo android`) | `android`              | `example-expo/package.json` `android` script                                |
| Apple CI phone job                                     | `apple`                | `pnpm native:refresh --only apple` before install                           |
| Linux/Android CI Expo job                              | `android` (check-only) | `pnpm native:status --only android` before prebuild                         |
| Publish release job                                    | everything assembled   | `pnpm native:status` after the `--check-*` gates                            |
| Pre-push filter                                        | `android` (check-only) | `pnpm native:status --only android` in `scripts/ci/preflight.sh`            |
| Canonical gate                                         | everything applicable  | `pnpm native:status` in [`../AGENTS.md`](../AGENTS.md)                      |

A refresh failure aborts the consumer with the builder's error. Nothing —
local script, CI job, or release — continues on a stale artifact.

Clean checkouts (CI fresh clones, `preflight.sh` worktrees) have no
gitignored stagings by design, so those gates check only what can be there:
the committed Android tree, or the artifacts the job just assembled.
