import { createTauriBleManager } from 'unified-ble-manager/tauri'
import { createBatteryProof, runBatteryProofButton } from './proof.ts'

const output = document.querySelector<HTMLPreElement>('#output')!
const button = document.querySelector<HTMLButtonElement>('#run')!

function log(value: unknown): void {
  output.textContent += `${JSON.stringify(value, (_key, field) =>
    field instanceof Error
      ? {
          ...field,
          name: field.name,
          message: field.message,
          ...(field instanceof AggregateError ? { errors: field.errors } : {})
        }
      : field
  )}\n`
}

const proof = createBatteryProof(createTauriBleManager)
button.addEventListener('click', async () => {
  await runBatteryProofButton(button, proof, log)
})
