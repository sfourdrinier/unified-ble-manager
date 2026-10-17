// src/core/native-recording-bytes.ts
import { contractError } from '../backend-contract/errors'
import { utf8ByteLength } from '../backend-contract/serializable'

interface Span {
  readonly start: number
  readonly end: number
}

function malformed(): never {
  throw contractError('protocol.malformed', 'restoration', 'continuation.recording.serialized-bytes')
}

/** Used only after the bounded native envelope JSON has passed syntax validation.
 * Keep the original numeric lexemes: reserializing parsed floats changes lengths
 * between Serde and JavaScript. No second JSON value or numeric codec is created.
 */
export function nativeRecordingRecordBytes(text: string): number {
  const space = (index: number) => {
    while (index < text.length && /\s/.test(text[index] ?? '')) index++
    return index
  }
  const stringEnd = (start: number) => {
    let index = start + 1
    while (index < text.length) {
      if (text[index] === '\\') index += 2
      else if (text[index++] === '"') return index
    }
    return malformed()
  }
  const valueEnd = (start: number) => {
    if (text[start] === '"') return stringEnd(start)
    if (text[start] !== '{' && text[start] !== '[') {
      let index = start
      while (index < text.length && !/[\s,}\]]/.test(text[index] ?? '')) index++
      if (index === start) return malformed()
      return index
    }
    let depth = 0
    let index = start
    while (index < text.length) {
      const character = text[index]
      if (character === '"') {
        index = stringEnd(index)
        continue
      }
      if (character === '{' || character === '[') depth++
      else if (character === '}' || character === ']') {
        depth--
        if (depth === 0) return index + 1
      }
      index++
    }
    return malformed()
  }
  const fields = (span: Span) => {
    if (text[span.start] !== '{') return malformed()
    const result = new Map<string, Span>()
    let index = space(span.start + 1)
    while (text[index] !== '}') {
      if (text[index] !== '"') return malformed()
      const end = stringEnd(index)
      const key: unknown = JSON.parse(text.slice(index, end))
      if (typeof key !== 'string' || result.has(key)) return malformed()
      index = space(end)
      if (text[index] !== ':') return malformed()
      const start = space(index + 1)
      index = valueEnd(start)
      result.set(key, { start, end: index })
      index = space(index)
      if (text[index] === ',') index = space(index + 1)
      else if (text[index] !== '}') return malformed()
    }
    if (index + 1 !== span.end) return malformed()
    return result
  }
  const rootStart = space(0)
  const root = fields({ start: rootStart, end: valueEnd(rootStart) })
  const value = root.get('value')
  if (value === undefined) return malformed()
  const records = fields(value).get('records')
  if (records === undefined || text[records.start] !== '[') return malformed()
  let index = space(records.start + 1)
  let bytes = 0
  while (text[index] !== ']') {
    const end = valueEnd(index)
    bytes += utf8ByteLength(text.slice(index, end))
    index = space(end)
    if (text[index] === ',') index = space(index + 1)
    else if (text[index] !== ']') return malformed()
  }
  if (index + 1 !== records.end) return malformed()
  return bytes
}
