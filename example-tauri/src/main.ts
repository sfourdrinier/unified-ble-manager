import { createTauriBleManager } from 'unified-ble-manager/tauri'
import { readBatteryLevel } from './battery'

const output = document.querySelector<HTMLPreElement>('#output')!
const button = document.querySelector<HTMLButtonElement>('#run')!

function log(value: unknown): void {
  output.textContent += `${JSON.stringify(value)}\n`
}

button.addEventListener('click', async () => {
  button.disabled = true
  const manager = await createTauriBleManager()
  let scan: Awaited<ReturnType<typeof manager.scan>> | null = null
  let connection: Awaited<ReturnType<typeof manager.connect>> | null = null
  try {
    log(await manager.adapter.state())
    log(manager.capabilities.list())
    scan = await manager.scan()
    const first = await scan.observations[Symbol.asyncIterator]().next()
    if (first.done || first.value.kind !== 'value') throw new Error('No BLE peer observed')
    await scan.stop()
    scan = null
    connection = await manager.connect(first.value.peer, { timeoutMs: 10_000 })
    const gatt = await connection.discover({ timeoutMs: 10_000 })
    log(await readBatteryLevel(gatt, { timeoutMs: 5_000 }))
  } catch (error) {
    log(error instanceof Error ? error.message : error)
  } finally {
    if (scan !== null) await scan.stop()
    if (connection !== null) await connection.release()
    await manager.destroy()
    button.disabled = false
  }
})
