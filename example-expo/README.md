<!-- example-expo/README.md -->

# Expo SDK 57 CNG fixture

This repository fixture validates the Expo SDK 57 continuous-native-generation
path for `unified-ble-manager`. It uses `unified-ble-manager: file:..` and the
`unified-ble-manager` config plugin, so it is not a published-package install
recipe.

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

The fixture exercises the current source-tree CNG/plugin contract for the
PR10 integration gate; its native configuration is not an application recipe
for PR6 and must not be copied as a restoration contract. It tears the manager
down with `destroy()`. A successful CNG prebuild or native
assembly is package/compile proof only. Physical-device permissions, background
behavior, restoration, and radio reliability require separate evidence for the
specific host and hardware. See the root [README](../README.md),
[Expo plugin reference](../docs/EXPO_PLUGIN.md), and
[platform evidence page](../docs/PLATFORMS.md).
