import { createTauriBleManager } from 'unified-ble-manager/tauri'

const output = document.querySelector<HTMLPreElement>('#output')!
const button = document.querySelector<HTMLButtonElement>('#run')!

function log(value: unknown): void {
  output.textContent += `${JSON.stringify(value)}\n`
}

button.addEventListener('click', async () => {
  button.disabled = true
  const manager = await createTauriBleManager()
  let scan: Awaited<ReturnType<typeof manager.scan>> | null = null
  try {
    log(await manager.adapter.state())
    log(manager.capabilities.list())
    scan = await manager.scan()
    const first = await scan.observations[Symbol.asyncIterator]().next()
    if (first.done || first.value.kind !== 'value') throw new Error('No BLE peer observed')
    await scan.stop()
    scan = null
  } catch (error) {
    log(error instanceof Error ? error.message : error)
  } finally {
    if (scan !== null) await scan.stop()
    await manager.destroy()
    button.disabled = false
  }
})
