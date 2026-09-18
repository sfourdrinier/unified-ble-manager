// examples-shared/driver/scenarios/device-info.ts
//
// Battery Service and Device Information reads through the public profile
// helpers. Every characteristic is read independently and reports its own
// outcome, so one missing characteristic never hides the others.

import type { GattDatabase } from 'unified-ble-manager'
import { BATTERY_LEVEL_CHARACTERISTIC, BATTERY_SERVICE, parseBatteryLevel } from 'unified-ble-manager/profiles/battery-service'
import {
  DEVICE_INFORMATION_SERVICE,
  FIRMWARE_REVISION_CHARACTERISTIC,
  HARDWARE_REVISION_CHARACTERISTIC,
  MANUFACTURER_NAME_CHARACTERISTIC,
  MODEL_NUMBER_CHARACTERISTIC,
  PNP_ID_CHARACTERISTIC,
  SERIAL_NUMBER_CHARACTERISTIC,
  SOFTWARE_REVISION_CHARACTERISTIC,
  SYSTEM_ID_CHARACTERISTIC,
  decodeDeviceInformationString,
  parsePnpId,
  parseSystemId
} from 'unified-ble-manager/profiles/device-information'
import type { DriverHost } from '../host.ts'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { bytesToHex, toJsonValue } from '../protocol.ts'
import { defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import { BleScenario, DEVICE_ARGUMENT_HELP, IDLE_BLE_STATE, OPERATION_TIMEOUT_MS, outcomeOf, parseDevice, type BleScenarioState } from './ble-scenario.ts'

type ReadSpec = {
  readonly field: string
  readonly service: string
  readonly characteristic: string
  readonly decode: (bytes: Uint8Array) => JsonValue
}

const READS: readonly ReadSpec[] = [
  { field: 'batteryLevelPercent', service: BATTERY_SERVICE, characteristic: BATTERY_LEVEL_CHARACTERISTIC, decode: parseBatteryLevel },
  { field: 'manufacturerName', service: DEVICE_INFORMATION_SERVICE, characteristic: MANUFACTURER_NAME_CHARACTERISTIC, decode: decodeDeviceInformationString },
  { field: 'modelNumber', service: DEVICE_INFORMATION_SERVICE, characteristic: MODEL_NUMBER_CHARACTERISTIC, decode: decodeDeviceInformationString },
  { field: 'serialNumber', service: DEVICE_INFORMATION_SERVICE, characteristic: SERIAL_NUMBER_CHARACTERISTIC, decode: decodeDeviceInformationString },
  { field: 'hardwareRevision', service: DEVICE_INFORMATION_SERVICE, characteristic: HARDWARE_REVISION_CHARACTERISTIC, decode: decodeDeviceInformationString },
  { field: 'firmwareRevision', service: DEVICE_INFORMATION_SERVICE, characteristic: FIRMWARE_REVISION_CHARACTERISTIC, decode: decodeDeviceInformationString },
  { field: 'softwareRevision', service: DEVICE_INFORMATION_SERVICE, characteristic: SOFTWARE_REVISION_CHARACTERISTIC, decode: decodeDeviceInformationString },
  { field: 'systemId', service: DEVICE_INFORMATION_SERVICE, characteristic: SYSTEM_ID_CHARACTERISTIC, decode: bytes => toJsonValue(parseSystemId(bytes)) },
  { field: 'pnpId', service: DEVICE_INFORMATION_SERVICE, characteristic: PNP_ID_CHARACTERISTIC, decode: bytes => toJsonValue(parsePnpId(bytes)) }
]

export type DeviceInfoState = BleScenarioState & {
  readonly reads: JsonObject
}

export class DeviceInfoScenario extends BleScenario<DeviceInfoState> {
  readonly id = 'device-info'
  readonly title = 'Battery + device information'
  readonly description = 'Connect to the H10, read Battery Level (180F/2A19) and every Device Information (180A) characteristic, release.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    read: defineCommand({
      label: 'Read all',
      description: `One-shot connect, read, release. args: {${DEVICE_ARGUMENT_HELP}}. Result: {peer, reads}; each read reports {ok, value, raw} or {ok: false, error}.`,
      acceptsDevice: true,
      parse: raw => ({ device: parseDevice(raw) }),
      run: ({ device }) =>
        this.runJourney(async signal => {
          const { gatt } = await this.connectH10(device, signal)
          this.patchBase({ phase: 'reading' })
          const reads = await this.readAll(gatt, signal)
          await this.teardown('done')
          return { peer: this.peerReport(), reads }
        })
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, { ...IDLE_BLE_STATE, reads: {} })
  }

  override headline(): string | null {
    const battery = this.snapshot().reads.batteryLevelPercent
    return battery !== null && typeof battery === 'object' && !Array.isArray(battery) && 'value' in battery && typeof battery.value === 'number'
      ? `battery ${battery.value.toString()}%`
      : this.snapshot().phase
  }

  private async readAll(gatt: GattDatabase, signal: AbortSignal): Promise<JsonObject> {
    const reads: Record<string, JsonValue> = {}
    for (const spec of READS) {
      const outcome = await outcomeOf(async () => {
        const bytes = await gatt.characteristic(spec.service, spec.characteristic).read({ signal, timeoutMs: OPERATION_TIMEOUT_MS })
        return { value: spec.decode(bytes), raw: bytesToHex(bytes) }
      })
      const entry: JsonValue = outcome.ok ? { ok: true, ...outcome.value } : { ok: false, error: outcome.error }
      reads[spec.field] = entry
      this.emit('read', { field: spec.field, outcome: entry })
      this.replace({ ...this.snapshot(), reads: { ...reads } })
    }
    return reads
  }
}
