import type { GattDatabase, OperationOptions } from 'unified-ble-manager'

/** The executable counterpart of the battery read taught in docs/TAURI.md. */
export async function readBatteryLevel(gatt: GattDatabase, options: OperationOptions = {}): Promise<number> {
  const characteristic = gatt.characteristic('180f', '2a19')
  const value = await characteristic.read(options)
  if (value.length < 1) throw new Error('Battery Level returned an empty value')
  return value[0]
}
