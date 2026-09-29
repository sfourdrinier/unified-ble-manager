import type { JsonObject } from './shared'

export interface RecordingFilePort {
  writeDocument(contents: string): Promise<string>
  isSharingAvailable(): Promise<boolean>
  shareDocument(uri: string): Promise<void>
}

export type RecordingFileResult =
  | { readonly uri: string; readonly sharing: 'closed' | 'unavailable' }
  | { readonly uri: string; readonly sharing: 'failed'; readonly error: string }

interface RecordingDocument {
  readonly uri: string
  readonly exists: boolean
  create(options: { overwrite: false }): void
  write(contents: string): void
  text(): Promise<string>
}

/** Structural boundary implemented directly by Expo File, testable without a phone. */
export async function persistRecordingDocument(file: RecordingDocument, contents: string): Promise<string> {
  try {
    file.create({ overwrite: false })
    file.write(contents)
    if (!file.exists || (await file.text()) !== contents)
      throw new Error('File verification did not match the exported recording')
    return file.uri
  } catch (error) {
    throw new Error(
      `Recording file was not verified at ${file.uri}: ${error instanceof Error ? error.message : String(error)}`
    )
  }
}

/** Persist before sharing. A void share result proves neither external save nor
 * user acceptance; closing/cancelling the sheet deliberately has one outcome. */
export async function exportRecordingFile(
  recording: JsonObject,
  port: RecordingFilePort
): Promise<RecordingFileResult> {
  const uri = await port.writeDocument(JSON.stringify(recording))
  try {
    if (!(await port.isSharingAvailable())) return { uri, sharing: 'unavailable' }
    await port.shareDocument(uri)
    return { uri, sharing: 'closed' }
  } catch (error) {
    return { uri, sharing: 'failed', error: error instanceof Error ? error.message : String(error) }
  }
}
