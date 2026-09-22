// examples-shared/driver/scenarios/mtu.ts
//
// Reports what the platform says about MTU and write length before and after
// asking for a larger MTU. Each probe carries the API's own answer
// (`measured` / `unavailable` / `unsupported`, with limitations); a thrown
// error is reported as that probe's outcome.

import type { BleConnection } from 'unified-ble-manager'
import type { DriverHost } from '../host.ts'
import type { JsonObject, JsonValue } from '../protocol.ts'
import { toJsonValue } from '../protocol.ts'
import { args, defineCommand, type ScenarioCommand } from '../scenario-core.ts'
import { BleScenario, DEVICE_ARGUMENT_HELP, IDLE_BLE_STATE, OPERATION_TIMEOUT_MS, outcomeOf, parseDevice, type BleScenarioState } from './ble-scenario.ts'

export type MtuState = BleScenarioState & {
  readonly requestedMtu: number | null
  readonly probes: JsonObject
}

export class MtuScenario extends BleScenario<MtuState> {
  readonly id = 'mtu'
  readonly title = 'MTU / write length'
  readonly description = 'effectiveMtu, maximumWriteLength (both modes), requestMtu(n), then the same reads again; plus PHY, parameters and RSSI.'
  protected readonly commands: Readonly<Record<string, ScenarioCommand>> = {
    probe: defineCommand({
      label: 'Probe',
      description: `args: {mtu?: number (23..517, default 517), ${DEVICE_ARGUMENT_HELP}}. Result: {peer, probes}.`,
      presets: [
        { label: 'Probe, request 517', args: {} },
        { label: 'Probe, request 247', args: { mtu: 247 } }
      ],
      acceptsDevice: true,
      parse: raw => ({ mtu: args.number(raw, 'mtu', 517, { min: 23, max: 517 }), device: parseDevice(raw) }),
      run: ({ mtu, device }) =>
        this.runJourney(async signal => {
          this.replace({ ...this.snapshot(), requestedMtu: mtu })
          const { manager, connection } = await this.connectH10(device, signal)
          this.patchBase({ phase: 'probing' })
          const capabilities = manager.capabilities
            .list()
            .filter(descriptor => /mtu|write|phy|rssi|connection-parameters|priority/i.test(String(descriptor.id)))
          await this.probe('capabilities', async () => toJsonValue(capabilities))
          await this.probeLink(connection, 'before')
          await this.probe('requestMtu', async () => toJsonValue(await connection.controls.requestMtu(mtu, { signal, timeoutMs: OPERATION_TIMEOUT_MS })))
          await this.probeLink(connection, 'after')
          await this.probe('readPhy', async () => toJsonValue(await connection.controls.readPhy({ signal, timeoutMs: OPERATION_TIMEOUT_MS })))
          await this.probe('parameters', async () => toJsonValue(await connection.controls.parameters()))
          await this.probe('readRssi', async () => toJsonValue(await connection.controls.readRssi({ signal, timeoutMs: OPERATION_TIMEOUT_MS })))
          const probes = this.snapshot().probes
          await this.teardown('done')
          return { peer: this.peerReport(), probes }
        })
    }),
    stop: this.stopCommand
  }

  constructor(host: DriverHost) {
    super(host, { ...IDLE_BLE_STATE, requestedMtu: null, probes: {} })
  }

  override headline(): string | null {
    const after = this.snapshot().probes['effectiveMtu.after']
    if (after !== undefined && after !== null && typeof after === 'object' && !Array.isArray(after) && 'value' in after) {
      const value = after.value
      if (value !== null && typeof value === 'object' && !Array.isArray(value) && 'attMtu' in value) return `ATT MTU ${JSON.stringify(value.attMtu)}`
    }
    return this.snapshot().phase
  }

  private async probeLink(connection: BleConnection, when: 'before' | 'after'): Promise<void> {
    await this.probe(`effectiveMtu.${when}`, async () => toJsonValue(await connection.controls.effectiveMtu()))
    await this.probe(`maximumWriteLength.with-response.${when}`, async () => toJsonValue(await connection.controls.maximumWriteLength('with-response')))
    await this.probe(`maximumWriteLength.without-response.${when}`, async () => toJsonValue(await connection.controls.maximumWriteLength('without-response')))
  }

  private async probe(name: string, run: () => Promise<JsonValue>): Promise<void> {
    const outcome = await outcomeOf(run)
    const entry: JsonValue = outcome.ok ? { ok: true, value: outcome.value } : { ok: false, error: outcome.error }
    this.emit('probe', { name, outcome: entry })
    this.replace({ ...this.snapshot(), probes: { ...this.snapshot().probes, [name]: entry } })
  }
}
