import { BleError, type BleManager, type CleanupRecord, type ScanSession } from 'unified-ble-manager'
import { readBatteryLevel } from './battery.ts'

const SCAN_TIMEOUT_MS = 10_000

async function firstPeer(scan: ScanSession, deadline: number, report: (value: unknown) => void) {
  const iterator = scan.observations[Symbol.asyncIterator]()
  const timedOut = () => new BleError('operation.timed-out', 'scan', 'tauri-example.scan')
  let timer: ReturnType<typeof setTimeout> | undefined
  const timeout = new Promise<never>((_resolve, reject) => {
    timer = setTimeout(() => reject(timedOut()), Math.max(0, deadline - Date.now()))
  })
  try {
    while (true) {
      if (Date.now() >= deadline) throw timedOut()
      const next = await Promise.race([iterator.next(), timeout])
      if (Date.now() >= deadline) throw timedOut()
      if (next.done) throw new BleError('stream.closed', 'scan', 'tauri-example.scan')
      const item = next.value
      if (item.kind === 'value') return item.value.peer
      report(item)
      if (item.kind === 'terminal') {
        throw item.error ?? new BleError('stream.closed', 'scan', `tauri-example.scan.${item.reason}`)
      }
    }
  } finally {
    clearTimeout(timer)
  }
}

/** Always restores the control, including factory and cleanup failures. */
export async function runBatteryProofButton(
  button: Pick<HTMLButtonElement, 'disabled'>,
  proof: ReturnType<typeof createBatteryProof>,
  report: (value: unknown) => void
): Promise<void> {
  button.disabled = true
  try {
    await proof.run(report)
  } catch (error) {
    report(error)
  } finally {
    button.disabled = false
  }
}

/** Retains refused cleanup for another click; no new owner is admitted until it settles. */
export function createBatteryProof(createManager: () => Promise<BleManager>) {
  const pending: { step: string; release: () => Promise<CleanupRecord> }[] = []
  let running = false
  async function drain(): Promise<void> {
    const failures: unknown[] = []
    for (const entry of [...pending].reverse()) {
      try {
        const receipt = await entry.release()
        if (receipt.state !== 'released') throw receipt
        pending.splice(pending.indexOf(entry), 1)
      } catch (error) {
        failures.push({ step: entry.step, error })
      }
    }
    if (failures.length > 0) throw new AggregateError(failures, 'BLE proof cleanup remains owned; retry the button')
  }
  return {
    async run(report: (value: unknown) => void): Promise<void> {
      if (running) throw new Error('BLE proof is already running')
      running = true
      try {
        await drain()
        const manager = await createManager()
        pending.push({ step: 'manager.destroy', release: () => manager.destroy() })
        const failures: unknown[] = []
        try {
          report(await manager.adapter.state())
          report(manager.capabilities.list())
          const scanDeadline = Date.now() + SCAN_TIMEOUT_MS
          const scan = await manager.scan({ timeoutMs: SCAN_TIMEOUT_MS })
          const scanOwner = { step: 'scan.stop', release: () => scan.stop() }
          pending.push(scanOwner)
          const peer = await firstPeer(scan, scanDeadline, report)
          const stopped = await scan.stop()
          if (stopped.state !== 'released') throw stopped
          pending.splice(pending.indexOf(scanOwner), 1)
          const connection = await manager.connect(peer, { timeoutMs: 10_000 })
          pending.push({ step: 'connection.release', release: () => connection.release() })
          const gatt = await connection.discover({ timeoutMs: 10_000 })
          report(await readBatteryLevel(gatt, { timeoutMs: 5_000 }))
        } catch (error) {
          failures.push(error)
        }
        try {
          await drain()
        } catch (error) {
          failures.push(error)
        }
        if (failures.length > 0) throw new AggregateError(failures, 'BLE proof failed')
      } finally {
        running = false
      }
    }
  }
}
