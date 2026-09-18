<!-- example-expo/README.md -->

# Expo SDK 57 CNG fixture

This repository fixture validates the Expo SDK 57 continuous-native-generation
path. The app constructs the host with `createExpoBleManager()` from
`unified-ble-manager/expo`. It uses `unified-ble-manager: file:..` and the v2
config plugin, so it is not a published-package install recipe. The v2 Expo
surface ships in the `5.0.0-rc.0` package; until that version is published, this
workspace fixture remains source-checkout evidence only.

The BLE host includes native code and cannot run in Expo Go. Generate and build
the native project from the repository root:

```sh
pnpm --dir example-expo install --no-frozen-lockfile
pnpm --dir example-expo exec expo prebuild --clean --no-install
pnpm --dir example-expo android
```

On macOS with the required Xcode and CocoaPods environment, use
`pnpm --dir example-expo ios` after prebuild. `expo prebuild --clean` regenerates
the fixture's ignored native project directories; it does not validate a live
Bluetooth journey.

The fixture exercises the current source-tree CNG/plugin contract; its native
configuration is not an application restoration recipe. It tears the manager
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
`background`. The same code runs on Web, Tauri, Electron and a Node CLI.
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
