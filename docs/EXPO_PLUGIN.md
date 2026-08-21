<!-- docs/EXPO_PLUGIN.md -->

# Expo plugin option reference

> PR10 deferral: the RC1 five-field restoration/plugin schema documented below
> is retained for native/plugin contract tests, but is not the current PR6
> application configuration recipe. Do not copy `iosNativeProtocolRestoration`
> or author `clientId`/`hostSessionScope` in application code until the PR10
> Expo v2 schema and native-authoritative restoration slice lands.

Everything below this notice is historical schema/reference material for the
plugin contract tests. It is intentionally non-copyable application guidance
until PR10 lands; current PR6 consumers must not treat the table or JSON block
as a supported release recipe.

Configure the published plugin as `unified-ble-manager`. The plugin's supported
options are exactly the schema implemented in `plugin/src/withBLE.ts`:

```sh
pnpm add unified-ble-manager expo@^57.0.0
```

`expo` is an optional host peer. The plugin imports Expo's supported
`expo/config-plugins` subpath; applications must not install the internal
`@expo/config-plugins` package directly. Web, bare React Native, and Node
consumers do not resolve Expo tooling.

The plugin writes native project configuration. That is not a live-radio or
restoration support claim. See [`PLATFORMS.md`](PLATFORMS.md).

| Option | Type | Effect |
| --- | --- | --- |
| `debug` | `boolean` | Enables plugin diagnostics; `UNIFIED_BLE_MANAGER_PLUGIN_DEBUG=1` also enables them. `BLEPLX_PLUGIN_DEBUG=1` remains a deprecated alias. |
| `requiresBluetoothLeHardware` | `boolean` | Adds the required Android BLE hardware feature (`android.hardware.bluetooth_le`). It does not create a foreground service or change manager lifecycle. |
| `neverForLocation` | `boolean` | Adds Android's `neverForLocation` scan flag and caps legacy location permissions at API 30. Android treats this as a strong assertion and may filter some BLE beacons. Set it only when the product makes that assertion. |
| `modes` | `('central')[]` | Adds iOS `bluetooth-central` background mode. Peripheral mode is rejected; this library is a BLE central. |
| `bluetoothAlwaysPermission` | `string \| false` | Sets, or suppresses, `NSBluetoothAlwaysUsageDescription`. |
| `iosNativeProtocolRestoration` | `{ identifier, namespace, epoch, clientId, hostSessionScope }` | Atomically writes the five non-empty native restoration identity values required by `UnifiedBleProtocolControl`. |

For example:

```json
[
  "unified-ble-manager",
  {
    "requiresBluetoothLeHardware": true,
    "modes": ["central"],
    "neverForLocation": false,
    "bluetoothAlwaysPermission": "Allow $(PRODUCT_NAME) to connect to Bluetooth devices",
    "iosNativeProtocolRestoration": {
      "identifier": "com.example.app.ble",
      "namespace": "com.example.app.ble",
      "epoch": "2026-07-30",
      "clientId": "com.example.app.ble-client",
      "hostSessionScope": "com.example.app.mobile-ble"
    }
  }
]
```

Every provided plugin property is validated exactly: unknown keys, non-boolean
flags, invalid or duplicate modes, invalid permission values, and incomplete
restoration objects fail configuration. `iosNativeProtocolRestoration` writes
`UnifiedBleProtocolRestoreIdentifier`,
`UnifiedBleProtocolRestorationNamespace`,
`UnifiedBleProtocolRestorationEpoch`,
`UnifiedBleProtocolRestorationClientId`, and
`UnifiedBleProtocolRestorationHostSessionScope` as one unit. When absent, the
plugin removes all five values rather than leaving a partial native identity.

This configuration does not create a second CoreBluetooth central, restore a
connection, reconnect a peripheral, or define a product restoration policy. Use
it only with the explicit manager-owned adoption flow in
[`MIGRATION_4.0.md`](../MIGRATION_4.0.md), and ensure `clientId` and
`hostSessionScope` exactly match the app's host-owned manager/adoption values.
Do not claim restoration support from plugin configuration alone.

These `react-native-ble-plx` plugin keys are not accepted:

- `iosEnableRestoration`
- `iosRestorationIdentifier`
- `iosNativeProtocolRestorationIdentifier`
- `androidEnableForegroundService`

Do not add aliases or compatibility transforms for those names. A host that
needs an Android foreground service must own and validate that platform policy;
the plugin does not silently provide it.

[`../example-expo/`](../example-expo/) is the repository CNG fixture and uses a
`file:..` dependency. It demonstrates the source-tree integration path only;
pin the published package version in a consumer and validate that consumer's
native build separately.

## Related records

- [`MIGRATION_4.0.md`](../MIGRATION_4.0.md)
- [`BACKGROUND.md`](BACKGROUND.md)
- [`PLATFORMS.md`](PLATFORMS.md)
- [`UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)
