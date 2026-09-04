<!-- docs/EXPO_PLUGIN.md -->

# Expo plugin option reference

The plugin configures native projects for Expo development builds. It does not
start a radio, request runtime permissions during prebuild, or prove physical
radio/restoration reliability. Expo Go is not a supported BLE execution
environment because it cannot contain this native module.

Use the v2 plugin options in this `4.0.21` source. Those options match
the schema introduced at `4.0.0-rc.4`. Expo Go cannot load this native module.

## Installation and development build

Pin the package so a later `latest` bump does not change native plugin options
without a rebuild:

    pnpm add unified-ble-manager
    pnpm add expo@^57.0.0 expo-dev-client
    npx expo prebuild --clean
    npx expo run:ios
    # or
    npx expo run:android

For EAS development builds:

    bunx eas build --profile development --platform ios
    bunx eas build --profile development --platform android

These commands are build recipes, not evidence by themselves. The packed-host
gate (`pnpm prepack && node scripts/ci/packed-host-consumer-check.js`) proves
the installed tarball's conditional `./expo`, `./react`, and `./tauri` exports
by CJS and ESM runtime import/loadability checks and TypeScript imports under
Bundler and NodeNext resolution. That exact packed export/type/import proof
does not prove a full Expo application build.

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

The Expo host also exposes an Android-only Companion Device Manager ceremony:
`ble.association.associate({ name, serviceUuid })`. It launches Android system
UI and returns an `associated` peer-directory record. Association is not a
bond, connection, GATT attachment, or broad scan-permission bypass; unsupported
hosts fail explicitly.

### background

- ios.mode must be central; peripheral mode is rejected.
- ios.restoration accepts one application-facing id and an optional
  generation. The plugin writes only UnifiedBleProtocolRestorationId and
  UnifiedBleProtocolRestorationGeneration. The trusted native host derives the
  restore identifier, namespace, client identity, and host scope from its
  bundle identifier plus these values.
- android.mode is none or connected-device-foreground-service. The latter
  requires a complete notification (channelId, channelName, and title) and may
  set body, icon, and an explicit restart policy. Background is absent by
  default; restart is `never` by default.

When the Android connected-device service is active, applications can publish
current user-facing state without changing service ownership:

    const lease = await manager.background.acquire({
      kind: 'connected-device', reason: 'active glucose monitoring'
    })
    await manager.background.updateNotification({
      title: 'Glucose 108', body: 'Updates are private'
    })

`updateNotification` requires an active lease, validates bounded non-empty
text, updates the existing UBM notification in place, and never acquires or
starts a service. The notification retains its configured channel, icon,
connected-device service type, ongoing state, and session-intent policy; its
tap opens the host app. Release the lease when monitoring ends. Optional
reconnection remains application-owned: `while-session-intent-exists` recovers
the configured service after boot or package replacement only when the native
session intent exists; it never scans or reconnects.

The platform matrix is intentionally explicit: Android supports the lease and
notification update when the connected-device service is configured; Apple,
Web, desktop, and other hosts reject these Android-only operations with
`capability.unsupported` (or `capability.unavailable` when a required Android
lease/configuration is absent). There is one contract for the public API and
native bridge; unsupported surfaces do not silently discard options.

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

The example's source-tree CNG prebuild and Android debug APK/assembly are
separate source/plugin and Android compile evidence. Apple/Xcode and EAS build
evidence require successful platform-specific runs, while physical-device
permissions, restoration, background behavior, and radio reliability require
separate device evidence.

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
