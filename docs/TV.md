# TV hosts

Status: Current 5.x consumer guidance. See
[Current 5.0 authority](README.md#current-50-authority) for the shared contract.

## One public runtime

Apple TV and Android TV share the phone manager's connection ownership, GATT
operations, profiles and teardown. Bare React Native applications use
`createReactNativeBleManager` from `unified-ble-manager/react-native`. Expo TV
applications use `createExpoBleManager` from `unified-ble-manager/expo` when they
want `manager.readiness()` and `manager.permissions.request`; an Expo app that
manages permissions itself may keep the bare factory. Both factories use the
production `UnifiedBleRustCore` radio; there is no TV-only manager or alternate
BLE implementation. Reading readiness never prompts; permission requests are
explicit. See [the permission flow](GETTING_STARTED.md#2-request-permission-explicitly).
Choose TV-compatible React Native/Expo build inputs for the application. The reference consumer pins Expo SDK 57 and
`react-native-tvos@0.86.3-0`; the package's Apple deployment floor is tvOS 16.4.

The reference app and shared test driver live in
[`example-expo`](../example-expo/README.md#apple-tv-tvos). TV staging copies the
same sources and uses a measured, focusable list viewport. It does not fork the
BLE driver, inject a mock radio, or alter the phone projects.

## Exact packed Apple TV consumer

On an Apple Silicon Mac with Xcode and the required tvOS SDK installed, point
the existing acceptance gate at the immutable package you intend to test:

```sh
TV_PACKAGE_TARBALL=/absolute/path/unified-ble-manager-release.tgz pnpm test:tvos:packed
```

The gate stages outside the checkout, installs the supplied tarball, verifies the
installed package identity and generates the TV project. It builds the ARM64
simulator and unsigned physical Apple TV targets sequentially. It never refreshes
the packed RustCore bytes from repository source. Successful builds establish
compile/link evidence, not physical BLE qualification. The existing Apple CI lane
runs this gate in addition to the library-level Swift check.

Simulator launch can verify UI, native TurboModule registration, UniFFI/Rust
linkage and non-radio control routes. It cannot establish peripheral visibility,
Bluetooth permission behavior, notification delivery or radio recovery.
Physical installation, signing, Metro and driver commands are documented in the
[reference app's TV instructions](../example-expo/README.md#apple-tv-tvos).

## Platform limitations and qualification

tvOS provides foreground CoreBluetooth operations, but does not offer iOS-style
background Bluetooth execution or state restoration. AccessorySetupKit is an
iOS capability, not a tvOS replacement. Request readiness/capabilities from the
instantiated manager and preserve an explicit unsupported result; never promise
phone background behavior on Apple TV.

Do not copy the phone reference app's restoration, native continuation or
connected-device foreground-service configuration into a TV application. Expo
Apple TV prebuilds use `EXPO_TV=1`; the plugin removes iOS-only background,
restoration and continuation keys. Android TV consumers should start with
`background.android.mode: 'none'`. For non-location BLE scans on API 31+, declare
`permissions.android.neverForLocation: true`; do not request location merely to
imitate the phone example. Android below API 31 has its own location requirements;
`legacyLocation: 'none'` does not provide those declarations or authorize scans.

For Android/Fire OS API 24–30, set `permissions.android.legacyLocation: 'auto'`
in the Expo plugin so the application declares fine-location permission.
Only when `neverForLocation: true` is also selected does the plugin add
`maxSdkVersion: 30` to that declaration. Before scanning, explicitly call
`manager.permissions.request({ purpose: 'scan-and-connect' })`, inspect the returned
permission outcome, and require ready scan readiness. The Expo manager resolves
the required runtime permissions for the actual API level; a declaration alone
is not a permission grant. Readiness also measures location services and the
legacy location permission from the native Android runtime on each probe, so a
policy declaration alone never implies that location services are disabled.
API 31+ uses Bluetooth permissions instead. Bare
React Native consumers must declare/request the corresponding permissions in
their application; the factory does not perform runtime requests for them.

The maintained Android native package includes `armeabi-v7a`, `arm64-v8a` and
`x86_64`. ARM32 is required by 32-bit Android application environments such as
Fire TV Stick 4K Plus (2025), even though its CPU is ARM64
([Amazon device specifications](https://developer.amazon.com/docs/device-specs/device-specifications-fire-tv-streaming-media-player.html)). This is Android/Fire
OS coverage, not a Vega OS port. The packed TV consumer gate builds the Expo
SDK 57 / React Native TV 0.86 graph for ARM32 and inspects every native object in
the APK. Compilation does not qualify Bluetooth on a physical Fire TV.

Android TV, Google TV and Fire TV use Android's native mechanisms where the device
actually provides them. Bluetooth hardware, permissions, companion-device services
and background policy vary by device/OS; a television brand is not a static
capability claim. Use the shared Android reference consumer and driver, then
qualify the actual hardware. Do not infer Android TV radio support from an Apple
TV simulator build or a phone test.

For physical qualification, retain exact app/package and native artifact
identities, device/OS and peer identities, permission outcomes, scan/connect/GATT
results, ECG/ACC or other profile values, cancellation, link-loss/reconnect and
confirmed/refused cleanup. Relevant retained receipts feed
[`generated/PLATFORM_SUPPORT.md`](generated/PLATFORM_SUPPORT.md); a stable package
version does not automatically promote those evidence labels.
