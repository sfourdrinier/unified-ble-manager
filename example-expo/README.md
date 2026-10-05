<!-- example-expo/README.md -->

# Expo SDK 57 CNG fixture

This repository fixture validates the Expo SDK 57 continuous-native-generation
path. The app constructs the host with `createExpoBleManager()` from
`unified-ble-manager/expo`. It uses `unified-ble-manager: file:..` and the v2
config plugin, so it is not a published-package install recipe. The v2 Expo
surface is in the `5.0.0-rc.19` source; this workspace fixture remains
source-checkout evidence only. Registry installation is checked separately.

The BLE host includes native code and cannot run in Expo Go. Generate and build
the native project from the repository root:

```sh
pnpm --dir example-expo install --no-frozen-lockfile
pnpm --dir example-expo exec expo prebuild --clean --no-install
pnpm --dir example-expo android
```

On macOS with the required Xcode and CocoaPods environment, use
`pnpm --dir example-expo ios` after prebuild. That command refreshes the root
Apple RustCore if stale, reinstalls this example's `file:..` package copy if
needed, and verifies the copied framework before Xcode links it. It also checks that the
generated `Info.plist` still carries this fixture's restoration ID, generation,
and `bluetooth-central` mode before Xcode starts. If it reports a mismatch, run
`pnpm --dir example-expo exec expo prebuild --clean --no-install` and rerun the
build. `expo prebuild --clean` regenerates the fixture's ignored native project
directories; it does not validate a live Bluetooth journey.

The fixture exercises the current source-tree CNG/plugin contract. Its
`app.json` opts into the restoration paths — the iOS
`background.ios.restoration` id and the Android companion-presence
foreground service — so the `restoration` scenario can be tested physically
(the procedure in [`BACKGROUND.md`](../docs/BACKGROUND.md) depends on that
opt-in); the fixture config is still not a production restoration recipe.
In the app the two actions stay distinct: **Claim native restoration
(iOS)** adopts the OS journal via `restoration.claim()`, while **Show
restored peers (Android)** reads `peers.restored()`, because Android has no
journal to claim. It tears the manager
down with `destroy()`. A successful CNG prebuild and Android debug
APK/assembly are source-tree/plugin and Android compile proof only. The packed
host gate separately proves the installed tarball's conditional `./expo`,
`./react`, and `./tauri` exports through CJS/ESM runtime import/loadability and
TypeScript Bundler/NodeNext imports; neither proof is a full Expo app build.
Apple/Xcode, EAS, and physical-device permissions, background behavior,
restoration, and radio reliability require separate evidence for the specific
host and hardware. See the root [README](../README.md),
[Expo plugin reference](../docs/EXPO_PLUGIN.md), and
[platform evidence page](../docs/PLATFORMS.md).

## Test scenarios and the remote test driver

The **Test scenarios** screens render the shared cross-host scenarios from
[`../examples-shared/driver`](../examples-shared/driver/README.md):
`h10-stream`, `link-loss`, `device-info`, `mtu`, `scan-details`, `ecg`,
`background`, `restoration`, `h10-capture`, `live-dashboard`. The same code
runs on Web, Tauri, Electron and a Node CLI.
`src/driver/app-driver.ts` is the Expo host adapter. It provides the Expo manager
with its readiness and permission step and its background lease, plus
`AppState`, the React Native WebSocket and the driver URL. In development builds,
and explicitly opted-in Release builds, the same registry is also reachable
from the control server. The app therefore
runs the same code whether a person taps a button or an agent sends the command.

The app connects out to `ws://<Metro host>:8795/host` (protocol
`ubm-test-driver/1`). It takes the host from the URL of the bundle it loaded;
set `EXPO_PUBLIC_UBM_DRIVER_URL` (or `off`) to override it. Release defaults off:
it never auto-discovers Metro. To automate a Release reference build locally,
set `EXPO_PUBLIC_UBM_DRIVER_URL=ws://127.0.0.1:8795/host` when building the bundle;
on Android, reverse USB port 8795 to the local control server as below. On iOS,
use an explicitly reachable trusted control-server address instead; phone
localhost is not the Mac without a separately configured forwarding route.
Changing the environment after installation does not change the embedded bundle.
Keep that build environment available to Gradle: the app-only Expo plugin tracks
the endpoint as the named `ubmReferenceDriverUrl` bundle-task input, so changing
enabled/off/absent configuration invalidates cached Release bundles. Existing
generated Android projects need regeneration through Expo prebuild to receive
this plugin change; do not hand-edit generated Gradle files.
Explicit `off` or an invalid URL opens no connection and never falls back.
This is a reference-app automation opt-in, not a production library setting:
remote commands control BLE, background leases and recording/handoff operations.
Only opt in for trusted local testing; do not distribute a remotely controllable
reference build as a production app. `metro.config.js`
watches `../examples-shared` and resolves its `unified-ble-manager` imports to
this app's installed copy, so the bundle holds one package instance. The
TypeScript paths use that same installed copy for app and shared-driver types;
the resolver regression checks those paths against its package export targets.
Every target ends in `.d.ts`, which Expo's runtime resolver excludes; a regression
also executes the installed Expo resolver to check that runtime imports fall
through to normal Metro resolution rather than these type-only aliases.

