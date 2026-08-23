<!-- docs/EXPO_PLUGIN.md -->

# Expo plugin option reference

The plugin configures native projects for Expo development builds. It does not
start a radio, request runtime permissions during prebuild, or prove physical
radio/restoration reliability. Expo Go is not a supported BLE execution
environment because it cannot contain this native module.

## Installation and development build

    bunx expo install unified-ble-manager expo-dev-client
    bunx expo prebuild --clean
    bunx expo run:ios
    # or
    bunx expo run:android

For EAS development builds:

    bunx eas build --profile development --platform ios
    bunx eas build --profile development --platform android

expo is an optional host peer. Bare React Native, Web, Node, Electron, and
Tauri consumers do not resolve Expo tooling.

## Current v2 schema

    [
      "unified-ble-manager",
      {
        "requiredHardware": true,
        "permissions": {
          "bluetoothAlways": "Allow $(PRODUCT_NAME) to connect to Bluetooth devices",
          "android": {
            "neverForLocation": true,
            "legacyLocation": "none"
          }
        },
        "background": {
          "ios": {
            "mode": "central",
            "restoration": {
              "id": "primary-ble-central",
              "generation": "1"
            },
            "showPowerAlert": true
          },
          "android": {
            "mode": "none"
          }
        },
        "diagnostics": {
          "nativeLogging": "errors"
        }
      }
    ]

The TypeScript contract is UnifiedBleExpoPluginOptions in
plugin/src/expoPluginSchema.ts. That module is the single runtime/type
definition source for validation and normalization.

### requiredHardware

When true, declares android.hardware.bluetooth_le as required. When omitted or
false, the plugin does not manage that feature declaration.

### permissions

- bluetoothAlways: a non-empty iOS Bluetooth usage description, or false to
  remove the managed key.
- android.neverForLocation: the explicit Android scan assertion. It is never
  inferred from product behavior.
- android.legacyLocation: auto, required, or none. Location permission
  declarations are managed only according to this policy and target-SDK rules.

The plugin never requests runtime permission during import or prebuild.

### background

- ios.mode must be central; peripheral mode is rejected.
- ios.restoration accepts one application-facing id and an optional
  generation. The plugin writes only UnifiedBleProtocolRestorationId and
  UnifiedBleProtocolRestorationGeneration. The trusted native host derives the
  restore identifier, namespace, client identity, and host scope from its
  bundle identifier plus these values.
- android.mode is none or connected-device-foreground-service. The latter
  requires a complete notification (channelId, channelName, and title) and may
  set body, icon, and an explicit restart policy.

Foreground-service declarations do not acquire a runtime lease or guarantee
background reliability. The application must explicitly acquire and release
the runtime background lease exposed by the host when that surface is
available.

### diagnostics

nativeLogging is off, errors, or events. The normalized configuration is
deterministic and managed values are removed when the option is removed.

## Validation and reconciliation

Unknown keys, empty strings, malformed restoration tokens, invalid
discriminated-union combinations, and incomplete foreground-service
notifications fail closed. Consecutive prebuilds are idempotent; duplicate
managed declarations are reconciled; unrelated Info.plist and manifest
configuration is preserved; stale RC1 managed keys are removed.

The old flat keys are intentionally rejected:

- requiresBluetoothLeHardware
- neverForLocation
- modes
- bluetoothAlwaysPermission
- iosNativeProtocolRestoration
- androidEnableForegroundService

Do not author clientId, hostSessionScope, namespace, or protocol epoch values
in application configuration. Those are native protocol identities, not
application policy.

## Retired RC1 migration note

> **DO NOT COPY: HISTORICAL CONTRACT-TEST FIXTURE ONLY.**

The RC1 five-field restoration shape is not supported application configuration.
It is retained only in historical native-contract evidence so
the migration boundary remains auditable. Use the v2 one-token schema above.

## Related records

- MIGRATION_4.0.md
- BACKGROUND.md
- PLATFORMS.md
- UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md
