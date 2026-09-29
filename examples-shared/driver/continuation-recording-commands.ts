import type { ContinuationRecordingController } from 'unified-ble-manager'
import type { JsonValue } from './protocol.ts'
import { toJsonValue } from './protocol.ts'
import { args, defineCommand, ScenarioError, type ScenarioCommand } from './scenario-core.ts'
import { requiredText } from './continuation-arguments.ts'

function field(value: JsonValue, key: string): unknown {
  return typeof value === 'object' && value !== null ? Reflect.get(value, key) : undefined
}

/** Shared explicit offline operations. No automatic ACK, deletion or radio admission. */
export function continuationRecordingCommands(access: () => Promise<ContinuationRecordingController>): Readonly<Record<string, ScenarioCommand>> {
  const operation = async (run: (controller: ContinuationRecordingController) => Promise<unknown>): Promise<JsonValue> => {
    const result = toJsonValue(await run(await access()))
    if (typeof result !== 'object' || result === null || Array.isArray(result)) throw new ScenarioError('protocol.malformed', 'Continuation response must be an object')
    return result
  }
  return {
    'recording-status': defineCommand({
      label: 'Recording status', description: 'Read the app-private journal by explicit recordingId without opening a BLE session.',
      parse: raw => requiredText(raw, 'recordingId'),
      run: id => operation(controller => controller.status(id))
    }),
    'recording-prepare': defineCommand({
      label: 'Prepare recording batch', description: 'Read a retained batch by recordingId; no acknowledgement or deletion is performed. Retain the returned token until records are safely handled.',
      parse: raw => ({ id: requiredText(raw, 'recordingId'), maxItems: args.number(raw, 'maxItems', 128), maxBytes: args.number(raw, 'maxBytes', 262144) }),
      run: options => operation(controller => controller.prepare(options.id, { maxItems: options.maxItems, maxBytes: options.maxBytes })),
      summarizeResult: result => {
        const records = field(result, 'records')
        return { records: Array.isArray(records) ? records.length : 0, bytes: toJsonValue(field(result, 'bytes')), more: toJsonValue(field(result, 'more')) }
      }
    }),
    'recording-acknowledge': defineCommand({
      label: 'Acknowledge recording batch', description: 'Explicitly acknowledge recordingId and token only after saving or processing that prepared batch.',
      parse: raw => ({ id: requiredText(raw, 'recordingId'), token: requiredText(raw, 'token') }),
      run: options => operation(controller => controller.acknowledge(options.id, options.token))
    }),
    'recording-stop': defineCommand({
      label: 'Stop recording admission', description: 'Stop journal admission by recordingId; retains records and does not release the radio. Disarm the standing order separately.',
      parse: raw => requiredText(raw, 'recordingId'),
      run: id => operation(controller => controller.stop(id))
    }),
    'recording-clear': defineCommand({
      label: 'Delete stopped recording', description: 'Explicitly delete retained data for a stopped recordingId. The public API refuses an active recording.',
      parse: raw => requiredText(raw, 'recordingId'),
      run: id => operation(controller => controller.clear(id))
    }),
  }
}