After a canonical installed-package refresh, a long-running Metro process can
still serve old transformed JavaScript/Hermes package identity. Once owned BLE
runs have been stopped and released, stop only this app's owned Metro process
and restart it with the supported cache reset:

```sh
pnpm --dir example-expo start --clear --port 8082
```

Perform a full app Reload, not just Fast Refresh/HMR: retained module state can
leave a newly added export undefined. Verify the actually loaded bundle reports
the expected current package/native identities before qualification; a copied
file or a successful build alone is not loaded-runtime proof. Do not weaken
identity or protocol guards to work around a stale bundle. The reset is an
operator refresh step, not a claim that Metro automatically detects every
replacement. Keep separately owned Metro servers and other apps untouched.

```sh
pnpm driver serve                                        # control server: JSON lines on stdout + log file
adb reverse tcp:8795 tcp:8795                            # Android over USB with Metro on localhost
pnpm driver hosts
pnpm driver run android h10-stream start '{"autoReconnect":true}'
pnpm driver sequence ../examples-shared/driver/server/sequences/ecg.json --out /tmp/ecg.json
pnpm test:driver                                         # shared, server and Expo-adapter tests (Node >= 22.18)
```

Every host's launch command, the protocol, and what each host can and cannot
run are in [`../examples-shared/driver/README.md`](../examples-shared/driver/README.md).

## Restoration testing (physical)

`app.json` opts the fixture into the restoration paths so the `restoration`
scenario can be tested physically: the plugin option for iOS
`restoreIdentifierKey` (`background.ios.restoration`, id
`example-expo-primary`) and Android companion presence
(`background.android` connected-device foreground service with its
notification). Plugin options bake into the native project at prebuild
time, so after changing them regenerate and rebuild from the repository
root:

```sh
pnpm --dir example-expo exec expo prebuild --clean --no-install
pnpm --dir example-expo android   # or: pnpm --dir example-expo ios
```

Then run the scenario against the physical host (a paired strap nearby;
iOS kills and relaunches the app, Android binds the companion service):

```sh
pnpm driver run android restoration start '{}'
```

Without the opt-in the scenario has nothing to restore: `background acquire`
answers `capability.unsupported`. tvOS never opts in — it has no background
Bluetooth mode and no state restoration (see Apple TV below).

## Live dashboard screen

