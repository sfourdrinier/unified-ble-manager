# AGENTS.md — Unified BLE Manager 4.x

The single source of agent guidance for this repository. `CLAUDE.md` imports
this file and holds no content of its own, so the two cannot drift apart.

## What this repository is

The canonical home of `unified-ble-manager` 4.x: a host-neutral Bluetooth Low
Energy central/GATT package for React Native, Web, Electron, and Node/desktop
hosts. `sfourdrinier/react-native-ble-plx` is historical and owns the 3.x line;
never reintroduce its public contract here, and never infer 4.x behaviour from
3.x source or docs.

Read `docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md` before cross-cutting
changes. `README.md` and `RELEASE.md` are current guidance.

## How we work

Extreme DRY and test-first. Write the test before the behaviour, for logic,
metadata, build configuration and contract guards alike.

Never silently swallow a failure. A dropped record, a swallowed exception or a
filtered-out observation turns a specific fault into "nothing happened", which
is the most expensive class of bug to diagnose — especially against real
hardware, where the symptom surfaces far from its cause.

Do not add deprecated APIs, libraries, configuration or build patterns when a
supported alternative exists. If deprecated usage cannot be removed safely,
document why and add focused regression coverage.

**Typing.** No `as unknown`, `as any`, or `as T` to silence the checker. Infer
by default; annotate only exported boundaries; use mappers and guards, and fix
the types.

## Package manager and checks

pnpm with Corepack. The canonical gate:

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

Before pushing, `scripts/ci/preflight.sh` runs the Linux-reproducible CI jobs
against a clean detached worktree outside the working tree — the same thing
`actions/checkout` gives a runner, so uncommitted edits and stale build output
cannot make it pass. `--fast` skips the two Android Gradle builds. It does not
cover the windows/macos legs, the `apple` job, the CoreBluetooth and WinRT
boundaries, or the Node matrix; green there means "worth pushing", never "CI
will pass".

Focused commands while iterating:

- `pnpm test:native-protocol` (and `:android`, `:apple`, `:winrt`)
- `pnpm build:example:web`
- `pnpm build:electron:macos`, `pnpm build:electron:winrt`
- `pnpm performance:check`

CI owns the broader cross-platform compile/ABI matrix.

## Public architecture

The neutral root exports shared public manager and types and **does not choose
a radio**. Consumers use explicit host entrypoints:

`unified-ble-manager/react-native` · `/web` · `/electron/main` ·
`/electron/renderer` · `/node/corebluetooth` · `/node/winrt` · `/node/bluez` ·
`/backend-sdk` · `/testing` · `/codecs` · `/cli`

Profile exports are documented in `README.md` and
`docs/PROFILES_AND_COMMANDS.md`.

## 4.x contract invariants

Preserve these unless the user explicitly requests a versioned contract change:

- public BLE values are `Uint8Array` / `Readonly<Uint8Array>`; Base64 is an
  explicit codec for external protocols, never the public value contract;
- cancellation uses `AbortSignal`; applications never create public
  transaction IDs;
- a signal **requests** cancellation; the result **reports what happened**. An
  operation's outcome is read from what the platform actually did, never
  inferred from the fact that cancellation was asked for. `cancelPairing()`
  therefore answers from the in-flight pairing's own result rather than forming
  a second opinion — two observations of one fact can disagree, and that
  disagreement is the defect. Every backend answers from the same vocabulary,
  and a word means the same thing in every result type that uses it
  (`'paired'` is "a bond exists because of this operation" in both
  `SecurityPairResult` and `SecurityCancelPairingResult`; `'already-paired'`
  means "bonded before the call" and belongs only to the former). Where a
  platform genuinely cannot answer, it says so with `capability.unsupported`
  and a reason — never a plausible substitute. Known gap: `pair()`'s abort path
  still reports `'cancelled'` without knowing whether the peer bonded anyway,
  because learning the truth means waiting for the radio and waiting risks
  hanging the caller behind a wedged daemon (#157); `state()` and `watch()` are
  authoritative for the bond;
- the root is host-neutral and never silently picks or falls back to a backend;
- managers, connections, GATT databases, subscriptions and backend resources
  have explicit ownership and asynchronous teardown; stale discoveries and
  handles are not immortal device-object state;
- capabilities are typed and reported by the instantiated backend at runtime,
  never a static platform matrix;
- Electron main owns the radio; renderers use the versioned IPC client and do
  not load Node-API radio addons;
- native and private backend protocols are versioned and fail closed;
- deterministic backends and mocks are test infrastructure, never production
  radio fallbacks;
- package SemVer and backend support/evidence labels are independent
  dimensions.

Do not reintroduce the legacy 3.x `BleManager`/`Device`/`Service`/
`Characteristic` facade, Base64 public payloads, caller transaction IDs, static
`supports()` matrices, or Noble compatibility paths.

## Host implementations

**React Native** uses the versioned `UnifiedBleProtocolControl` boundary and
explicit manager construction. The modernization floor is React Native 0.86+;
Expo integration targets SDK 57+. The package contains native code and does not
run in Expo Go. Keep package metadata, examples, native defaults and docs
aligned unless the project intentionally raises a floor.

**Web** uses Web Bluetooth's explicit chooser/session integration. Browser
user-activation and security restrictions are part of the host contract; do not
emulate background scanning or restoration that Web Bluetooth does not provide.

**Electron**: only trusted main-process code selects or owns the radio.
Renderer reload/rebind is an ownership and security boundary.

**Node desktop**: first-party CoreBluetooth, WinRT and BlueZ backends.
CoreBluetooth/WinRT addons are built for the exact Node/Electron ABI and
architecture that loads them. BlueZ is isolated behind its explicit entrypoint
and optional `dbus-next` dependency.

## Evidence and support

Package SemVer and backend qualification are separate. Stable `4.0.0`
stabilizes the documented package/API contract; it does not promote any
backend's evidence label.

`docs/generated/PLATFORM_SUPPORT.md` is generated from retained evidence.
Compilation, deterministic tests, ABI loading and mocks prove only those
levels. **Never describe deterministic, mock or compile evidence as
physical-radio proof.**

## Generated artifacts

Do not hand-edit generated support/reference artifacts, API reports,
`SBOM.cdx.json` or `THIRD_PARTY_LICENSES.json` when a generator owns them. Run
the generator and commit its output; verify with `pnpm release:artifacts:check`
and `pnpm docs:check`.

## Releases

`main` is the canonical release branch and stable releases are tag-driven from
the exact current `main` commit, published by `.github/workflows/publish.yml`
through npm trusted publishing/OIDC with provenance once the release gates
pass. Follow `RELEASE.md`.

When a release carries **more than one PR**, cut a named release branch
(`release/<version>`), retarget every PR onto it, merge them there, and open a
single PR from the release branch into `main`. That keeps `main` releasable,
gives one place to resolve changelog collisions between PRs, and leaves linked
issues open until the release actually lands — GitHub only auto-closes on merge
to the default branch. CI runs on `release/**` for both pushes and PRs; a
release branch is gated in its own right because the integrated combination is
what ships.

Never publish a normal release manually, move or recreate a published version
tag, or weaken evidence/support labels to make a release pass.

## Historical names

Inherited native and example identifiers such as `BlePlxExample` may remain
where they are internal fixture or scheme names. Cosmetic native renames are
not a release goal; change them only with an explicit compatibility and build
rationale, and complete validation.
