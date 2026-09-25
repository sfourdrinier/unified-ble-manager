<!-- example-expo/README.md -->

# Expo SDK 57 CNG fixture

This repository fixture validates the Expo SDK 57 continuous-native-generation
path. The app constructs the host with `createExpoBleManager()` from
`unified-ble-manager/expo`. It uses `unified-ble-manager: file:..` and the v2
config plugin, so it is not a published-package install recipe. The v2 Expo
surface is in the `5.0.0-rc.8` source; this workspace fixture remains
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
`AppState`, the React Native WebSocket and the driver URL. In development builds
the same registry is also reachable from the control server. The app therefore
runs the same code whether a person taps a button or an agent sends the command.

The app connects out to `ws://<Metro host>:8795/host` (protocol
`ubm-test-driver/1`). It takes the host from the URL of the bundle it loaded;
set `EXPO_PUBLIC_UBM_DRIVER_URL` (or `off`) to override it. `metro.config.js`
watches `../examples-shared` and resolves its `unified-ble-manager` imports to
this app's installed copy, so the bundle holds one package instance.

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
charting dependency), battery %, firmware revision, model and serial. Tile
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

## Apple TV (tvOS)

The same app and shared scenarios run on Apple TV from one source tree — not
a fork. The phones keep building from the ignored `ios/` and `android/`
directories; the TV builds from a separate generated stage, `ios-tv/`
(gitignored like `ios/`), produced by `scripts/build-tv.sh`. That script
never touches `ios/` or `android/`.

TV dependency versions (pinned in the script):

- `react-native` via `npm:react-native-tvos@0.86-stable` (resolves to
  `0.86.3-0`), the tvOS fork release matching Expo SDK 57 / React Native
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
