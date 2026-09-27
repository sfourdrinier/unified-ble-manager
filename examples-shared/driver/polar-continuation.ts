import type { BackgroundContinuationDeclaration } from 'unified-ble-manager'
import { HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC } from 'unified-ble-manager/profiles/heart-rate'
import {
  PMD_SERVICE,
  PMD_CONTROL_POINT,
  PMD_DATA,
  POLAR_PREFERRED_MTU,
  buildStartEcgCommand,
  buildStopEcgCommand,
  buildStartAccCommand,
  buildStopAccCommand,
  type H10AccSettings
} from './polar-pmd.ts'

type Declaration = BackgroundContinuationDeclaration
type SetupStep = NonNullable<Declaration['setup']>[number]

function selector(serviceUuid: string, characteristicUuid: string) {
  return { serviceUuid, characteristicUuid, serviceOccurrence: 1, characteristicOccurrence: 1 }
}

/** H10 protocol recipe at the application boundary. The native library knows
 * neither Polar UUIDs nor measurement commands. Validate advertised features
 * and selected settings in the foreground before arming this standing order.
 * On a cold wake, rejection is reported rather than silently choosing settings. */
export function buildH10Continuation(options: {
  readonly peerId?: string
  readonly ecg: boolean
  readonly acc?: H10AccSettings
}): Declaration {
  const resubscribe = [selector(HEART_RATE_SERVICE, HEART_RATE_MEASUREMENT_CHARACTERISTIC)]
  const setup: SetupStep[] = []
  if (options.ecg || options.acc !== undefined) {
    const control = selector(PMD_SERVICE, PMD_CONTROL_POINT)
    resubscribe.push(control, selector(PMD_SERVICE, PMD_DATA))
    const command = (value: Uint8Array, accepted: readonly number[]): SetupStep => {
      const opcode = value[0]
      const measurement = value[1]
      if (opcode === undefined || measurement === undefined) throw new Error('PMD command has no correlation header')
      return {
        selector: control,
        value,
        timeoutMs: 3000,
        response: {
          subscriptionIndex: 1,
          prefix: new Uint8Array([0xf0, opcode, measurement]),
          minLength: 4,
          maxLength: 5,
          status: { offset: 3, accepted },
          trailing: { offset: 4, accepted: [0] }
        }
      }
    }
    // STOP accepts already-stopped; START must positively confirm our settings.
    if (options.ecg) setup.push(command(buildStopEcgCommand(), [0, 6]), command(buildStartEcgCommand(), [0]))
    if (options.acc !== undefined) {
      setup.push(command(buildStopAccCommand(), [0, 6]), command(buildStartAccCommand(options.acc), [0]))
    }
  }
  return {
    onAppearance: 'native',
    ...(options.peerId === undefined ? {} : { peerId: options.peerId }),
    resubscribe,
    setup,
    ...(setup.length === 0
      ? {}
      : {
          link: {
            mtu: { requested: POLAR_PREFERRED_MTU, timeoutMs: 10000, onUnsupported: 'continue' }
          }
        })
  }
}
