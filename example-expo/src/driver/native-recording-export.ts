import { Directory, File, Paths } from 'expo-file-system'
import { Platform } from 'react-native'
import type { JsonObject } from './shared'
import { exportRecordingFile, persistRecordingDocument } from './recording-file-export'

let exportSequence = 0

/** Local-only: no network upload or text-based sharing fallback. */
export function exportNativeRecording(recording: JsonObject) {
  return exportRecordingFile(recording, {
    async writeDocument(contents) {
      const directory = new Directory(Paths.document, 'ubm-recordings')
      directory.create({ idempotent: true, intermediates: true })
      const file = new File(directory, `h10-recording-${Date.now()}-${++exportSequence}.json`)
      return persistRecordingDocument(file, contents)
    },
    async isSharingAvailable() {
      // ExpoSharing's pod supports iOS, not tvOS. Do not eagerly import its
      // requireNativeModule on TV; a local file remains a valid export there.
      if (Platform.isTV) return false
      const Sharing = await import('expo-sharing')
      return Sharing.isAvailableAsync()
    },
    async shareDocument(uri) {
      const Sharing = await import('expo-sharing')
      await Sharing.shareAsync(uri, {
        mimeType: 'application/json',
        UTI: 'public.json',
        dialogTitle: 'Export H10 recording'
      })
    }
  })
}
