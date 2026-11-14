// src/backends/reactnative/react-native-ordered-recording-controls.ts
import { contractError } from '../../backend-contract/errors'
import { createNativeContinuationRecordingController } from '../../core/continuation-recording'
import type { ReactNativeRustCoreSession } from './react-native-rust-core'

/** Live controls always use the authorized session; offline context is a separate native operation. */
export function createSessionOrderedRecordingControls(session: ReactNativeRustCoreSession) {
  const missing = async (): Promise<never> => {
    throw contractError('capability.unsupported', 'restoration', 'react-native.ordered-recording')
  }
  const context = session.orderedRecordingContext
  const stop = session.orderedRecordingStop
  const version = session.orderedRecordingVersion
  const ready = async () => {
    if (
      version === undefined ||
      context === undefined ||
      stop === undefined ||
      session.orderedRecordingStart === undefined ||
      session.orderedRecordingReserve === undefined ||
      session.orderedRecordingStartStatus === undefined ||
      session.orderedRecordingRetire === undefined
    )
      throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.version')
    if ((await version.call(session)) !== 1)
      throw contractError('protocol.incompatible', 'boundary', 'react-native.ordered-recording.version')
  }
  const controller = createNativeContinuationRecordingController(
    {
      status: missing,
      prepare: missing,
      acknowledge: missing,
      stop: missing,
      clear: missing,
      orderedContext: async id => {
        await ready()
        return context ? context.call(session, id) : missing()
      },
      stopOrdered: async id => {
        await ready()
        return stop ? stop.call(session, id) : missing()
      }
    },
    'ubm-mobile',
    async () => {
      await ready()
      return 1
    }
  )
  return {
    context: async (id: string) => {
      const contextOrdered = controller.contextOrdered
      return contextOrdered === undefined ? missing() : contextOrdered.call(controller, id)
    },
    stop: async (id: string) => {
      const stopOrdered = controller.stopOrdered
      return stopOrdered === undefined ? missing() : stopOrdered.call(controller, id)
    }
  }
}
