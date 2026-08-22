<!-- docs/PROFILES_AND_COMMANDS.md -->

# Profiles, codecs, and GATT commands

> Application code should follow the object-based `GattDatabase` and
> `GattCharacteristic` recipes in [`HELPERS.md`](HELPERS.md). The
> advanced/path-oriented snippets below are not the application façade and are
> not a current root-import recipe.

Optional helpers for SIG services (Heart Rate, Battery, DIS, thermometer, blood
pressure). They sit on the public `Connection`, `DiscoveredGattDatabase`, and
`Subscription` handles. They do not pick a backend or reconnect.

BLE values stay `Uint8Array`. `unified-ble-manager/codecs` is IEEE-11073 and
byte views — not Base64.

## Public subpaths

| Subpath | Contents |
| --- | --- |
| `unified-ble-manager/codecs` | `DataView`/byte-copy primitives, typed profile errors, IEEE-11073 FLOAT and SFLOAT codecs |
| `unified-ble-manager/profiles/commands` | Generic discovery, duplicate-safe characteristic lookup, read, write, and subscribe commands |
| `unified-ble-manager/profiles/standard-commands` | Read, control-point, and subscription commands for the shipped SIG profiles |
| `unified-ble-manager/profiles/heart-rate` | Heart Rate UUIDs, selectors, and measurement/body-location codecs |
| `unified-ble-manager/profiles/battery-service` | Battery Service UUIDs, selector, and level codec |
| `unified-ble-manager/profiles/device-information` | DIS UUIDs, selectors, UTF-8, System ID, and PnP ID codecs |
| `unified-ble-manager/profiles/health-thermometer` | Thermometer UUIDs, selectors, and temperature codec |
| `unified-ble-manager/profiles/blood-pressure` | Blood Pressure UUIDs, selectors, and measurement codec |
| `unified-ble-manager/profiles/ieee-11073` | IEEE-11073 value codecs for direct profile use |

The old camel-cased profile modules are not package exports and are not a
supported 4.0 import surface.

## Advanced/path-oriented reference — not root or ordinary application construction

> **Advanced/path-oriented reference — not root or ordinary application construction.**
> The following helpers operate on generation-bound paths and branded budgets.
> Keep them in maintainer, host-authoring, or specialized integration code; use
> the object-based application recipes for ordinary consumers.

## Canonical GATT path selection

Bluetooth peripherals may contain repeated services or repeated
characteristics with the same UUID. A UUID alone is therefore not an attribute
address. Commands operate on the generation-bound path returned by discovery
and fail with `gatt.ambiguous-path` when the selector is not specific enough.

```ts
import { defaultScanDelivery, firstNotification } from 'unified-ble-manager/advanced'
import { resolveCharacteristicPath } from 'unified-ble-manager/profiles/commands'
import {
  HEART_RATE_SERVICE,
  heartRateMeasurementSelector
} from 'unified-ble-manager/profiles/heart-rate'

const snapshot = await database.snapshot()

// This is valid only when the snapshot has exactly one HRS measurement.
const path = await resolveCharacteristicPath(snapshot, heartRateMeasurementSelector())

// For a repeated service, choose an occurrence from this specific snapshot.
const selectedService = snapshot.services.find(
  service => service.path.serviceUuid === HEART_RATE_SERVICE && String(service.path.serviceOccurrence) === '1'
)
if (selectedService === undefined) {
  throw new Error('The requested HRS service occurrence was not discovered.')
}
const selectedPath = await resolveCharacteristicPath(
  snapshot,
  heartRateMeasurementSelector({ serviceOccurrence: String(selectedService.path.serviceOccurrence) })
)

const bytes = await firstNotification(database, selectedPath, {
  signal: abortController.signal,
  deadline: journeyDeadline,
  delivery: defaultScanDelivery()
})
```

Selector occurrences may be copied from a current snapshot. Full paths, database
generations, and connection generations must never be constructed manually. A
profile selector only describes the match; the command always returns a path
copied from the current database snapshot.

## Standard profile commands

The standard command module decodes only after the underlying public operation
has completed successfully. It forwards the caller's `AbortSignal`, deadline,
write mode, and subscription delivery limits without changing them.

```ts
import { defaultScanDelivery, firstNotification } from 'unified-ble-manager/advanced'
import {
  readBatteryLevel,
  resetHeartRateEnergyExpended,
  subscribeHeartRateMeasurements
} from 'unified-ble-manager/profiles/standard-commands'

const delivery = defaultScanDelivery()
const operation = { signal: abortController.signal, deadline: journeyDeadline }
const batteryPercent = await readBatteryLevel(database, operation)
const subscription = await subscribeHeartRateMeasurements(database, {
  ...operation,
  delivery
}, { serviceOccurrence: '0' })
const measurement = await firstNotification(database, selectedPath, { ...operation, delivery })

try {
  for await (const item of subscription.values) {
    if (item.kind === 'value') {
      // Parse the receiver-owned bytes with parseHeartRateMeasurement(item.value.value).
    }
  }
} finally {
  const cleanup = await subscription.remove()
  if (cleanup.state === 'release-failed') {
    throw new Error('The Heart Rate subscription did not release cleanly.')
  }
}

await resetHeartRateEnergyExpended(database, { ...operation, mode: 'with-response' })
```

The subscriber owns the subscription until `remove()` settles. Closing it is
required even after an abort, stream terminal notice, or consumer error.

## Codec validation

All profile decoders accept `Readonly<Uint8Array>` and use a range-correct
`DataView`. They return structured values rather than lossy fallback numbers.
For example, IEEE-11073 finite values retain their signed mantissa and exponent,
while `nan`, `nres`, and signed infinities remain distinct tagged values.

Malformed data fails with `ProfileCodecError`:

| Code | Meaning |
| --- | --- |
| `profile.codec.truncated` | An advertised optional field or numeric value is incomplete |
| `profile.codec.malformed` | A payload has an invalid exact length or trailing bytes |
| `profile.codec.reserved` | Reserved flags, enum values, IEEE mantissas, or status bits were used |
| `profile.codec.invalid-value` | A defined field is outside its Bluetooth SIG range |

Battery Level must be exactly one byte in the range 0–100. Heart Rate,
Temperature, and Blood Pressure reject all reserved flag bits and reject
truncated optional fields. Device Information strings are strict UTF-8; System
ID and PnP ID require their exact wire lengths and reject reserved PnP vendor
sources. IEEE-11073 RFU mantissas are rejected rather than being silently
coerced to `NaN`.

## Verification

The profile suite uses `DeterministicTestBackend` through the public manager
path. It proves duplicate occurrence handling, pre-admission cancellation,
subscription removal, late-notification suppression, and final resource
cleanup without claiming live-radio evidence.
