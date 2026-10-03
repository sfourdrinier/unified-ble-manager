# TV hosts

Status: Current 5.x consumer guidance. See
[Current 5.0 authority](README.md#current-50-authority) for the shared contract.

## One public runtime

Apple TV and Android TV use `unified-ble-manager/react-native`, the same explicit
manager construction, permissions, connection ownership, GATT operations, profiles
and teardown as the phone reference app. The production radio is
`UnifiedBleRustCore`; there is no TV-only manager or alternate BLE implementation.
Choose TV-compatible React Native/Expo build inputs for the application, not a
different UBM entrypoint. The reference consumer pins Expo SDK 57 and
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