The **Live dashboard** screen (`src/screens/MainStack/LiveDashboardScreen/`)
renders the shared `live-dashboard` scenario: one tile per Polar H10 in
range with the strap name, live heart rate (large), RR intervals, skin
contact, a scrolling PMD ECG trace (~5 s, drawn with plain Views — no
charting dependency), optional three-axis accelerometer trace/readings in
milli-g, battery %, firmware revision, model and serial. Tile
states are `discovered`, `connecting`, `streaming`, `reconnecting` (through
the scenario's `createConnectionSupervisor`), and greyed `lost`/`off` with
the last-seen age; each tile also shows the library's own words
(supervisor state, lifecycle cause). Opening the screen auto-starts the
scenario when it is idle; **Stop** ends it. The screen never stops the
scenario on unmount, so the control server keeps observing the same run
after the phone moves to another screen.

How to open it:

- Phone: **Dashboard → Live dashboard: Polar H10 tiles**, or **Test
  scenarios → Live dashboard: Polar H10 tiles**. Tiles stack vertically.
- Apple TV: the same two entries (same source tree, staged by
  `scripts/build-tv.sh`). Tiles form a two-column grid in couch-readable
  type; tiles are focusable and the first tile takes preferred focus, so
  the Siri Remote moves between them — pressing a tile expands its
  connection generation, supervisor attempt, counters and recent lifecycle
  lines.

UI updates ride the scenario's throttled snapshots (250 ms); heart-rate
values, the bounded ECG ring buffer and battery-on-change keep the BLE
delivery path unblocked. Drive it headlessly with
`pnpm driver run <host> live-dashboard start '{"devices":"all-polar"}'`.

ACC controls select 25/50/100/200 Hz and ±2/4/8 G at 16 bits; ECG remains
independently selectable. Stop the live run before changing its settings.
The recording controls start/stop an opt-in bounded raw PMD capture and export
a real JSON document before opening the platform share sheet. A closed sheet
does not prove an external save; the UI reports the retained local file URI
separately, including sharing failures. Rebuild the development app after
installing the `expo-file-system` and `expo-sharing` dependencies.

See [the shared recording and comparison guide](../examples-shared/driver/README.md#record-and-compare-a-simulator-with-a-real-h10)
for capture limits, privacy, exact timestamps, the offline comparison command,
and the stationary/motion protocol to repeat with a real H10. An unexported
recording is lost on process termination; this UI recorder does not substitute
for native background collection or establish real-device fidelity.

## Apple TV (tvOS)

The same app and shared scenarios run on Apple TV from one source tree — not
a fork. The phones keep building from the ignored `ios/` and `android/`
directories; the TV builds from a separate generated stage, `ios-tv/`
(gitignored like `ios/`), produced by `scripts/build-tv.sh`. That script
never touches `ios/` or `android/`.

TV dependency versions (pinned in the script):

- `react-native` via `npm:react-native-tvos@0.86.3-0`, the exact tvOS fork
  release matching Expo SDK 57 / React Native
  0.86 — the version rule in Expo's "Build Expo apps for TV" guide.
- `@react-native-tvos/config-tv` 0.1.6 (peer `expo >= 52`), which rewrites
  the prebuilt native project for TV when `EXPO_TV=1`.

The TV variant is switchable by env: `EXPO_TV=1` only affects the staged
prebuild (and the plugin's tvOS Info.plist handling, see
[`../docs/EXPO_PLUGIN.md`](../docs/EXPO_PLUGIN.md)); a phone prebuild without
it is unchanged.

```sh
bash example-expo/scripts/build-tv.sh stage      # sync sources -> ios-tv, apply TV inputs, drop the staged library copy
bash example-expo/scripts/build-tv.sh install    # pnpm install in ios-tv (needs heap: see script)
bash example-expo/scripts/build-tv.sh verify-identity  # staged library identity equals the repo
bash example-expo/scripts/build-tv.sh prebuild   # EXPO_TV=1 expo prebuild --platform ios + pod install
bash example-expo/scripts/build-tv.sh bundle-url # point the staged AppDelegate at the TV Metro
bash example-expo/scripts/build-tv.sh metro      # serve the staged bundle on 192.168.68.116:8081
DEVELOPMENT_TEAM=<team> bash example-expo/scripts/build-tv.sh build  # Debug .app, team on CLI only
```

The phone Metro stays on 8082: the staged tree resolves `react-native-tvos`,
so it needs its own packager on 8081 (override with `TV_METRO_PORT`). The
driver server stays shared on 8795 — the staged AppDelegate override points
the TV bundle at `192.168.68.116:8081` (override host with `TV_LAN_HOST`),
and the driver URL derives from that bundle host exactly like the phone
build. Install and launch on a paired Apple TV with devicectl:

```sh
TV_DEVICE_ID=<devicectl-id> bash example-expo/scripts/build-tv.sh install-tv
TV_DEVICE_ID=<devicectl-id> bash example-expo/scripts/build-tv.sh launch-tv
node examples-shared/driver/server/cli.mjs hosts   # expect expo-tvos-<model>
```

`DEVELOPMENT_TEAM` is passed on the `xcodebuild` command line only and is
never written into a file; tvOS uses the same bundle id but needs its own
provisioning profile (automatic signing with `-allowProvisioningUpdates`
fetches it when the Mac's Xcode account is available).

The TV reports platform `tvos` (`Platform.OS` stays `'ios'` on
react-native-tvos; `Platform.isTV` selects the label), so the driver shows a
distinct `expo-tvos-<model>` host id. tvOS has no background Bluetooth mode
and no state restoration: `background acquire` answers
`capability.unsupported` and `restoration.claim()` answers
`capability.unavailable` — the library's own answers. Scenario buttons are
focusable for the Siri Remote with no UI fork (`TouchableOpacity` is
TV-focusable by default). Do not run Bluetooth scenarios against hardware
the owner has not made available: launch, driver `hosts`, and the
`readiness` report are the no-hardware check.

#### Packed Apple TV consumer acceptance

The same reference app can be staged outside the repository from an **actual
release tarball**, rather than `file:..`. This path copies the shared scenario
driver into the stage; Metro and native autolinking must not read UBM source or
dependencies from the checkout. It does not rebuild the packed RustCore bytes.

```sh
export TV_STAGE_DIR="$(mktemp -d /tmp/ubm-packed-tv.XXXXXX)"
export TV_PACKAGE_TARBALL=/absolute/path/to/unified-ble-manager-release.tgz
bash example-expo/scripts/build-tv.sh stage
bash example-expo/scripts/build-tv.sh install
bash example-expo/scripts/build-tv.sh verify-identity
bash example-expo/scripts/build-tv.sh prebuild
bash example-expo/scripts/build-tv.sh build-simulator
bash example-expo/scripts/build-tv.sh build-target
```

`verify-identity` binds this acceptance to the checkout's package version and
native identity. The builds link the full React Native TV app, production
`UnifiedBleRustCore` TurboModule, UniFFI bindings and packaged RustCore—not just
the six-file Swift typecheck. Both generic destinations explicitly select
`ARCHS=arm64`; Intel simulator artifacts are not maintained. `build-target`
compiles for physical Apple TV without signing, installation or a radio claim.
For launch, use the built simulator app with the stage's own Metro (`metro`),
or use the signed `build` / `install-tv` / `launch-tv` flow on an available TV.
Keep compile/link, simulator launch, runtime boundary and physical-radio receipts
separate. A successful simulator launch is not proof of CoreBluetooth traffic.
The dashboard's `ReferenceFlatList` measures the TV viewport because this pinned
RN TV release wraps its virtualized list in a non-flex focus guide; a flex-only
inner list collapses. The measured-height adapter preserves virtualization and
the identical controls/scenarios, while phones retain their normal list layout.

#### Android TV acceptance boundary

`scripts/android-tv-emu.sh` installs the reference Android APK, reverses its
driver/Metro ports and launches its activity without forking BLE behavior. The
shared host retains the `android` platform label; identify the selected TV
serial/model explicitly in its receipt rather than treating that label as a
phone or TV qualification. An emulator without a
Bluetooth adapter is a UI/native-module/capability-refusal host, not a BLE radio
fixture. A physical Android TV must independently report adapter, permissions,
companion-presence and background-service capabilities at runtime; a phone pass
or a TV build does not establish those capabilities or hardware qualification.
Run the shared scenarios only for mechanisms that the instantiated backend
reports, retaining platform refusals rather than replacing them with phone
assumptions. Android TV store/launcher packaging is application-owned.

### Android headless continuation reference task

The application entrypoint registers `UBMContinuationWake` before mounting the
UI. The continuation scenario's **Android headless battery check** preset
declares that exact name. Associate the intended device and arm its presence
observation after persisting the preset; this preset does not scan, choose, associate, or request
permissions on a cold wake.

The task accepts the native `{peerId, event: 'companion.appeared'}` payload,
uses the public Expo manager's explicit `{address: peerId, addressType: 'public'}`
target to connect only that known H10/simulator public address, discovers GATT,
and reads the standard Battery Level once. Connection, discovery and read share
a 15-second work budget and cancellation signal. The public scoped helper
releases its connection; manager cleanup is awaited separately, and failed
cleanup remains owned for retry before the next task.
The native MAC is not a fresh manager's opaque peer id. This reference job's
public-address policy is specific to these fixtures; applications using random
or private addresses must supply their own address-kind/durable-reference policy,
not reinterpret an arbitrary opaque peer id as a MAC.
The JavaScript runner admits at most four active-plus-pending invocations and
rejects additional wakes promptly. Admission capacity returns only when the
actual promise settles; Android task bookkeeping timeout does not cancel JS.
The process-owned runner also survives Metro module reevaluation, preserving
its pending jobs and failed-cleanup ledger. A task implementation change requires
a full JS reload; Fast Refresh must not replace that owner.

Up to 16 structured started/completed/failed records are retained in app-private
AsyncStorage under `ubm.reference.headless-continuation.v1`, including the
battery result and cleanup receipt. They are not automatically logged or
exported. Storage failure rejects the task instead of claiming persisted
success. This bounded diagnostic history is not the native durable sensor
journal. Native `task-dispatched` means dispatch acceptance only; actual job
completion requires the separate persisted `completed` record with successful
cleanup. Read it explicitly through the `continuation` scenario's
`headless-history` command (no arguments), for example:

```sh
node examples-shared/driver/server/cli.mjs run <android-host-id> continuation headless-history '{}'
```

This offline diagnostic returns at most 16 summaries: state, observation time,
battery percentage when measured, cleanup state/failure count and failure flag.
Failed summaries also retain bounded error code, domain and operation tokens
when present. Invalid or overlong tokens are omitted and marked with
`errorIdentityRedacted`; this token filter is not general-purpose anonymization.
It omits peer identifiers and raw error/detail payloads. Malformed persisted
entries and storage failures reject the command; they are never silently filtered.
Other hosts report `capability.unsupported`. Reading does not acknowledge or
delete history, open BLE, or alter the durable native sensor journal.
Deterministic task tests are not physical Android background evidence.
