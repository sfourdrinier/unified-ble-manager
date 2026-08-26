<!-- example-expo/README.md -->

# Expo SDK 57 CNG fixture

This repository fixture validates the Expo SDK 57 continuous-native-generation
path. The app constructs the host with `createExpoBleManager()` from
`unified-ble-manager/expo`. It uses `unified-ble-manager: file:..` and the v2
config plugin, so it is not a published-package install recipe. The v2 Expo
surface is in the published `4.0.6` package.

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
