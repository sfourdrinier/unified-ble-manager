import { contractError } from '../../backend-contract/errors'
import { awaitWithOperationAdmission } from '../../core/unified-ble-core-helpers'
import type { BlePeer, ChooseOptions } from '../../public/ble-manager'
import { normalizeOperationOptions } from '../../public/operation-options'
import type { ReactNativeRustCoreBinding } from './react-native-rust-core'
import { nativeChooserFilters } from './react-native-native-chooser-filters'

const REVISION = 'ubm-accessory-chooser/1'
const OPERATION = 'react-native.accessory-chooser'
export const ACCESSORY_CANCELLATION_DRAIN_MS = 1000
function isAborted(signal: AbortSignal | null): boolean {
  return signal?.aborted === true
}

/** ASK authorizes an accessory, not a scan, connection or restoration event.
 * A native setup that completes after cancellation remains authorized by the OS;
 * this layer never silently revokes a person's choice or publishes a late peer. */
export function createReactNativeAccessoryChooser(
  binding: ReactNativeRustCoreBinding,
  peerId: (nativeIdentifier: string) => string,
  now: () => number,
  admit: (requestId: string, cancel: () => Promise<void>) => () => void = () => () => undefined
): (options: ChooseOptions) => Promise<BlePeer> {
  return async options => {
    const operation = normalizeOperationOptions({ ...options, timeoutMs: options.timeoutMs ?? 60000 }, now)
    const choose = binding.chooseAccessory
    const cancel = binding.cancelAccessoryChoice
    if (choose === undefined || cancel === undefined) {
      throw contractError('capability.unsupported', 'chooser', OPERATION)
    }
    if (isAborted(operation.signal)) throw contractError('operation.aborted', 'chooser', OPERATION)
    const filters = nativeChooserFilters(options, 'apple')
    const bytes = await awaitWithOperationAdmission(binding.randomBytes(16), operation, now, OPERATION)
    const requestId = Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('')
    const deadline = operation.deadline
    if (deadline === null || deadline <= now()) throw contractError('operation.timed-out', 'chooser', OPERATION)
    if (isAborted(operation.signal)) throw contractError('operation.aborted', 'chooser', OPERATION)
    const retire = admit(requestId, () => cancel.call(binding, requestId))
    let pending: Promise<string>
    try {
      pending = choose.call(
        binding,
        requestId,
        JSON.stringify({ revision: REVISION, filters }),
        Math.max(1, Math.floor(deadline - now()))
      )
    } catch (error) {
      retire()
      throw error
    }
    // Keep failed cancellation owned until the native operation's terminal
    // answer or an explicit destroy retry confirms release.
    pending = pending.then(
      value => {
        retire()
        return value
      },
      error => {
        retire()
        throw error
      }
    )
    let text: string
    try {
      text = await awaitWithOperationAdmission(pending, operation, now, OPERATION)
      if (isAborted(operation.signal)) throw contractError('operation.aborted', 'chooser', OPERATION)
      if (deadline <= now()) throw contractError('operation.timed-out', 'chooser', OPERATION)
    } catch (error) {
      if (isAborted(operation.signal) || deadline <= now()) {
        try {
          await awaitWithOperationAdmission(
            cancel.call(binding, requestId),
            normalizeOperationOptions({ timeoutMs: ACCESSORY_CANCELLATION_DRAIN_MS }, now),
            now,
            `${OPERATION}.cancel`
          )
        } catch (cleanupError) {
          throw new AggregateError([error, cleanupError], `${OPERATION}.cancel-failed`)
        }
      }
      throw error
    }
    const selected = parseSelection(text)
    return Object.freeze({
      id: peerId(selected.peripheralIdentifier),
      name: selected.name,
      rssi: null,
      reference: null,
      sources: Object.freeze(['origin-authorized'] as const),
      lastAdvertisement: null
    })
  }
}

function parseSelection(text: string): { readonly peripheralIdentifier: string; readonly name: string } {
  const malformed = (): never => {
    throw contractError('protocol.malformed', 'chooser', OPERATION)
  }
  let value: unknown
  try {
    value = JSON.parse(text)
  } catch {
    return malformed()
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return malformed()
  const revision = Reflect.get(value, 'revision')
  const peripheralIdentifier = Reflect.get(value, 'peripheralIdentifier')
  const name = Reflect.get(value, 'name')
  if (
    Object.keys(value).length !== 3 ||
    revision !== REVISION ||
    typeof peripheralIdentifier !== 'string' ||
    !/^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$/i.test(peripheralIdentifier) ||
    typeof name !== 'string' ||
    name.length > 1024
  )
    return malformed()
  return { peripheralIdentifier, name }
}
