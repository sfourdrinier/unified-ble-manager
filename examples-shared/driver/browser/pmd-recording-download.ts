import type { JsonObject } from '../protocol.ts'
import { isJsonObject } from '../protocol.ts'
import { isPmdRecording } from '../pmd-recording.ts'

export function preparePmdDownload(
  value: unknown,
  now = new Date()
): {
  readonly filename: string
  readonly contents: string
  readonly summary: JsonObject
} {
  if (!isPmdRecording(value) || !isJsonObject(value.summary)) throw new Error('invalid PMD recording export')
  return {
    filename: `ubm-pmd-${now.toISOString().replace(/[:.]/g, '-')}.json`,
    contents: JSON.stringify(value),
    summary: value.summary
  }
}
