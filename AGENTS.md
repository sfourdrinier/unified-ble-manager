# AGENTS.md — Unified BLE Manager 5.x

The single source of agent guidance for this repository. `CLAUDE.md` imports
this file and holds no content of its own, so the two cannot drift apart.

## What this repository is

The canonical home of `unified-ble-manager` 5.x: a host-neutral Bluetooth Low
Energy central/GATT package for React Native, Web, Electron, and Node/desktop
hosts. `sfourdrinier/react-native-ble-plx` is historical and owns the 3.x line;
never reintroduce its public contract here, and never infer 5.x behaviour from
3.x source or docs.

Read `docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md` for the clean-baseline
architecture before cross-cutting changes, then read the applicable current
5.0 distribution and release guidance. `README.md`,
`docs/NATIVE_ARTIFACTS.md`, and `RELEASE.md` are current operational guidance.

## How we work

**This file holds principles, not procedure.** A rule here must survive being
skimmed at the start of a session: state it, then point to where the detail
lives — `RELEASE.md` for the release process, `docs/BONDING.md` for pairing
semantics, `docs/PROFILES_AND_COMMANDS.md` for profiles, the types and their
doc comments for contract specifics. If an entry here needs a worked example,
an implementation note, or an issue number, it belongs in the document it
points to. `CLAUDE.md` imports this file and holds nothing of its own.

**A change is not done until the documents it touches say so.** Behaviour and
its documentation ship together — a generated artifact is regenerated, a
contract change reaches the type, the changelog and the guide that describes
it, and a rule that stops being true is removed rather than left standing.
Documentation that lags behind the code is a defect of the same kind as a
swallowed failure: it reports something that is not so.

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
pnpm native:status
pnpm lint
pnpm prepack
pnpm release:artifacts:check
node scripts/ci/pack-install-smoke.js
```

Precompiled Rust artifacts are never rebuilt by hand: consumers refresh what
they consume themselves; see `docs/NATIVE_ARTIFACTS.md`.

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

## 5.x contract invariants

Preserve these unless the user explicitly requests a versioned contract change:

- public BLE values are `Uint8Array` / `Readonly<Uint8Array>`; Base64 is an
  explicit codec for external protocols, never the public value contract;
- cancellation uses `AbortSignal`; applications never create public
  transaction IDs;
- a signal **requests**; the result **reports what happened**. An outcome is
  read from what the platform did, never inferred from what was asked of it.
  Two observations of one fact can disagree, so a result reads the operation's
  own answer rather than forming a second opinion;
- **one vocabulary, not one capability.** A word means the same thing in every
  result type that uses it, and every backend answers the same question from
  the same set of answers. A platform that cannot answer says so —
  `capability.unsupported` with a reason — and never substitutes something
  plausible;
- **same behaviour on every platform.** Every host behaves identically unless
  its platform capability genuinely differs, and then it says so rather than
  diverging quietly. A library-side refusal of something the platform can do
  is a defect. Android is the reference behaviour wherever a platform can
  match it. The same physical event carries the same specific public name on
  every backend — the platform's own detail rides underneath in `platform` —
  and reconnect policy makes identical decisions everywhere. The event
  vocabulary lives in `docs/UNIFIED_SEMANTICS.md`, pinned by tests;
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
- **elevated privilege is permitted, never implicit.** The package never
  escalates, shells out to a privileged tool, or assumes it is root: the host
  supplies the privileged operation, so the escalation is auditable where it
  was chosen. Without it the capability reports `unsupported` and the default
  posture is unchanged. Document the privilege, its blast radius, and what a
  failure leaves behind beside the option that requests it;
- **support the mechanism; the consumer owns its entitlements.** Where a
  platform offers a capability, this package implements it, whatever approvals,
  exemptions or store review the consuming application must obtain to use it
  (background execution, foreground-service types, battery-optimisation
  exemptions, notification permission, restoration identifiers). We never
  withhold a mechanism because an app might not be entitled to it, and we never
  quietly substitute a lesser path. When the platform refuses at runtime, the
  result reports that refusal with the platform's own reason under `platform`,
  and the capability says what is available; the app's negotiation with Apple
  or Google is the app's business, not ours;
- package SemVer and backend support/evidence labels are independent
  dimensions.

Do not reintroduce the legacy 3.x `BleManager`/`Device`/`Service`/
`Characteristic` facade, Base64 public payloads, caller transaction IDs, static
`supports()` matrices, or Noble compatibility paths.

## Host implementations

**React Native** uses explicit manager construction. The production factory
resolves the `UnifiedBleRustCore` TurboModule and speaks the versioned
`ubm-mobile-wire/1` session protocol. The historical
`UnifiedBleProtocolControl` boundary remains only where legacy protocol
artifacts and their compatibility tests still name it; it is not the active
factory route. The modernization floor is React Native 0.86+; Expo integration
targets SDK 57+. The package contains native code and does not run in Expo Go.
Keep package metadata, examples, native defaults and docs aligned unless the
project intentionally raises a floor.

**Web** uses Web Bluetooth's explicit chooser/session integration. Browser
user-activation and security restrictions are part of the host contract; do not
emulate background scanning or restoration that Web Bluetooth does not provide.

**Electron**: only trusted main-process code selects or owns the radio.
Renderer reload/rebind is an ownership and security boundary.

**Node desktop**: first-party CoreBluetooth, WinRT and BlueZ backends.
CoreBluetooth/WinRT addons are built for the exact Node/Electron ABI and
architecture that loads them. BlueZ is isolated behind its explicit entrypoint
and needs no `dbus-next` on the production path.

## Evidence and support

Package SemVer and backend qualification are separate. A 5.0 release
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
