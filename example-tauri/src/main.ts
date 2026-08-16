import { Channel, invoke } from '@tauri-apps/api/core'
import { createTauriBleManager } from 'unified-ble-manager/tauri'

const output = document.querySelector<HTMLPreElement>('#output')!
const button = document.querySelector<HTMLButtonElement>('#run')!

function log(value: unknown): void {
  output.textContent += `${JSON.stringify(value)}\n`
}

button.addEventListener('click', async () => {
  button.disabled = true
  const manager = await createTauriBleManager({ invoke, Channel })
  let scan: Awaited<ReturnType<typeof manager.scan>> | null = null
  let connection: Awaited<ReturnType<typeof manager.connect>> | null = null
  try {
    log(await manager.adapterState())
    scan = await manager.scan()
    const first = await scan.observations[Symbol.asyncIterator]().next()
    if (first.done || first.value.kind !== 'value') throw new Error('No BLE peer observed')
    await scan.stop()
    scan = null

    connection = await manager.connect(first.value.value.peerId, { timeoutMs: 15_000 })
    const database = await connection.discover({ timeoutMs: 15_000 })
    const readable = database.characteristics.find(item => item.record.properties.includes('read'))
    if (readable !== undefined) log(await readable.read({ timeoutMs: 5_000 }))

    const notifiable = database.characteristics.find(item =>
      item.record.properties.some(property => property === 'notify' || property === 'indicate')
    )
    if (notifiable !== undefined) {
      const subscription = await notifiable.subscribe({ timeoutMs: 5_000 })
      const notification = await subscription.values[Symbol.asyncIterator]().next()
      log(notification)
      await subscription.remove()
    }
  } catch (error) {
    log(error instanceof Error ? error.message : error)
  } finally {
    if (scan !== null) await scan.stop()
    if (connection !== null) await connection.disconnect()
    await manager.destroy()
    button.disabled = false
  }
})
